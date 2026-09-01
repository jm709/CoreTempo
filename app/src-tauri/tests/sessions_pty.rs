//! The PTY pump: base64 chunks off the daemon's SSE stream, raw bytes onto the
//! Channel, and the resume cursor the *shell* keeps — the daemon is told where
//! to resume, it does not remember (spec §5/§7).
#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]
#![expect(clippy::unwrap_used, reason = "assertions are the vocabulary of tests")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use coretempo_app_lib::sessions::client::DaemonClient;
use coretempo_app_lib::sessions::pty::{subscribe, unsubscribe};
use coretempo_app_lib::sessions::supervisor::{Conn, SESSION_EVENT, SessionsState};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{Listener, Manager};
use tokio::sync::mpsc;

const SESSION: &str = "s-1f2e3d4c";

/// `\x1b[2Jhello `: 10 bytes from ring offset 0, so the SSE `id:` is 10.
const CHUNK_ONE: &[u8] = b"\x1b[2Jhello ";
const CHUNK_ONE_B64: &str = "G1sySmhlbGxvIA==";

/// `café\r\n` in UTF-8: 7 bytes from offset 10, so the `id:` is 17. Escape bytes
/// and a multi-byte character both have to survive byte-exact.
const CHUNK_TWO: &[u8] = b"caf\xc3\xa9\r\n";
const CHUNK_TWO_B64: &str = "Y2Fmw6kNCg==";

/// Where the second chunk ends — the cursor a resume must ask for.
const NEXT_CURSOR: u64 = 17;

/// The stub's body sender. Its error type is real (not `Infallible`) so a test
/// can kill a stream mid-flight the way a dying daemon does, rather than only
/// closing it cleanly.
type Frames = mpsc::Sender<Result<Bytes, std::io::Error>>;

/// What the stub daemon recorded, one entry per `/v1/sessions/{id}/pty`
/// connection it served.
#[derive(Default)]
struct Stub {
    /// The query string of each connection, in order.
    queries: Mutex<Vec<Option<String>>>,
    /// The still-open body sender of each connection, so a test can push a
    /// frame at one and watch a dropped subscriber make the send fail.
    frames: Mutex<Vec<Frames>>,
}

fn frame(id: u64, seq: u64, b64: &str) -> Bytes {
    Bytes::from(format!(
        "id: {id}\nevent: pty\ndata: {{\"seq\":{seq},\"b64\":\"{b64}\"}}\n\n"
    ))
}

/// Every connection replays the same two chunks, then stays open for whatever
/// the test pushes.
async fn pty_stream(State(stub): State<Arc<Stub>>, uri: axum::http::Uri) -> Response {
    stub.queries
        .lock()
        .unwrap()
        .push(uri.query().map(ToString::to_string));
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    let _ = tx.send(Ok(frame(10, 0, CHUNK_ONE_B64))).await;
    let _ = tx.send(Ok(frame(NEXT_CURSOR, 10, CHUNK_TWO_B64))).await;
    stub.frames.lock().unwrap().push(tx);
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    (
        [("content-type", "text/event-stream")],
        Body::from_stream(stream),
    )
        .into_response()
}

async fn spawn_stub() -> anyhow::Result<(u16, Arc<Stub>)> {
    let stub = Arc::new(Stub::default());
    let app = axum::Router::new()
        .route("/v1/sessions/{id}/pty", get(pty_stream))
        .with_state(Arc::clone(&stub));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Ok((port, stub))
}

