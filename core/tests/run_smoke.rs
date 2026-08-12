#![expect(clippy::unwrap_used, reason = "assertions are the vocabulary of tests")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use coretempo_core::run::Run;
use coretempo_core::types::config::ServerOverrides;
use coretempo_core::types::event::EventPayload;
use coretempo_core::workflow::{load_workflow, resolve_server};

/// HOME/PATH are process-global; every test in this file holds this for as
/// long as those vars matter (through spawn), even tests that only read them
/// indirectly — `set_var`'s safety contract forbids concurrent reads too, and
/// cargo runs the tests in this binary concurrently on separate threads.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Polls for a file the spawned agent writes; the child runs asynchronously.
async fn read_when_written(path: &std::path::Path) -> String {
    for _ in 0..100_u32 {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.trim().is_empty()
        {
            return text;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    String::new()
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread")]
async fn run_starts_serves_health_and_stops() {
    let _env_guard = ENV_LOCK.lock().await;
    let root = std::env::temp_dir().join(format!("coretempo-smoke-{}", std::process::id()));
    let home = root.join("home");
    let bin = root.join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin).unwrap();

    // Fake `claude`: records its argv, prints a prompt marker, then idles.
    let fake = bin.join("claude");
    let argv_log = root.join("argv.txt");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\nprintf '> '\nsleep 300\n",
            argv_log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::fs::remove_file(&argv_log);

    // SAFETY: guarded by ENV_LOCK against the other env-mutating tests in this
    // binary; HOME/PATH are set before any core code runs.
    unsafe {
        std::env::set_var("HOME", &home);
        let path = std::env::var("PATH").unwrap();
        std::env::set_var("PATH", format!("{}:{path}", bin.display()));
    }

    let port = free_port();
    let toml_text = format!(
        "[workflow]\nname = \"smoke\"\nport = {port}\ndb = \"{db}\"\n\
         [agents.echo]\ndir = \"{dir}\"\nprompt = \"You echo.\"\n",
        db = root.join("tempo.db").display(),
        dir = root.display(),
    );
    let config = root.join("tempo.toml");
    std::fs::write(&config, toml_text).unwrap();

    let (file, frozen) = load_workflow(&config).unwrap();
    let server = resolve_server(
        ServerOverrides::default(),
        ServerOverrides::default(),
        &file,
    )
    .unwrap();
    let run = Run::start(frozen, server).await.unwrap();

    // run.started is seq 1 on the bus
    let events = run.bus().replay_since(0).unwrap();
    assert!(matches!(events[0].payload, EventPayload::RunStarted { .. }));
    assert_eq!(events[0].seq, 1);

    // api.json exists, mode 0600, correct port
    let api_json = home
        .join(".coretempo/runs")
        .join(&run.run_id().0)
        .join("api.json");
    let meta = std::fs::metadata(&api_json).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&api_json).unwrap()).unwrap();
    assert_eq!(parsed["port"], u64::from(port));

    // `current` follows an interactive run: it is how the tempo CLI finds this
    // api.json when an agent has no port in its environment.
    assert_eq!(
        std::fs::read_link(home.join(".coretempo/runs/current")).unwrap(),
        std::path::PathBuf::from(&run.run_id().0)
    );

    // agent-settings-<id>.json sits next to api.json, 0600, with the turn-boundary hooks
    let settings = home
        .join(".coretempo/runs")
        .join(&run.run_id().0)
        .join("agent-settings-echo.json");
    let meta = std::fs::metadata(&settings).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    let hooks: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    let submit = hooks["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(submit.ends_with("/tempo state working"), "got: {submit}");

    // /v1/health answers without auth (raw HTTP/1.1 — no client dependency)
    let mut sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        sock,
        "GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut resp = String::new();
    sock.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 200"), "got: {resp}");
    assert!(resp.contains("\"status\":\"ok\""));

    // The agent was spawned with the role prompt AND the protocol primer
    // (contracts amendment 3), not the raw tempo.toml prompt.
    let argv = read_when_written(&argv_log).await;
    assert!(!argv.is_empty(), "the fake agent never recorded its argv");
    assert!(argv.contains("--append-system-prompt"), "argv: {argv}");
    assert!(argv.contains("You echo."), "argv: {argv}");
    assert!(argv.contains("CoreTempo protocol"), "argv: {argv}");
    assert!(
        argv.contains(&format!("--settings {}", settings.display())),
        "argv: {argv}"
    );

    run.stop().await.unwrap();
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn start_refuses_when_source_changed_since_freeze() {
    let _env_guard = ENV_LOCK.lock().await;
    let root = std::env::temp_dir().join(format!("coretempo-drift-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let config = root.join("tempo.toml");
    let text = format!(
        "[workflow]\nname = \"drift\"\nport = {}\ndb = \"{}\"\n\
         [agents.a]\ndir = \"{}\"\nprompt = \"p\"\n",
        free_port(),
        root.join("t.db").display(),
        root.display(),
    );
    std::fs::write(&config, &text).unwrap();
    let (file, frozen) = load_workflow(&config).unwrap();
    let server = resolve_server(
        ServerOverrides::default(),
        ServerOverrides::default(),
        &file,
    )
    .unwrap();
    // mutate the file after freezing
    std::fs::write(&config, format!("{text}# changed\n")).unwrap();
    let err = Run::start(frozen, server).await.unwrap_err();
    assert!(err.to_string().contains("changed since"));
}

/// Regression test for the `Router`<->`PtyManager` Arc cycle: `Router` holds an
/// `Arc<PtyManager>` (its injection queue) and `PtyManager` held an
/// `Arc<dyn ClearGate>` back to the `Router` (set via `set_clear_gate`), so
/// stopping and dropping a run never freed either — the whole run graph
/// (bus, rings, per-agent worker threads) leaked on every stop.
#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_run_frees_the_router() {
    let _env_guard = ENV_LOCK.lock().await;
    let root = std::env::temp_dir().join(format!("coretempo-router-leak-{}", std::process::id()));
    let home = root.join("home");
    let bin = root.join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin).unwrap();

    let fake = bin.join("claude");
    std::fs::write(&fake, "#!/bin/sh\nprintf '> '\nsleep 300\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    // SAFETY: guarded by ENV_LOCK against the other env-mutating tests in this
    // binary; HOME/PATH are set before any core code runs.
    unsafe {
        std::env::set_var("HOME", &home);
        let path = std::env::var("PATH").unwrap();
        std::env::set_var("PATH", format!("{}:{path}", bin.display()));
    }

    let port = free_port();
    let toml_text = format!(
        "[workflow]\nname = \"router-leak\"\nport = {port}\ndb = \"{db}\"\n\
         [agents.echo]\ndir = \"{dir}\"\nprompt = \"You echo.\"\n",
        db = root.join("tempo.db").display(),
        dir = root.display(),
    );
    let config = root.join("tempo.toml");
    std::fs::write(&config, toml_text).unwrap();

    let (file, frozen) = load_workflow(&config).unwrap();
    let server = resolve_server(
        ServerOverrides::default(),
        ServerOverrides::default(),
        &file,
    )
    .unwrap();
    let run = Run::start(frozen, server).await.unwrap();

    // No public accessor hands out a `Weak<Router>` (Run's API surface is frozen
    // by contracts §4.1), so borrow the router through `watch_inputs`, which only
    // clones existing handles into a plain struct — it starts no watcher task and
    // opens no connection, so it adds no extra strong owner of its own.
    //
    // This test's determinism depends on that: the run must send no messages and
    // open no watcher for the whole body, or a lingering `drive_message`/watch
    // task would itself hold the router alive past `stop()` + drop.
    let weak_router = Arc::downgrade(&run.watch_inputs(std::time::Duration::ZERO, None).router);
    run.stop().await.unwrap();
    drop(run);

    assert!(
        weak_router.upgrade().is_none(),
        "Router still alive after stop+drop: PtyManager holds a strong clear_gate Arc \
         (Router->PtyManager->Router cycle) and the whole run graph leaks"
    );
}
