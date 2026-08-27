#![expect(clippy::unwrap_used, reason = "assertions are the vocabulary of tests")]

//! `RunOptions` behaviour: the API port agents are handed, the `current`
//! symlink gate, and run-directory cleanup on stop. Both tests drive a real
//! `Run` against a fake `claude` on PATH — the port an agent receives is only
//! observable in the spawned process's environment.

mod support;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

use coretempo_core::run::RunOptions;
use coretempo_core::trust::TrustPolicy;
use support::run::RunScaffold;

/// A fake `claude` that records the `CORETEMPO_PORT` it was spawned with in
/// `<root>/port-<agent>`, then idles.
fn record_port(scaffold: &RunScaffold) {
    scaffold.fake_claude(&format!(
        "printf '%s' \"$CORETEMPO_PORT\" > '{}'/port-\"$CORETEMPO_AGENT_ID\"\n\
         printf '> '\nsleep 300\n",
        scaffold.root.display()
    ));
}

/// The port recorded by the fake agent; the child runs asynchronously.
async fn agent_port(scaffold: &RunScaffold, agent: &str) -> String {
    let path = scaffold.root.join(format!("port-{agent}"));
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
    let mut scaffold = RunScaffold::new("options-ephemeral").await;
    record_port(&scaffold);
    // Hold the configured port for the whole test: an ephemeral bind cannot
    // land on it, so `!=` below is a fact rather than a coin flip.
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let configured = held.local_addr().unwrap().port();
    scaffold.port = configured;
    scaffold.write_workflow(&format!(
        "[agents.echo]\ndir = \"{}\"\nprompt = \"p\"\n",
        scaffold.agent_dir.display()
    ));

    let run = scaffold
        .start(RunOptions {
            ephemeral_port: true,
            repoint_current: true,
            cleanup_run_dir: false,
            // Fresh temp agent dirs: without this the preflight refuses.
            trust: TrustPolicy { grant: true },
        })
        .await
        .unwrap();

    let bound = run.port();
    assert_ne!(
        bound, configured,
        "ephemeral run reused the configured port"
    );
    assert_ne!(bound, 0, "run.port() is the pre-bind placeholder port");
    // The accessor and the file agents discover the run through must agree.
    assert_eq!(
        u64::from(bound),
        scaffold.api_json(&run)["port"].as_u64().unwrap(),
        "run.port() disagrees with api.json"
    );
    assert!(health_ok(bound), "nothing serving /v1/health on {bound}");

    // The agent must be handed the port the API actually bound.
    let seen = agent_port(&scaffold, "echo").await;
    assert_eq!(
        seen,
        bound.to_string(),
        "agent got the wrong CORETEMPO_PORT"
    );

    // repoint_current: true is the default path — `current` follows this run.
    assert_eq!(
        std::fs::read_link(scaffold.runs_dir().join("current")).unwrap(),
        PathBuf::from(&run.run_id().0)
    );

    run.stop().await.unwrap();
    drop(held);
}

#[tokio::test(flavor = "multi_thread")]
async fn cleanup_run_dir_removes_artifacts_on_stop() {
    let scaffold = RunScaffold::new("options-cleanup").await;
    record_port(&scaffold);

    let run = scaffold
        .start(RunOptions {
            ephemeral_port: false,
            repoint_current: false,
            cleanup_run_dir: true,
            trust: TrustPolicy { grant: true },
        })
        .await
        .unwrap();

    assert_eq!(
        run.port(),
        scaffold.port,
        "configured port was not honoured"
    );
    let dir = scaffold.run_dir(&run);
    assert!(dir.join("api.json").is_file());

    run.stop().await.unwrap();

    assert!(!dir.exists(), "{} survived stop", dir.display());
    assert_ne!(
        std::fs::read_link(scaffold.runs_dir().join("current")).ok(),
        Some(PathBuf::from(&run.run_id().0)),
        "current was repointed despite repoint_current: false"
    );
}
