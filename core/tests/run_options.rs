#![expect(clippy::unwrap_used, reason = "assertions are the vocabulary of tests")]

//! `RunOptions` behaviour: the API port agents are handed, the `current`
//! symlink gate, and run-directory cleanup on stop. Both tests drive a real
//! `Run` against a fake `claude` on PATH — the port an agent receives is only
//! observable in the spawned process's environment.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Once;

use coretempo_core::run::{Run, RunOptions};
use coretempo_core::types::config::ServerOverrides;
use coretempo_core::workflow::{load_workflow, resolve_server};

/// HOME and the `current` symlink are process-global, so the tests in this
/// binary run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static SETUP: Once = Once::new();

fn root() -> PathBuf {
    std::env::temp_dir().join(format!("coretempo-run-options-{}", std::process::id()))
}

/// Points HOME at a scratch runs dir and puts a fake `claude` on PATH that
/// records the `CORETEMPO_PORT` it was spawned with, then idles.
fn setup() -> PathBuf {
    let root = root();
    SETUP.call_once(|| {
        let bin = root.join("bin");
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("claude");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\nprintf '%s' \"$CORETEMPO_PORT\" > '{}'/port-\"$CORETEMPO_AGENT_ID\"\n\
                 printf '> '\nsleep 300\n",
                root.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        // SAFETY: guarded by `Once` and by SERIAL; HOME/PATH are set before any
        // core code in this binary runs.
        unsafe {
            std::env::set_var("HOME", root.join("home"));
            let path = std::env::var("PATH").unwrap();
            std::env::set_var("PATH", format!("{}:{path}", bin.display()));
        }
    });
    root
}

fn runs_dir() -> PathBuf {
    root().join("home/.coretempo/runs")
}

/// Writes a one-agent workflow named `name` (also the agent id) on `port`.
fn write_workflow(root: &Path, name: &str, port: u16) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let path = root.join(format!("{name}.toml"));
    std::fs::write(
        &path,
        format!(
            "[workflow]\nname = \"{name}\"\nport = {port}\ndb = \"{db}\"\n\
             [agents.{name}]\ndir = \"{dir}\"\nprompt = \"p\"\n",
            db = root.join(format!("{name}.db")).display(),
            dir = dir.display(),
        ),
    )
    .unwrap();
    path
}

async fn start(config: &Path, options: RunOptions) -> std::sync::Arc<Run> {
    let (file, frozen) = load_workflow(config).unwrap();
    let server = resolve_server(
        ServerOverrides::default(),
        ServerOverrides::default(),
        &file,
    )
    .unwrap();
    Run::start_with(frozen, server, options).await.unwrap()
}

/// The port recorded by the fake agent; the child runs asynchronously.
async fn agent_port(root: &Path, agent: &str) -> String {
    let path = root.join(format!("port-{agent}"));
    for _ in 0..100_u32 {
        if let Ok(text) = std::fs::read_to_string(&path)
            && !text.trim().is_empty()
        {
            return text.trim().to_string();
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    String::new()
}

fn api_json_port(run: &Run) -> u16 {
    let path = runs_dir().join(&run.run_id().0).join("api.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    u16::try_from(parsed["port"].as_u64().unwrap()).unwrap()
}

fn health_ok(port: u16) -> bool {
    let Ok(mut sock) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    write!(
        sock,
        "GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut resp = String::new();
    sock.read_to_string(&mut resp).unwrap();
    resp.starts_with("HTTP/1.1 200")
}

#[tokio::test(flavor = "multi_thread")]
async fn ephemeral_run_gives_agents_the_bound_port() {
    let _serial = SERIAL.lock().await;
    let root = setup();
    // Hold the configured port for the whole test: an ephemeral bind cannot
    // land on it, so `!=` below is a fact rather than a coin flip.
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let configured = held.local_addr().unwrap().port();

    let config = write_workflow(&root, "ephemeral", configured);
    let run = start(
        &config,
        RunOptions {
            ephemeral_port: true,
            repoint_current: true,
            cleanup_run_dir: false,
        },
    )
    .await;

    let bound = api_json_port(&run);
    assert_ne!(
        bound, configured,
        "ephemeral run reused the configured port"
    );
    assert_ne!(bound, 0, "api.json recorded the pre-bind placeholder port");
    assert!(health_ok(bound), "nothing serving /v1/health on {bound}");

    // The agent must be handed the port the API actually bound.
    let seen = agent_port(&root, "ephemeral").await;
    assert_eq!(
        seen,
        bound.to_string(),
        "agent got the wrong CORETEMPO_PORT"
    );

    // repoint_current: true is the default path — `current` follows this run.
    assert_eq!(
        std::fs::read_link(runs_dir().join("current")).unwrap(),
        PathBuf::from(&run.run_id().0)
    );

    run.stop().await.unwrap();
    drop(held);
}

#[tokio::test(flavor = "multi_thread")]
async fn cleanup_run_dir_removes_artifacts_on_stop() {
    let _serial = SERIAL.lock().await;
    let root = setup();
    let port = {
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };

    let config = write_workflow(&root, "cleanup", port);
    let run = start(
        &config,
        RunOptions {
            ephemeral_port: false,
            repoint_current: false,
            cleanup_run_dir: true,
        },
    )
    .await;

    assert_eq!(
        api_json_port(&run),
        port,
        "configured port was not honoured"
    );
    let dir = runs_dir().join(&run.run_id().0);
    assert!(dir.join("api.json").is_file());

    run.stop().await.unwrap();

    assert!(!dir.exists(), "{} survived stop", dir.display());
    assert_ne!(
        std::fs::read_link(runs_dir().join("current")).ok(),
        Some(PathBuf::from(&run.run_id().0)),
        "current was repointed despite repoint_current: false"
    );
}
