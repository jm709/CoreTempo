//! The sessions daemon's `/v1` (spec 2026-08-27 §6, amendment 47): projects,
//! sessions, and — through [`shared_routes`] and [`pty_routes`] — the hook
//! target, the event stream and the PTY routes a run mounts too.

use std::sync::Arc;

use axum::Json;
use axum::extract::{FromRef, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde::Deserialize;

use crate::api::{ApiCore, ApiError, auth, pty_routes, shared_routes};
use crate::sessions::{SessionError, SessionManager};
use crate::types::id::{AgentId, ProjectId};
use crate::types::session::{CreateProjectRequest, CreateSessionRequest, SessionsHealth};

#[derive(Clone)]
pub struct SessionsApi {
    pub core: ApiCore,
    pub sessions: Arc<SessionManager>,
}

impl FromRef<SessionsApi> for ApiCore {
    fn from_ref(api: &SessionsApi) -> ApiCore {
        api.core.clone()
    }
}

/// `SessionError` → HTTP, with the manager's message verbatim (it names the fix).
///
/// By reference because nothing here consumes the error — `map_router_error`
/// takes ownership only because its arms move ids out of theirs.
pub(crate) fn map_session_error(error: &SessionError) -> ApiError {
    let message = error.to_string();
    let (status, code) = match error {
        SessionError::UnknownSession { .. } => (StatusCode::NOT_FOUND, "unknown_session"),
        SessionError::UnknownProject { .. } => (StatusCode::NOT_FOUND, "unknown_project"),
        SessionError::ProjectExists { .. } => (StatusCode::CONFLICT, "project_exists"),
        SessionError::ProjectInUse { .. } => (StatusCode::CONFLICT, "project_in_use"),
        SessionError::NotAGitRepo { .. } => (StatusCode::UNPROCESSABLE_ENTITY, "not_a_git_repo"),
        SessionError::CwdOutsideProject { .. } => {
            (StatusCode::UNPROCESSABLE_ENTITY, "cwd_outside_project")
        }
        SessionError::CwdMissing { .. } => (StatusCode::UNPROCESSABLE_ENTITY, "cwd_missing"),
        SessionError::WrongState { .. } => (StatusCode::CONFLICT, "wrong_state"),
        SessionError::WorktreeMissing { .. } => (StatusCode::CONFLICT, "worktree_missing"),
        SessionError::Dirty { .. } => (StatusCode::UNPROCESSABLE_ENTITY, "dirty_worktree"),
        SessionError::ShuttingDown => (StatusCode::SERVICE_UNAVAILABLE, "shutting_down"),
        SessionError::Trust(_) => (StatusCode::CONFLICT, "untrusted"),
        SessionError::Git(_) => (StatusCode::UNPROCESSABLE_ENTITY, "git_failed"),
        SessionError::Spawn(_) => (StatusCode::INTERNAL_SERVER_ERROR, "spawn_failed"),
        SessionError::Store(_) | SessionError::Files(_) | SessionError::Io { .. } => {
            (StatusCode::INTERNAL_SERVER_ERROR, "internal")
        }
    };
    ApiError::new(status, code, message)
}

/// A malformed body names the shape this route wants (the messages module's
/// `parse_body` would show an ask/send example — wrong roster, wrong fix).
fn parse_as<T: serde::de::DeserializeOwned>(body: &[u8], example: &str) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|error| {
        ApiError::invalid(format!("malformed body: {error}; expected e.g. {example}"))
    })
}

async fn health(State(api): State<SessionsApi>) -> Result<Json<SessionsHealth>, ApiError> {
    let sessions = api
        .sessions
        .counts()
        .await
        .map_err(|e| map_session_error(&e))?;
    Ok(Json(SessionsHealth { ok: true, sessions }))
}

async fn list_projects(State(api): State<SessionsApi>) -> Result<Response, ApiError> {
    let projects = api
        .sessions
        .list_projects()
        .await
        .map_err(|e| map_session_error(&e))?;
    Ok(Json(projects).into_response())
}

