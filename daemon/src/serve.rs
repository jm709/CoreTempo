//! `coretempod serve`: the standing webhook listener (multi-flow spec §4).
//!
//! Serve mode holds no agents between triggers. It owns one public HTTP port
//! and gives every webhook flow its own bounded FIFO and worker. A worker
//! dequeues, takes its flow's members' locks, takes one `max_concurrent_runs`
//! permit, and spawns the run un-awaited — so disjoint flows overlap, an
//! `exclusive` member serializes whoever shares it, and an all-`shared` flow
//! overlaps with itself. Each run cold-starts a `Run` whose own `/v1` API is
//! bound to an ephemeral loopback port and torn down when the kickoff
//! completes.
//!
//! The workflow is frozen at daemon startup. A tempo.toml edited while triggers
//! are queued fails those triggers rather than being adopted mid-queue: an edit
//! can remove the trigger or break validation, and a queued caller is owed the
//! workflow it addressed.

use std::collections::{BTreeMap, BTreeSet};
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
use coretempo_core::api::auth::{TokenHint, require_bearer, require_host};
use coretempo_core::api::messages::{parse_wait, wait_duration};
use coretempo_core::api::trigger::unknown_trigger;
use coretempo_core::api::{ApiError, stop_accept_loop};
use coretempo_core::locks::{AgentLocks, MemberGuards};
use coretempo_core::router::FlowKickoff;
use coretempo_core::run::{Run, RunOptions};
use coretempo_core::trigger::{
    QUEUE_CAP, SettleOnDrop, SettleSink, TriggerAccepted, TriggerHub, TriggerStatus, TriggerView,
    await_terminal, completion_status, read_payload, watch_completion, watcher_deadline,
};
use coretempo_core::trust::{TrustPolicy, TrustStore, preflight};
use coretempo_core::types::FlowView;
use coretempo_core::types::config::{
    FrozenFlow, FrozenWorkflow, ResolvedServer, TriggerType, WorkflowFile,
};
use coretempo_core::types::id::{AgentId, FlowName};
use coretempo_core::types::message::Origin;
use coretempo_core::workflow::load_workflow;
use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};

/// Reason recorded for every trigger the daemon could not honour because it was
/// interrupted. Matches the spec's `daemon_shutdown` label.
const SHUTDOWN_REASON: &str = "daemon_shutdown: the daemon was interrupted before this \
                               trigger completed; restart it and fire the trigger again";

/// How long shutdown waits for the workers to stop their runs and drain the
/// queues before giving up on a clean exit.
const WORKER_DRAIN_GRACE: Duration = Duration::from_secs(20);

/// Everything `serve` needs, already loaded and validated by `main`.
pub struct ServeInputs {
    pub config: PathBuf,
    pub file: WorkflowFile,
    pub frozen: FrozenWorkflow,
    pub server: ResolvedServer,
    pub trust: TrustPolicy,
    pub interrupt: crate::signal::Interrupt,
}

/// One queued trigger.
struct Job {
    id: String,
    payload: String,
}

/// One flow's ingress: its FIFO plus the counters health reads.
struct FlowIngress {
    jobs: mpsc::Sender<Job>,
    /// The accept/drain interlock: true while this flow's worker is still there
    /// to run what it is handed.
    ///
    /// Held across the whole accept — reserve, register, send — and across the
    /// worker's close-and-drain, so a trigger is either on the queue in time to
    /// be drained (and failed with `daemon_shutdown`) or refused before it is
    /// registered. Without it, one that reserved its slot just as the drain ran
    /// lands on a queue nobody reads again: accepted, `queued`, never settled.
    open: Mutex<bool>,
    /// Accepted and not yet settled (running ones included) — the `position`
    /// a new caller is told, and the base `queued` is derived from.
    ///
    /// Counted here rather than derived from the queue and the live runs: a
    /// job a worker has taken off the channel but not yet begun belongs to
    /// neither, and a trigger arriving in that window would be told
    /// `position: 0` with one still ahead of it.
    outstanding: AtomicUsize,
    /// Live runs this flow has right now (>1 only for all-shared flows).
    running: AtomicUsize,
}

/// Queue depth and live-run count for one flow.
pub(crate) struct FlowLoad {
    pub queued: usize,
    pub running: usize,
}

/// Trigger type and target of one declared flow — the listing's static half.
/// Covers `on_start` flows too, which have no ingress.
struct FlowMeta {
    trigger_type: TriggerType,
    target: AgentId,
}

