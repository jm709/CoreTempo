//! `coretempod serve`: the standing webhook listener (spec triggers §3).
//!
//! Serve mode holds no agents between triggers. It owns one public HTTP port,
//! queues inbound triggers in a bounded FIFO, and runs them one at a time —
//! each one cold-starting a `Run` whose own `/v1` API is bound to an ephemeral
//! loopback port and torn down when the kickoff completes.
//!
//! The workflow is frozen at daemon startup. A tempo.toml edited while triggers
//! are queued fails those triggers rather than being adopted mid-queue: an edit
//! can remove the trigger or break validation, and a queued caller is owed the
//! workflow it addressed.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::Context;
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header::HOST};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use coretempo_core::api::auth::{require_bearer, require_host};
use coretempo_core::api::messages::{parse_wait, wait_duration};
use coretempo_core::api::trigger::unknown_trigger;
use coretempo_core::api::{ApiError, stop_accept_loop};
use coretempo_core::run::{Run, RunOptions};
use coretempo_core::trigger::{
    QUEUE_CAP, TriggerAccepted, TriggerHub, TriggerStatus, TriggerView, await_terminal,
    completion_status, read_payload, watch_completion, watcher_deadline,
};
use coretempo_core::types::config::{
    Edge, FrozenWorkflow, ResolvedServer, TriggerType, WorkflowFile,
};
use coretempo_core::types::id::RunId;
use coretempo_core::types::message::Origin;
use coretempo_core::workflow::load_workflow;
use serde::Serialize;
use tokio::sync::{mpsc, watch};

/// Reason recorded for every trigger the daemon could not honour because it was
/// interrupted. Matches the spec's `daemon_shutdown` label.
const SHUTDOWN_REASON: &str = "daemon_shutdown: the daemon was interrupted before this \
                               trigger completed; restart it and fire the trigger again";

/// How long shutdown waits for the worker to stop its run and drain the queue
/// before giving up on a clean exit.
const WORKER_DRAIN_GRACE: Duration = Duration::from_secs(20);

/// Everything `serve` needs, already loaded and validated by `main`.
pub struct ServeInputs {
    pub config: PathBuf,
    pub file: WorkflowFile,
    pub frozen: FrozenWorkflow,
    pub server: ResolvedServer,
}

/// One queued trigger.
struct Job {
    id: String,
    payload: String,
}

/// Serve-mode health. Deliberately not core's `Health`: there is no run id or
/// uptime to report between triggers, and a caller wants the queue instead.
#[derive(Serialize)]
struct ServeHealth {
    status: &'static str,
    version: &'static str,
    queue_depth: usize,
    current_run_id: Option<RunId>,
}