async fn create_project(
    State(api): State<SessionsApi>,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let req: CreateProjectRequest = parse_as(
        &body,
        r#"{"path":"/abs/path/to/repo","name":"optional display name"}"#,
    )?;
    let project = api
        .sessions
        .register_project(std::path::Path::new(&req.path), req.name)
        .await
        .map_err(|e| map_session_error(&e))?;
    Ok((StatusCode::CREATED, Json(project)).into_response())
}

async fn forget_project(
    State(api): State<SessionsApi>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    api.sessions
        .forget_project(&ProjectId(id))
        .await
        .map_err(|e| map_session_error(&e))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_sessions(State(api): State<SessionsApi>) -> Result<Response, ApiError> {
    let sessions = api
        .sessions
        .list()
        .await
        .map_err(|e| map_session_error(&e))?;
    Ok(Json(sessions).into_response())
}

async fn create_session(
    State(api): State<SessionsApi>,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let req: CreateSessionRequest = parse_as(
        &body,
        r#"{"project":"p-0a1b2c3d","worktree":true,"cwd":"pkg","title":null,"prompt":null,"model":null,"permission_mode":null,"isolated_config":false}"#,
    )?;
    let view = api
        .sessions
        .create(req)
        .await
        .map_err(|e| map_session_error(&e))?;
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

async fn get_session(
    State(api): State<SessionsApi>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let view = api
        .sessions
        .get(&AgentId(id))
        .await
        .map_err(|e| map_session_error(&e))?;
    Ok(Json(view).into_response())
}

async fn stop_session(
    State(api): State<SessionsApi>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let view = api
        .sessions
        .stop(&AgentId(id))
        .await
        .map_err(|e| map_session_error(&e))?;
    Ok(Json(view).into_response())
}

async fn resume_session(
    State(api): State<SessionsApi>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let resumed = api
        .sessions
        .resume(&AgentId(id))
        .await
        .map_err(|e| map_session_error(&e))?;
    Ok(Json(resumed).into_response())
}

#[derive(Debug, Default, Deserialize)]
struct DeleteQuery {
    #[serde(default)]
    remove_worktree: bool,
    #[serde(default)]
    force: bool,
}

async fn delete_session(
    State(api): State<SessionsApi>,
    Path(id): Path<String>,
    Query(query): Query<DeleteQuery>,
) -> Result<Response, ApiError> {
    let deleted = api
        .sessions
        .delete(&AgentId(id), query.remove_worktree, query.force)
        .await
        .map_err(|e| map_session_error(&e))?;
    Ok(Json(deleted).into_response())
}

async fn not_found(req: axum::extract::Request) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "invalid_request",
        format!(
            "no route {} {}; the sessions daemon answers GET /v1/health, GET|POST /v1/projects, \
             DELETE /v1/projects/{{id}}, GET|POST /v1/sessions, GET /v1/sessions/{{id}}, \
             POST /v1/sessions/{{id}}/stop|resume, DELETE /v1/sessions/{{id}}, \
             GET|POST /v1/sessions/{{id}}/pty, POST /v1/sessions/{{id}}/pty/resize|pause, \
             POST /v1/agents/{{id}}/state, GET /v1/events",
            req.method(),
            req.uri().path()
        ),
    )
}

/// The daemon's whole `/v1`.
pub fn build_sessions_router(api: SessionsApi) -> axum::Router {
    axum::Router::new()
        .merge(shared_routes())
        .merge(pty_routes("/v1/sessions"))
        .route("/v1/health", get(health))
        .route("/v1/projects", get(list_projects).post(create_project))
        .route("/v1/projects/{id}", delete(forget_project))
        .route("/v1/sessions", get(list_sessions).post(create_session))
        .route("/v1/sessions/{id}", get(get_session).delete(delete_session))
        .route("/v1/sessions/{id}/stop", post(stop_session))
        .route("/v1/sessions/{id}/resume", post(resume_session))
        .fallback(not_found)
        .layer(axum::middleware::from_fn_with_state(
            api.core.clone(),
            auth::guard,
        ))
        .with_state(api)
}
