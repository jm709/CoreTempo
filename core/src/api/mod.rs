//! Axum /v1 REST + SSE surface (contract §5/§6). Assembled by `build_router`,
//! served by `serve`; `Run::start` (workflow-run concept) owns the wiring.

pub mod agents;
pub mod auth;
pub mod messages;
pub mod pty;
pub mod sse;
pub mod trigger;

use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{FromRef, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tokio::sync::{mpsc, watch};

use crate::bus::EventBus;
use crate::pty::{Cursor, PtyChunk, PtyError, PtyManager};
use crate::router::{Router as CoreRouter, RouterError};
use crate::time::Timestamp;
use crate::trigger::TriggerHub;
use crate::types::config::{FrozenWorkflow, WorkflowFile};
use crate::types::{
    AgentExit, AgentId, AgentState, ApiErrorBody, ApiErrorDetail, Health, RunId, Token,
    WorkflowResponse,
};

/// The PTY facts the API needs, abstracted so integration tests run without real PTYs.
/// `PtyManagerSource` is the production impl over the frozen `PtyManager` surface.
pub trait PtySource: Send + Sync + 'static {
    /// Current RAW (undebounced) state of `agent`.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the frozen roster.
    fn state(&self, agent: &AgentId) -> Result<AgentState, PtyError>;

    /// Publish an externally reported state onto the raw state channel.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the frozen roster.
    fn report_state(&self, agent: &AgentId, state: AgentState) -> Result<(), PtyError>;

    /// The agent's `PermissionRequest` hook fired: a permission dialog is up.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the frozen roster.
    fn report_blocked(
        &self,
        agent: &AgentId,
        tool: Option<String>,
        agent_id: Option<String>,
    ) -> Result<(), PtyError>;

    /// The agent's `PermissionRequest` hook answered a dialog with a deny
    /// (`on_permission_prompt = "deny"`): nothing is waiting, but the refused
    /// tool and its input summary are worth an event and a log line.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the frozen roster.
    fn report_refused(
        &self,
        agent: &AgentId,
        tool: Option<String>,
        input: Option<String>,
    ) -> Result<(), PtyError>;

    /// The agent's `PostToolBatch` hook fired: the dialog was answered. Clears
    /// only when `agent_id` matches the one the dialog was reported with.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the frozen roster.
    fn report_unblocked(&self, agent: &AgentId, agent_id: Option<String>) -> Result<(), PtyError>;

    /// How the last session ended, set only while the agent is `exited`.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the frozen roster.
    fn exit(&self, agent: &AgentId) -> Result<Option<AgentExit>, PtyError>;

    /// Current end-of-stream byte cursor.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the frozen roster.
    fn end_cursor(&self, agent: &AgentId) -> Result<Cursor, PtyError>;

    /// Live output from `since` (or the ring tail when `None`).
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the frozen roster.
    fn subscribe_output(
        &self,
        agent: &AgentId,
        since: Option<Cursor>,
    ) -> Result<mpsc::Receiver<PtyChunk>, PtyError>;

    /// Fire-and-forget restart (the endpoint answers 202 before completion).
    fn begin_restart(&self, agent: AgentId);

    /// Number of injections enqueued for `agent` and not yet delivered or failed
    /// (spec triggers §2 quiescence input).
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the frozen roster.
    fn queue_depth(&self, agent: &AgentId) -> Result<u64, PtyError>;

    /// 2 s stable idle debounced state signal for `agent`.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the frozen roster.
    fn subscribe_debounced(&self, agent: &AgentId)
    -> Result<watch::Receiver<AgentState>, PtyError>;

    /// Whether `agent` is parked on a permission dialog (spec 2026-08-17 §3).
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the frozen roster.
    fn blocked(&self, agent: &AgentId) -> Result<bool, PtyError>;

    /// How many agents are parked on a permission dialog.
    fn blocked_count(&self) -> usize;

    /// Raw keystrokes (bypasses the injection queue; shares the write pump).
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`], [`PtyError::AgentExited`] with no live session.
    fn write<'a>(&'a self, agent: &'a AgentId, bytes: Vec<u8>) -> PtyFuture<'a, ()>;

    /// Resizes the agent's pty window.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`], [`PtyError::AgentExited`], [`PtyError::Io`].
    fn resize<'a>(&'a self, agent: &'a AgentId, cols: u16, rows: u16) -> PtyFuture<'a, ()>;

    /// UI backpressure flag; unknown agents are logged and ignored.
    fn pause(&self, agent: &AgentId, paused: bool);
}

