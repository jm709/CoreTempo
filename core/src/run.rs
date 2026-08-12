//! Run orchestration: wires store → bus → pty → router → api (contracts §4.1).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Mutex;

use crate::api::auth::{default_runs_dir, repoint_current, write_api_file};
use crate::api::{ApiContext, ApiServerHandle, PtyManagerSource, ServeError, check_bind, serve_on};
use crate::bus::EventBus;
use crate::pty::hooks::write_agent_settings_files;
use crate::pty::{AgentEnv, ClearGate, InjectionQueue, PtyError, PtyManager};
use crate::router::{Router, StateSource};
use crate::store::{Store, StoreError};
use crate::time::Timestamp;
use crate::trigger::{TriggerHub, WatchInputs};
use crate::types::agent::AgentState;
use crate::types::config::{FrozenWorkflow, ResolvedServer, WorkflowFile};
use crate::types::event::EventPayload;
use crate::types::id::{AgentId, RunId};
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
    #[error("cannot locate a home directory for ~/.coretempo/runs; set HOME")]
    NoHome,
    #[error(
        "tempo.toml at '{path}' changed since it was loaded (hash mismatch); \
         reload the workflow and start again"
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
}

impl Default for RunOptions {
    fn default() -> RunOptions {
        RunOptions {
            ephemeral_port: false,
            repoint_current: true,
            cleanup_run_dir: false,
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
    options: RunOptions,
    /// `~/.coretempo/runs/<run_id>`: api.json + per-agent settings files live here.
    dir: PathBuf,
    stopped: AtomicBool,
    /// Trigger history for this run, shared with the API's `ApiContext`.
    triggers: Arc<TriggerHub>,
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
}

impl Run {
    /// Wires store→bus→pty→router (`set_clear_gate` + `set_state_source`)→api; writes
    /// per-agent settings files and api.json (both 0600) + the `current` symlink; emits
    /// `run.started` (always seq 1) and then spawns the agents.
    ///
    /// # Errors
    /// [`RunError::SourceChanged`] if tempo.toml changed since it was frozen;
    /// [`RunError::Config`] if the re-read fails to parse or validate;
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
        let workflow = Arc::new(workflow);
        let workflow_file = Arc::new(file);

        let run_id = RunId::generate();
        let started_at = Timestamp::now();
        tracing::info!(run_id = %run_id.0, name = %workflow.name, "starting run");

        let store = Store::open(&server.db)?;
        store
            .insert_run(&run_id, &workflow.name, &workflow.hash, &started_at)
            .await?;

        let bus = EventBus::new();
        let tempo_bin_dir = std::env::current_exe()?
            .parent()
            .map(std::path::Path::to_path_buf)
            .ok_or(RunError::NoBinDir)?;
        let runs_dir = default_runs_dir().ok_or(RunError::NoHome)?;
        let settings_paths = write_agent_settings_files(
            &runs_dir,
            &run_id,
            &tempo_bin_dir.join("tempo"),
            &workflow.agents,
        )?;
        tracing::debug!(
            count = settings_paths.len(),
            "wrote per-agent settings files"
        );
        let (listener, port) = Run::bind_api(&server, options).await?;
        let pty = PtyManager::new(
            Arc::clone(&workflow),
            bus.clone(),
            AgentEnv {
                port,
                token: server.token.clone(),
                tempo_bin_dir,
                settings_paths,
            },
        );
        let router = Router::new(
            store.clone(),
            bus.clone(),
            Arc::clone(&pty) as Arc<dyn InjectionQueue>,
            Arc::clone(&workflow),
        );
        pty.set_clear_gate(Arc::clone(&router) as Arc<dyn ClearGate>);
        router.set_state_source(Arc::new(DebouncedStates(Arc::clone(&pty))));

        let triggers = TriggerHub::new();
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
            },
        )?;

        let api_json = write_api_file(&runs_dir, &run_id, port, &server.token)?;
        tracing::debug!(path = %api_json.display(), "wrote api.json");
        if options.repoint_current {
            repoint_current(&runs_dir, &run_id)?;
        }

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
            options,
            stopped: AtomicBool::new(false),
            triggers,
        }))
    }

    /// Inputs for a kickoff completion watcher over this run's live wiring
    /// (spec triggers §2). `deadline` is measured from the watcher's own start,
    /// so callers build this immediately before awaiting it.
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
            output: self.workflow.output.clone(),
            trigger_id,
        }
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