struct ServeState {
    hub: Arc<TriggerHub>,
    flows: BTreeMap<FlowName, FlowIngress>,
    /// Every declared flow, webhook and `on_start` alike: what `GET /v1/flows`
    /// lists and what tells an `on_start` name apart from an undeclared one.
    meta: BTreeMap<FlowName, FlowMeta>,
    server: ResolvedServer,
    trust: TrustPolicy,
}

impl ServeState {
    fn ingress(&self, flow: &FlowName) -> Option<&FlowIngress> {
        self.flows.get(flow)
    }

    /// Every flow's waiting and running counts.
    ///
    /// `queued` is what a flow holds minus what it is running, not what its
    /// channel holds: a worker takes its job off the channel before waiting on
    /// the member locks and the run permit, so channel occupancy reports
    /// nothing waiting while a trigger sits parked on either.
    pub(crate) fn flow_loads(&self) -> BTreeMap<FlowName, FlowLoad> {
        self.flows
            .iter()
            .map(|(name, ingress)| {
                let running = ingress.running.load(Ordering::SeqCst);
                (
                    name.clone(),
                    FlowLoad {
                        queued: ingress
                            .outstanding
                            .load(Ordering::SeqCst)
                            .saturating_sub(running),
                        running,
                    },
                )
            })
            .collect()
    }

    /// Records the terminal status of a trigger the scheduler never committed
    /// to a run — refused at shutdown while queued or parked — and gives its
    /// queue slot back.
    fn settle_queued(&self, flow: &FlowName, id: &str, status: TriggerStatus) {
        if let Some(ingress) = self.ingress(flow) {
            release(&ingress.outstanding);
        }
        self.hub.finish(id, status);
    }

    /// Records a running trigger's terminal status and gives back both the
    /// queue slot and the run count.
    ///
    /// The order is load-bearing. `queued` is `outstanding - running`, so
    /// `outstanding` drops first: a reader landing between the two decrements
    /// sees the trigger still running rather than spuriously back in the queue.
    /// The hub record goes terminal last, so a caller that polls a settled
    /// trigger and then reads health never sees its own run still counted.
    fn settle_running(&self, flow: &FlowName, id: &str, status: TriggerStatus) {
        if let Some(ingress) = self.ingress(flow) {
            release(&ingress.outstanding);
            release(&ingress.running);
        }
        self.hub.finish(id, status);
    }
}

/// Decrements a flow counter without wrapping past zero: one stray settle would
/// otherwise leave the flow reporting `usize::MAX` waiting for the rest of the
/// daemon's life.
fn release(counter: &AtomicUsize) {
    let _ = counter.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
        Some(count.saturating_sub(1))
    });
}

/// Serve-mode health. Deliberately not core's `Health`: there is no single run
/// or uptime to report between triggers, and a caller wants the queues instead.
#[derive(Serialize)]
struct ServeHealth {
    status: &'static str,
    version: &'static str,
    /// Waiting per declared flow, not counting running ones; `on_start` flows
    /// have no queue and report 0.
    queued: BTreeMap<FlowName, usize>,
    /// Live runs right now, bounded by `max_concurrent_runs`.
    running: usize,
}

/// The webhook flows serve listens for, or why this workflow cannot be served.
fn webhook_flows(
    frozen: &FrozenWorkflow,
    config: &std::path::Path,
) -> anyhow::Result<BTreeMap<FlowName, BTreeSet<AgentId>>> {
    let flows: BTreeMap<FlowName, BTreeSet<AgentId>> = frozen
        .flows
        .iter()
        .filter(|(_, flow)| flow.trigger_type == TriggerType::Webhook)
        .map(|(name, flow)| (name.clone(), flow.members.clone()))
        .collect();
    if flows.is_empty() {
        if frozen.flows.is_empty() {
            anyhow::bail!(
                "'{}' declares no [flows.<name>], so there is nothing for serve \
                 mode to listen for; add a flow with trigger = {{ type = \
                 \"webhook\", edge = {{ to = \"<agent>\", kind = \"ask\" }} }}, \
                 or start the workflow with `coretempod run`",
                config.display()
            );
        }
        let names: Vec<&str> = frozen.flows.keys().map(|name| name.0.as_str()).collect();
        // Non-empty: the zero-flow case bailed above. The first name is the one
        // the `run --flow` example uses, so the advice is runnable as-is.
        let first = names.first().copied().unwrap_or("<flow>");
        anyhow::bail!(
            "'{}' declares only on_start flows ({}), which fire at launch rather \
             than over HTTP, so serve mode has nothing to listen for; run one with \
             `coretempod run {} --flow {first}`, or add a webhook flow with \
             trigger = {{ type = \"webhook\", edge = {{ to = \"<agent>\", \
             kind = \"ask\" }} }}",
            config.display(),
            names.join(", "),
            config.display(),
        );
    }
    Ok(flows)
}