/// Production `PtySource` over `PtyManager` (constructed by `Run::start`).
pub struct PtyManagerSource(pub Arc<PtyManager>);

impl PtySource for PtyManagerSource {
    fn state(&self, agent: &AgentId) -> Result<AgentState, PtyError> {
        self.0.state(agent)
    }
    fn report_state(&self, agent: &AgentId, state: AgentState) -> Result<(), PtyError> {
        self.0.report_state(agent, state)
    }
    fn report_blocked(
        &self,
        agent: &AgentId,
        tool: Option<String>,
        agent_id: Option<String>,
    ) -> Result<(), PtyError> {
        self.0.report_blocked(agent, tool, agent_id)
    }
    fn report_refused(
        &self,
        agent: &AgentId,
        tool: Option<String>,
        input: Option<String>,
    ) -> Result<(), PtyError> {
        self.0.report_refused(agent, tool, input)
    }
    fn report_unblocked(&self, agent: &AgentId, agent_id: Option<String>) -> Result<(), PtyError> {
        self.0.report_unblocked(agent, agent_id)
    }
    fn exit(&self, agent: &AgentId) -> Result<Option<AgentExit>, PtyError> {
        self.0.exit(agent)
    }
    fn end_cursor(&self, agent: &AgentId) -> Result<Cursor, PtyError> {
        self.0.read_ring(agent, None).map(|(cursor, _)| cursor)
    }
    fn subscribe_output(
        &self,
        agent: &AgentId,
        since: Option<Cursor>,
    ) -> Result<mpsc::Receiver<PtyChunk>, PtyError> {
        self.0.subscribe_output(agent, since)
    }
    fn begin_restart(&self, agent: AgentId) {
        let pty = Arc::clone(&self.0);
        tokio::spawn(async move {
            if let Err(error) = pty.restart(&agent).await {
                tracing::error!(agent = %agent.0, %error, "restart failed");
            }
        });
    }
    fn queue_depth(&self, agent: &AgentId) -> Result<u64, PtyError> {
        self.0.queue_depth(agent)
    }
    fn subscribe_debounced(
        &self,
        agent: &AgentId,
    ) -> Result<watch::Receiver<AgentState>, PtyError> {
        self.0.subscribe_state_debounced(agent)
    }
    fn blocked(&self, agent: &AgentId) -> Result<bool, PtyError> {
        self.0.blocked(agent)
    }
    fn blocked_count(&self) -> usize {
        self.0.blocked_count()
    }
    fn write<'a>(&'a self, agent: &'a AgentId, bytes: Vec<u8>) -> PtyFuture<'a, ()> {
        Box::pin(async move { self.0.write(agent, &bytes).await })
    }
    fn resize<'a>(&'a self, agent: &'a AgentId, cols: u16, rows: u16) -> PtyFuture<'a, ()> {
        Box::pin(async move { self.0.resize(agent, cols, rows).await })
    }
    fn pause(&self, agent: &AgentId, paused: bool) {
        self.0.pause_output(agent, paused);
    }
}

