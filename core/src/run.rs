//! Run orchestration: wires store → bus → pty → router → api (contracts §4.1).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{Mutex, watch};

use crate::api::auth::{default_runs_dir, repoint_current, write_api_file};
use crate::api::{ApiContext, ApiServerHandle, PtyManagerSource, ServeError, check_bind, serve_on};
use crate::bus::EventBus;
use crate::locks::{AgentLocks, MemberGuards};
use crate::pty::hooks::write_agent_settings_files;
use crate::pty::{AgentEnv, ClearGate, InjectionQueue, PtyError, PtyManager};
use crate::router::{Router, StateSource};
use crate::store::{Store, StoreError};
use crate::time::Timestamp;
use crate::trigger::{TriggerHub, WatchInputs};
use crate::trust::{TrustGate, TrustPolicy, TrustStore, preflight};
use crate::types::agent::AgentState;
use crate::types::config::{FrozenWorkflow, ResolvedServer, WorkflowFile};
use crate::types::event::EventPayload;
use crate::types::id::{AgentId, FlowName, RunId};
use crate::workflow::{ConfigError, load_workflow};

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("workflow config error: {0}")]
    Config(#[from] ConfigError),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("pty error: {0}")]
    Pty(#[from] PtyError),
    #[error("api server error: {0}")]
    Api(#[from] ServeError),
    #[error("io error during run setup: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "cannot locate a home directory for ~/.coretempo/runs and .claude.json; set HOME \
         (CLAUDE_CONFIG_DIR relocates .claude.json only)"
    )]
    NoHome,
    #[error(transparent)]
    Trust(#[from] crate::trust::TrustError),
    #[error(transparent)]
    ClaudeConfig(#[from] crate::claude_config::ClaudeConfigError),
    #[error(
        "tempo.toml at '{path}' changed since it was loaded (hash mismatch); the hash also \
         covers flow schema files, each agent's resolved MCP servers (mcp = [...]) and every \
         file under a declared skill dir (skills = [...]), so an edit to ~/.claude.json, \
         ~/.mcp.json, an agent dir's .mcp.json or a SKILL.md counts too; reload the workflow \
         and start again"
    )]
    SourceChanged { path: PathBuf },
    #[error("cannot locate the directory containing this executable for tempo PATH setup")]
    NoBinDir,
}

/// How a run differs from an interactive one. The defaults are the interactive
/// shape; serve mode (a daemon starting one run per trigger) flips all three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunOptions {
    /// Bind the /v1 API to an ephemeral port instead of the configured one; the
    /// public port belongs to the trigger server. Agents are still handed the
    /// port that was actually bound.
    pub ephemeral_port: bool,
    /// Repoint `~/.coretempo/runs/current` at this run.
    pub repoint_current: bool,
    /// Delete `~/.coretempo/runs/<run_id>` on stop, so a long-lived daemon does
    /// not accumulate one directory per trigger.
    pub cleanup_run_dir: bool,
    /// May this run grant Claude Code trust for its agents' git roots? The
    /// embedding binary resolves it from `~/.coretempo/config.toml` and
    /// `[server] trust_agent_dirs` (spec 2026-08-17 §1). Default: no.
    pub trust: TrustPolicy,
}

impl Default for RunOptions {
    fn default() -> RunOptions {
        RunOptions {
            ephemeral_port: false,
            repoint_current: true,
            cleanup_run_dir: false,
            trust: TrustPolicy::default(),
        }
    }
}

