//! One long-lived connection task per app: find the daemon, forward its
//! `/v1/events` stream to the webview, and on any stream death announce
//! `unreachable` and reconnect with backoff.
//!
//! The shell holds no session state of its own — the daemon's events *are* the
//! model. What it does hold is the connection and the PTY cursors, both of which
//! belong to a single daemon process: a restarted daemon numbers its PTY rings
//! from scratch, so every cursor is dropped on reconnect rather than resumed
//! against numbering that no longer means anything (spec §6).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use tauri::{Emitter, Manager};

use crate::commands::CmdError;
use crate::sessions::client::DaemonClient;
use crate::sessions::discovery::Discovery;
use crate::sessions::sse::SseParser;

/// Connection state transitions, as `{"state": "idle" | "starting" |
/// "connected" | "unreachable"}`.
pub const STATUS_EVENT: &str = "coretempo:sessions-status";

/// One daemon event, forwarded verbatim — the shell adds nothing and drops
/// nothing.
pub const SESSION_EVENT: &str = "coretempo:session-event";

/// First retry delay after a failed connect or a dropped stream.
const BACKOFF_START: Duration = Duration::from_secs(1);

/// Ceiling for the doubling backoff. A daemon the operator restarts by hand
/// should be picked up within seconds, so this stays short.
const BACKOFF_MAX: Duration = Duration::from_secs(15);

/// Where the shell is with the sessions daemon.
#[derive(Clone, Default)]
pub enum Conn {
    /// Sessions mode has never been opened, so nothing has looked for a daemon.
    #[default]
    Idle,
    Starting,
    Connected(DaemonClient),
    Unreachable,
}

impl Conn {
    /// The wire name the webview switches on.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Conn::Idle => "idle",
            Conn::Starting => "starting",
            Conn::Connected(_) => "connected",
            Conn::Unreachable => "unreachable",
        }
    }
}

/// Everything the shell keeps for Sessions mode. Managed once, for the life of
/// the app.
#[derive(Default)]
pub struct SessionsState {
    pub conn: Mutex<Conn>,
    /// session id → next `since` value (the SSE `id:`: start + len, amendment
    /// 40). The shell owns these, not the daemon: it is the shell that knows
    /// which bytes actually reached a terminal.
    pub cursors: Mutex<HashMap<String, u64>>,
    /// session id → the task forwarding that session's PTY stream.
    pub pty_pumps: Mutex<HashMap<String, tauri::async_runtime::JoinHandle<()>>>,
    pub supervisor_started: AtomicBool,
}

/// The connected client, or the error every command answers with when there is
/// no daemon to ask.
///
/// # Errors
/// `daemon_unreachable` whenever the supervisor is not currently connected.
pub fn connected(state: &SessionsState) -> Result<DaemonClient, CmdError> {
    let unreachable = || {
        CmdError::new(
            "daemon_unreachable",
            "not connected to the sessions daemon; open Sessions mode to start one, or check \
             ~/.coretempo/sessions/daemon.log if it keeps failing",
        )
    };
    let guard = state.conn.lock().map_err(|_| {
        CmdError::new(
            "internal",
            "the sessions connection lock is poisoned; restart the app",
        )
    })?;
    match &*guard {
        Conn::Connected(client) => Ok(client.clone()),
        Conn::Idle | Conn::Starting | Conn::Unreachable => Err(unreachable()),
    }
}

/// Starts the connection task. Idempotent: the first call spawns it, later ones
/// are no-ops, because a second supervisor would double every event the UI
/// renders.
pub fn ensure_supervisor<R: tauri::Runtime>(app: tauri::AppHandle<R>, discovery: Discovery) {
    if app
        .state::<SessionsState>()
        .supervisor_started
        .swap(true, Ordering::SeqCst)
    {
        return;
    }
    tauri::async_runtime::spawn(run_supervisor(app, discovery));
}

async fn run_supervisor<R: tauri::Runtime>(app: tauri::AppHandle<R>, discovery: Discovery) {
    let mut backoff = BACKOFF_START;
    loop {
        announce(&app, Conn::Starting);
        match discovery.connect().await {
            Ok(client) => {
                backoff = BACKOFF_START;
                reset_cursors(&app);
                announce(&app, Conn::Connected(client.clone()));
                pump_events(&app, &client).await;
                announce(&app, Conn::Unreachable);
            }
            Err(err) => {
                tracing::warn!(%err, "sessions daemon connect failed");
                announce(&app, Conn::Unreachable);
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Forwards the daemon's events until the stream ends, whatever ended it: the
/// daemon exiting, the socket dying, or a 401 from a token this client no
/// longer shares with it. All three mean the same thing — this connection is
/// over — and all three come back here to be retried from scratch.
async fn pump_events<R: tauri::Runtime>(app: &tauri::AppHandle<R>, client: &DaemonClient) {
    let request = client.request(reqwest::Method::GET, "/v1/events");
    let response = match request.send().await {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!(%err, "sessions event stream could not be opened");
            return;
        }
    };
    if !response.status().is_success() {
        tracing::warn!(
            status = %response.status(),
            "sessions daemon refused the event stream; reconnecting"
        );
        return;
    }
    let mut parser = SseParser::default();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = match chunk {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(%err, "sessions event stream failed");
                return;
            }
        };
        for event in parser.push(&bytes) {
            match serde_json::from_str::<serde_json::Value>(&event.data) {
                Ok(value) => {
                    let _ = app.emit(SESSION_EVENT, value);
                }
                Err(err) => tracing::warn!(%err, "sessions daemon sent an unparsable event"),
            }
        }
    }
}

/// Stores the new state and tells the webview, in that order: a listener that
/// reacts by invoking a command must find the state it was just told about.
fn announce<R: tauri::Runtime>(app: &tauri::AppHandle<R>, conn: Conn) {
    let name = conn.name();
    if let Ok(mut guard) = app.state::<SessionsState>().conn.lock() {
        *guard = conn;
    } else {
        // Announce anyway: the webview's status is the only thing left that can
        // still tell the operator what happened.
        tracing::error!("the sessions connection lock is poisoned");
    }
    let _ = app.emit(STATUS_EVENT, serde_json::json!({ "state": name }));
}

fn reset_cursors<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    let state = app.state::<SessionsState>();
    let Ok(mut cursors) = state.cursors.lock() else {
        tracing::error!("the sessions cursor lock is poisoned");
        return;
    };
    cursors.clear();
}