/// The ids an API instance answers for. A workflow run's is its frozen
/// roster; the sessions daemon's is its live handle set (amendment 47).
pub trait Roster: Send + Sync + 'static {
    fn contains(&self, id: &AgentId) -> bool;

    /// Every id, in id order — what an unknown-id error lists.
    fn ids(&self) -> Vec<AgentId>;

    /// `POST /v1/agents/{id}/state` carried a `claude_session_id`. The
    /// sessions daemon stores it for `--resume`; runs keep this no-op. The
    /// handler awaits it *before* publishing the state, so once a caller
    /// observes `idle` the id is durable (spec §2: latest `SessionStart` wins).
    fn on_claude_session_id<'a>(
        &'a self,
        _id: &'a AgentId,
        _session_id: String,
    ) -> RosterFuture<'a> {
        Box::pin(async {})
    }
}

/// A boxed future so [`Roster`] stays object-safe with an async hook.
pub type RosterFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

impl Roster for FrozenWorkflow {
    fn contains(&self, id: &AgentId) -> bool {
        self.agents.contains_key(id)
    }
    fn ids(&self) -> Vec<AgentId> {
        self.agents.keys().cloned().collect()
    }
}

/// Who a bearer token says the caller is (spec 2026-08-27 §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// The operator token: everything.
    Operator,
    /// An agent's own hook token: exactly `POST /v1/agents/{id}/state`.
    Hook(AgentId),
    Unknown,
}

/// Classifies bearer tokens. Runs implement it as the single operator token;
/// the sessions daemon compares the operator token, then every live hook
/// token, in constant time.
pub trait TokenAuth: Send + Sync + 'static {
    fn classify(&self, bearer: &str) -> Caller;

    /// Which token a 401 points the caller at.
    fn hint(&self) -> auth::TokenHint;
}

/// The one-token model of a workflow run.
pub struct OperatorToken(pub Token);

impl TokenAuth for OperatorToken {
    fn classify(&self, bearer: &str) -> Caller {
        if auth::token_matches(&self.0, bearer) {
            Caller::Operator
        } else {
            Caller::Unknown
        }
    }
    fn hint(&self) -> auth::TokenHint {
        auth::TokenHint::Run
    }
}

/// A boxed PTY future, so [`PtySource`] stays object-safe with async writes.
pub type PtyFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, PtyError>> + Send + 'a>>;

/// What every `/v1` instance needs — a workflow run and the sessions daemon
/// alike. `Clone` is cheap (Arcs + small values). Amendment 47.
#[derive(Clone)]
pub struct ApiCore {
    pub pty: Arc<dyn PtySource>,
    pub bus: EventBus,
    pub roster: Arc<dyn Roster>,
    pub auth: Arc<dyn TokenAuth>,
    pub token_provisioned: bool,
    pub bind: IpAddr,
    pub port: u16,
    pub started_at: Timestamp,
    pub started: Instant,
}

/// The core plus the run-only extension (router, frozen workflow, triggers).
#[derive(Clone)]
pub struct ApiContext {
    pub core: ApiCore,
    pub router: Arc<CoreRouter>,
    pub workflow: Arc<FrozenWorkflow>,
    pub workflow_file: Arc<WorkflowFile>,
    pub run_id: RunId,
    /// Trigger bookkeeping for `/v1/trigger` (spec triggers §4). Warm runs of a
    /// webhook workflow fire against the live roster.
    pub triggers: Arc<TriggerHub>,
    /// Per-agent readers/writers locks for warm triggers (multi-flow spec §5):
    /// a warm trigger holds its flow's member locks for its duration, so two
    /// flows sharing an exclusive agent serialize rather than interleaving
    /// conversations in its one live session. This run's own table — a
    /// separate instance from the serve scheduler's; each guards its roster.
    pub agent_locks: Arc<crate::locks::AgentLocks>,
    /// Trips when the owning run starts stopping (`Run::stop`), and stays
    /// tripped. A warm trigger parked on its flow's member locks races this:
    /// past the stop the PTY manager is dead, so a kickoff created then is
    /// typed into nothing. A dropped sender counts as stopped — the run it
    /// belonged to is gone.
    pub stopping: watch::Receiver<bool>,
}