pub struct Run {
    id: RunId,
    started_at: Timestamp,
    bus: EventBus,
    pty: Arc<PtyManager>,
    router: Arc<Router>,
    store: Store,
    workflow: Arc<FrozenWorkflow>,
    workflow_file: Arc<WorkflowFile>,
    api: Mutex<Option<ApiServerHandle>>,
    /// The port the API listener actually bound; see [`Run::port`].
    port: u16,
    options: RunOptions,
    /// `~/.coretempo/runs/<run_id>`: api.json + per-agent settings files live here.
    dir: PathBuf,
    stopped: AtomicBool,
    /// Tripped by [`stop`](Run::stop) before anything is torn down, and read
    /// through the `ApiContext` clone it was handed: a warm trigger parked on
    /// its flow's member locks abandons the wait rather than injecting a
    /// kickoff into a dead PTY manager (multi-flow spec §5).
    stopping: watch::Sender<bool>,
    /// Trigger history for this run, shared with the API's `ApiContext`.
    triggers: Arc<TriggerHub>,
    /// Per-agent lock table for this run, shared with the API's `ApiContext`:
    /// warm `on_start` kickoffs and warm webhook triggers contend on the same
    /// locks (multi-flow spec §5).
    agent_locks: Arc<AgentLocks>,
}