/// Every declared flow's trigger type and target, `on_start` ones included:
/// what the listing reports and what a trigger's name is resolved against.
fn flow_meta(frozen: &FrozenWorkflow) -> BTreeMap<FlowName, FlowMeta> {
    frozen
        .flows
        .iter()
        .map(|(name, flow)| {
            (
                name.clone(),
                FlowMeta {
                    trigger_type: flow.trigger_type,
                    target: flow.edge.to.clone(),
                },
            )
        })
        .collect()
}

/// Spec §1: preflight the whole pool at boot — the file is frozen, so the
/// roster cannot change — rather than discover an untrusted root after a
/// trigger has been 202'd, queued, and holds locks.
///
/// # Errors
/// When `HOME` is unset, or a root is untrusted and the policy forbids
/// granting it.
fn preflight_pool(frozen: &FrozenWorkflow, trust: TrustPolicy) -> anyhow::Result<()> {
    let store = TrustStore::from_env()
        .context("cannot determine HOME or CLAUDE_CONFIG_DIR for .claude.json")?;
    preflight(
        &store,
        frozen.agents.values().map(|cfg| cfg.dir.as_path()),
        trust,
    )
    .context("refusing to serve")?;
    Ok(())
}

/// Serve mode is headless: it writes no `api.json` and never repoints
/// `~/.coretempo/runs/current` — both belong to interactive runs — so a
/// generated token is a secret nobody can read. Refusing at startup beats
/// listening on a port whose every trigger 401s (#57).
///
/// # Errors
/// When neither the environment, a token file, nor `[server] token_file`
/// provisioned a token.
fn require_provisioned_token(
    server: &ResolvedServer,
    config: &std::path::Path,
) -> anyhow::Result<()> {
    if server.token_provisioned {
        return Ok(());
    }
    anyhow::bail!(
        "refusing to serve without a provisioned API token: `coretempod serve` is \
         headless — it writes no api.json and never repoints \
         ~/.coretempo/runs/current, so a token generated here would reach no caller \
         and every trigger would answer 401. Provision one (64 lowercase hex \
         characters, e.g. `openssl rand -hex 32`) in any one of: CORETEMPO_TOKEN in \
         the daemon's environment, `--token-file <path>` (or CORETEMPO_TOKEN_FILE), \
         or `[server]` `token_file = \"<path>\"` in '{}'",
        config.display()
    );
}

