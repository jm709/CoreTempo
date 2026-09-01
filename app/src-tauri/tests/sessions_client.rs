//! `DaemonClient` against a stub sessions daemon: route, bearer, body shape,
//! and the daemon's error envelope surfacing verbatim.
#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]
#![expect(clippy::unwrap_used, reason = "assertions are the vocabulary of tests")]

use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use coretempo_app_lib::sessions::client::DaemonClient;
use coretempo_core::types::session::{CreateProjectRequest, CreateSessionRequest};

/// What the stub recorded about the last request it served.
#[derive(Default)]
struct Seen {
    auth: Option<String>,
    path: Option<String>,
    query: Option<String>,
    /// The body parsed as JSON; `None` when it was not JSON.
    body: Option<serde_json::Value>,
    raw: Vec<u8>,
}

type Recorder = Arc<Mutex<Seen>>;

/// Records every request before its handler runs, so the handlers below stay
/// one-liners and no route can forget to record.
async fn record(
    State(seen): State<Recorder>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 64 * 1024).await.unwrap();
    {
        let mut guard = seen.lock().unwrap();
        guard.auth = parts
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap().to_string());
        guard.path = Some(parts.uri.path().to_string());
        guard.query = parts.uri.query().map(ToString::to_string);
        guard.body = serde_json::from_slice(&bytes).ok();
        guard.raw = bytes.to_vec();
    }
    let request = axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes));
    next.run(request).await
}

fn session_json(id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "project": "p-0a1b2c3d",
        "cwd": "/home/u/proj",
        "worktree": null,
        "title": "fix the parser",
        "claude_session_id": null,
        "model": null,
        "permission_mode": null,
        "isolated_config": false,
        "prompt": null,
        "created_at": "2026-09-01T10:00:00Z",
        "stopped_at": null,
        "state": "idle",
        "blocked": null,
        "exit": null,
        "pty_cursor": 4096,
        "branch": "main",
        "changed_files": 2,
        "ahead": null,
        "worktree_status": "none"
    })
}

fn project_json() -> serde_json::Value {
    serde_json::json!({
        "id": "p-0a1b2c3d",
        "path": "/home/u/proj",
        "name": "proj",
        "created_at": "2026-09-01T09:00:00Z"
    })
}

/// A stub daemon covering every route `DaemonClient` speaks. `POST /v1/sessions`
/// answers the daemon's 422 error envelope, so the passthrough is exercised on a
/// real failure shape; the four routes that answer `204` do so with no body.
async fn spawn_stub() -> anyhow::Result<(u16, Recorder)> {
    let seen: Recorder = Arc::new(Mutex::new(Seen::default()));
    let untrusted = serde_json::json!(
        {"error": {"code": "untrusted", "message": "root /x is untrusted; fix A or B"}}
    );
    let app = axum::Router::new()
        .route(
            "/v1/health",
            get(|| async {
                Json(serde_json::json!({"ok": true, "sessions": {"live": 1, "total": 3}}))
            }),
        )
        .route(
            "/v1/sessions",
            get(|| async { Json(serde_json::json!([session_json("s-1f2e3d4c")])) })
                .post(move || async move { (StatusCode::UNPROCESSABLE_ENTITY, Json(untrusted)) }),
        )
        .route(
            "/v1/sessions/{id}",
            delete(|| async { Json(serde_json::json!({"branch_kept": true})) }),
        )
        .route(
            "/v1/sessions/{id}/stop",
            post(|| async { Json(session_json("s-1f2e3d4c")) }),
        )
        .route(
            "/v1/sessions/{id}/resume",
            post(|| async {
                Json(serde_json::json!({"session": session_json("s-1f2e3d4c"), "resumed": true}))
            }),
        )
        .route(
            "/v1/sessions/{id}/pty",
            post(|| async { StatusCode::NO_CONTENT }),
        )
        .route(
            "/v1/sessions/{id}/pty/resize",
            post(|| async { StatusCode::NO_CONTENT }),
        )
        .route(
            "/v1/sessions/{id}/pty/pause",
            post(|| async { StatusCode::NO_CONTENT }),
        )
        .route(
            "/v1/projects",
            get(|| async { Json(serde_json::json!([project_json()])) })
                .post(|| async { (StatusCode::CREATED, Json(project_json())) }),
        )
        .route(
            "/v1/projects/{id}",
            delete(|| async { StatusCode::NO_CONTENT }),
        )
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&seen),
            record,
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Ok((port, seen))
}

