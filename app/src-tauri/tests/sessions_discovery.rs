//! Spec 2026-08-27 §6 steps 1–3: `api.json` → health probe → spawn detached →
//! poll a fresh `api.json` → health. A health answer is the only proof the
//! daemon is up; the pid in `api.json` is never consulted.
#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]
#![expect(clippy::unwrap_used, reason = "assertions are the vocabulary of tests")]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use coretempo_app_lib::sessions::discovery::{Discovery, read_api_file};

/// The `authorization` header of the last health request the stub served.
type SeenAuth = Arc<Mutex<Option<String>>>;

/// A stub daemon that answers `/v1/health` only for `expect_token`; anything
/// else gets the 401 envelope, so a client carrying a stale token cannot pass
/// the probe by accident.
async fn spawn_health(expect_token: &'static str) -> anyhow::Result<(u16, SeenAuth)> {
    let seen: SeenAuth = Arc::new(Mutex::new(None));
    let recorder = Arc::clone(&seen);
    let app = axum::Router::new().route(
        "/v1/health",
        get(move |headers: HeaderMap| {
            let recorder = Arc::clone(&recorder);
            async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(ToString::to_string);
                recorder.lock().unwrap().clone_from(&auth);
                if auth.as_deref() == Some(format!("Bearer {expect_token}").as_str()) {
                    Json(serde_json::json!({"ok": true, "sessions": {"live": 0, "total": 0}}))
                        .into_response()
                } else {
                    (
                        StatusCode::UNAUTHORIZED,
                        Json(serde_json::json!(
                            {"error": {"code": "unauthorized", "message": "bad token"}}
                        )),
                    )
                        .into_response()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Ok((port, seen))
}

/// A per-test scratch directory standing in for `~/.coretempo/sessions`.
fn scratch(name: &str) -> anyhow::Result<PathBuf> {
    let dir =
        std::env::temp_dir().join(format!("coretempo-discovery-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn api_json(port: u16, token: &str, pid: u32) -> String {
    format!(r#"{{"port":{port},"token":"{token}","pid":{pid}}}"#)
}

/// A stand-in for `coretempod`: `body` is the whole of its behaviour.
fn fake_daemon(dir: &Path, body: &str) -> anyhow::Result<PathBuf> {
    let path = dir.join("fake-coretempod");
    std::fs::write(&path, format!("#!/bin/bash\n{body}\n"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

fn discovery(dir: &Path, bin: PathBuf, deadline: Duration) -> Discovery {
    Discovery {
        sessions_dir: dir.to_path_buf(),
        bin,
        deadline,
    }
}

/// A port nothing is listening on: bind it, learn it, drop it.
async fn dead_port() -> anyhow::Result<u16> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Step 1: a daemon that answers health is used as-is. The binary is a path
/// that does not exist and the pid is 1 — proof neither was consulted.
#[tokio::test]
async fn live_api_file_connects_without_spawning() -> anyhow::Result<()> {
    let dir = scratch("live")?;
    let (port, seen) = spawn_health("t").await?;
    std::fs::write(dir.join("api.json"), api_json(port, "t", 1))?;

    let client = discovery(
        &dir,
        PathBuf::from("/nonexistent-coretempod"),
        Duration::from_secs(5),
    )
    .connect()
    .await?;

    assert!(client.health().await?.ok);
    assert_eq!(seen.lock().unwrap().as_deref(), Some("Bearer t"));
    Ok(())
}

/// Steps 2–3: the file names a port nothing answers, so the daemon is spawned
/// and the *fresh* file it writes is what connects — new port, new token.
#[tokio::test]
async fn stale_api_file_spawns_and_reconnects_with_the_new_token() -> anyhow::Result<()> {
    let dir = scratch("stale")?;
    let (port, seen) = spawn_health("new-token").await?;
    std::fs::write(
        dir.join("api.json"),
        api_json(dead_port().await?, "old", 99999),
    )?;
    let bin = fake_daemon(
        &dir,
        &format!(
            "printf '%s' '{}' > {}/api.json",
            api_json(port, "new-token", 4242),
            dir.display()
        ),
    )?;

    let client = discovery(&dir, bin, Duration::from_secs(5))
        .connect()
        .await?;

    assert_eq!(client.health().await?.sessions.total, 0);
    assert_eq!(seen.lock().unwrap().as_deref(), Some("Bearer new-token"));
    Ok(())
}

/// A spawn that exits 1 lost the race to a peer that got there first. That is
/// success: the peer's `api.json` is what the next poll finds.
#[tokio::test]
async fn spawn_exiting_nonzero_is_a_lost_race_not_a_failure() -> anyhow::Result<()> {
    let dir = scratch("race")?;
    let (port, _seen) = spawn_health("winner").await?;
    let bin = fake_daemon(&dir, "sleep 0.1\nexit 1")?;

    let peer = dir.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::write(peer.join("api.json"), api_json(port, "winner", 7)).unwrap();
    });

    let client = discovery(&dir, bin, Duration::from_secs(5))
        .connect()
        .await?;
    assert!(client.health().await?.ok);
    Ok(())
}

/// Nothing ever answers: the error names the hand-start command and the log.
#[tokio::test]
async fn nothing_answers_within_the_deadline_names_the_fix() -> anyhow::Result<()> {
    let dir = scratch("dead")?;
    let started = std::time::Instant::now();

    let err = discovery(&dir, PathBuf::from("/bin/false"), Duration::from_secs(1))
        .connect()
        .await
        .err()
        .expect("connect must fail when no daemon ever answers");

    assert_eq!(err.code, "daemon_unreachable");
    assert!(
        err.message.contains("coretempod sessions"),
        "{}",
        err.message
    );
    assert!(err.message.contains("daemon.log"), "{}", err.message);
    assert!(started.elapsed() < Duration::from_secs(5), "gave up late");
    Ok(())
}

/// A missing or half-written file is not an error — it just means "no daemon".
#[tokio::test]
async fn read_api_file_is_none_for_missing_and_unparsable() -> anyhow::Result<()> {
    let dir = scratch("unparsable")?;
    assert!(read_api_file(&dir).is_none());
    std::fs::write(dir.join("api.json"), "{\"port\": 4821, \"tok")?;
    assert!(read_api_file(&dir).is_none());
    std::fs::write(dir.join("api.json"), api_json(4821, "t", 9))?;
    assert_eq!(read_api_file(&dir).unwrap().port, 4821);
    Ok(())
}

/// A binary that cannot be executed fails now, with its path — waiting out the
/// deadline first would bury the cause.
#[tokio::test]
async fn a_missing_binary_fails_immediately_with_its_path() -> anyhow::Result<()> {
    let dir = scratch("nobin")?;
    let err = discovery(
        &dir,
        PathBuf::from("/nonexistent-coretempod"),
        Duration::from_secs(5),
    )
    .connect()
    .await
    .err()
    .expect("connect must fail when the binary cannot be run");
    assert_eq!(err.code, "spawn_failed");
    assert!(
        err.message.contains("/nonexistent-coretempod"),
        "{}",
        err.message
    );
    Ok(())
}