/// Runs the standing trigger listener until ctrl-c.
///
/// # Errors
/// When the workflow declares no webhook flow, when no token was provisioned,
/// when an agent's trust root is not trusted by Claude Code and the policy does
/// not grant it (spec 2026-08-17 §1 preflight), or when the public listener
/// cannot be bound.
pub async fn serve(inputs: ServeInputs) -> anyhow::Result<()> {
    let ServeInputs {
        config,
        file,
        frozen,
        server,
        trust,
        mut interrupt,
    } = inputs;
    // The file first: a workflow serve cannot listen for at all is the more
    // fundamental complaint, and its fix is the same with or without a token.
    let flows = webhook_flows(&frozen, &config)?;
    require_provisioned_token(&server, &config)?;
    preflight_pool(&frozen, trust)?;

    let hub = TriggerHub::new();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let locks = Arc::new(AgentLocks::new(&frozen.agents));
    let permits = Arc::new(Semaphore::new(file.server.max_concurrent_runs));
    // Every spawned run task holds a clone of `live_tx`; after the workers
    // stop, `live_rx.recv()` returning None is proof every detached run task
    // has dropped — the completion barrier for the drain grace.
    let (live_tx, mut live_rx) = mpsc::channel::<()>(1);

    let mut ingresses = BTreeMap::new();
    let mut worker_inputs = Vec::new();
    for (flow, members) in flows {
        let (jobs_tx, jobs_rx) = mpsc::channel::<Job>(QUEUE_CAP);
        ingresses.insert(
            flow.clone(),
            FlowIngress {
                jobs: jobs_tx,
                open: Mutex::new(true),
                outstanding: AtomicUsize::new(0),
                running: AtomicUsize::new(0),
            },
        );
        worker_inputs.push((flow, members, jobs_rx));
    }
    let state = Arc::new(ServeState {
        hub: Arc::clone(&hub),
        flows: ingresses,
        meta: flow_meta(&frozen),
        server: server.clone(),
        trust,
    });
    let mut workers = Vec::new();
    for (flow, members, jobs_rx) in worker_inputs {
        workers.push(tokio::spawn(run_flow_worker(FlowWorker {
            flow,
            members,
            jobs: jobs_rx,
            config: config.clone(),
            baseline_hash: frozen.hash.clone(),
            state: Arc::clone(&state),
            shutdown: shutdown_rx.clone(),
            locks: Arc::clone(&locks),
            permits: Arc::clone(&permits),
            live: live_tx.clone(),
        })));
    }
    drop(live_tx);

    let addr = SocketAddr::new(server.bind, server.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind the trigger listener on {addr}"))?;
    // What was bound, not what was asked for: with `--port 0` the kernel picks,
    // and this line is where a caller learns which port to talk to.
    let addr = listener.local_addr().unwrap_or(addr);
    tracing::info!(
        %addr,
        workflow = %frozen.name,
        hash = %frozen.hash,
        flows = state.flows.len(),
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

    interrupt.wait().await?;
    tracing::info!("interrupt received; stopping the in-flight runs and failing the queues");
    // Order matters: the workers record the shutdown failures first, so a
    // parked `?wait` long-poll is answered before the listener stops.
    let _ = shutdown_tx.send(true);
    let drain = async {
        for worker in workers {
            let _ = worker.await;
        }
        // None = every detached run task has dropped its token.
        while live_rx.recv().await.is_some() {}
    };
    if tokio::time::timeout(WORKER_DRAIN_GRACE, drain)
        .await
        .is_err()
    {
        tracing::warn!("flow workers or runs did not stop in time; exiting anyway");
    }
    let _ = http_tx.send(true);
    stop_accept_loop(http).await;
    Ok(())
}

fn app(state: Arc<ServeState>) -> Router {
    Router::new()
        .route("/v1/flows/{name}/trigger", post(post_flow_trigger))
        .route("/v1/flows", get(list_flows))
        .route("/v1/trigger", post(removed_trigger_route))
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
    require_bearer(&state.server.token, req.headers(), TokenHint::Serve)
}

async fn not_found(req: Request) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "invalid_request",
        format!(
            "no route {} {}; a serve-mode daemon answers \
             POST /v1/flows/{{name}}/trigger, GET /v1/flows, GET /v1/trigger/{{id}}, \
             and GET /v1/health — the full /v1 API belongs to each triggered run, \
             which is private to the daemon",
            req.method(),
            req.uri().path()
        ),
    )
}

async fn health(State(state): State<Arc<ServeState>>) -> Json<ServeHealth> {
    let loads = state.flow_loads();
    let queued = state
        .meta
        .keys()
        .map(|name| (name.clone(), loads.get(name).map_or(0, |load| load.queued)))
        .collect();
    Json(ServeHealth {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        queued,
        running: loads.values().map(|load| load.running).sum(),
    })
}

/// `GET /v1/flows`: every declared flow with its live counters (multi-flow
/// spec §5). An `on_start` flow has no ingress, so it lists at depth 0.
async fn list_flows(State(state): State<Arc<ServeState>>) -> Json<Vec<FlowView>> {
    let loads = state.flow_loads();
    Json(
        state
            .meta
            .iter()
            .map(|(name, meta)| {
                let load = loads.get(name);
                FlowView {
                    name: name.clone(),
                    trigger_type: meta.trigger_type,
                    target: meta.target.clone(),
                    queue_depth: load.map_or(0, |load| load.queued),
                    running: load.map_or(0, |load| load.running),
                }
            })
            .collect(),
    )
}

/// Every declared flow name: the roster the flow errors have to carry.
fn declared_flows(state: &ServeState) -> String {
    state
        .meta
        .keys()
        .map(|name| name.0.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Bare `POST /v1/trigger` was removed (multi-flow spec §5); the 404 names the
/// declared flows and the per-flow route so an old caller can rewrite itself.
async fn removed_trigger_route(State(state): State<Arc<ServeState>>) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "invalid_request",
        format!(
            "POST /v1/trigger was replaced by POST /v1/flows/{{name}}/trigger; \
             declared flows: {}. @coretempo/client 2.x targets the new route",
            declared_flows(&state)
        ),
    )
}