#[tokio::test]
async fn create_sends_bearer_and_surfaces_api_error_verbatim() -> anyhow::Result<()> {
    let (port, seen) = spawn_stub().await?;
    let client = DaemonClient::new(port, "tok-abc".into());
    assert!(client.health().await.is_ok());
    let req: CreateSessionRequest = serde_json::from_str(r#"{"project":"p-0a1b2c3d"}"#)?;
    let err = client.create_session(&req).await.unwrap_err();
    assert_eq!(err.code, "untrusted");
    assert_eq!(err.message, "root /x is untrusted; fix A or B");
    let guard = seen.lock().unwrap();
    assert_eq!(guard.auth.as_deref(), Some("Bearer tok-abc"));
    assert_eq!(guard.path.as_deref(), Some("/v1/sessions"));
    assert_eq!(guard.body.as_ref().unwrap()["project"], "p-0a1b2c3d");
    Ok(())
}

#[tokio::test]
async fn health_decodes_session_counts() -> anyhow::Result<()> {
    let (port, _seen) = spawn_stub().await?;
    let health = DaemonClient::new(port, "t".into()).health().await?;
    assert!(health.ok);
    assert_eq!(health.sessions.live, 1);
    assert_eq!(health.sessions.total, 3);
    Ok(())
}

#[tokio::test]
async fn list_routes_decode_the_core_wire_types() -> anyhow::Result<()> {
    let (port, seen) = spawn_stub().await?;
    let client = DaemonClient::new(port, "t".into());

    let sessions = client.list_sessions().await?;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id.0, "s-1f2e3d4c");
    assert_eq!(sessions[0].changed_files, Some(2));
    assert_eq!(seen.lock().unwrap().path.as_deref(), Some("/v1/sessions"));

    let projects = client.list_projects().await?;
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].name, "proj");
    assert_eq!(seen.lock().unwrap().path.as_deref(), Some("/v1/projects"));
    Ok(())
}

#[tokio::test]
async fn lifecycle_routes_carry_the_id_in_the_path() -> anyhow::Result<()> {
    let (port, seen) = spawn_stub().await?;
    let client = DaemonClient::new(port, "t".into());

    let stopped = client.stop_session("s-1f2e3d4c").await?;
    assert_eq!(stopped.id.0, "s-1f2e3d4c");
    assert_eq!(
        seen.lock().unwrap().path.as_deref(),
        Some("/v1/sessions/s-1f2e3d4c/stop")
    );

    let resumed = client.resume_session("s-1f2e3d4c").await?;
    assert!(resumed.resumed);
    assert_eq!(
        seen.lock().unwrap().path.as_deref(),
        Some("/v1/sessions/s-1f2e3d4c/resume")
    );
    Ok(())
}

#[tokio::test]
async fn delete_session_query_encodes_both_flags() -> anyhow::Result<()> {
    let (port, seen) = spawn_stub().await?;
    let client = DaemonClient::new(port, "t".into());

    let deleted = client.delete_session("s-1f2e3d4c", true, false).await?;
    assert!(deleted.branch_kept);
    {
        let guard = seen.lock().unwrap();
        assert_eq!(guard.path.as_deref(), Some("/v1/sessions/s-1f2e3d4c"));
        assert_eq!(
            guard.query.as_deref(),
            Some("remove_worktree=true&force=false")
        );
    }

    client.delete_session("s-1f2e3d4c", false, true).await?;
    assert_eq!(
        seen.lock().unwrap().query.as_deref(),
        Some("remove_worktree=false&force=true")
    );
    Ok(())
}

/// The daemon answers 204 on these four, so nothing may try to decode a body.
#[tokio::test]
async fn no_content_routes_send_their_payload_and_decode_nothing() -> anyhow::Result<()> {
    let (port, seen) = spawn_stub().await?;
    let client = DaemonClient::new(port, "t".into());

    client
        .write_pty("s-1f2e3d4c", b"\x1b[Ahi\r".to_vec())
        .await?;
    {
        let guard = seen.lock().unwrap();
        assert_eq!(guard.path.as_deref(), Some("/v1/sessions/s-1f2e3d4c/pty"));
        assert_eq!(guard.raw, b"\x1b[Ahi\r");
    }

    client.resize_pty("s-1f2e3d4c", 120, 40).await?;
    {
        let guard = seen.lock().unwrap();
        assert_eq!(
            guard.path.as_deref(),
            Some("/v1/sessions/s-1f2e3d4c/pty/resize")
        );
        assert_eq!(
            guard.body.as_ref().unwrap(),
            &serde_json::json!({"cols": 120, "rows": 40})
        );
    }

    client.pause_pty("s-1f2e3d4c", true).await?;
    {
        let guard = seen.lock().unwrap();
        assert_eq!(
            guard.path.as_deref(),
            Some("/v1/sessions/s-1f2e3d4c/pty/pause")
        );
        assert_eq!(
            guard.body.as_ref().unwrap(),
            &serde_json::json!({"paused": true})
        );
    }

    client.forget_project("p-0a1b2c3d").await?;
    assert_eq!(
        seen.lock().unwrap().path.as_deref(),
        Some("/v1/projects/p-0a1b2c3d")
    );
    Ok(())
}

#[tokio::test]
async fn register_project_posts_the_request_body() -> anyhow::Result<()> {
    let (port, seen) = spawn_stub().await?;
    let req = CreateProjectRequest {
        path: "/home/u/proj".into(),
        name: Some("proj".into()),
    };
    let project = DaemonClient::new(port, "t".into())
        .register_project(&req)
        .await?;
    assert_eq!(project.id.0, "p-0a1b2c3d");
    let guard = seen.lock().unwrap();
    assert_eq!(guard.path.as_deref(), Some("/v1/projects"));
    assert_eq!(guard.body.as_ref().unwrap()["path"], "/home/u/proj");
    Ok(())
}

/// Nothing is listening, so the transport error must name the daemon and the fix
/// rather than leaking a bare reqwest message.
#[tokio::test]
async fn unreachable_daemon_names_the_daemon() -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let dead = listener.local_addr()?.port();
    drop(listener);
    let err = DaemonClient::new(dead, "t".into())
        .health()
        .await
        .unwrap_err();
    assert_eq!(err.code, "daemon_unreachable");
    assert!(err.message.contains("sessions daemon"), "{}", err.message);
    Ok(())
}