impl std::fmt::Debug for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Run")
            .field("run_id", &self.id)
            .field("workflow", &self.workflow.name)
            .field("started_at", &self.started_at)
            .field("stopped", &self.stopped.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

/// Adapter wiring the router's [`StateSource`] (contracts amendment 1) to the PTY
/// manager's debounced state watch.
struct DebouncedStates(Arc<PtyManager>);

impl StateSource for DebouncedStates {
    fn subscribe_debounced(
        &self,
        agent: &AgentId,
    ) -> Option<tokio::sync::watch::Receiver<AgentState>> {
        self.0.subscribe_state_debounced(agent).ok()
    }

    fn blocked_since(&self, agent: &AgentId) -> Option<crate::pty::Blocked> {
        self.0.blocked_since(agent).ok().flatten()
    }
}

/// A derived (subset) `FrozenWorkflow` spawns only one flow's members, but the
/// API's `GET /v1/workflow` copy is re-read from disk and would show the whole
/// pool. Narrow it to the frozen roster (multi-flow spec §2): the warm trigger
/// path resolves flows against this file, and a subset run's API must never
/// name agents or flows it did not spawn. A whole-pool run passes through
/// unchanged.
fn narrow_file(mut file: WorkflowFile, workflow: &FrozenWorkflow) -> WorkflowFile {
    file.agents.retain(|id, _| workflow.agents.contains_key(id));
    file.flows
        .retain(|name, _| workflow.flows.contains_key(name));
    file
}

/// What [`Run::write_agent_files`] wrote, keyed by agent: `--settings` files,
/// `--mcp-config` files, and the `CLAUDE_CONFIG_DIR` of each `isolated_config`
/// agent. A missing entry in any of them means that agent takes the default.
/// `credential_store` is the operator's, shared by every isolated agent.
struct AgentFiles {
    settings_paths: BTreeMap<AgentId, PathBuf>,
    mcp_paths: BTreeMap<AgentId, PathBuf>,
    config_dirs: BTreeMap<AgentId, PathBuf>,
    credential_store: Option<PathBuf>,
}

impl Run {
    /// Wires store→bus→pty→router (`set_clear_gate` + `set_state_source`)→api; writes
    /// per-agent settings files and api.json (both 0600) + the `current` symlink; emits
    /// `run.started` (always seq 1) and then spawns the agents.
    ///
    /// # Errors
    /// [`RunError::SourceChanged`] if tempo.toml changed since it was frozen;
    /// [`RunError::Config`] if the re-read fails to parse or validate;
    /// [`RunError::Trust`] if an agent dir is untrusted and this run may not
    /// grant trust ([`RunOptions::trust`]);
    /// [`RunError::ClaudeConfig`] if a managed config dir cannot be written;
    /// [`RunError::Store`], [`RunError::Api`], [`RunError::Pty`] from the respective
    /// layers; [`RunError::NoHome`]/[`RunError::NoBinDir`]/[`RunError::Io`] from the
    /// filesystem setup.
    pub async fn start(
        workflow: FrozenWorkflow,
        server: ResolvedServer,
    ) -> Result<Arc<Run>, RunError> {
        Run::start_with(workflow, server, RunOptions::default()).await
    }

    /// Binds the API socket. This happens before anything else reads a port because
    /// the port agents are told about (`CORETEMPO_PORT`, fixed at `PtyManager::new`)
    /// must be the one actually bound — under `ephemeral_port` the configured value
    /// is not it, and an agent with the wrong port cannot reach the server at all.
    async fn bind_api(
        server: &ResolvedServer,
        options: RunOptions,
    ) -> Result<(tokio::net::TcpListener, u16), RunError> {
        check_bind(server.bind, server.token_provisioned)?;
        let addr = SocketAddr::new(
            server.bind,
            if options.ephemeral_port {
                0
            } else {
                server.port
            },
        );
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|source| ServeError::Bind { addr, source })?;
        let port = listener
            .local_addr()
            .map_err(|source| ServeError::Bind { addr, source })?
            .port();
        Ok((listener, port))
    }

    /// Writes the api.json the `tempo` CLI and the desktop shell find this run
    /// by, and — for an interactive run — repoints the `current` symlink at it.
    fn write_discovery_files(
        runs_dir: &std::path::Path,
        run_id: &RunId,
        port: u16,
        server: &ResolvedServer,
        options: RunOptions,
    ) -> Result<(), RunError> {
        let api_json = write_api_file(runs_dir, run_id, port, &server.token)?;
        tracing::debug!(path = %api_json.display(), "wrote api.json");
        if options.repoint_current {
            repoint_current(runs_dir, run_id)?;
        }
        Ok(())
    }

    /// Spec §1 preflight over the roster about to spawn — the whole pool or a
    /// flow subset — before the store is opened or the API bound: an untrusted
    /// root is a configuration problem, reported whole. Returns the store so the
    /// per-spawn gate reuses it.
    fn preflight_trust(
        workflow: &FrozenWorkflow,
        policy: TrustPolicy,
    ) -> Result<TrustStore, RunError> {
        let store = TrustStore::from_env().ok_or(RunError::NoHome)?;
        preflight(
            &store,
            workflow.agents.values().map(|cfg| cfg.dir.as_path()),
            policy,
        )?;
        Ok(store)
    }

    /// The `PtyManager` spawn gate re-checking trust before every spawn and
    /// restart. `mirrors` carries one store per `isolated_config` agent — the
    /// managed dir's own `.claude.json`, which the gate writes the operator's
    /// decision into (spec 2026-08-24 §3).
    fn trust_gate(
        trust_store: TrustStore,
        policy: TrustPolicy,
        mirrors: BTreeMap<AgentId, TrustStore>,
    ) -> Arc<TrustGate> {
        Arc::new(TrustGate::new(trust_store, policy, mirrors))
    }

    /// Every per-agent file a run writes before it spawns anything: the
    /// turn-boundary hook settings, the resolved MCP servers, and the managed
    /// Claude config dirs `isolated_config` agents spawn against. The config
    /// dirs must exist before the trust gate mirrors into them, and that
    /// happens before the first spawn.
    ///
    /// # Errors
    /// From the respective writers; [`RunError::ClaudeConfig`] names the agent
    /// and the path it could not build.
    fn write_agent_files(
        runs_dir: &std::path::Path,
        run_id: &RunId,
        tempo_bin_dir: &std::path::Path,
        workflow: &FrozenWorkflow,
    ) -> Result<AgentFiles, RunError> {
        let settings_paths = write_agent_settings_files(
            runs_dir,
            run_id,
            &tempo_bin_dir.join("tempo"),
            &workflow.agents,
        )?;
        let mcp_paths = crate::mcp::write_agent_mcp_files(runs_dir, run_id, workflow)?;
        let config_dirs =
            crate::claude_config::write_agent_config_dirs(runs_dir, run_id, workflow)?;
        tracing::debug!(
            settings = settings_paths.len(),
            mcp = mcp_paths.len(),
            isolated = config_dirs.len(),
            "wrote per-agent settings, MCP files and managed config dirs"
        );
        let credential_store = crate::claude_config::operator_credential_store();
        if credential_store.is_none() && !config_dirs.is_empty() {
            tracing::warn!(
                "no home directory or CLAUDE_CONFIG_DIR known: isolated agents get no \
                 credential store and will start logged out unless they use an API key"
            );
        }
        Ok(AgentFiles {
            settings_paths,
            mcp_paths,
            config_dirs,
            credential_store,
        })
    }

    /// The two directories every run needs before it can write anything: the
    /// one holding this executable (agents get `tempo` from it on PATH) and
    /// `~/.coretempo/runs`.
    fn setup_dirs() -> Result<(PathBuf, PathBuf), RunError> {
        let tempo_bin_dir = std::env::current_exe()?
            .parent()
            .map(std::path::Path::to_path_buf)
            .ok_or(RunError::NoBinDir)?;
        let runs_dir = default_runs_dir().ok_or(RunError::NoHome)?;
        Ok((tempo_bin_dir, runs_dir))
    }

    /// Opens the store off the tokio worker — `Store::open` blocks on the WAL
    /// conversion retry and the migration's `BEGIN IMMEDIATE`, each waiting out
    /// contention for up to `BUSY_TIMEOUT` — then records this run's `runs` row.
    async fn open_store_and_record_run(
        server: &ResolvedServer,
        run_id: &RunId,
        workflow: &FrozenWorkflow,
        started_at: &Timestamp,
    ) -> Result<Store, RunError> {
        let db = server.db.clone();
        let store_run_id = run_id.clone();
        let store = tokio::task::spawn_blocking(move || Store::open(&db, store_run_id))
            .await
            .map_err(|e| StoreError::Sqlite(format!("store open task failed: {e}")))??;
        store
            .insert_run(run_id, &workflow.name, &workflow.hash, started_at)
            .await?;
        Ok(store)
    }

    /// [`start`](Run::start) with the interactive defaults replaced; see [`RunOptions`].
    ///
    /// # Errors
    /// As [`start`](Run::start).
    pub async fn start_with(
        workflow: FrozenWorkflow,
        server: ResolvedServer,
        options: RunOptions,
    ) -> Result<Arc<Run>, RunError> {
        // Freeze integrity: re-read the source for the API copy and verify the hash.
        let (file, reread) = load_workflow(&workflow.source_path)?;
        if reread.hash != workflow.hash {
            return Err(RunError::SourceChanged {
                path: workflow.source_path.clone(),
            });
        }
        let trust_store = Run::preflight_trust(&workflow, options.trust)?;
        let workflow = Arc::new(workflow);
        let workflow_file = Arc::new(narrow_file(file, &workflow));

        let run_id = RunId::generate();
        let started_at = Timestamp::now();
        tracing::info!(run_id = %run_id.0, name = %workflow.name, "starting run");

        let store =
            Run::open_store_and_record_run(&server, &run_id, &workflow, &started_at).await?;

        let bus = EventBus::new();
        let (tempo_bin_dir, runs_dir) = Run::setup_dirs()?;
        let AgentFiles {
            settings_paths,
            mcp_paths,
            config_dirs,
            credential_store,
        } = Run::write_agent_files(&runs_dir, &run_id, &tempo_bin_dir, &workflow)?;
        let mirrors = config_dirs
            .iter()
            .map(|(id, dir)| (id.clone(), TrustStore::at(dir.join(".claude.json"))))
            .collect();
        let (listener, port) = Run::bind_api(&server, options).await?;
        let pty = PtyManager::new(
            Arc::clone(&workflow),
            bus.clone(),
            AgentEnv {
                port,
                token: server.token.clone(),
                tempo_bin_dir,
                settings_paths,
                mcp_paths,
                config_dirs,
                credential_store,
            },
        );
        let router = Router::new(
            store.clone(),
            bus.clone(),
            Arc::clone(&pty) as Arc<dyn InjectionQueue>,
            Arc::clone(&workflow),
        );
        pty.set_clear_gate(Arc::clone(&router) as Arc<dyn ClearGate>);
        pty.set_spawn_gate(Run::trust_gate(trust_store, options.trust, mirrors));
        router.set_state_source(Arc::new(DebouncedStates(Arc::clone(&pty))));

        let triggers = TriggerHub::new();
        let agent_locks = Arc::new(AgentLocks::new(&workflow.agents));
        let (stopping, stopping_rx) = watch::channel(false);
        let api = serve_on(
            listener,
            ApiContext {
                router: Arc::clone(&router),
                pty: Arc::new(PtyManagerSource(Arc::clone(&pty))),
                bus: bus.clone(),
                workflow: Arc::clone(&workflow),
                workflow_file: Arc::clone(&workflow_file),
                run_id: run_id.clone(),
                started_at: started_at.clone(),
                started: std::time::Instant::now(),
                token: server.token.clone(),
                token_provisioned: server.token_provisioned,
                bind: server.bind,
                port,
                triggers: Arc::clone(&triggers),
                agent_locks: Arc::clone(&agent_locks),
                stopping: stopping_rx,
            },
        )?;

        Run::write_discovery_files(&runs_dir, &run_id, port, &server, options)?;

        bus.publish(EventPayload::RunStarted {
            run_id: run_id.clone(),
            workflow_name: workflow.name.clone(),
            started_at: started_at.clone(),
        });

        pty.spawn_all().await?;

        Ok(Arc::new(Run {
            dir: runs_dir.join(&run_id.0),
            id: run_id,
            started_at,
            bus,
            pty,
            router,
            store,
            workflow,
            workflow_file,
            api: Mutex::new(Some(api)),
            port,
            options,
            stopped: AtomicBool::new(false),
            stopping,
            triggers,
            agent_locks,
        }))
    }

    /// Inputs for a kickoff completion watcher over this run's live wiring
    /// (spec triggers §2). `deadline` is measured from the watcher's own start,
    /// so callers build this immediately before awaiting it.
    ///
    /// No output contract: a contract belongs to a flow, and a flow-scoped
    /// caller gets it from
    /// [`watch_inputs_for_flow`](Run::watch_inputs_for_flow), which overrides
    /// the field. This bare variant now serves only flowless runs.
    #[must_use]
    pub fn watch_inputs(
        &self,
        deadline: std::time::Duration,
        trigger_id: Option<String>,
    ) -> WatchInputs {
        WatchInputs {
            bus: self.bus.clone(),
            router: Arc::clone(&self.router),
            pty: Arc::new(PtyManagerSource(Arc::clone(&self.pty))),
            roster: self.workflow.agents.keys().cloned().collect(),
            idle_debounce: self.workflow.idle_debounce,
            deadline,
            output: None,
            trigger_id,
        }
    }

    /// [`watch_inputs`](Run::watch_inputs) scoped to one flow (multi-flow
    /// spec §4): the roster is the flow's member set — quiescence and
    /// exit-watching ignore other flows' agents — and the output contract is
    /// that flow's. `None` for an undeclared name.
    #[must_use]
    pub fn watch_inputs_for_flow(
        &self,
        name: &FlowName,
        deadline: std::time::Duration,
        trigger_id: Option<String>,
    ) -> Option<WatchInputs> {
        let flow = self.workflow.flows.get(name)?;
        let mut inputs = self.watch_inputs(deadline, trigger_id);
        inputs.roster = flow.members.iter().cloned().collect();
        inputs.output.clone_from(&flow.output);
        Some(inputs)
    }

    /// Acquires this run's per-agent locks for one flow's members (multi-flow
    /// spec §5), held until the returned guards drop. This is the same table
    /// the API's warm webhook triggers take, so an `on_start` kickoff and a
    /// webhook trigger sharing an `exclusive` agent serialize in its one live
    /// session instead of interleaving conversations. Callers hold the guards
    /// across the kickoff *and* its completion watcher.
    ///
    /// Waits as long as the contending flow takes; a caller that must abandon
    /// the wait races this future against its shutdown signal. `None` for an
    /// undeclared flow — no locks are taken.
    pub async fn lock_flow(&self, name: &FlowName) -> Option<MemberGuards> {
        let flow = self.workflow.flows.get(name)?;
        Some(self.agent_locks.acquire(&flow.members).await)
    }

    #[must_use]
    pub fn run_id(&self) -> &RunId {
        &self.id
    }

    #[must_use]
    pub fn started_at(&self) -> &Timestamp {
        &self.started_at
    }

    #[must_use]
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    #[must_use]
    pub fn pty(&self) -> &PtyManager {
        &self.pty
    }

    #[must_use]
    pub fn router(&self) -> &Router {
        &self.router
    }

    #[must_use]
    pub fn triggers(&self) -> &Arc<TriggerHub> {
        &self.triggers
    }

    #[must_use]
    pub fn workflow(&self) -> &FrozenWorkflow {
        &self.workflow
    }

    #[must_use]
    pub fn workflow_file(&self) -> &WorkflowFile {
        &self.workflow_file
    }

    /// The port the API listener actually bound, and the one agents were handed
    /// as `CORETEMPO_PORT`. Under [`RunOptions::ephemeral_port`] this is the
    /// port the kernel picked, not the configured `[workflow] port`.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Kills the PTYs, stops the HTTP server, and marks the run stopped in the store.
    /// Idempotent: a second call is a no-op returning `Ok`.
    ///
    /// # Errors
    /// [`RunError::Store`] if the run row cannot be updated.
    pub async fn stop(&self) -> Result<(), RunError> {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        tracing::info!(run_id = %self.id.0, "stopping run");
        // Announced before the teardown: a trigger task parked on a contended
        // member settles itself instead of waking into a dead run.
        let _ = self.stopping.send(true);
        self.pty.shutdown().await;
        if let Some(api) = self.api.lock().await.take() {
            api.shutdown().await;
        }
        self.store
            .mark_run_stopped(&self.id, &Timestamp::now())
            .await?;
        if self.options.cleanup_run_dir
            && let Err(error) = tokio::fs::remove_dir_all(&self.dir).await
        {
            tracing::warn!(
                path = %self.dir.display(),
                %error,
                "could not remove the run directory; it will have to be cleaned up by hand"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FlowName;
    use crate::workflow::load_workflow;

    #[test]
    fn narrow_file_matches_a_derived_frozen_roster() {
        let text = "[workflow]\nname = \"dev\"\n\
            [agents.reader]\ndir = \"/tmp\"\nprompt = \"p\"\n\
            [agents.writer]\ndir = \"/tmp\"\nprompt = \"p\"\n\
            [flows.solo]\nagents = [\"reader\"]\n\
            trigger = { type = \"webhook\", edge = { to = \"reader\", kind = \"ask\" } }\n\
            [flows.both]\nagents = [\"reader\", \"writer\"]\n\
            trigger = { type = \"webhook\", edge = { to = \"writer\", kind = \"ask\" } }\n";
        let dir = std::env::temp_dir().join(format!("coretempo-narrow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("tempo.toml");
        std::fs::write(&path, text).expect("write config");
        let (file, frozen) = load_workflow(&path).expect("loads");

        let derived = frozen
            .for_flow(&FlowName("solo".into()))
            .expect("declared flow");
        let narrowed = narrow_file(file.clone(), &derived);
        assert_eq!(
            narrowed
                .agents
                .keys()
                .map(|a| a.0.as_str())
                .collect::<Vec<_>>(),
            ["reader"],
            "agents narrowed to the flow's members"
        );
        assert_eq!(
            narrowed
                .flows
                .keys()
                .map(|f| f.0.as_str())
                .collect::<Vec<_>>(),
            ["solo"],
            "flows narrowed to just this flow"
        );
        // Whole-pool run: a no-op.
        let untouched = narrow_file(file.clone(), &frozen);
        assert_eq!(untouched, file);
    }
}
