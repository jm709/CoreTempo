use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use coretempo_core::pty::{Cursor, PtyChunk};
use coretempo_core::router::{FlowKickoff, MessageFilter, RouterError};
use coretempo_core::run::{Run, RunOptions};
use coretempo_core::trigger::{Completion, completion_status, watch_completion, watcher_deadline};
use coretempo_core::trust::{TrustPolicy, TrustStore};
use coretempo_core::types::{
    AgentDetail, AgentInfo, FlowName, MessageKind, MessageRecord, Origin, RunInfo, ServerOverrides,
    Snapshot, ValidationIssue,
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

/// Fires an `on_start` flow's kickoff and returns the hub trigger id plus the
/// handle of the task watching it.
///
/// The trigger is registered in the hub so the snapshot's trigger history
/// includes the `on_start` kickoff; the `Origin::Trigger` hex is the hub id
/// minus "t-" so the UI correlates the kickoff message to the trigger
/// (contracts amendments 24 and 38). `create_kickoff` names the flow in the
/// injected header, as the daemon and serve paths do.
///
/// The task mirrors the warm webhook path (`api::trigger::fire_flow`): member
/// locks first (multi-flow spec §5 — a webhook trigger sharing an `exclusive`
/// agent serializes with this batch instead of interleaving prompts in that
/// agent's one live session), then the kickoff, then the watcher. Locking off
/// the command path keeps the caller from blocking on a contending flow while
/// it holds the active-run mutex; the cost is that a kickoff the router rejects
/// settles as a failed trigger rather than failing the command.
fn spawn_kickoff(
    run: &Arc<Run>,
    flow_name: &FlowName,
    plan: (coretempo_core::types::AgentId, MessageKind, String),
) -> Result<(String, tauri::async_runtime::JoinHandle<Completion>), CmdError> {
    let (to, kind, message) = plan;
    let trigger_id = run.triggers().try_begin(flow_name).map_err(|active| {
        CmdError::new(
            "trigger_in_flight",
            format!(
                "trigger '{active}' is still running on flow '{flow}'; wait for it to \
                 settle, then fire again",
                flow = flow_name.0,
            ),
        )
    })?;
    let origin_hex = trigger_id
        .strip_prefix("t-")
        .unwrap_or(&trigger_id)
        .to_string();
    let run = Arc::clone(run);
    let flow_name = flow_name.clone();
    let fired = trigger_id.clone();
    // No exit here (this is the desktop app, not batch mode): the result reaches
    // the UI as the `workflow.completed` bus event via the bridge. The handle is
    // kept so `run_stop` can abort it — see `ActiveRun::kickoffs`.
    let handle = tauri::async_runtime::spawn(async move {
        let _guards = run.lock_flow(&flow_name).await;
        let kickoff = match run
            .router()
            .create_kickoff(FlowKickoff {
                flow: flow_name.clone(),
                from: Origin::Trigger(origin_hex),
                to,
                kind,
                body: message,
            })
            .await
        {
            Ok(kickoff) => kickoff,
            Err(error) => {
                tracing::error!(
                    flow = %flow_name.0,
                    error = %error,
                    "the on_start kickoff was rejected"
                );
                let completion = Completion::Failed {
                    reason: error.to_string(),
                    reason_code: "kickoff_rejected",
                };
                run.triggers()
                    .finish(&trigger_id, completion_status(completion.clone()));
                return completion;
            }
        };
        // Built immediately after creation: the watcher's deadline runs from
        // here, scoped to the on_start flow's members and contract.
        let inputs = run
            .watch_inputs_for_flow(
                &flow_name,
                watcher_deadline(run.workflow().ask_timeout),
                Some(trigger_id.clone()),
            )
            .unwrap_or_else(|| {
                run.watch_inputs(
                    watcher_deadline(run.workflow().ask_timeout),
                    Some(trigger_id.clone()),
                )
            });
        let completion = watch_completion(inputs, kickoff).await;
        run.triggers()
            .finish(&trigger_id, completion_status(completion.clone()));
        completion
    });
    Ok((fired, handle))
}

/// Trust roots the desktop must ask about before `run_start` (spec 2026-08-17
/// §1): empty when every root is trusted or when policy lets the run grant.
#[tauri::command(rename_all = "snake_case")]
pub async fn run_untrusted_dirs(
    state: tauri::State<'_, AppState>,
    config_path: String,
) -> Result<Vec<String>, CmdError> {
    let path = resolve_workflow_path(&config_path)?;
    let (file, frozen) = load_workflow(&path).map_err(|err| {
        CmdError::new(
            "config",
            format!("failed to load workflow '{config_path}': {err}"),
        )
    })?;
    if TrustPolicy::resolve(state.trust_grant, file.server.trust_agent_dirs).grant {
        return Ok(Vec::new());
    }
    let store = TrustStore::from_env().ok_or_else(|| {
        CmdError::new(
            "io",
            "cannot determine HOME or CLAUDE_CONFIG_DIR for .claude.json",
        )
    })?;
    let roots = store
        .untrusted_roots(frozen.agents.values().map(|cfg| cfg.dir.as_path()))
        .map_err(|err| CmdError::new("trust", err.to_string()))?;
    Ok(roots.into_iter().map(|r| r.display().to_string()).collect())
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
    trust_confirmed: bool,
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
    // The user's dialog answer travels with the run: its TrustGate may re-grant
    // after a live Claude session reverts the key, for the run's whole life.
    let policy = TrustPolicy::resolve(state.trust_grant, file.server.trust_agent_dirs);
    let trust = TrustPolicy {
        grant: policy.grant || trust_confirmed,
    };
    let run = Run::start_with(
        frozen,
        server,
        RunOptions {
            trust,
            ..RunOptions::default()
        },
    )
    .await
    .map_err(|err| CmdError::new("run", format!("failed to start run: {err}")))?;

    let bridge = spawn_event_bridge(app, run.bus().clone());

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
        kickoffs: BTreeMap::new(),
    });
    Ok(info)
}

