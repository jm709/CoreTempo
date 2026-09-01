//! The one long-lived connection task: connect, forward `/v1/events`, and on a
//! dropped stream announce `unreachable`, drop every cursor, and reconnect.
#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]
#![expect(clippy::unwrap_used, reason = "assertions are the vocabulary of tests")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use coretempo_app_lib::sessions::discovery::Discovery;
use coretempo_app_lib::sessions::supervisor::{
    SESSION_EVENT, STATUS_EVENT, SessionsState, ensure_supervisor,
};
use futures_util::StreamExt;
use tauri::{Listener, Manager};

/// `(event name, payload)` in the order the shell emitted them.
type Log = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

/// How many `/v1/events` connections the stub has served.
type Connections = Arc<AtomicUsize>;

fn event_json(seq: u64, session: &str) -> serde_json::Value {
    serde_json::json!({
        "seq": seq,
        "ts": "2026-09-01T10:00:00Z",
        "type": "session.created",
        "agent": session,
    })
}

fn frame(id: u64, event: &str, data: &serde_json::Value) -> Bytes {
    Bytes::from(format!("id: {id}\nevent: {event}\ndata: {data}\n\n"))
}

/// `/v1/events`: the first connection sends two events and then ends the body —
/// what a daemon exiting mid-stream looks like from here. Every later one sends
/// a third event and holds the connection open.
async fn events(State(connections): State<Connections>) -> Response {
    let nth = connections.fetch_add(1, Ordering::SeqCst);
    let body = if nth == 0 {
        let frames = vec![
            Ok::<Bytes, std::convert::Infallible>(frame(
                1,
                "session.created",
                &event_json(1, "s-1"),
            )),
            Ok(frame(2, "session.stopped", &event_json(2, "s-2"))),
        ];
        Body::from_stream(futures_util::stream::iter(frames))
    } else {
        let frames = vec![Ok::<Bytes, std::convert::Infallible>(frame(
            3,
            "session.created",
            &event_json(3, "s-3"),
        ))];
        Body::from_stream(futures_util::stream::iter(frames).chain(futures_util::stream::pending()))
    };
    ([("content-type", "text/event-stream")], body).into_response()
}

async fn spawn_stub() -> anyhow::Result<(u16, Connections)> {
    let connections: Connections = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new()
        .route(
            "/v1/health",
            get(|| async {
                Json(serde_json::json!({"ok": true, "sessions": {"live": 0, "total": 0}}))
            }),
        )
        .route("/v1/events", get(events))
        .with_state(Arc::clone(&connections));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Ok((port, connections))
}

fn scratch(name: &str) -> anyhow::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "coretempo-supervisor-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn mock_app() -> anyhow::Result<tauri::App<tauri::test::MockRuntime>> {
    Ok(tauri::test::mock_builder()
        .manage(SessionsState::default())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))?)
}

/// Records both event names into one log, so their relative order is asserted
/// too — `connected` arriving after the first payload would be a bug the UI
/// would render as a flash of "connecting" over live output.
fn record(app: &tauri::App<tauri::test::MockRuntime>) -> Log {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    for name in [STATUS_EVENT, SESSION_EVENT] {
        let sink = Arc::clone(&log);
        app.listen(name, move |event| {
            let payload = serde_json::from_str(event.payload()).unwrap();
            sink.lock().unwrap().push((name.to_string(), payload));
        });
    }
    log
}

/// Waits for the log to reach `want` entries, so the test never sleeps longer
/// than the supervisor takes.
async fn wait_for(log: &Log, want: usize) -> Vec<(String, serde_json::Value)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let entries = log.lock().unwrap().clone();
        if entries.len() >= want || tokio::time::Instant::now() > deadline {
            return entries;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn status(state: &str) -> (String, serde_json::Value) {
    (
        STATUS_EVENT.to_string(),
        serde_json::json!({"state": state}),
    )
}

fn session(seq: u64, id: &str) -> (String, serde_json::Value) {
    (SESSION_EVENT.to_string(), event_json(seq, id))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_supervisor_forwards_events_and_reconnects_after_a_drop() -> anyhow::Result<()> {
    let dir = scratch("reconnect")?;
    let (port, connections) = spawn_stub().await?;
    std::fs::write(
        dir.join("api.json"),
        format!(r#"{{"port":{port},"token":"t","pid":1}}"#),
    )?;

    let app = mock_app()?;
    let log = record(&app);
    // A cursor from the previous daemon: its numbering does not survive a
    // restart, so reconnecting must drop it (spec §6).
    app.state::<SessionsState>()
        .cursors
        .lock()
        .unwrap()
        .insert("s-1".to_string(), 4096);

    ensure_supervisor(
        app.handle().clone(),
        Discovery {
            sessions_dir: dir,
            bin: PathBuf::from("/nonexistent-coretempod"),
            deadline: Duration::from_secs(2),
        },
    );

    let entries = wait_for(&log, 8).await;
    assert_eq!(
        entries,
        vec![
            status("starting"),
            status("connected"),
            session(1, "s-1"),
            session(2, "s-2"),
            status("unreachable"),
            status("starting"),
            status("connected"),
            session(3, "s-3"),
        ],
    );
    assert_eq!(connections.load(Ordering::SeqCst), 2);
    assert!(
        app.state::<SessionsState>()
            .cursors
            .lock()
            .unwrap()
            .is_empty(),
        "reconnecting must drop cursors from the previous daemon's numbering",
    );
    Ok(())
}

/// A second call must not start a second connection: two supervisors would
/// double every event the UI renders.
#[tokio::test(flavor = "multi_thread")]
async fn ensure_supervisor_is_idempotent() -> anyhow::Result<()> {
    let dir = scratch("idempotent")?;
    let (port, connections) = spawn_stub().await?;
    std::fs::write(
        dir.join("api.json"),
        format!(r#"{{"port":{port},"token":"t","pid":1}}"#),
    )?;

    let app = mock_app()?;
    let log = record(&app);
    for _ in 0..3 {
        ensure_supervisor(
            app.handle().clone(),
            Discovery {
                sessions_dir: dir.clone(),
                bin: PathBuf::from("/nonexistent-coretempod"),
                deadline: Duration::from_secs(2),
            },
        );
    }

    // The prefix, not the whole log: this stub's first connection ends after two
    // events, so the supervisor is already on its way to `unreachable`.
    let entries = wait_for(&log, 4).await;
    assert_eq!(
        entries[..4],
        [
            status("starting"),
            status("connected"),
            session(1, "s-1"),
            session(2, "s-2"),
        ],
    );
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    Ok(())
}

/// Nothing is listening and no binary can be spawned: the supervisor must
/// announce `unreachable` rather than give up silently or take the run down.
#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_that_never_comes_up_announces_unreachable() -> anyhow::Result<()> {
    let dir = scratch("dead")?;
    let app = mock_app()?;
    let log = record(&app);

    ensure_supervisor(
        app.handle().clone(),
        Discovery {
            sessions_dir: dir,
            bin: PathBuf::from("/nonexistent-coretempod"),
            deadline: Duration::from_millis(200),
        },
    );

    let entries = wait_for(&log, 2).await;
    assert_eq!(entries[0], status("starting"));
    assert_eq!(entries[1], status("unreachable"));
    Ok(())
}
