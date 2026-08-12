use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use coretempo_core::pty::{Cursor, PtyChunk};
use coretempo_core::router::{MessageFilter, RouterError};
use coretempo_core::run::Run;
use coretempo_core::trigger::{
    TriggerStatus, completion_status, startup_kickoff, watch_completion, watcher_deadline,
};
use coretempo_core::types::{
    AgentDetail, AgentInfo, MessageKind, MessageRecord, Origin, RunInfo, ServerOverrides, Snapshot,
    ValidationIssue,
};
use coretempo_core::workflow::{load_workflow, resolve_server, validate_workflow};
use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::bridge::spawn_event_bridge;
use crate::state::{ActiveRun, AppState};

/// Command error crossing the Tauri IPC boundary. Contracts §8.1: serializes as
/// `{"code":"…","message":"…"}`. Codes reuse the REST table (§5.2) where the cause matches
/// (`unknown_agent`, `not_an_ask`, …) plus app-local codes: `no_run`, `run_active`, `config`,
/// `io`, `pty`, `run`, `internal`. Messages are written for LLM/human readers: name the
/// operation, the input, and the fix.
#[derive(Debug, Clone, Serialize)]
pub struct CmdError {
    pub code: String,
    pub message: String,
}

impl CmdError {
    pub fn new(code: &str, message: impl Into<String>) -> CmdError {
        CmdError {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CmdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for CmdError {}

impl From<RouterError> for CmdError {
    fn from(err: RouterError) -> CmdError {
        let code = match &err {
            RouterError::UnknownAgent(_) => "unknown_agent",
            RouterError::UnknownMessage(_) => "unknown_message",
            RouterError::AlreadyReplied(_) => "already_replied",
            RouterError::NotAnAsk(_) => "not_an_ask",
            RouterError::WrongReplier(_) => "wrong_replier",
            RouterError::NoLoopEdge { .. } => "no_loop_edge",
            RouterError::InvalidCode(_) => "invalid_request",
            RouterError::OutputSchema { .. } => "schema_validation_failed",
            RouterError::Store(_) => "internal",
        };
        CmdError::new(code, err.to_string())
    }
}

impl From<coretempo_core::pty::PtyError> for CmdError {
    fn from(err: coretempo_core::pty::PtyError) -> CmdError {
        CmdError::new("pty", err.to_string())
    }
}

/// Contracts §8.1 successor to `workflow_validate` (amendment, Task 14): same
/// ok/errors semantics plus the parsed model the graph editor edits.
#[derive(Debug, Clone, Serialize)]
pub struct ParseReport {
    pub ok: bool,
    pub errors: Vec<ValidationIssue>,
    pub model: Option<coretempo_core::types::WorkflowFile>,
}

/// The app accepts typed paths: expand `~` the same way core does for agent
/// dirs. (Root cause of the launch-card ENOENT bug: paths were used verbatim.)
fn resolve_workflow_path(path: &str) -> Result<PathBuf, CmdError> {
    coretempo_core::workflow::expand_home(std::path::Path::new(path)).ok_or_else(|| {
        CmdError::new(
            "io",
            format!("cannot determine a home directory to expand '~' in '{path}'; set HOME"),
        )
    })
}

#[tauri::command(rename_all = "snake_case")]
pub async fn workflow_open(path: String) -> Result<String, CmdError> {
    let resolved = resolve_workflow_path(&path)?;
    tokio::fs::read_to_string(&resolved).await.map_err(|err| {
        let code = if err.kind() == std::io::ErrorKind::NotFound {
            "not_found"
        } else {
            "io"
        };
        CmdError::new(
            code,
            format!("failed to read workflow file '{path}': {err}"),
        )
    })
}

#[tauri::command(rename_all = "snake_case")]
pub async fn workflow_save(path: String, text: String) -> Result<(), CmdError> {
    let resolved = resolve_workflow_path(&path)?;
    if let Some(parent) = resolved.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|err| {
            CmdError::new(
                "io",
                format!("failed to create directory '{}': {err}", parent.display()),
            )
        })?;
    }
    tokio::fs::write(&resolved, text).await.map_err(|err| {
        CmdError::new(
            "io",
            format!("failed to write workflow file '{path}': {err}"),
        )
    })
}

#[tauri::command(rename_all = "snake_case")]
pub async fn workflow_parse(text: String) -> Result<ParseReport, CmdError> {
    match validate_workflow(&text) {
        Ok(file) => Ok(ParseReport {
            ok: true,
            errors: Vec::new(),
            model: Some(file),
        }),
        Err(errors) => Ok(ParseReport {
            ok: false,
            errors,
            model: None,
        }),
    }
}

#[tauri::command(rename_all = "snake_case")]
pub async fn workflow_merge(
    text: String,
    model: coretempo_core::types::WorkflowFile,
) -> Result<String, CmdError> {
    crate::merge::merge_workflow(&text, &model).map_err(|reason| CmdError::new("config", reason))
}

/// Clone the active run out of state without holding the lock across the caller's awaits.
pub(crate) async fn active_run(state: &AppState) -> Result<Arc<Run>, CmdError> {
    let guard = state.active.lock().await;
    match guard.as_ref() {
        Some(active) => Ok(Arc::clone(&active.run)),
        None => Err(CmdError::new(
            "no_run",
            "no run is active; call run_start first",
        )),
    }
}

/// Forward PTY chunks until the source closes (run stopped / agent respawned subscription
/// superseded) or `send` reports a dead channel (webview reloaded). Bytes only — the
/// frontend feeds them straight to `term.write` with no decode.
pub(crate) async fn pump_chunks<F>(mut rx: tokio::sync::mpsc::Receiver<PtyChunk>, mut send: F)
where
    F: FnMut(Vec<u8>) -> bool + Send,
{
    while let Some(chunk) = rx.recv().await {
        if !send(chunk.bytes) {
            break;
        }
    }
}

/// Contracts §8.1/§8.2: ring-buffer replay from `since_cursor` then live, raw bytes on the
/// Channel (`InvokeResponseBody::Raw`) — NEVER the Tauri event system, which JSON-encodes
/// and may reorder. Replay contiguity is `PtyManager::subscribe_output`'s guarantee.
#[tauri::command(rename_all = "snake_case")]
pub async fn subscribe_pty(
    state: tauri::State<'_, AppState>,
    agent: String,
    since_cursor: Option<u64>,
    channel: Channel<InvokeResponseBody>,
) -> Result<(), CmdError> {
    let run = active_run(&state).await?;
    let agent = coretempo_core::types::AgentId(agent);
    let rx = run
        .pty()
        .subscribe_output(&agent, since_cursor.map(Cursor))?;
    tauri::async_runtime::spawn(pump_chunks(rx, move |bytes| {
        channel.send(InvokeResponseBody::Raw(bytes)).is_ok()
    }));
    Ok(())
}

/// Raw user keystrokes — bypasses the injection queue entirely (contracts §3).
#[tauri::command(rename_all = "snake_case")]
pub async fn write_pty(
    state: tauri::State<'_, AppState>,
    agent: String,
    data: Vec<u8>,
) -> Result<(), CmdError> {
    let run = active_run(&state).await?;
    run.pty()
        .write(&coretempo_core::types::AgentId(agent), &data)
        .await?;
    Ok(())
}

#[tauri::command(rename_all = "snake_case")]
pub async fn resize_pty(
    state: tauri::State<'_, AppState>,
    agent: String,
    cols: u16,
    rows: u16,
) -> Result<(), CmdError> {
    let run = active_run(&state).await?;
    run.pty()
        .resize(&coretempo_core::types::AgentId(agent), cols, rows)
        .await?;
    Ok(())
}

/// UI backpressure (spec §4.4): past ~1 MB unparsed bytes the frontend pauses this PTY.
#[tauri::command(rename_all = "snake_case")]
pub async fn pause_pty(
    state: tauri::State<'_, AppState>,
    agent: String,
    paused: bool,
) -> Result<(), CmdError> {
    let run = active_run(&state).await?;
    run.pty()
        .pause_output(&coretempo_core::types::AgentId(agent), paused);
    Ok(())
}

/// Load + freeze the workflow, resolve server settings (env layer only — the app passes no
/// flag layer), start the core run, spawn the event bridge. Generic over `Runtime` so tests
/// can drive it with `MockRuntime`. Startup stays fast: this runs strictly on user action,
/// never before the window shows.
#[tauri::command(rename_all = "snake_case")]
pub async fn run_start<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    config_path: String,
) -> Result<RunInfo, CmdError> {
    let mut guard = state.active.lock().await;
    if guard.is_some() {
        return Err(CmdError::new(
            "run_active",
            "a run is already active; call run_stop before starting another",
        ));
    }
    let path = resolve_workflow_path(&config_path)?;
    let (file, frozen) = load_workflow(&path).map_err(|err| {
        CmdError::new(
            "config",
            format!("failed to load workflow '{config_path}': {err}"),
        )
    })?;
    let env = ServerOverrides::from_env().map_err(|err| {
        CmdError::new(
            "config",
            format!("failed to read server settings from the environment: {err}"),
        )
    })?;
    let server = resolve_server(ServerOverrides::default(), env, &file).map_err(|err| {
        CmdError::new(
            "config",
            format!("failed to resolve server settings: {err}"),
        )
    })?;
    let port = server.port;
    let run = Run::start(frozen, server)
        .await
        .map_err(|err| CmdError::new("run", format!("failed to start run: {err}")))?;

    let bridge = spawn_event_bridge(app, run.bus().clone());

    let kickoff = if let Some((to, kind, message)) = startup_kickoff(&file) {
        // Register in the hub so the snapshot's trigger history includes the
        // on_start kickoff; the Origin hex is the hub id minus "t-" so the UI
        // correlates the kickoff message to the trigger (contracts amendment 24).
        let trigger_id = run.triggers().try_begin().map_err(|active| {
            CmdError::new(
                "trigger_in_flight",
                format!("trigger '{active}' is already running on a freshly started run"),
            )
        })?;
        let origin_hex = trigger_id
            .strip_prefix("t-")
            .unwrap_or(&trigger_id)
            .to_string();
        let kickoff = match run
            .router()
            .create_message(Origin::Http(origin_hex), to, kind, message)
            .await
        {
            Ok(kickoff) => kickoff,
            Err(error) => {
                run.triggers().finish(
                    &trigger_id,
                    TriggerStatus::Failed {
                        reason: error.to_string(),
                        reason_code: "kickoff_rejected".to_string(),
                    },
                );
                return Err(error.into());
            }
        };
        // Built immediately after creation: the watcher's deadline runs from here.
        let inputs = run.watch_inputs(
            watcher_deadline(run.workflow().ask_timeout),
            Some(trigger_id.clone()),
        );
        let hub = Arc::clone(run.triggers());
        // No exit here (this is the desktop app, not batch mode): the result reaches
        // the UI as the `workflow.completed` bus event via the bridge. The handle is
        // kept so `run_stop` can abort it — see `ActiveRun::kickoff`.
        Some(tauri::async_runtime::spawn(async move {
            let completion = watch_completion(inputs, kickoff).await;
            hub.finish(&trigger_id, completion_status(completion.clone()));
            completion
        }))
    } else {
        None
    };

    let info = RunInfo {
        run_id: run.run_id().clone(),
        workflow_name: run.workflow().name.clone(),
        workflow_path: config_path,
        started_at: run.started_at().clone(),
        port,
        scrollback: run.workflow().scrollback,
    };
    *guard = Some(ActiveRun {
        run,
        port,
        workflow_path: info.workflow_path.clone(),
        bridge,
        kickoff,
    });
    Ok(info)
}