/// One flow as the Run tab needs it: its name, how it fires, and its target.
#[derive(Debug, Clone, Serialize)]
pub struct FlowInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub trigger_type: String,
    pub target: String,
}

/// The active run's flows, for the Run tab's fire controls (multi-flow spec §6).
#[tauri::command(rename_all = "snake_case")]
pub async fn run_flows(state: tauri::State<'_, AppState>) -> Result<Vec<FlowInfo>, CmdError> {
    use coretempo_core::types::config::TriggerType;

    let run = active_run(&state).await?;
    Ok(run
        .workflow()
        .flows
        .iter()
        .map(|(name, flow)| FlowInfo {
            name: name.0.clone(),
            trigger_type: match flow.trigger_type {
                TriggerType::OnStart => "on_start".to_string(),
                TriggerType::Webhook => "webhook".to_string(),
            },
            target: flow.edge.to.0.clone(),
        })
        .collect())
}

/// Fires an `on_start` flow's configured kickoff into the warm pool — the
/// desktop's per-flow fire control (multi-flow spec §6). Webhook flows fire
/// over HTTP instead. Returns the hub trigger id; the kickoff itself is
/// created off the command path (after `spawn_kickoff`'s lock acquisition),
/// and the watcher handle lands in `ActiveRun.kickoffs` (keyed by flow) so
/// `run_stop` aborts it. `try_begin` already 409s a second fire of the same
/// flow, so this insert never clobbers a live handle.
#[tauri::command(rename_all = "snake_case")]
pub async fn fire_flow(
    state: tauri::State<'_, AppState>,
    name: String,
) -> Result<String, CmdError> {
    use coretempo_core::types::config::TriggerType;

    let mut guard = state.active.lock().await;
    let Some(active) = guard.as_mut() else {
        return Err(CmdError::new(
            "no_run",
            "no run is active; call run_start first",
        ));
    };
    let run = Arc::clone(&active.run);
    let flow_name = FlowName(name.clone());
    let Some(flow) = run.workflow().flows.get(&flow_name) else {
        let declared = run
            .workflow()
            .flows
            .keys()
            .map(|f| f.0.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(CmdError::new(
            "unknown_flow",
            format!("no flow named '{name}'; declared flows: {declared}"),
        ));
    };
    // Explicit match, no wildcard (repo style): a third trigger kind must be
    // decided here, not silently fired.
    match flow.trigger_type {
        TriggerType::OnStart => {}
        TriggerType::Webhook => {
            return Err(CmdError::new(
                "invalid_request",
                format!(
                    "flow '{name}' is a webhook flow; it fires over the run's HTTP \
                     trigger endpoint, not from the Run tab — the fire control is \
                     for on_start flows"
                ),
            ));
        }
    }
    let plan = (
        flow.edge.to.clone(),
        flow.edge.kind.message_kind(),
        // Validation guarantees an on_start trigger carries a message.
        flow.message.clone().unwrap_or_default(),
    );
    let (trigger_id, handle) = spawn_kickoff(&run, &flow_name, plan)?;
    active.kickoffs.insert(flow_name, handle);
    Ok(trigger_id)
}

/// Idempotent: stopping with no active run is Ok. The bridge task ends on its own when the
/// bus closes; `abort()` is cleanup insurance. A fired flow's kickoff watcher ends on its own
/// too, but only at its deadline or on a member's exit — its `Arc<Run>` keeps the bus open
/// past `stop()` — so it can await a reply against torn-down wiring for as long as
/// `ask_timeout`. Each one is therefore aborted explicitly.
#[tauri::command(rename_all = "snake_case")]
pub async fn run_stop(state: tauri::State<'_, AppState>) -> Result<(), CmdError> {
    let mut guard = state.active.lock().await;
    let Some(active) = guard.take() else {
        return Ok(());
    };
    drop(guard);
    let result = active.run.stop().await;
    active.bridge.abort();
    for kickoff in active.kickoffs.into_values() {
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
        // Contracts amendment 10: `PtyManager` is the single source of truth for how an
        // agent exited; it reports `Some` only while the agent is `exited`.
        let (cursor, _tail) = run.pty().read_ring(id, None)?;
        agents.push(AgentDetail {
            info: AgentInfo {
                id: id.clone(),
                state: run.pty().state(id)?,
                pending_asks: run.router().pending_asks(id),
                exit: run.pty().exit(id)?,
                blocked: run.pty().blocked(id)?,
            },
            dir: cfg.dir.display().to_string(),
            model: cfg.model.clone(),
            permission_mode: cfg.permission_mode.clone(),
            auto_clear: cfg.auto_clear,
            isolated_config: cfg.isolated_config,
            skills: cfg.skills.iter().map(|p| p.display().to_string()).collect(),
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

    /// Same mock app with an explicit `~/.coretempo/config.toml` trust grant, so
    /// the policy branches of `run_untrusted_dirs` are reachable in tests.
    fn test_app_with_trust(
        trust_grant: bool,
    ) -> anyhow::Result<tauri::App<tauri::test::MockRuntime>> {
        Ok(tauri::test::mock_builder()
            .manage(AppState::with_trust(trust_grant))
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
        match run_start(handle, app.state::<AppState>(), missing, false).await {
            Ok(info) => anyhow::bail!("expected config error, got run {}", info.run_id.0),
            Err(err) => {
                assert_eq!(err.code, "config");
                assert!(err.message.contains("/nonexistent/coretempo/tempo.toml"));
            }
        }
        Ok(())
    }

    /// A workflow whose single agent lives in `dir`, plus whatever extra
    /// `[server]` keys a trust test needs.
    fn trust_workflow(dir: &std::path::Path, server: &str) -> String {
        format!(
            "[workflow]\nname = \"trust-flow\"\n{server}\n\
             [agents.builder]\ndir = \"{}\"\nprompt = \"You implement tasks.\"\n",
            dir.display()
        )
    }

    /// Writes `tempo.toml` and an agent dir with no `.git`, both under a unique
    /// name so the HOME-serialized trust tests never read each other's files.
    fn trust_fixture(tag: &str, server: &str) -> anyhow::Result<(PathBuf, PathBuf, PathBuf)> {
        let home = temp_file(&format!("{tag}-home"))?;
        let agent_dir = temp_file(&format!("{tag}-agent"))?;
        std::fs::create_dir_all(&home)?;
        std::fs::create_dir_all(&agent_dir)?;
        let config = temp_file(&format!("{tag}-tempo.toml"))?;
        std::fs::write(&config, trust_workflow(&agent_dir, server))?;
        Ok((home, agent_dir, config))
    }

    /// Runs `run_untrusted_dirs` with `HOME` pointed at an empty temp home, so
    /// `~/.claude.json` is absent and every root reads as untrusted.
    async fn untrusted_dirs_under_home(
        app: &tauri::App<tauri::test::MockRuntime>,
        home: &std::path::Path,
        config: &std::path::Path,
    ) -> Result<Vec<String>, CmdError> {
        use tauri::Manager;
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home) };
        let result =
            run_untrusted_dirs(app.state::<AppState>(), config.display().to_string()).await;
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        result
    }

    #[tokio::test]
    async fn run_untrusted_dirs_lists_the_root_when_policy_does_not_grant() -> anyhow::Result<()> {
        let (home, agent_dir, config) = trust_fixture("untrusted-ask", "")?;
        let app = test_app_with_trust(false)?;
        let _guard = HOME_LOCK.lock().await;
        let roots = untrusted_dirs_under_home(&app, &home, &config).await?;
        assert_eq!(
            roots,
            vec![
                coretempo_core::trust::trust_root(&agent_dir)
                    .display()
                    .to_string()
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_untrusted_dirs_is_empty_when_the_user_config_grants() -> anyhow::Result<()> {
        let (home, _agent_dir, config) = trust_fixture("untrusted-user-grant", "")?;
        let app = test_app_with_trust(true)?;
        let _guard = HOME_LOCK.lock().await;
        let roots = untrusted_dirs_under_home(&app, &home, &config).await?;
        assert!(
            roots.is_empty(),
            "the run may grant, so the desktop must not ask: {roots:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_untrusted_dirs_is_empty_when_the_workflow_grants() -> anyhow::Result<()> {
        let (home, _agent_dir, config) = trust_fixture(
            "untrusted-workflow-grant",
            "\n[server]\ntrust_agent_dirs = true\n",
        )?;
        let app = test_app_with_trust(false)?;
        let _guard = HOME_LOCK.lock().await;
        let roots = untrusted_dirs_under_home(&app, &home, &config).await?;
        assert!(
            roots.is_empty(),
            "[server] trust_agent_dirs grants too: {roots:?}"
        );
        Ok(())
    }

    /// The dialog answer must reach `Run::start_with`'s policy. Without it the
    /// preflight refuses the untrusted root instead of parking an agent on the
    /// trust dialog — and it refuses before any store, port, or PTY exists.
    #[tokio::test]
    async fn run_start_without_confirmation_fails_the_trust_preflight() -> anyhow::Result<()> {
        use tauri::Manager;
        let (home, agent_dir, config) = trust_fixture("start-unconfirmed", "")?;
        let app = test_app_with_trust(false)?;
        let handle = app.handle().clone();
        let _guard = HOME_LOCK.lock().await;
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", &home) };
        let result = run_start(
            handle,
            app.state::<AppState>(),
            config.display().to_string(),
            false,
        )
        .await;
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match result {
            Ok(info) => anyhow::bail!("expected a trust failure, got run {}", info.run_id.0),
            Err(err) => {
                assert_eq!(err.code, "run");
                let root = coretempo_core::trust::trust_root(&agent_dir);
                assert!(
                    err.message.contains(&root.display().to_string()),
                    "the error must name the untrusted root: {}",
                    err.message
                );
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
    async fn flow_commands_without_run_return_no_run() -> anyhow::Result<()> {
        use tauri::Manager;
        let app = test_app()?;
        match run_flows(app.state::<AppState>()).await {
            Ok(flows) => anyhow::bail!("run_flows: expected no_run, got {} flows", flows.len()),
            Err(err) => assert_eq!(err.code, "no_run"),
        }
        match fire_flow(app.state::<AppState>(), "main".to_string()).await {
            Ok(id) => anyhow::bail!("fire_flow: expected no_run, got trigger {id}"),
            Err(err) => assert_eq!(err.code, "no_run"),
        }
        Ok(())
    }

    #[test]
    fn flow_info_serializes_with_a_type_key() -> anyhow::Result<()> {
        let info = FlowInfo {
            name: "main".to_string(),
            trigger_type: "on_start".to_string(),
            target: "worker".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&info)?,
            serde_json::json!({"name": "main", "type": "on_start", "target": "worker"})
        );
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
