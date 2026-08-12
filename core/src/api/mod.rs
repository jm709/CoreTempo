//! Axum /v1 REST + SSE surface (contract §5/§6). Assembled by `build_router`,
//! served by `serve`; `Run::start` (workflow-run concept) owns the wiring.

pub mod agents;
pub mod auth;
pub mod messages;
pub mod sse;
pub mod trigger;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
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
    AgentId, AgentState, ApiErrorBody, ApiErrorDetail, Health, RunId, Token, WorkflowResponse,
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

    /// Exit code of the last session, set only while the agent is `exited`.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the frozen roster.
    fn exit_code(&self, agent: &AgentId) -> Result<Option<i32>, PtyError>;

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
    fn exit_code(&self, agent: &AgentId) -> Result<Option<i32>, PtyError> {
        self.0.exit_code(agent)
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
}

/// Everything the handlers need; `Clone` is cheap (Arcs + small values).
#[derive(Clone)]
pub struct ApiContext {
    pub router: Arc<CoreRouter>,
    pub pty: Arc<dyn PtySource>,
    pub bus: EventBus,
    pub workflow: Arc<FrozenWorkflow>,
    pub workflow_file: Arc<WorkflowFile>,
    pub run_id: RunId,
    pub started_at: Timestamp,
    pub started: Instant,
    pub token: Token,
    pub token_provisioned: bool,
    pub bind: IpAddr,
    pub port: u16,
    /// Trigger bookkeeping for `/v1/trigger` (spec triggers §4). Warm runs of a
    /// webhook workflow fire against the live roster.
    pub triggers: Arc<TriggerHub>,
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

pub(crate) fn roster_list(workflow: &FrozenWorkflow) -> String {
    workflow
        .agents
        .keys()
        .map(|a| a.0.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn unknown_agent(workflow: &FrozenWorkflow, id: &AgentId) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "unknown_agent",
        format!(
            "no agent named '{}'; roster: {}",
            id.0,
            roster_list(workflow)
        ),
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

async fn health(State(ctx): State<ApiContext>) -> Json<Health> {
    Json(Health {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        run_id: ctx.run_id.clone(),
        uptime_secs: ctx.started.elapsed().as_secs(),
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
             POST /v1/agents/{{id}}/restart, GET /v1/agents/{{id}}/pty, GET /v1/events, \
             GET /v1/workflow, GET /v1/health, POST /v1/trigger, GET /v1/trigger/{{id}}",
            req.method(),
            req.uri().path()
        ),
    )
}

async fn get_workflow(State(ctx): State<ApiContext>) -> Json<WorkflowResponse> {
    Json(WorkflowResponse {
        run_id: ctx.run_id.clone(),
        started_at: ctx.started_at.clone(),
        workflow: (*ctx.workflow_file).clone(),
    })
}

/// Assembles the full /v1 router. Public so tests and `Run` can serve it themselves.
pub fn build_router(ctx: ApiContext) -> axum::Router {
    axum::Router::new()
        .route(
            "/v1/messages",
            post(messages::create_message).get(messages::list_messages),
        )
        .route("/v1/messages/{id}", get(messages::get_message))
        .route("/v1/messages/{id}/reply", post(messages::reply_message))
        .route("/v1/agents", get(agents::list_agents))
        .route("/v1/agents/{id}", get(agents::get_agent))
        .route("/v1/agents/{id}/state", post(agents::report_state))
        .route("/v1/agents/{id}/restart", post(agents::restart_agent))
        .route("/v1/agents/{id}/loop-done", post(agents::loop_done))
        .route("/v1/agents/{id}/pty", get(sse::agent_pty))
        .route("/v1/events", get(sse::events))
        .route("/v1/trigger", post(trigger::post_trigger))
        .route("/v1/trigger/{id}", get(trigger::get_trigger))
        .route("/v1/workflow", get(get_workflow))
        .route("/v1/health", get(health))
        .fallback(not_found)
        .layer(axum::middleware::from_fn_with_state(
            ctx.clone(),
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

/// Binds `ctx.bind:ctx.port` and serves the /v1 app. Refuses non-loopback binds unless the
/// token was explicitly provisioned (defense in depth on top of `resolve_server`).
///
/// # Errors
/// [`ServeError::NonLoopbackWithoutToken`] when a non-loopback bind is requested with a
/// generated token; [`ServeError::Bind`] when the listener cannot be bound.
pub async fn serve(ctx: ApiContext) -> Result<ApiServerHandle, ServeError> {
    check_bind(ctx.bind, ctx.token_provisioned)?;
    let addr = SocketAddr::new(ctx.bind, ctx.port);
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
/// [`ServeError::NonLoopbackWithoutToken`] when a non-loopback bind is requested with a
/// generated token; [`ServeError::Bind`] when the listener has no local address.
pub fn serve_on(
    listener: tokio::net::TcpListener,
    ctx: ApiContext,
) -> Result<ApiServerHandle, ServeError> {
    check_bind(ctx.bind, ctx.token_provisioned)?;
    let local_addr = listener.local_addr().map_err(|source| ServeError::Bind {
        addr: SocketAddr::new(ctx.bind, ctx.port),
        source,
    })?;
    let app = build_router(ctx);
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