impl FromRef<ApiContext> for ApiCore {
    fn from_ref(ctx: &ApiContext) -> ApiCore {
        ctx.core.clone()
    }
}

/// Uniform error response: `{"error":{"code","message"}}`, message written for LLM readers.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    #[must_use]
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            code,
            message: message.into(),
        }
    }

    /// 400 `invalid_request`.
    #[must_use]
    pub fn invalid(message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    /// 500 `internal`, with the underlying error rendered into the message.
    #[must_use]
    pub fn internal(error: impl std::fmt::Display) -> ApiError {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("internal error: {error}; retry, and report if it persists"),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ApiErrorBody {
            error: ApiErrorDetail {
                code: self.code.to_string(),
                message: self.message,
            },
        };
        (self.status, Json(body)).into_response()
    }
}

pub(crate) fn roster_list(roster: &dyn Roster) -> String {
    roster
        .ids()
        .iter()
        .map(|a| a.0.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn unknown_agent(roster: &dyn Roster, id: &AgentId) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "unknown_agent",
        format!("no agent named '{}'; roster: {}", id.0, roster_list(roster)),
    )
}

pub(crate) fn map_router_error(error: RouterError, workflow: &FrozenWorkflow) -> ApiError {
    match error {
        RouterError::UnknownAgent(id) => unknown_agent(workflow, &id),
        RouterError::UnknownMessage(id) => ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown_message",
            format!(
                "no message with id '{}'; use the id printed by tempo ask/send \
                 (shape: m-a3f91c2e)",
                id.0
            ),
        ),
        RouterError::AlreadyReplied(id) => ApiError::new(
            StatusCode::CONFLICT,
            "already_replied",
            format!(
                "message '{}' already has a different reply; a message is replied to \
                 once — create a new ask if you have more to say",
                id.0
            ),
        ),
        RouterError::NotAnAsk(id) => ApiError::new(
            StatusCode::CONFLICT,
            "not_an_ask",
            format!(
                "message '{}' is a send, not an ask; sends never take replies — \
                 nothing to do",
                id.0
            ),
        ),
        RouterError::WrongReplier(id) => ApiError::new(
            StatusCode::FORBIDDEN,
            "wrong_replier",
            format!(
                "only the agent message '{}' was addressed to may reply; check the id \
                 against the [CoreTempo …] line in your prompt",
                id.0
            ),
        ),
        RouterError::InvalidCode(code) => ApiError::invalid(format!(
            "reply code {code} is invalid; code must be 0 (success) or 1 (failure)"
        )),
        RouterError::OutputSchema { rendered } => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "schema_validation_failed",
            rendered,
        ),
        RouterError::NoLoopEdge {
            owner,
            target,
            edges,
        } => ApiError::new(
            StatusCode::CONFLICT,
            "no_loop_edge",
            format!(
                "agent '{}' has no loop edge to '{}' (its edges: {edges}); \
                 `tempo done` only ends a loop edge from your workflow config",
                owner.0, target.0
            ),
        ),
        RouterError::Store(error) => ApiError::internal(error),
    }
}

/// A warm run has no queue, so every declared flow reports depth 0; `running`
/// counts its in-flight triggers (multi-flow spec §5).
async fn health(State(ctx): State<ApiContext>) -> Json<Health> {
    let queued = ctx
        .workflow
        .flows
        .keys()
        .map(|name| (name.clone(), 0))
        .collect();
    Json(Health {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        run_id: ctx.run_id.clone(),
        uptime_secs: ctx.core.started.elapsed().as_secs(),
        queued,
        running: ctx.triggers.in_flight_by_flow().len(),
        blocked: ctx.core.pty.blocked_count(),
    })
}