/// A daemon that refuses the PTY route — the stale-token 401 a same-port daemon
/// restart produces, and the shape `unknown_agent` takes too.
async fn spawn_refusing_stub() -> anyhow::Result<u16> {
    let app = axum::Router::new().route(
        "/v1/sessions/{id}/pty",
        get(|| async {
            (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!(
                    {"error": {"code": "unauthorized", "message": "bad token"}}
                )),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Ok(port)
}

/// Everything the pump forwarded, in order.
type Sink = Arc<Mutex<Vec<Vec<u8>>>>;

fn sink_channel() -> (Sink, Channel<InvokeResponseBody>) {
    let sink: Sink = Arc::new(Mutex::new(Vec::new()));
    let writer = Arc::clone(&sink);
    let channel = Channel::new(move |body| {
        if let InvokeResponseBody::Raw(bytes) = body {
            writer.lock().unwrap().push(bytes);
        }
        Ok(())
    });
    (sink, channel)
}

/// A mock app already connected to the stub, which is the state every command
/// runs in — the supervisor is what puts it there in production.
fn connected_app(port: u16) -> anyhow::Result<tauri::App<tauri::test::MockRuntime>> {
    let app = tauri::test::mock_builder()
        .manage(SessionsState::default())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))?;
    *app.state::<SessionsState>().conn.lock().unwrap() =
        Conn::Connected(DaemonClient::new(port, "t".into()));
    Ok(app)
}

async fn wait_for_chunks(sink: &Sink, want: usize) -> Vec<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let chunks = sink.lock().unwrap().clone();
        if chunks.len() >= want || tokio::time::Instant::now() > deadline {
            return chunks;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Everything the shell emitted on the daemon's event channel.
type Events = Arc<Mutex<Vec<serde_json::Value>>>;

fn record_events(app: &tauri::App<tauri::test::MockRuntime>) -> Events {
    let events: Events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    app.listen(SESSION_EVENT, move |event| {
        sink.lock()
            .unwrap()
            .push(serde_json::from_str(event.payload()).unwrap());
    });
    events
}

async fn wait_for_events(events: &Events, want: usize) -> Vec<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let seen = events.lock().unwrap().clone();
        if seen.len() >= want || tokio::time::Instant::now() > deadline {
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn query(stub: &Stub, nth: usize) -> Option<String> {
    stub.queries.lock().unwrap().get(nth).cloned().flatten()
}

fn sender(stub: &Stub, nth: usize) -> Frames {
    stub.frames.lock().unwrap()[nth].clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn subscribing_decodes_chunks_and_tracks_the_resume_cursor() -> anyhow::Result<()> {
    let (port, stub) = spawn_stub().await?;
    let app = connected_app(port)?;
    let (sink, channel) = sink_channel();

    subscribe(app.handle(), SESSION.to_string(), false, channel)?;

    let chunks = wait_for_chunks(&sink, 2).await;
    assert_eq!(chunks, vec![CHUNK_ONE.to_vec(), CHUNK_TWO.to_vec()]);
    // resume=false replays from the ring's start, so the daemon is asked for no
    // particular offset.
    assert_eq!(query(&stub, 0), None);
    assert_eq!(
        app.state::<SessionsState>()
            .cursors
            .lock()
            .unwrap()
            .get(SESSION)
            .copied(),
        Some(NEXT_CURSOR),
        "the cursor must be the last `id:`, which is where the next chunk starts",
    );
    Ok(())
}

/// Unsubscribing must close the connection, not just stop reading it: a daemon
/// still writing into a socket nobody drains is a leak on both ends.
#[tokio::test(flavor = "multi_thread")]
async fn unsubscribing_drops_the_connection_and_the_pump() -> anyhow::Result<()> {
    let (port, stub) = spawn_stub().await?;
    let app = connected_app(port)?;
    let (sink, channel) = sink_channel();

    subscribe(app.handle(), SESSION.to_string(), false, channel)?;
    wait_for_chunks(&sink, 2).await;
    unsubscribe(&app.state::<SessionsState>(), SESSION);

    assert!(
        !app.state::<SessionsState>()
            .pty_pumps
            .lock()
            .unwrap()
            .contains_key(SESSION),
        "the pump handle must be gone, not left behind to be aborted twice",
    );
    let frames = sender(&stub, 0);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if frames.send(Ok(frame(20, 17, CHUNK_ONE_B64))).await.is_err() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the daemon end never saw the subscriber go away",
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        sink.lock().unwrap().len(),
        2,
        "nothing arrived after the drop"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn resuming_asks_the_daemon_for_the_stored_cursor() -> anyhow::Result<()> {
    let (port, stub) = spawn_stub().await?;
    let app = connected_app(port)?;

    let (first, channel) = sink_channel();
    subscribe(app.handle(), SESSION.to_string(), false, channel)?;
    wait_for_chunks(&first, 2).await;
    unsubscribe(&app.state::<SessionsState>(), SESSION);

    let (second, channel) = sink_channel();
    subscribe(app.handle(), SESSION.to_string(), true, channel)?;
    wait_for_chunks(&second, 2).await;

    assert_eq!(query(&stub, 1).as_deref(), Some("since=17"));
    Ok(())
}

/// resume=false is "give me the whole ring again" — a stored cursor from an
/// earlier attachment must not survive it, or the fresh terminal opens missing
/// its scrollback.
#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_subscribe_clears_the_stored_cursor() -> anyhow::Result<()> {
    let (port, stub) = spawn_stub().await?;
    let app = connected_app(port)?;

    let (first, channel) = sink_channel();
    subscribe(app.handle(), SESSION.to_string(), false, channel)?;
    wait_for_chunks(&first, 2).await;
    unsubscribe(&app.state::<SessionsState>(), SESSION);

    let (second, channel) = sink_channel();
    subscribe(app.handle(), SESSION.to_string(), false, channel)?;
    wait_for_chunks(&second, 2).await;

    assert_eq!(query(&stub, 1), None);
    Ok(())
}

/// Two subscribes without an unsubscribe between them: the webview reloaded, or
/// the terminal was reattached. The old pump must die, or every byte is
/// rendered twice.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_subscribe_aborts_the_first_pump() -> anyhow::Result<()> {
    let (port, stub) = spawn_stub().await?;
    let app = connected_app(port)?;

    let (first, channel) = sink_channel();
    subscribe(app.handle(), SESSION.to_string(), false, channel)?;
    wait_for_chunks(&first, 2).await;

    let (second, channel) = sink_channel();
    subscribe(app.handle(), SESSION.to_string(), true, channel)?;
    let seen = wait_for_chunks(&second, 2).await;
    assert_eq!(seen, vec![CHUNK_ONE.to_vec(), CHUNK_TWO.to_vec()]);

    // A frame at the live connection reaches the new subscriber, once.
    sender(&stub, 1)
        .send(Ok(frame(27, 17, CHUNK_ONE_B64)))
        .await?;
    let seen = wait_for_chunks(&second, 3).await;
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[2], CHUNK_ONE.to_vec());
    assert_eq!(
        first.lock().unwrap().len(),
        2,
        "the superseded pump must be dead, not forwarding in parallel",
    );
    Ok(())
}

/// Two invokes racing for the same session — a reattach landing while a reload
/// is still in flight. Every superseded pump must be *aborted*, not merely
/// dropped: a dropped `JoinHandle` leaves its task running with nothing left
/// that can ever stop it, still forwarding to a stale channel and still writing
/// this session's cursor.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_subscribes_leave_exactly_one_live_pump() -> anyhow::Result<()> {
    const RACERS: usize = 8;
    let (port, stub) = spawn_stub().await?;
    let app = connected_app(port)?;

    let pairs: Vec<(Sink, Channel<InvokeResponseBody>)> =
        (0..RACERS).map(|_| sink_channel()).collect();
    let mut sinks = Vec::new();
    let mut channels = Vec::new();
    for (sink, channel) in pairs {
        sinks.push(sink);
        channels.push(channel);
    }
    let racers: Vec<_> = channels
        .into_iter()
        .map(|channel| {
            let handle = app.handle().clone();
            std::thread::spawn(move || subscribe(&handle, SESSION.to_string(), false, channel))
        })
        .collect();
    for racer in racers {
        racer
            .join()
            .map_err(|_| anyhow::anyhow!("racer panicked"))??;
    }

    assert_eq!(
        app.state::<SessionsState>().pty_pumps.lock().unwrap().len(),
        1,
    );
    // Let every abort land and the survivor's connection open.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let before: Vec<usize> = sinks.iter().map(|s| s.lock().unwrap().len()).collect();

    // A frame at every connection the stub still holds. Only the survivor's is
    // still open, so only its sink can grow.
    let senders = stub.frames.lock().unwrap().clone();
    for tx in senders {
        let _ = tx.send(Ok(frame(27, 17, CHUNK_ONE_B64))).await;
    }
    let grown = wait_for_growth(&sinks, &before).await;
    assert_eq!(grown, 1, "{grown} pumps are still forwarding, not 1");
    Ok(())
}

/// Waits for at least one sink to grow, then lets stragglers land before
/// counting — so "exactly one" is not just "the others have not arrived yet".
async fn wait_for_growth(sinks: &[Sink], before: &[usize]) -> usize {
    let count = |sinks: &[Sink]| {
        sinks
            .iter()
            .zip(before)
            .filter(|(sink, was)| sink.lock().unwrap().len() > **was)
            .count()
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while count(sinks) == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    count(sinks)
}

/// A stream that never opens would otherwise be silent: `subscribe` has already
/// answered `Ok`, so the webview believes it is attached, and nothing retries a
/// PTY stream. The pump says so itself on the event channel.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_pty_stream_reports_itself_on_the_event_channel() -> anyhow::Result<()> {
    let port = spawn_refusing_stub().await?;
    let app = connected_app(port)?;
    let events = record_events(&app);

    let (chunks, channel) = sink_channel();
    subscribe(app.handle(), SESSION.to_string(), false, channel)?;

    let reported = wait_for_events(&events, 1).await;
    assert_eq!(reported.len(), 1, "{reported:?}");
    assert_eq!(reported[0]["type"], "pty.stream_error");
    assert_eq!(reported[0]["agent"], SESSION);
    let message = reported[0]["message"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no message in {}", reported[0]))?;
    assert!(message.contains("401"), "{message}");
    assert!(chunks.lock().unwrap().is_empty());
    Ok(())
}

/// A PTY stream can die on its own while the daemon lives — the session's
/// process exits, or that one connection breaks. The supervisor never notices
/// (its own stream is fine), so the pump has to say so or the terminal is dead
/// and silent.
#[tokio::test(flavor = "multi_thread")]
async fn a_pty_stream_that_dies_mid_flight_reports_itself() -> anyhow::Result<()> {
    let (port, stub) = spawn_stub().await?;
    let app = connected_app(port)?;
    let events = record_events(&app);

    let (chunks, channel) = sink_channel();
    subscribe(app.handle(), SESSION.to_string(), false, channel)?;
    wait_for_chunks(&chunks, 2).await;
    assert!(events.lock().unwrap().is_empty(), "nothing wrong yet");

    sender(&stub, 0)
        .send(Err(std::io::Error::other("the daemon dropped the stream")))
        .await?;

    let reported = wait_for_events(&events, 1).await;
    assert_eq!(reported.len(), 1, "{reported:?}");
    assert_eq!(reported[0]["type"], "pty.stream_error");
    assert_eq!(reported[0]["agent"], SESSION);
    Ok(())
}

/// A stream the daemon closed deliberately is not a failure — the session was
/// stopped, and its own `session.stopped` event already said so. Reporting it
/// as an error would put a red banner over every normal shutdown.
#[tokio::test(flavor = "multi_thread")]
async fn a_cleanly_closed_pty_stream_stays_silent() -> anyhow::Result<()> {
    let (port, stub) = spawn_stub().await?;
    let app = connected_app(port)?;
    let events = record_events(&app);

    let (chunks, channel) = sink_channel();
    subscribe(app.handle(), SESSION.to_string(), false, channel)?;
    wait_for_chunks(&chunks, 2).await;

    // Dropping the last sender ends the body the way a deliberate close does.
    drop(stub.frames.lock().unwrap().remove(0));

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        events.lock().unwrap().clone(),
        Vec::<serde_json::Value>::new(),
    );
    Ok(())
}

/// With no daemon, the command must say so with the code the UI switches on
/// rather than spawn a pump that can never connect.
#[tokio::test(flavor = "multi_thread")]
async fn subscribing_without_a_connection_fails_unreachable() -> anyhow::Result<()> {
    let app = tauri::test::mock_builder()
        .manage(SessionsState::default())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))?;
    let (_sink, channel) = sink_channel();

    let err = subscribe(app.handle(), SESSION.to_string(), false, channel)
        .expect_err("subscribing with no daemon must fail");

    assert_eq!(err.code, "daemon_unreachable");
    assert!(
        app.state::<SessionsState>()
            .pty_pumps
            .lock()
            .unwrap()
            .is_empty(),
    );
    Ok(())
}
