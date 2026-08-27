#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns are not covered by allow-*-in-tests"
)]

mod support;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use coretempo_core::pty::PtyChunk;
use coretempo_core::run::Run;
use coretempo_core::types::config::ServerOverrides;
use coretempo_core::types::event::{Event, EventPayload};
use coretempo_core::types::message::MessageStatus;
use coretempo_core::types::{AgentId, MessageId};
use coretempo_core::workflow::resolve_server;
use support::run::{GRANT_TRUST, RunScaffold};
use tokio::sync::{broadcast, mpsc};

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

/// The bearer token from the api.json a run wrote.
fn api_token(scaffold: &RunScaffold, run: &Run) -> String {
    scaffold.api_json(run)["token"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn run_starts_serves_health_and_stops() {
    let scaffold = RunScaffold::new("smoke").await;
    // Fake `claude`: records its argv, prints a prompt marker, then idles.
    let argv_log = scaffold.root.join("argv.txt");
    scaffold.fake_claude(&format!(
        "printf '%s\\n' \"$*\" > '{}'\nprintf '> '\nsleep 300\n",
        argv_log.display()
    ));
    let port = scaffold.port;

    let run = scaffold.start(GRANT_TRUST).await.unwrap();

    // run.started is seq 1 on the bus
    let events = run.bus().replay_since(0).unwrap();
    assert!(matches!(events[0].payload, EventPayload::RunStarted { .. }));
    assert_eq!(events[0].seq, 1);

    // api.json exists, mode 0600, correct port
    let api_json = scaffold.run_dir(&run).join("api.json");
    let meta = std::fs::metadata(&api_json).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(scaffold.api_json(&run)["port"], u64::from(port));

    // `current` follows an interactive run: it is how the tempo CLI finds this
    // api.json when an agent has no port in its environment.
    assert_eq!(
        std::fs::read_link(scaffold.runs_dir().join("current")).unwrap(),
        std::path::PathBuf::from(&run.run_id().0)
    );

    // agent-settings-<id>.json sits next to api.json, 0600, with the turn-boundary hooks
    let settings = scaffold.run_dir(&run).join("agent-settings-echo.json");
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
    assert!(argv.contains("--strict-mcp-config"), "argv: {argv}");
    assert!(!argv.contains("--mcp-config"), "no agent opted in: {argv}");

    run.stop().await.unwrap();
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn start_refuses_when_source_changed_since_freeze() {
    let scaffold = RunScaffold::new("drift").await;
    let (file, frozen) = scaffold.load();
    let server = resolve_server(
        ServerOverrides::default(),
        ServerOverrides::default(),
        &file,
    )
    .unwrap();
    // mutate the file after freezing
    let text = std::fs::read_to_string(&scaffold.config).unwrap();
    std::fs::write(&scaffold.config, format!("{text}# changed\n")).unwrap();
    // Deliberately not granted: the hash check fires before the trust preflight
    // either way, so the worst case with no grant is still a refusal — never a
    // write to the developer's real ~/.claude.json.
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
    let scaffold = RunScaffold::new("router-leak").await;
    let run = scaffold.start(GRANT_TRUST).await.unwrap();

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

/// One POST to a run's own API. `as_agent` adds the `X-CoreTempo-Agent` header
/// the `tempo` CLI sends from inside an agent's session.
#[derive(Clone, Copy)]
struct Post<'a> {
    path: &'a str,
    content_type: &'a str,
    body: &'a str,
    as_agent: Option<&'a str>,
}

/// Sends `req` over raw HTTP/1.1 (no client dependency) and returns the raw response.
fn post(port: u16, token: &str, req: Post<'_>) -> String {
    let mut sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let agent_header = req
        .as_agent
        .map_or(String::new(), |id| format!("X-CoreTempo-Agent: {id}\r\n"));
    write!(
        sock,
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\n\
         {agent_header}Content-Type: {content_type}\r\nContent-Length: {len}\r\n\
         Connection: close\r\n\r\n{body}",
        path = req.path,
        content_type = req.content_type,
        body = req.body,
        len = req.body.len(),
    )
    .unwrap();
    let mut response = String::new();
    sock.read_to_string(&mut response).unwrap();
    response
}

/// The `trigger_id` field of an accepted trigger, read out of the raw response.
fn trigger_id(response: &str) -> String {
    let marker = "\"trigger_id\":\"";
    let start = response
        .find(marker)
        .unwrap_or_else(|| panic!("no trigger_id in: {response}"))
        + marker.len();
    let rest = &response[start..];
    let end = rest.find('"').unwrap();
    rest[..end].to_string()
}

/// A warm trigger parked on a contended member when the run stops must be
/// abandoned: `Run::stop` kills the PTY manager, so a kickoff created after it
/// would be typed into a dead session and the caller would wait for a reply
/// nobody can send.
#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_run_abandons_a_trigger_parked_on_a_member_lock() {
    let scaffold = RunScaffold::new("stop-race").await;
    scaffold.write_workflow(&format!(
        "[agents.echo]\ndir = \"{}\"\nprompt = \"You echo.\"\n\
         [flows.hook]\nagents = [\"echo\"]\n\
         trigger = {{ type = \"webhook\", edge = {{ to = \"echo\", kind = \"ask\" }} }}\n",
        scaffold.agent_dir.display(),
    ));
    let port = scaffold.port;
    let run = scaffold.start(GRANT_TRUST).await.unwrap();
    let token = api_token(&scaffold, &run);

    // Hold `echo` the way a batch kickoff or another flow's trigger would, so
    // the trigger below parks on the member lock instead of running.
    let flow = coretempo_core::types::FlowName("hook".to_string());
    let held = run.lock_flow(&flow).await.unwrap();
    let accepted = post(
        port,
        &token,
        Post {
            path: "/v1/flows/hook/trigger",
            content_type: "text/plain",
            body: "do it",
            as_agent: None,
        },
    );
    assert!(accepted.starts_with("HTTP/1.1 202"), "got: {accepted}");
    let id = trigger_id(&accepted);

    run.stop().await.unwrap();
    drop(held);

    let mut settled = None;
    for _ in 0..100_u32 {
        match run.triggers().get(&id) {
            Some(coretempo_core::trigger::TriggerStatus::Running) | None => {}
            Some(status) => {
                settled = Some(status);
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let Some(coretempo_core::trigger::TriggerStatus::Failed { reason_code, .. }) = settled else {
        panic!("the parked trigger never settled after the stop: {settled:?}");
    };
    assert_eq!(reason_code, "run_stopped");

    let created = run
        .bus()
        .replay_since(0)
        .unwrap()
        .into_iter()
        .any(|event| matches!(event.payload, EventPayload::MessageCreated { .. }));
    assert!(
        !created,
        "the abandoned trigger created a kickoff into a stopped run"
    );
}

/// Waits for the `message.status` event reporting `id` at `want`; on the
/// deadline, panics naming the status the message stalled at.
async fn wait_status(
    run: &Run,
    events: &mut broadcast::Receiver<Event>,
    id: &MessageId,
    want: MessageStatus,
) {
    let deadline = Duration::from_secs(10);
    let reached = tokio::time::timeout(deadline, async {
        loop {
            match events.recv().await {
                Ok(Event {
                    payload:
                        EventPayload::MessageCreated { message }
                        | EventPayload::MessageStatusChanged { message },
                    ..
                }) if message.id == *id => {
                    if message.status == want {
                        return;
                    }
                    assert!(
                        !message.status.is_terminal(),
                        "message {} ended {:?} before reaching {want:?}: {:?}",
                        id.0,
                        message.status,
                        message.reason
                    );
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("bus closed"),
            }
        }
    })
    .await;
    if reached.is_err() {
        let stalled = run.router().get_message(id).await.unwrap().status;
        panic!(
            "message {} never reached {want:?} within {deadline:?}; stalled at {stalled:?}",
            id.0
        );
    }
}

/// Accumulates `output` until `needle` appears and returns everything seen, or
/// panics with the output so far.
async fn wait_for_output(output: &mut mpsc::Receiver<PtyChunk>, needle: &str) -> String {
    let deadline = Duration::from_secs(10);
    let until = tokio::time::Instant::now() + deadline;
    let mut seen = String::new();
    loop {
        let remaining = until.saturating_duration_since(tokio::time::Instant::now());
        let Ok(next) = tokio::time::timeout(remaining, output.recv()).await else {
            panic!("{needle:?} was never typed within {deadline:?}; saw: {seen:?}");
        };
        let Some(chunk) = next else {
            panic!("pty output ended before {needle:?} was typed; saw: {seen:?}");
        };
        seen.push_str(&String::from_utf8_lossy(&chunk.bytes));
        if seen.contains(needle) {
            return seen;
        }
    }
}

/// `Run::start` wires two reverse hooks nothing below the run layer covers:
/// `router.set_state_source` (without it every message stalls at `injected`)
/// and `pty.set_clear_gate` (without it auto-`/clear` never fires) — issue #4.
/// Drives a `send` through the real `PtyManager` with the state reports an
/// agent's hooks would make (`SessionStart` → idle, `UserPromptSubmit` →
/// working, `Stop` → idle) and watches the PTY for the `/clear` that follows.
#[tokio::test(flavor = "multi_thread")]
async fn run_drives_a_send_to_done_and_types_clear_when_the_queue_drains() {
    let scaffold = RunScaffold::new("wiring").await;
    // Fake `claude`: consumes its prompt. The PTY's line discipline echoes
    // everything typed into it, which is how the test sees `/clear`.
    scaffold.fake_claude("exec cat\n");
    scaffold.write_workflow(&format!(
        "idle_debounce_seconds = 0.2\n\
         [agents.echo]\ndir = \"{}\"\nprompt = \"You echo.\"\n",
        scaffold.agent_dir.display(),
    ));
    let port = scaffold.port;
    let run = scaffold.start(GRANT_TRUST).await.unwrap();
    let token = api_token(&scaffold, &run);
    let echo = AgentId("echo".to_string());
    let mut events = run.bus().subscribe();
    let mut output = run.pty().subscribe_output(&echo, None).unwrap();
    let report = |state: &str| {
        let body = format!("{{\"state\":\"{state}\"}}");
        let req = Post {
            path: "/v1/agents/echo/state",
            content_type: "application/json",
            body: &body,
            as_agent: Some("echo"),
        };
        let response = post(port, &token, req);
        assert!(response.starts_with("HTTP/1.1 200"), "got: {response}");
    };

    report("idle"); // SessionStart
    let created = post(
        port,
        &token,
        Post {
            path: "/v1/messages",
            content_type: "application/json",
            body: r#"{"to":"echo","kind":"send","body":"hello"}"#,
            as_agent: None,
        },
    );
    assert!(created.starts_with("HTTP/1.1 201"), "got: {created}");
    let body = created.split("\r\n\r\n").nth(1).unwrap();
    let record: serde_json::Value = serde_json::from_str(body).unwrap();
    let id = MessageId(record["id"].as_str().unwrap().to_string());

    // The queue types the message once the agent is debounced-idle, then
    // presses Enter; only then would a real agent's UserPromptSubmit fire.
    wait_status(&run, &mut events, &id, MessageStatus::Injected).await;
    report("working"); // UserPromptSubmit
    wait_status(&run, &mut events, &id, MessageStatus::Working).await;
    report("idle"); // Stop
    // Without `set_state_source` the router never sees the working→idle
    // transition and the send stalls at `injected`.
    wait_status(&run, &mut events, &id, MessageStatus::Done).await;

    // Without `set_clear_gate` the queue worker has no gate to consult at the
    // debounced working→idle transition and never types `/clear`.
    let seen = wait_for_output(&mut output, "/clear").await;
    assert!(
        seen.find("hello").unwrap() < seen.find("/clear").unwrap(),
        "/clear must follow the drained injection, not precede it; saw: {seen:?}"
    );

    run.stop().await.unwrap();
}