/// Idempotent: stopping with no active run is Ok. The bridge task ends on its own when the
/// bus closes; `abort()` is cleanup insurance. The kickoff watcher (if any) has no such
/// self-ending path — it awaits a reply or quiescence against wiring `stop()` just tore
/// down — so it is always aborted explicitly.
#[tauri::command(rename_all = "snake_case")]
pub async fn run_stop(state: tauri::State<'_, AppState>) -> Result<(), CmdError> {
    let mut guard = state.active.lock().await;
    let Some(active) = guard.take() else {
        return Ok(());
    };
    drop(guard);
    let result = active.run.stop().await;
    active.bridge.abort();
    if let Some(kickoff) = active.kickoff {
        kickoff.abort();
    }
    result.map_err(|err| CmdError::new("run", format!("failed to stop run: {err}")))
}

/// Restart semantics (spec §4.3): messages to the agent fail; pending asks from it get
/// log+event-only replies. `on_agent_restarted` first so the router suppresses before the
/// respawn, then `PtyManager::restart` kills + respawns from frozen config.
#[tauri::command(rename_all = "snake_case")]
pub async fn restart_agent(
    state: tauri::State<'_, AppState>,
    agent: String,
) -> Result<(), CmdError> {
    let run = active_run(&state).await?;
    let agent = coretempo_core::types::AgentId(agent);
    run.router().on_agent_restarted(&agent).await;
    run.pty().restart(&agent).await?;
    Ok(())
}