async fn not_found(req: axum::extract::Request) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "invalid_request",
        format!(
            "no route {} {}; valid routes: POST/GET /v1/messages, \
             GET /v1/messages/{{id}}, POST /v1/messages/{{id}}/reply, GET /v1/agents, \
             GET /v1/agents/{{id}}, POST /v1/agents/{{id}}/state, \
             POST /v1/agents/{{id}}/restart, GET /v1/agents/{{id}}/pty, \
             POST /v1/agents/{{id}}/pty, POST /v1/agents/{{id}}/pty/resize, \
             POST /v1/agents/{{id}}/pty/pause, GET /v1/events, \
             GET /v1/workflow, GET /v1/health, GET /v1/flows, \
             POST /v1/flows/{{name}}/trigger, GET /v1/trigger/{{id}}",
            req.method(),
            req.uri().path()
        ),
    )
}

async fn get_workflow(State(ctx): State<ApiContext>) -> Json<WorkflowResponse> {
    Json(WorkflowResponse {
        run_id: ctx.run_id.clone(),
        started_at: ctx.core.started_at.clone(),
        workflow: (*ctx.workflow_file).clone(),
    })
}

/// Routes both a run and the sessions daemon mount (amendment 47): the hook
/// target and the control-plane stream.
pub fn shared_routes<S>() -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
    ApiCore: FromRef<S>,
{
    axum::Router::new()
        .route("/v1/agents/{id}/state", post(agents::report_state))
        .route("/v1/events", get(sse::events))
}

/// The PTY routes under `prefix` (`/v1/agents` for runs, `/v1/sessions` for
/// the daemon): stream, raw write, resize, pause.
pub fn pty_routes<S>(prefix: &str) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
    ApiCore: FromRef<S>,
{
    axum::Router::new()
        .route(
            &format!("{prefix}/{{id}}/pty"),
            get(pty::pty_stream).post(pty::pty_write),
        )
        .route(
            &format!("{prefix}/{{id}}/pty/resize"),
            post(pty::pty_resize),
        )
        .route(&format!("{prefix}/{{id}}/pty/pause"), post(pty::pty_pause))
}

/// Assembles the full run /v1 router. Public so tests and `Run` can serve it themselves.
pub fn build_router(ctx: ApiContext) -> axum::Router {
    axum::Router::new()
        .merge(shared_routes())
        .merge(pty_routes("/v1/agents"))
        .route(
            "/v1/messages",
            post(messages::create_message).get(messages::list_messages),
        )
        .route("/v1/messages/{id}", get(messages::get_message))
        .route("/v1/messages/{id}/reply", post(messages::reply_message))
        .route("/v1/agents", get(agents::list_agents))
        .route("/v1/agents/{id}", get(agents::get_agent))
        .route("/v1/agents/{id}/restart", post(agents::restart_agent))
        .route("/v1/agents/{id}/loop-done", post(agents::loop_done))
        .route("/v1/trigger", post(trigger::removed_trigger_route))
        .route("/v1/trigger/{id}", get(trigger::get_trigger))
        .route("/v1/flows", get(trigger::list_flows))
        .route("/v1/flows/{name}/trigger", post(trigger::post_flow_trigger))
        .route("/v1/workflow", get(get_workflow))
        .route("/v1/health", get(health))
        .fallback(not_found)
        .layer(axum::middleware::from_fn_with_state(
            ctx.core.clone(),
            auth::guard,
        ))
        .with_state(ctx)
}

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error(
        "refusing to bind {0}: non-loopback bind requires a provisioned token \
         (set CORETEMPO_TOKEN, CORETEMPO_TOKEN_FILE, or [server].token_file)"
    )]
    NonLoopbackWithoutToken(IpAddr),
    #[error("failed to bind {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
}