/// 503 for a trigger that arrives after its flow stopped accepting: the daemon
/// was interrupted and is draining. Deliberately not [`queue_full`] — a 429
/// tells the caller to retry once something completes, which is advice for a
/// daemon that is still running.
fn shutting_down(flow: &FlowName) -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "shutting_down",
        format!(
            "the daemon is shutting down and flow '{}' is no longer accepting triggers; \
             the ones it had already accepted are being failed with daemon_shutdown — \
             restart the daemon and fire this trigger again",
            flow.0
        ),
    )
}

fn queue_full(flow: &FlowName, depth: usize) -> ApiError {
    ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "queue_full",
        format!(
            "flow '{}''s trigger queue is full ({depth} waiting, cap {QUEUE_CAP}); \
             each flow queues independently — retry once one completes, and poll \
             GET /v1/trigger/{{id}} for the ones already accepted",
            flow.0
        ),
    )
}

async fn post_flow_trigger(
    State(state): State<Arc<ServeState>>,
    Path(name): Path<String>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    body: axum::body::Body,
) -> Result<Response, ApiError> {
    let flow = FlowName(name);
    let Some(ingress) = state.ingress(&flow) else {
        return Err(no_ingress(&state, &flow));
    };
    enqueue(&state, &flow, ingress, params, body).await
}

/// Why a flow name has no queue to join: it fires at launch, it was never
/// declared, or — impossible by construction — its ingress went missing.
fn no_ingress(state: &ServeState, flow: &FlowName) -> ApiError {
    match state.meta.get(flow).map(|meta| meta.trigger_type) {
        Some(TriggerType::OnStart) => ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!(
                "flow '{name}' is on_start: it fires its configured message at launch, \
                 not over HTTP — start it with `coretempod run <config> --flow {name}`; \
                 HTTP triggers drive webhook flows",
                name = flow.0
            ),
        ),
        Some(TriggerType::Webhook) => {
            internal_error(&format!("flow '{}' vanished from serve state", flow.0))
        }
        None => ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown_flow",
            format!(
                "no flow named '{}' in this workflow; declared flows: {}. Fire a webhook \
                 flow with POST /v1/flows/{{name}}/trigger, and list them with GET /v1/flows",
                flow.0,
                declared_flows(state)
            ),
        ),
    }
}

fn internal_error(detail: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        format!("{detail}; this is a CoreTempo bug — report it"),
    )
}

/// This flow's accept/drain interlock.
fn ingress_gate(ingress: &FlowIngress) -> MutexGuard<'_, bool> {
    ingress.open.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The accept itself: reserve a queue slot, take a place in line, register,
/// send — a refused trigger must leave no record behind, and an accepted one
/// must be on the queue before the worker that drains it can look.
///
/// Synchronous on purpose: the whole sequence runs under the flow's interlock,
/// which nothing may await while holding.
fn admit(
    state: &ServeState,
    flow: &FlowName,
    ingress: &FlowIngress,
    payload: String,
) -> Result<(String, usize), ApiError> {
    let open = ingress_gate(ingress);
    if !*open {
        return Err(shutting_down(flow));
    }
    let permit = ingress.jobs.try_reserve().map_err(|error| match error {
        mpsc::error::TrySendError::Full(()) => queue_full(
            flow,
            state.flow_loads().get(flow).map_or(0, |load| load.queued),
        ),
        // The worker is gone without having closed the gate: its task died.
        // Nothing will run this trigger either way.
        mpsc::error::TrySendError::Closed(()) => shutting_down(flow),
    })?;
    let position = ingress.outstanding.fetch_add(1, Ordering::SeqCst);
    let id = state.hub.register(TriggerStatus::Queued { position });
    permit.send(Job {
        id: id.clone(),
        payload,
    });
    Ok((id, position))
}

/// Stops accepting for `flow` and fails everything its queue still holds.
///
/// The flag flip and the drain share one critical section: an accept that is
/// mid-flight either got its job on the queue before the flip — and is failed
/// right here — or finds the gate shut and is refused.
fn close_queue(state: &ServeState, flow: &FlowName, jobs: &mut mpsc::Receiver<Job>) {
    let mut drained = Vec::new();
    {
        let mut gate = state.ingress(flow).map(ingress_gate);
        if let Some(open) = gate.as_deref_mut() {
            *open = false;
        }
        jobs.close();
        while let Ok(job) = jobs.try_recv() {
            drained.push(job);
        }
    }
    for job in drained {
        tracing::info!(
            trigger = job.id,
            flow = %flow.0,
            "failing a queued trigger: daemon shutdown"
        );
        state.settle_queued(flow, &job.id, interrupted());
    }
}