struct ServeState {
    hub: Arc<TriggerHub>,
    jobs: mpsc::Sender<Job>,
    server: ResolvedServer,
    current: Mutex<Option<RunId>>,
    /// Triggers accepted and not yet settled, the running one included.
    ///
    /// Counted here rather than derived from the queue and the in-flight id:
    /// a job that the worker has taken off the channel but not yet marked
    /// running belongs to neither, and a trigger arriving in that window would
    /// be told `position: 0` with one still ahead of it.
    outstanding: AtomicUsize,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ServeState {
    /// Triggers waiting in the FIFO, not counting the one being run.
    fn queue_depth(&self) -> usize {
        self.jobs
            .max_capacity()
            .saturating_sub(self.jobs.capacity())
    }

    /// Claims a place in line, returning how many triggers are ahead of it.
    fn take_place(&self) -> usize {
        self.outstanding.fetch_add(1, Ordering::SeqCst)
    }

    /// Records a trigger's terminal status and gives up its place in line.
    fn settle(&self, id: &str, status: TriggerStatus) {
        self.hub.finish(id, status);
        self.outstanding.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The webhook trigger's edge, or the reason this workflow cannot be served.
fn webhook_edge(file: &WorkflowFile, config: &std::path::Path) -> anyhow::Result<Edge> {
    match &file.trigger {
        Some(trigger) if trigger.trigger_type == TriggerType::Webhook => Ok(trigger.edge.clone()),
        Some(_) => anyhow::bail!(
            "'{}' declares an on_start trigger, which fires at launch rather than over \
             HTTP; run it with `coretempod run` instead of `serve`",
            config.display()
        ),
        None => anyhow::bail!(
            "'{}' declares no [trigger], so there is nothing for serve mode to listen \
             for; either add [trigger] type = \"webhook\" with \
             edge = {{ to = \"<agent>\", kind = \"ask\" }}, or start the workflow with \
             `coretempod run`",
            config.display()
        ),
    }
}

/// Runs the standing trigger listener until ctrl-c.
///
/// # Errors
/// When the workflow declares no webhook trigger, or the public listener cannot
/// be bound.
pub async fn serve(inputs: ServeInputs) -> anyhow::Result<()> {
    let ServeInputs {
        config,
        file,
        frozen,
        server,
    } = inputs;
    webhook_edge(&file, &config)?;

    let hub = TriggerHub::new();
    let (jobs_tx, jobs_rx) = mpsc::channel::<Job>(QUEUE_CAP);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let state = Arc::new(ServeState {
        hub: Arc::clone(&hub),
        jobs: jobs_tx,
        server: server.clone(),
        current: Mutex::new(None),
        outstanding: AtomicUsize::new(0),
    });

    let worker = tokio::spawn(run_worker(Worker {
        hub,
        jobs: jobs_rx,
        config,
        baseline_hash: frozen.hash.clone(),
        state: Arc::clone(&state),
        shutdown: shutdown_rx,
    }));

    let addr = SocketAddr::new(server.bind, server.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind the trigger listener on {addr}"))?;
    tracing::info!(
        %addr,
        workflow = %frozen.name,
        hash = %frozen.hash,
        "serve listening; no agents run until a trigger arrives"
    );
    let (http_tx, mut http_rx) = watch::channel(false);
    let http = tokio::spawn(async move {
        let stop = async move {
            let _ = http_rx.changed().await;
        };
        if let Err(error) = axum::serve(listener, app(state))
            .with_graceful_shutdown(stop)
            .await
        {
            tracing::error!(%error, "trigger listener exited with error");
        }
    });

    crate::signal::interrupted().await?;
    tracing::info!("interrupt received; stopping the in-flight run and failing the queue");
    // Order matters: the worker records the shutdown failures first, so a
    // parked `?wait` long-poll is answered before the listener stops.
    let _ = shutdown_tx.send(true);
    if tokio::time::timeout(WORKER_DRAIN_GRACE, worker)
        .await
        .is_err()
    {
        tracing::warn!("the trigger worker did not stop in time; exiting anyway");
    }
    let _ = http_tx.send(true);
    stop_accept_loop(http).await;
    Ok(())
}

fn app(state: Arc<ServeState>) -> Router {
    Router::new()
        .route("/v1/trigger", post(post_trigger))
        .route("/v1/trigger/{id}", get(get_trigger))
        .route("/v1/health", get(health))
        .fallback(not_found)
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            guard,
        ))
        .with_state(state)
}

async fn guard(State(state): State<Arc<ServeState>>, req: Request, next: Next) -> Response {
    match check(&state, &req) {
        Ok(()) => next.run(req).await,
        Err(error) => error.into_response(),
    }
}

fn check(state: &ServeState, req: &Request) -> Result<(), ApiError> {
    let host = req
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    require_host(host, state.server.bind)?;
    if req.uri().path() == "/v1/health" {
        return Ok(());
    }
    require_bearer(&state.server.token, req.headers())
}

async fn not_found(req: Request) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "invalid_request",
        format!(
            "no route {} {}; a serve-mode daemon answers POST /v1/trigger, \
             GET /v1/trigger/{{id}}, and GET /v1/health — the full /v1 API belongs to \
             each triggered run, which is private to the daemon",
            req.method(),
            req.uri().path()
        ),
    )
}

