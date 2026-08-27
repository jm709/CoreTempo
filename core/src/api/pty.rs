//! PTY routes shared by a workflow run (`/v1/agents/{id}/pty…`) and the
//! sessions daemon (`/v1/sessions/{id}/pty…`), amendment 47: the SSE stream
//! (contracts §6.2, amendment 40) and the three commands that were Tauri-only
//! — raw write, resize, backpressure pause.

use std::collections::HashMap;

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::api::sse::{pump_pty, resume_point, sse_response};
use crate::api::{ApiCore, ApiError, unknown_agent};
use crate::pty::{Cursor, PtyError};
use crate::types::AgentId;

#[derive(Debug, Deserialize)]
pub(crate) struct ResizeRequest {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PauseRequest {
    pub paused: bool,
}

fn known(core: &ApiCore, id: &AgentId) -> Result<(), ApiError> {
    if core.roster.contains(id) {
        return Ok(());
    }
    Err(unknown_agent(core.roster.as_ref(), id))
}

/// A malformed body is axum's rejection rendered into the uniform error shape,
/// with the schema the route wanted — `shape` is that literal.
fn malformed(rejection: &JsonRejection, shape: &str) -> ApiError {
    ApiError::invalid(format!("malformed body: {rejection}; expected {shape}"))
}

/// `PtyError` → HTTP: an unknown id 404s with the roster, a dead session 409s,
/// anything else is internal.
pub(crate) fn map_pty_error(core: &ApiCore, error: PtyError) -> ApiError {
    match error {
        PtyError::UnknownAgent(id) => unknown_agent(core.roster.as_ref(), &id),
        PtyError::AgentExited(id) => ApiError::new(
            StatusCode::CONFLICT,
            "agent_exited",
            format!(
                "agent '{}' has no live session to write to; restart it (runs) or resume \
                 it (sessions) first",
                id.0
            ),
        ),
        other @ (PtyError::Spawn { .. } | PtyError::Io { .. } | PtyError::AgentExists(_)) => {
            ApiError::internal(other)
        }
    }
}

/// `GET {prefix}/{id}/pty`: base64 raw-chunk SSE with ring replay by byte
/// cursor. Clients detect aged-out data by `first seq > since`; no reset event
/// here. `Last-Event-ID` and `?since=` take the same value — the cursor to
/// resume at — so either resumes byte-exactly (contract amendment 40).
pub(crate) async fn pty_stream(
    State(core): State<ApiCore>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = AgentId(id);
    known(&core, &id)?;
    let since = resume_point(&headers, &params)?.map(Cursor);
    let chunks = core
        .pty
        .subscribe_output(&id, since)
        .map_err(|e| map_pty_error(&core, e))?;
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(pump_pty(chunks, tx));
    Ok(sse_response(rx))
}

/// `POST {prefix}/{id}/pty`: the body is typed into the PTY verbatim.
pub(crate) async fn pty_write(
    State(core): State<ApiCore>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let id = AgentId(id);
    known(&core, &id)?;
    core.pty
        .write(&id, body.to_vec())
        .await
        .map_err(|e| map_pty_error(&core, e))?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST {prefix}/{id}/pty/resize`: the terminal the viewer is showing changed
/// size.
pub(crate) async fn pty_resize(
    State(core): State<ApiCore>,
    Path(id): Path<String>,
    body: Result<Json<ResizeRequest>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Json(req) = body.map_err(|e| malformed(&e, "{\"cols\":<u16>,\"rows\":<u16>}"))?;
    let id = AgentId(id);
    known(&core, &id)?;
    core.pty
        .resize(&id, req.cols, req.rows)
        .await
        .map_err(|e| map_pty_error(&core, e))?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST {prefix}/{id}/pty/pause`: UI backpressure — stop or resume reading
/// this PTY.
pub(crate) async fn pty_pause(
    State(core): State<ApiCore>,
    Path(id): Path<String>,
    body: Result<Json<PauseRequest>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Json(req) = body.map_err(|e| malformed(&e, "{\"paused\":<bool>}"))?;
    let id = AgentId(id);
    known(&core, &id)?;
    core.pty.pause(&id, req.paused);
    Ok(StatusCode::NO_CONTENT)
}