/// The shared accept path: [`admit`], then the caller's optional long-poll.
async fn enqueue(
    state: &Arc<ServeState>,
    flow: &FlowName,
    ingress: &FlowIngress,
    params: std::collections::HashMap<String, String>,
    body: axum::body::Body,
) -> Result<Response, ApiError> {
    let wait = parse_wait(params.get("wait").map(String::as_str))?;
    let payload = read_payload(body).await?;
    let (id, position) = admit(state, flow, ingress, payload)?;
    tracing::info!(trigger = id, flow = %flow.0, position, "trigger queued");

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

/// One flow's queue, its member set, and everything its runs need.
struct FlowWorker {
    flow: FlowName,
    members: BTreeSet<AgentId>,
    jobs: mpsc::Receiver<Job>,
    config: PathBuf,
    /// The hash serve froze at startup; every trigger is checked against it.
    baseline_hash: String,
    state: Arc<ServeState>,
    shutdown: watch::Receiver<bool>,
    locks: Arc<AgentLocks>,
    permits: Arc<Semaphore>,
    live: mpsc::Sender<()>,
}

impl FlowWorker {
    /// Fails a job this worker dequeued but never started, and gives back the
    /// queue slot it still counts against.
    fn abandon(&self, job: &Job) {
        self.state.settle_queued(&self.flow, &job.id, interrupted());
    }
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

/// One flow's FIFO worker: dequeue, locks, permit, spawn the run un-awaited.
async fn run_flow_worker(mut worker: FlowWorker) {
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
        // The flag can also be set between the dequeue and here.
        if *worker.shutdown.borrow() {
            worker.abandon(&job);
            break;
        }
        // Locks before permit (spec §4): parked on a busy exclusive agent we
        // hold nothing the cap counts, so a queued writer flow never parks a
        // permit a disjoint flow could use. Both waits race shutdown — a
        // parked worker settles its dequeued trigger immediately instead of
        // burning the drain grace.
        let guards = tokio::select! {
            biased;
            () = tripped(&mut shutdown) => {
                worker.abandon(&job);
                break;
            }
            guards = worker.locks.acquire(&worker.members) => guards,
        };
        let permit = tokio::select! {
            biased;
            () = tripped(&mut shutdown) => {
                worker.abandon(&job);
                break;
            }
            permit = Arc::clone(&worker.permits).acquire_owned() => {
                // Closed only if the semaphore is dropped, which cannot happen
                // while a worker holds its Arc — treated as shutdown anyway.
                let Ok(permit) = permit else {
                    worker.abandon(&job);
                    break;
                };
                permit
            }
        };
        worker.state.hub.begin(&worker.flow, &job.id);
        spawn_run(&worker, job, guards, permit);
        // Loop immediately: an all-shared flow overlaps with itself; an
        // exclusive member re-serializes us at the next lock acquisition.
    }
    // Everything still queued dies with the daemon: the queue is in memory, so
    // a caller has to be told rather than left waiting on a restart.
    close_queue(&worker.state, &worker.flow, &mut worker.jobs);
}

/// The sink core's [`SettleOnDrop`] settles through in serve mode: the hub
/// record plus this flow's queue and run counts, which a warm run has no
/// equivalent of.
///
/// Both counts ride on the one guard rather than a second guard of their own:
/// [`SettleOnDrop`] fires exactly once per run task — the panic and
/// dropped-before-poll paths included — so the run this sink settles is counted
/// down exactly once too.
struct FlowSettle {
    state: Arc<ServeState>,
    flow: FlowName,
}

impl SettleSink for FlowSettle {
    fn settle(&self, id: &str, status: TriggerStatus) {
        self.state.settle_running(&self.flow, id, status);
    }
}

/// Everything one detached run task needs; grouped for the 5-param rule.
struct RunTask {
    flow: FlowName,
    config: PathBuf,
    baseline_hash: String,
    state: Arc<ServeState>,
    shutdown: watch::Receiver<bool>,
}

fn spawn_run(worker: &FlowWorker, job: Job, guards: MemberGuards, permit: OwnedSemaphorePermit) {
    let task = RunTask {
        flow: worker.flow.clone(),
        config: worker.config.clone(),
        baseline_hash: worker.baseline_hash.clone(),
        state: Arc::clone(&worker.state),
        shutdown: worker.shutdown.clone(),
    };
    let live = worker.live.clone();
    // Counted before the task exists, so `GET /v1/flows` never under-counts a
    // run the scheduler has already committed to.
    if let Some(ingress) = task.state.ingress(&task.flow) {
        ingress.running.fetch_add(1, Ordering::SeqCst);
    }
    // Built before the spawn and captured below, so a task dropped before its
    // first poll settles too — the guard exists as soon as the job does. It
    // gives back the run count as well as the queue slot, so every exit path
    // here — a `Run::start` failure, a panic, a runtime torn down mid-run —
    // leaves this flow's metrics where it found them.
    let settle = SettleOnDrop::new(
        Arc::new(FlowSettle {
            state: Arc::clone(&task.state),
            flow: task.flow.clone(),
        }),
        job.id.clone(),
    );
    tokio::spawn(async move {
        // Held for the whole run, released together when the task drops:
        let _live = live;
        let _guards = guards;
        let _permit = permit;
        let status = run_trigger(&task, &job).await;
        tracing::info!(trigger = job.id, flow = %task.flow.0, ?status, "trigger finished");
        settle.settle(status);
    });
}

/// Re-freezes the workflow, refusing edits made since the daemon started.
fn reload(task: &RunTask) -> Result<FrozenWorkflow, TriggerStatus> {
    let (_, frozen) = load_workflow(&task.config).map_err(|error| TriggerStatus::Failed {
        reason: format!(
            "could not reload '{}' for this trigger: {error}; fix the file and restart \
             the daemon",
            task.config.display()
        ),
        reason_code: "workflow_changed".to_string(),
    })?;
    if frozen.hash != task.baseline_hash {
        return Err(TriggerStatus::Failed {
            reason: format!(
                "'{}' changed since the daemon started (serving {}, on disk {}); serve \
                 mode freezes the workflow at startup so a queued trigger cannot be \
                 answered by a different workflow — the hash also covers flow schema \
                 files, each agent's resolved MCP servers (mcp = [...]) and every file \
                 under a declared skill dir (skills = [...]); restart the daemon to \
                 adopt edits",
                task.config.display(),
                task.baseline_hash,
                frozen.hash
            ),
            reason_code: "workflow_changed".to_string(),
        });
    }
    Ok(frozen)
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

/// Every triggered run is ephemeral and disposable, and carries the trust
/// policy `main` resolved for the daemon.
fn run_options(trust: TrustPolicy) -> RunOptions {
    RunOptions {
        ephemeral_port: true,
        repoint_current: false,
        cleanup_run_dir: true,
        trust,
    }
}

/// Hash equality means the flow set matches the one serve froze, so a missing
/// name is a `CoreTempo` bug — reported as one rather than panicking.
fn flow_vanished(flow: &FlowName) -> TriggerStatus {
    TriggerStatus::Failed {
        reason: format!(
            "internal: flow '{}' vanished from an unchanged workflow; report this",
            flow.0
        ),
        reason_code: "internal".to_string(),
    }
}

/// Cold-starts a run for one trigger, awaits its kickoff, and tears it down.
async fn run_trigger(task: &RunTask, job: &Job) -> TriggerStatus {
    let frozen = match reload(task) {
        Ok(frozen) => frozen,
        Err(status) => return status,
    };
    let Some(flow) = frozen.flows.get(&task.flow).cloned() else {
        return flow_vanished(&task.flow);
    };
    // The derived subset is the flow's own roster, so everything downstream —
    // spawning, prompts, the quiescence watcher — sees only its members.
    let Some(derived) = frozen.for_flow(&task.flow) else {
        return flow_vanished(&task.flow);
    };
    let ask_timeout = derived.ask_timeout;
    let run = match Run::start_with(
        derived,
        per_run_server(&task.state.server),
        run_options(task.state.trust),
    )
    .await
    {
        Ok(run) => run,
        Err(error) => {
            return TriggerStatus::Failed {
                reason: format!("could not start a run for this trigger: {error}"),
                reason_code: "internal".to_string(),
            };
        }
    };
    let status = kickoff(task, job, &run, (&flow, ask_timeout)).await;
    if let Err(error) = run.stop().await {
        tracing::warn!(%error, "could not stop the trigger's run cleanly");
    }
    status
}

/// Injects the payload and waits for the workflow to finish it.
async fn kickoff(
    task: &RunTask,
    job: &Job,
    run: &Run,
    (flow, ask_timeout): (&FrozenFlow, Duration),
) -> TriggerStatus {
    let origin = job.id.strip_prefix("t-").unwrap_or(&job.id).to_string();
    // Bound before creation: this kickoff repairs against its own flow's
    // contract (multi-flow spec §5), and a reply can land the moment the
    // message exists.
    if let Some(contract) = flow.output.clone() {
        run.router().bind_kickoff_contract(&origin, contract);
    }
    let record = match run
        .router()
        .create_kickoff(FlowKickoff {
            flow: task.flow.clone(),
            from: Origin::Trigger(origin.clone()),
            to: flow.edge.to.clone(),
            kind: flow.edge.kind.message_kind(),
            body: job.payload.clone(),
        })
        .await
    {
        Ok(record) => record,
        Err(error) => {
            run.router().unbind_kickoff_contract(&origin);
            return TriggerStatus::Failed {
                reason: format!("could not inject the kickoff message: {error}"),
                reason_code: "kickoff_rejected".to_string(),
            };
        }
    };
    // Built immediately after creation: the watcher's deadline runs from here.
    // The derived run declares exactly this flow, so `None` is the vanished case.
    let Some(inputs) = run.watch_inputs_for_flow(
        &task.flow,
        watcher_deadline(ask_timeout),
        Some(job.id.clone()),
    ) else {
        return flow_vanished(&task.flow);
    };
    let mut shutdown = task.shutdown.clone();
    tokio::select! {
        completion = watch_completion(inputs, record) => completion_status(completion),
        () = tripped(&mut shutdown) => interrupted(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One flow's ingress and the receiver its worker would own.
    fn ingress_state(flow: &FlowName) -> (Arc<ServeState>, mpsc::Receiver<Job>) {
        let (jobs, jobs_rx) = mpsc::channel::<Job>(QUEUE_CAP);
        let mut flows = BTreeMap::new();
        flows.insert(
            flow.clone(),
            FlowIngress {
                jobs,
                open: Mutex::new(true),
                outstanding: AtomicUsize::new(0),
                running: AtomicUsize::new(0),
            },
        );
        let state = ServeState {
            hub: TriggerHub::new(),
            flows,
            meta: BTreeMap::new(),
            server: ResolvedServer {
                bind: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                port: 0,
                db: PathBuf::new(),
                token: coretempo_core::types::id::Token::generate(),
                token_provisioned: false,
                log: "info".to_string(),
            },
            trust: TrustPolicy::default(),
        };
        (Arc::new(state), jobs_rx)
    }

    /// A trigger accepted alongside the shutdown drain must end up terminal.
    ///
    /// The two sides race for the same flow: one thread accepts while the other
    /// runs the worker's close-and-drain. Whichever order they land in, the
    /// caller has to get an answer — a refusal, or a record it can poll to a
    /// terminal status. A trigger that reserved its slot and then landed on a
    /// queue nobody reads again would stay at `queued` forever.
    #[test]
    fn an_accept_racing_the_drain_is_refused_or_settled_but_never_stranded() {
        let flow = FlowName("a".to_string());
        for round in 0..200 {
            let (state, mut jobs) = ingress_state(&flow);
            let accepted = std::thread::scope(|scope| {
                let accepting = scope.spawn(|| {
                    let ingress = state.ingress(&flow).expect("the flow has an ingress");
                    (0..4)
                        .map(|n| admit(&state, &flow, ingress, format!("payload {n}")))
                        .collect::<Vec<_>>()
                });
                // Half the rounds start both sides together; the other half let
                // the accepts get under way first, which is the interleaving a
                // shutdown actually produces.
                if round % 2 == 0 {
                    std::thread::sleep(Duration::from_micros(200));
                }
                close_queue(&state, &flow, &mut jobs);
                accepting.join().expect("the accepting thread panicked")
            });
            for outcome in accepted {
                match outcome {
                    Ok((id, _)) => {
                        let status = state
                            .hub
                            .get(&id)
                            .expect("an accepted trigger is registered");
                        assert!(
                            matches!(status, TriggerStatus::Failed { .. }),
                            "trigger {id} was accepted and left at {status:?}"
                        );
                    }
                    Err(error) => {
                        assert_eq!(error.code, "shutting_down", "{}", error.message);
                    }
                }
            }
            assert_eq!(
                state.flow_loads()[&flow].queued,
                0,
                "every settled trigger gives its queue slot back"
            );
        }
    }
}