async fn health(State(state): State<Arc<ServeState>>) -> Json<ServeHealth> {
    Json(ServeHealth {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        queue_depth: state.queue_depth(),
        current_run_id: lock(&state.current).clone(),
    })
}

fn queue_full(depth: usize) -> ApiError {
    ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "queue_full",
        format!(
            "the trigger queue is full ({depth} waiting, cap {QUEUE_CAP}); serve mode runs \
             one workflow at a time — retry once a queued trigger completes, and poll \
             GET /v1/trigger/{{id}} for the ones already accepted"
        ),
    )
}

async fn post_trigger(
    State(state): State<Arc<ServeState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    body: axum::body::Body,
) -> Result<Response, ApiError> {
    let wait = parse_wait(params.get("wait").map(String::as_str))?;
    let payload = read_payload(body).await?;
    // Reserve before minting an id or taking a place in line: a refused trigger
    // must leave no record behind for a caller to poll forever.
    let permit = state
        .jobs
        .try_reserve()
        .map_err(|_| queue_full(state.queue_depth()))?;
    let position = state.take_place();
    let id = state.hub.register(TriggerStatus::Queued { position });
    permit.send(Job {
        id: id.clone(),
        payload,
    });
    tracing::info!(trigger = id, position, "trigger queued");

    if let Some(secs) = wait
        && let Some(status) = await_terminal(&state.hub, &id, wait_duration(secs)).await
    {
        return Ok(Json(TriggerView {
            trigger_id: id,
            status,
        })
        .into_response());
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(TriggerAccepted {
            trigger_id: id,
            position,
        }),
    )
        .into_response())
}

async fn get_trigger(
    State(state): State<Arc<ServeState>>,
    Path(id): Path<String>,
) -> Result<Json<TriggerView>, ApiError> {
    let status = state.hub.get(&id).ok_or_else(|| unknown_trigger(&id))?;
    Ok(Json(TriggerView {
        trigger_id: id,
        status,
    }))
}

struct Worker {
    hub: Arc<TriggerHub>,
    jobs: mpsc::Receiver<Job>,
    config: PathBuf,
    /// The hash serve froze at startup; every trigger is checked against it.
    baseline_hash: String,
    state: Arc<ServeState>,
    shutdown: watch::Receiver<bool>,
}

fn interrupted() -> TriggerStatus {
    TriggerStatus::Failed {
        reason: SHUTDOWN_REASON.to_string(),
        reason_code: "internal".to_string(),
    }
}