/// Chat panel path (contracts §8.1): direct core call as `Origin::User` — the desktop UI
/// never dogfoods HTTP loopback.
#[tauri::command(rename_all = "snake_case")]
pub async fn send_chat(
    state: tauri::State<'_, AppState>,
    to: String,
    kind: MessageKind,
    body: String,
) -> Result<MessageRecord, CmdError> {
    let run = active_run(&state).await?;
    let record = run
        .router()
        .create_message(Origin::User, coretempo_core::types::AgentId(to), kind, body)
        .await?;
    Ok(record)
}

/// Frozen constant: `snapshot()` carries the most recent 200 messages (contracts §8.1).
const SNAPSHOT_MESSAGE_LIMIT: u32 = 200;

/// Reload-mid-run contract (§8.2): frontend calls `snapshot()`, resubscribes events deduping by
/// `last_seq`, then `subscribe_pty` per agent. `pty_cursors` are the current end-of-stream
/// cursors (== `AgentDetail.pty_cursor`); a freshly reloaded terminal passes
/// `since_cursor: null` instead to replay the full ring tail.
#[tauri::command(rename_all = "snake_case")]
pub async fn snapshot(state: tauri::State<'_, AppState>) -> Result<Snapshot, CmdError> {
    let guard = state.active.lock().await;
    let Some(active) = guard.as_ref() else {
        return Ok(Snapshot {
            run: None,
            agents: Vec::new(),
            messages: Vec::new(),
            pty_cursors: BTreeMap::new(),
            last_seq: 0,
            triggers: Vec::new(),
        });
    };
    let run = &active.run;

    let mut agents = Vec::new();
    let mut pty_cursors = BTreeMap::new();
    for (id, cfg) in &run.workflow().agents {
        // Contracts amendment 10: `PtyManager` is the single source of truth for exit codes;
        // it reports `Some` only while the agent is `exited`.
        let (cursor, _tail) = run.pty().read_ring(id, None)?;
        agents.push(AgentDetail {
            info: AgentInfo {
                id: id.clone(),
                state: run.pty().state(id)?,
                pending_asks: run.router().pending_asks(id),
                exit_code: run.pty().exit_code(id)?,
            },
            dir: cfg.dir.display().to_string(),
            model: cfg.model.clone(),
            permission_mode: cfg.permission_mode.clone(),
            auto_clear: cfg.auto_clear,
            pty_cursor: cursor.0,
        });
        pty_cursors.insert(id.clone(), cursor.0);
    }

    let filter = MessageFilter {
        to: None,
        from: None,
        status: None,
        kind: None,
        since: None,
        limit: SNAPSHOT_MESSAGE_LIMIT,
    };
    let messages = run.router().list_messages(filter).await?;

    let info = RunInfo {
        run_id: run.run_id().clone(),
        workflow_name: run.workflow().name.clone(),
        workflow_path: active.workflow_path.clone(),
        started_at: run.started_at().clone(),
        port: active.port,
        scrollback: run.workflow().scrollback,
    };
    Ok(Snapshot {
        run: Some(info),
        agents,
        messages,
        pty_cursors,
        last_seq: run.bus().last_seq(),
        triggers: run.triggers().views(),
    })
}

