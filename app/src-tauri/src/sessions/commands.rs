#![expect(
    clippy::missing_errors_doc,
    reason = "these are IPC entry points, not a Rust API: every one fails as the daemon's own \
              envelope, whose per-route codes and messages are documented on the \
              crate::sessions::client method it calls, plus daemon_unreachable when the \
              supervisor has no connection"
)]
#![expect(
    clippy::unused_async,
    reason = "tauri runs a non-async command blocking on the main thread \
              (ExecutionContext::Blocking); the keyword keeps the UI responsive (spec §8)"
)]

//! Sessions mode's invoke surface (spec §7). Every command is a passthrough:
//! the shell holds no session state, so there is nothing here to get out of
//! step with the daemon. What little state there is — the connection and the
//! PTY cursors — lives in [`SessionsState`].
//!
//! Public only so `main.rs` can register these with `tauri::generate_handler!`.

use coretempo_core::types::session::{
    CreateProjectRequest, CreateSessionRequest, DeleteSessionResponse, ProjectView, ResumeResponse,
    SessionView, SessionsHealth,
};
use tauri::Manager;
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::commands::CmdError;
use crate::sessions::discovery::Discovery;
use crate::sessions::supervisor::{Conn, SessionsState, connected, ensure_supervisor};

/// What the Sessions header renders: the connection, and the daemon's own
/// counts once there is one to ask.
#[derive(serde::Serialize)]
pub struct SessionsStatusView {
    /// `idle` | `starting` | `connected` | `unreachable`.
    pub state: String,
    pub health: Option<SessionsHealth>,
}

/// A poisoned lock means the supervisor died mid-write; nothing works after
/// that, which is what `unreachable` says.
fn conn(state: &SessionsState) -> Conn {
    let Ok(guard) = state.conn.lock() else {
        tracing::error!("the sessions connection lock is poisoned");
        return Conn::Unreachable;
    };
    guard.clone()
}

/// Opening Sessions mode calls this, and this is what starts the daemon hunt —
/// the supervisor is spawned on the first call and kept by every later one.
#[tauri::command(rename_all = "snake_case")]
pub async fn sessions_status<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<SessionsStatusView, CmdError> {
    ensure_supervisor(app.clone(), Discovery::production()?);
    // Cloned out of the lock before any await: a guard held across one would
    // block every other command on a slow daemon.
    let conn = conn(&app.state::<SessionsState>());
    let health = match &conn {
        // A connection that cannot answer health is one the supervisor is about
        // to lose; report the counts as unknown rather than failing the status
        // the operator is looking at to find that out.
        Conn::Connected(client) => match client.health().await {
            Ok(health) => Some(health),
            Err(err) => {
                tracing::warn!(%err, "the sessions daemon did not answer health");
                None
            }
        },
        Conn::Idle | Conn::Starting | Conn::Unreachable => None,
    };
    Ok(SessionsStatusView {
        state: conn.name().to_string(),
        health,
    })
}

#[tauri::command(rename_all = "snake_case")]
pub async fn session_list(
    state: tauri::State<'_, SessionsState>,
) -> Result<Vec<SessionView>, CmdError> {
    connected(&state)?.list_sessions().await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn session_create(
    state: tauri::State<'_, SessionsState>,
    req: CreateSessionRequest,
) -> Result<SessionView, CmdError> {
    connected(&state)?.create_session(&req).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn session_stop(
    state: tauri::State<'_, SessionsState>,
    session: String,
) -> Result<SessionView, CmdError> {
    connected(&state)?.stop_session(&session).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn session_resume(
    state: tauri::State<'_, SessionsState>,
    session: String,
) -> Result<ResumeResponse, CmdError> {
    connected(&state)?.resume_session(&session).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn session_delete(
    state: tauri::State<'_, SessionsState>,
    session: String,
    remove_worktree: bool,
    force: bool,
) -> Result<DeleteSessionResponse, CmdError> {
    connected(&state)?
        .delete_session(&session, remove_worktree, force)
        .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn project_list(
    state: tauri::State<'_, SessionsState>,
) -> Result<Vec<ProjectView>, CmdError> {
    connected(&state)?.list_projects().await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn project_register(
    state: tauri::State<'_, SessionsState>,
    path: String,
    name: Option<String>,
) -> Result<ProjectView, CmdError> {
    let req = CreateProjectRequest { path, name };
    connected(&state)?.register_project(&req).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn project_forget(
    state: tauri::State<'_, SessionsState>,
    project: String,
) -> Result<(), CmdError> {
    connected(&state)?.forget_project(&project).await
}

/// Raw bytes on the Channel, never the event system — same rule as run mode
/// (contracts §8.2).
#[tauri::command(rename_all = "snake_case")]
pub async fn session_subscribe_pty<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    session: String,
    resume: bool,
    channel: Channel<InvokeResponseBody>,
) -> Result<(), CmdError> {
    crate::sessions::pty::subscribe(&app, session, resume, channel)
}

/// A no-op when nothing is subscribed: the webview tears terminals down without
/// tracking what is still attached. Takes the app rather than the state because
/// an async command borrowing state would have to return a `Result`, and this
/// one has nothing to fail at.
#[tauri::command(rename_all = "snake_case")]
pub async fn session_unsubscribe_pty<R: tauri::Runtime>(app: tauri::AppHandle<R>, session: String) {
    crate::sessions::pty::unsubscribe(&app.state::<SessionsState>(), &session);
}

#[tauri::command(rename_all = "snake_case")]
pub async fn session_write_pty(
    state: tauri::State<'_, SessionsState>,
    session: String,
    data: Vec<u8>,
) -> Result<(), CmdError> {
    connected(&state)?.write_pty(&session, data).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn session_resize_pty(
    state: tauri::State<'_, SessionsState>,
    session: String,
    cols: u16,
    rows: u16,
) -> Result<(), CmdError> {
    connected(&state)?.resize_pty(&session, cols, rows).await
}

/// UI backpressure (spec §4.4): the frontend pauses a session whose unparsed
/// bytes pile up.
#[tauri::command(rename_all = "snake_case")]
pub async fn session_pause_pty(
    state: tauri::State<'_, SessionsState>,
    session: String,
    paused: bool,
) -> Result<(), CmdError> {
    connected(&state)?.pause_pty(&session, paused).await
}