/// How long [`ApiServerHandle::shutdown`] lets in-flight responses finish
/// before it aborts the accept loop.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// Running server handle; dropping it aborts the accept loop task.
pub struct ApiServerHandle {
    local_addr: SocketAddr,
    shutdown: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl ApiServerHandle {
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Graceful shutdown with a bounded grace period: stop accepting, give
    /// in-flight responses [`SHUTDOWN_GRACE`] to finish, then abort the accept
    /// task.
    ///
    /// SSE streams never end on their own — their pumps live as long as the
    /// event bus does — so an unbounded graceful wait hangs forever whenever a
    /// UI is attached. The abort is the fix, not a safety net.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        stop_accept_loop(self.task).await;
    }
}

/// Waits [`SHUTDOWN_GRACE`] for an accept-loop task whose graceful shutdown has
/// already been signalled, then aborts it.
///
/// Public because serve mode runs its own listener with the same bound: a
/// shutdown that can hang is a daemon that cannot be stopped.
pub async fn stop_accept_loop(task: tokio::task::JoinHandle<()>) {
    let mut task = task;
    tokio::select! {
        _ = &mut task => {}
        () = tokio::time::sleep(SHUTDOWN_GRACE) => {
            tracing::info!("api shutdown grace elapsed; aborting the accept loop");
            task.abort();
            let _ = task.await;
        }
    }
}

/// A generated token off loopback would publish an unauthenticated API; every
/// bind path checks this before opening a socket.
///
/// # Errors
/// [`ServeError::NonLoopbackWithoutToken`] for a non-loopback bind with a generated token.
pub(crate) fn check_bind(bind: IpAddr, token_provisioned: bool) -> Result<(), ServeError> {
    if !bind.is_loopback() && !token_provisioned {
        return Err(ServeError::NonLoopbackWithoutToken(bind));
    }
    Ok(())
}

/// Binds `ctx.core.bind:ctx.core.port` and serves the /v1 app. Refuses non-loopback binds
/// unless the token was explicitly provisioned (defense in depth on top of `resolve_server`).
///
/// # Errors
/// [`ServeError::NonLoopbackWithoutToken`] when a non-loopback bind is requested with a
/// generated token; [`ServeError::Bind`] when the listener cannot be bound.
pub async fn serve(ctx: ApiContext) -> Result<ApiServerHandle, ServeError> {
    check_bind(ctx.core.bind, ctx.core.token_provisioned)?;
    let addr = SocketAddr::new(ctx.core.bind, ctx.core.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|source| ServeError::Bind { addr, source })?;
    serve_on(listener, ctx)
}

/// Serves the /v1 app on an already-bound listener, which lets a caller learn the
/// real port (ephemeral binds) before it builds the context and the agent
/// environment. Must be called inside a tokio runtime.
///
/// # Errors
/// As [`serve_app`].
pub fn serve_on(
    listener: tokio::net::TcpListener,
    ctx: ApiContext,
) -> Result<ApiServerHandle, ServeError> {
    let (bind, provisioned) = (ctx.core.bind, ctx.core.token_provisioned);
    serve_app(listener, build_router(ctx), bind, provisioned)
}

/// Serves an assembled app on an already-bound listener. [`serve_on`] is this
/// over [`build_router`]; the sessions daemon calls it with its own router.
///
/// # Errors
/// [`ServeError::NonLoopbackWithoutToken`] when a non-loopback bind is requested with a
/// generated token; [`ServeError::Bind`] when the listener has no local address.
pub fn serve_app(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    bind: IpAddr,
    token_provisioned: bool,
) -> Result<ApiServerHandle, ServeError> {
    check_bind(bind, token_provisioned)?;
    let local_addr = listener.local_addr().map_err(|source| ServeError::Bind {
        addr: SocketAddr::new(bind, 0),
        source,
    })?;
    let (shutdown, mut rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        let wait = async move {
            let _ = rx.changed().await;
        };
        if let Err(error) = axum::serve(listener, app)
            .with_graceful_shutdown(wait)
            .await
        {
            tracing::error!(%error, "api server exited with error");
        }
    });
    tracing::info!(%local_addr, "api listening");
    Ok(ApiServerHandle {
        local_addr,
        shutdown,
        task,
    })
}
