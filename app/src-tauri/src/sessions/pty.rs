//! Forwarding one session's PTY output from the daemon's SSE stream to a
//! webview Channel.
//!
//! The resume cursor lives here, not in the daemon: the daemon streams whatever
//! it is asked for and remembers nothing, so only the shell knows which bytes
//! actually reached a terminal. The cursor is the SSE `id:` — the offset the
//! *next* chunk starts at (start + len, contracts §6.2), which is exactly what
//! `?since=` wants; using the chunk's own `seq` would re-deliver it on every
//! resume.

use base64::Engine;
use futures_util::StreamExt;
use tauri::Manager;
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::commands::CmdError;
use crate::sessions::client::DaemonClient;
use crate::sessions::sse::SseParser;
use crate::sessions::supervisor::SessionsState;

/// One `{"seq":…, "b64":…}` PTY event (contracts §6.2).
#[derive(serde::Deserialize)]
struct PtyEventData {
    b64: String,
}

/// Starts forwarding `session`'s output to `channel`.
///
/// `resume = true` picks up at the stored cursor, which is what reattaching a
/// terminal that was only hidden wants. `resume = false` replays from the start
/// of the daemon's ring and drops the stored cursor first, which is what a
/// terminal being built from scratch wants.
///
/// Returns as soon as the pump is spawned — the stream is opened on the pump's
/// own task, so a slow daemon never holds up the invoke.
///
/// # Errors
/// `daemon_unreachable` when the supervisor has no live connection.
pub fn subscribe<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    session: String,
    resume: bool,
    channel: Channel<InvokeResponseBody>,
) -> Result<(), CmdError> {
    let state = app.state::<SessionsState>();
    let client = crate::sessions::supervisor::connected(&state)?;
    // One lock scope for the whole swap — abort, cursor, spawn, insert. Two
    // invokes that each released it in between would both find no pump, both
    // spawn, and the loser's handle would be *overwritten* rather than aborted:
    // dropping a `JoinHandle` does not stop its task, so that pump would keep
    // forwarding to a stale channel and writing this session's cursor with
    // nothing left holding a handle that could ever stop it.
    let mut pumps = state.pty_pumps.lock().map_err(|_| {
        CmdError::new(
            "internal",
            "the sessions pump lock is poisoned; restart the app",
        )
    })?;
    // Abort before the cursor is read, not after: a pump still forwarding would
    // advance the cursor under us and the new stream would reopen at an offset
    // whose bytes are already on screen.
    if let Some(previous) = pumps.remove(&session) {
        previous.abort();
    }
    let since = if resume {
        cursor(&state, &session)
    } else {
        forget_cursor(&state, &session);
        None
    };
    let path = match since {
        Some(cursor) => format!("/v1/sessions/{session}/pty?since={cursor}"),
        None => format!("/v1/sessions/{session}/pty"),
    };
    let handle =
        tauri::async_runtime::spawn(pump(app.clone(), client, session.clone(), path, channel));
    pumps.insert(session, handle);
    Ok(())
}

/// Ends the shell's subscription: aborting the task drops the response with it,
/// which is what closes the connection at the daemon end.
pub fn unsubscribe(state: &SessionsState, session: &str) {
    let Ok(mut pumps) = state.pty_pumps.lock() else {
        tracing::error!("the sessions pump lock is poisoned");
        return;
    };
    if let Some(handle) = pumps.remove(session) {
        handle.abort();
    }
}

async fn pump<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    client: DaemonClient,
    session: String,
    path: String,
    channel: Channel<InvokeResponseBody>,
) {
    let response = match client.request(reqwest::Method::GET, &path).send().await {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!(%err, %session, "session pty stream could not be opened");
            return;
        }
    };
    if !response.status().is_success() {
        tracing::warn!(status = %response.status(), %session, "session pty stream refused");
        return;
    }
    let mut parser = SseParser::default();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else {
            return; // the daemon went away; the supervisor is what reconnects
        };
        for event in parser.push(&bytes) {
            let Some(decoded) = decode(&session, &event.data) else {
                continue;
            };
            if channel.send(InvokeResponseBody::Raw(decoded)).is_err() {
                return; // the webview dropped its end
            }
            store_cursor(&app, &session, event.id.as_deref());
        }
    }
}

/// The chunk's raw bytes, or `None` with the reason logged — a malformed event
/// is the daemon's bug, and dropping one chunk beats tearing down the stream.
fn decode(session: &str, data: &str) -> Option<Vec<u8>> {
    let event: PtyEventData = match serde_json::from_str(data) {
        Ok(event) => event,
        Err(err) => {
            tracing::warn!(%err, %session, "session pty event was not the {{seq, b64}} shape");
            return None;
        }
    };
    match base64::engine::general_purpose::STANDARD.decode(&event.b64) {
        Ok(bytes) => Some(bytes),
        Err(err) => {
            tracing::warn!(%err, %session, "session pty chunk was not valid base64");
            None
        }
    }
}

/// Records where the next chunk starts, but only once the bytes before it are
/// on the channel: a cursor stored ahead of the write it describes would skip
/// those bytes on the next resume.
fn store_cursor<R: tauri::Runtime>(app: &tauri::AppHandle<R>, session: &str, id: Option<&str>) {
    let Some(next) = id.and_then(|id| id.parse::<u64>().ok()) else {
        tracing::warn!(%session, ?id, "session pty event carried no usable resume id");
        return;
    };
    let state = app.state::<SessionsState>();
    let Ok(mut cursors) = state.cursors.lock() else {
        tracing::error!("the sessions cursor lock is poisoned");
        return;
    };
    cursors.insert(session.to_string(), next);
}

fn cursor(state: &SessionsState, session: &str) -> Option<u64> {
    state
        .cursors
        .lock()
        .ok()
        .and_then(|cursors| cursors.get(session).copied())
}

fn forget_cursor(state: &SessionsState, session: &str) {
    let Ok(mut cursors) = state.cursors.lock() else {
        tracing::error!("the sessions cursor lock is poisoned");
        return;
    };
    cursors.remove(session);
}