/// Resolves once the shutdown flag is set (immediately if it already is).
async fn tripped(shutdown: &mut watch::Receiver<bool>) {
    while !*shutdown.borrow_and_update() {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

/// The FIFO worker: one trigger at a time, for the life of the daemon.
async fn run_worker(mut worker: Worker) {
    let mut shutdown = worker.shutdown.clone();
    loop {
        let next = tokio::select! {
            // Biased, shutdown first: an unbiased select can hand out a job to a
            // daemon that is already stopping, and `Run::start_with` — a whole
            // roster of PTYs — cannot be cancelled once it is underway.
            biased;
            () = tripped(&mut shutdown) => None,
            job = worker.jobs.recv() => job,
        };
        let Some(job) = next else { break };
        // The flag can also be set between the dequeue and here. Starting a run
        // now would spend the shutdown grace on work nobody will collect.
        if *worker.shutdown.borrow() {
            worker.state.settle(&job.id, interrupted());
            break;
        }
        worker.hub.begin(&job.id);
        let status = run_trigger(&worker, &job).await;
        tracing::info!(trigger = job.id, ?status, "trigger finished");
        worker.state.settle(&job.id, status);
        if *worker.shutdown.borrow() {
            break;
        }
    }
    // Everything still queued dies with the daemon: the queue is in memory, so
    // a caller has to be told rather than left waiting on a restart.
    worker.jobs.close();
    while let Ok(job) = worker.jobs.try_recv() {
        tracing::info!(
            trigger = job.id,
            "failing a queued trigger: daemon shutdown"
        );
        worker.state.settle(&job.id, interrupted());
    }
}

/// Re-freezes the workflow, refusing edits made since the daemon started.
fn reload(worker: &Worker) -> Result<(WorkflowFile, FrozenWorkflow), TriggerStatus> {
    let (file, frozen) = load_workflow(&worker.config).map_err(|error| TriggerStatus::Failed {
        reason: format!(
            "could not reload '{}' for this trigger: {error}; fix the file and restart \
             the daemon",
            worker.config.display()
        ),
        reason_code: "workflow_changed".to_string(),
    })?;
    if frozen.hash != worker.baseline_hash {
        return Err(TriggerStatus::Failed {
            reason: format!(
                "'{}' changed since the daemon started (serving {}, on disk {}); serve \
                 mode freezes the workflow at startup so a queued trigger cannot be \
                 answered by a different workflow — restart the daemon to adopt edits",
                worker.config.display(),
                worker.baseline_hash,
                frozen.hash
            ),
            reason_code: "workflow_changed".to_string(),
        });
    }
    Ok((file, frozen))
}

/// The run's own API is private to the daemon: loopback, ephemeral port, no
/// `current` symlink, artifacts removed on stop.
fn per_run_server(server: &ResolvedServer) -> ResolvedServer {
    ResolvedServer {
        bind: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        port: 0,
        db: server.db.clone(),
        token: server.token.clone(),
        token_provisioned: server.token_provisioned,
        log: server.log.clone(),
    }
}

const RUN_OPTIONS: RunOptions = RunOptions {
    ephemeral_port: true,
    repoint_current: false,
    cleanup_run_dir: true,
};

/// Cold-starts a run for one trigger, awaits its kickoff, and tears it down.
async fn run_trigger(worker: &Worker, job: &Job) -> TriggerStatus {
    let (file, frozen) = match reload(worker) {
        Ok(loaded) => loaded,
        Err(status) => return status,
    };
    let edge = match webhook_edge(&file, &worker.config) {
        Ok(edge) => edge,
        Err(error) => {
            return TriggerStatus::Failed {
                reason: error.to_string(),
                reason_code: "kickoff_rejected".to_string(),
            };
        }
    };
    let ask_timeout = frozen.ask_timeout;
    let run = match Run::start_with(frozen, per_run_server(&worker.state.server), RUN_OPTIONS).await
    {
        Ok(run) => run,
        Err(error) => {
            return TriggerStatus::Failed {
                reason: format!("could not start a run for this trigger: {error}"),
                reason_code: "internal".to_string(),
            };
        }
    };
    *lock(&worker.state.current) = Some(run.run_id().clone());
    let status = kickoff(worker, job, &run, (edge, ask_timeout)).await;
    if let Err(error) = run.stop().await {
        tracing::warn!(%error, "could not stop the trigger's run cleanly");
    }
    *lock(&worker.state.current) = None;
    status
}

/// Injects the payload and waits for the workflow to finish it.
async fn kickoff(
    worker: &Worker,
    job: &Job,
    run: &Run,
    (edge, ask_timeout): (Edge, Duration),
) -> TriggerStatus {
    let origin = Origin::Http(job.id.strip_prefix("t-").unwrap_or(&job.id).to_string());
    let record = match run
        .router()
        .create_message(
            origin,
            edge.to,
            edge.kind.message_kind(),
            job.payload.clone(),
        )
        .await
    {
        Ok(record) => record,
        Err(error) => {
            return TriggerStatus::Failed {
                reason: format!("could not inject the kickoff message: {error}"),
                reason_code: "kickoff_rejected".to_string(),
            };
        }
    };
    // Built immediately after creation: the watcher's deadline runs from here.
    let inputs = run.watch_inputs(watcher_deadline(ask_timeout), Some(job.id.clone()));
    let mut shutdown = worker.shutdown.clone();
    tokio::select! {
        completion = watch_completion(inputs, record) => completion_status(completion),
        () = tripped(&mut shutdown) => interrupted(),
    }
}