#[cfg(test)]
#[expect(
    clippy::panic_in_result_fn,
    reason = "tests assert inside Result-returning fns"
)]
mod tests {
    use super::*;
    use coretempo_core::types::AgentId;

    #[test]
    fn cmd_error_serializes_as_code_and_message() -> anyhow::Result<()> {
        let err = CmdError::new("no_run", "no run is active; call run_start first");
        assert_eq!(
            serde_json::to_value(&err)?,
            serde_json::json!({
                "code": "no_run",
                "message": "no run is active; call run_start first"
            })
        );
        Ok(())
    }

    #[test]
    fn router_errors_map_to_frozen_codes() {
        let err = CmdError::from(RouterError::UnknownAgent(AgentId("buidler".to_string())));
        assert_eq!(err.code, "unknown_agent");
        let err = CmdError::from(RouterError::NotAnAsk(coretempo_core::types::MessageId(
            "m-a3f91c2e".to_string(),
        )));
        assert_eq!(err.code, "not_an_ask");
    }

    const VALID_WORKFLOW: &str = r#"
[workflow]
name = "test-flow"

[agents.builder]
dir = "/tmp"
prompt = "You implement tasks sent to you."
"#;

    fn temp_file(name: &str) -> anyhow::Result<std::path::PathBuf> {
        let dir = std::env::temp_dir().join(format!("coretempo-app-tests-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(dir.join(name))
    }

    // tokio::sync::Mutex, not std::sync::Mutex: the guard below is held across an
    // `.await`, which trips clippy::await_holding_lock (denied workspace-wide).
    static HOME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn workflow_open_returns_file_text() -> anyhow::Result<()> {
        let path = temp_file("open.toml")?;
        std::fs::write(&path, VALID_WORKFLOW)?;
        let text = workflow_open(path.display().to_string()).await?;
        assert_eq!(text, VALID_WORKFLOW);
        Ok(())
    }

    #[tokio::test]
    async fn workflow_open_missing_file_is_not_found() -> anyhow::Result<()> {
        match workflow_open("/nonexistent/coretempo/tempo.toml".to_string()).await {
            Ok(text) => anyhow::bail!("expected not_found, got text: {text}"),
            Err(err) => assert_eq!(err.code, "not_found"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn workflow_open_unreadable_file_is_io_not_not_found() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let path = temp_file("unreadable.toml")?;
        std::fs::write(&path, "x")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))?;
        let result = workflow_open(path.display().to_string()).await;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
        match result {
            Ok(_) => anyhow::bail!("expected io error"),
            Err(err) => assert_eq!(err.code, "io", "permission errors must not seed templates"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn workflow_save_roundtrips() -> anyhow::Result<()> {
        let path = temp_file("save.toml")?;
        workflow_save(path.display().to_string(), VALID_WORKFLOW.to_string()).await?;
        assert_eq!(std::fs::read_to_string(&path)?, VALID_WORKFLOW);
        Ok(())
    }

    #[tokio::test]
    async fn workflow_save_expands_tilde_and_creates_parents() -> anyhow::Result<()> {
        // Isolate HOME so `~` lands in a temp dir (serial: env is process-global).
        let fake_home = temp_file("fake-home")?;
        std::fs::create_dir_all(&fake_home)?;
        // Tests in this file that touch HOME serialize on this lock.
        let _guard = HOME_LOCK.lock().await;
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", &fake_home) };
        let result = workflow_save(
            "~/CoreTempoWorkflows/tempo.toml".to_string(),
            VALID_WORKFLOW.to_string(),
        )
        .await;
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        result?;
        let written = fake_home.join("CoreTempoWorkflows/tempo.toml");
        assert_eq!(std::fs::read_to_string(written)?, VALID_WORKFLOW);
        Ok(())
    }

    #[tokio::test]
    async fn workflow_parse_returns_model_for_valid_toml() -> anyhow::Result<()> {
        let report = workflow_parse(VALID_WORKFLOW.to_string()).await?;
        assert!(report.ok && report.errors.is_empty());
        let model = report.model.expect("model present when ok");
        assert!(model.agents.contains_key(&AgentId("builder".into())));
        Ok(())
    }

    #[tokio::test]
    async fn workflow_parse_reports_issues_with_no_model() -> anyhow::Result<()> {
        let report = workflow_parse("this is not toml [".to_string()).await?;
        assert!(!report.ok && !report.errors.is_empty());
        assert!(report.model.is_none());
        Ok(())
    }

    fn test_app() -> anyhow::Result<tauri::App<tauri::test::MockRuntime>> {
        Ok(tauri::test::mock_builder()
            .manage(AppState::default())
            .build(tauri::test::mock_context(tauri::test::noop_assets()))?)
    }

    #[tokio::test]
    async fn pty_commands_without_run_return_no_run() -> anyhow::Result<()> {
        use tauri::Manager;
        let app = test_app()?;

        match write_pty(app.state::<AppState>(), "builder".to_string(), vec![b'x']).await {
            Ok(()) => anyhow::bail!("write_pty: expected no_run"),
            Err(err) => assert_eq!(err.code, "no_run"),
        }
        match resize_pty(app.state::<AppState>(), "builder".to_string(), 80, 24).await {
            Ok(()) => anyhow::bail!("resize_pty: expected no_run"),
            Err(err) => assert_eq!(err.code, "no_run"),
        }
        match pause_pty(app.state::<AppState>(), "builder".to_string(), true).await {
            Ok(()) => anyhow::bail!("pause_pty: expected no_run"),
            Err(err) => assert_eq!(err.code, "no_run"),
        }
        let channel = Channel::new(|_response| Ok(()));
        match subscribe_pty(
            app.state::<AppState>(),
            "builder".to_string(),
            None,
            channel,
        )
        .await
        {
            Ok(()) => anyhow::bail!("subscribe_pty: expected no_run"),
            Err(err) => assert_eq!(err.code, "no_run"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn pump_chunks_preserves_byte_order() -> anyhow::Result<()> {
        let (tx, rx) = tokio::sync::mpsc::channel::<PtyChunk>(8);
        tx.send(PtyChunk {
            start: Cursor(0),
            bytes: b"hel".to_vec(),
        })
        .await?;
        tx.send(PtyChunk {
            start: Cursor(3),
            bytes: b"lo".to_vec(),
        })
        .await?;
        drop(tx);

        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        pump_chunks(rx, move |bytes| out_tx.send(bytes).is_ok()).await;

        let mut all = Vec::new();
        while let Ok(bytes) = out_rx.try_recv() {
            all.extend(bytes);
        }
        assert_eq!(all, b"hello");
        Ok(())
    }

    #[tokio::test]
    async fn pump_chunks_stops_when_send_fails() -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (tx, rx) = tokio::sync::mpsc::channel::<PtyChunk>(8);
        tx.send(PtyChunk {
            start: Cursor(0),
            bytes: b"a".to_vec(),
        })
        .await?;
        tx.send(PtyChunk {
            start: Cursor(1),
            bytes: b"b".to_vec(),
        })
        .await?;
        drop(tx);

        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        pump_chunks(rx, move |_bytes| {
            counter.fetch_add(1, Ordering::SeqCst);
            false // dead webview channel
        })
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn run_start_with_missing_config_is_config_error() -> anyhow::Result<()> {
        use tauri::Manager;
        let app = test_app()?;
        let handle = app.handle().clone();
        let missing = "/nonexistent/coretempo/tempo.toml".to_string();
        match run_start(handle, app.state::<AppState>(), missing).await {
            Ok(info) => anyhow::bail!("expected config error, got run {}", info.run_id.0),
            Err(err) => {
                assert_eq!(err.code, "config");
                assert!(err.message.contains("/nonexistent/coretempo/tempo.toml"));
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn run_stop_without_run_is_ok() -> anyhow::Result<()> {
        use tauri::Manager;
        let app = test_app()?;
        run_stop(app.state::<AppState>()).await?;
        Ok(())
    }

    #[tokio::test]
    async fn restart_and_chat_without_run_return_no_run() -> anyhow::Result<()> {
        use tauri::Manager;
        let app = test_app()?;
        match restart_agent(app.state::<AppState>(), "builder".to_string()).await {
            Ok(()) => anyhow::bail!("restart_agent: expected no_run"),
            Err(err) => assert_eq!(err.code, "no_run"),
        }
        let chat = send_chat(
            app.state::<AppState>(),
            "builder".to_string(),
            MessageKind::Ask,
            "status?".to_string(),
        )
        .await;
        match chat {
            Ok(record) => anyhow::bail!("send_chat: expected no_run, got {}", record.id.0),
            Err(err) => assert_eq!(err.code, "no_run"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn snapshot_without_run_is_empty() -> anyhow::Result<()> {
        use tauri::Manager;
        let app = test_app()?;
        let snap = snapshot(app.state::<AppState>()).await?;
        assert!(snap.run.is_none());
        assert!(snap.agents.is_empty());
        assert!(snap.messages.is_empty());
        assert!(snap.pty_cursors.is_empty());
        assert_eq!(snap.last_seq, 0);
        assert!(snap.triggers.is_empty());
        Ok(())
    }
}
