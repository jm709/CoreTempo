#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]

mod support;

use support::{RawPost, start_default};

#[test]
fn pty_write_forwards_raw_bytes_to_the_agent() -> anyhow::Result<()> {
    let srv = start_default()?;
    let (status, _) = srv.post_raw(RawPost {
        path: "/v1/agents/builder/pty",
        content_type: Some("application/octet-stream"),
        body: b"ls\r",
        token: None,
    })?;
    assert_eq!(status, 204);
    let writes = srv.handles.fake_pty.writes();
    assert_eq!(writes, vec![("builder".to_string(), b"ls\r".to_vec())]);
    Ok(())
}

#[test]
fn pty_write_to_an_unknown_agent_lists_the_roster() -> anyhow::Result<()> {
    let srv = start_default()?;
    let (status, body) = srv.post_raw(RawPost {
        path: "/v1/agents/nobody/pty",
        content_type: None,
        body: b"x",
        token: None,
    })?;
    assert_eq!(status, 404);
    let body: serde_json::Value = serde_json::from_str(&body)?;
    assert_eq!(body["error"]["code"], "unknown_agent");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("builder") && message.contains("planner"),
        "{message}"
    );
    Ok(())
}

#[test]
fn pty_write_to_an_exited_agent_is_409() -> anyhow::Result<()> {
    let srv = start_default()?;
    srv.handles.fake_pty.set_agent(
        "builder",
        coretempo_core::types::AgentState::Exited,
        Some(coretempo_core::types::AgentExit::Code(0)),
        Vec::new(),
    );
    let (status, body) = srv.post_raw(RawPost {
        path: "/v1/agents/builder/pty",
        content_type: None,
        body: b"x",
        token: None,
    })?;
    assert_eq!(status, 409, "{body}");
    let body: serde_json::Value = serde_json::from_str(&body)?;
    assert_eq!(body["error"]["code"], "agent_exited");
    Ok(())
}

#[test]
fn pty_resize_and_pause_round_trip() -> anyhow::Result<()> {
    let srv = start_default()?;
    let (status, _) = srv.post(
        "/v1/agents/builder/pty/resize",
        None,
        &serde_json::json!({"cols": 200, "rows": 50}),
    )?;
    assert_eq!(status, 204);
    assert_eq!(
        srv.handles.fake_pty.resizes(),
        vec![("builder".to_string(), 200, 50)]
    );
    let (status, _) = srv.post(
        "/v1/agents/builder/pty/pause",
        None,
        &serde_json::json!({"paused": true}),
    )?;
    assert_eq!(status, 204);
    assert_eq!(
        srv.handles.fake_pty.pauses(),
        vec![("builder".to_string(), true)]
    );
    let (status, body) = srv.post(
        "/v1/agents/builder/pty/resize",
        None,
        &serde_json::json!({"cols": "wide"}),
    )?;
    assert_eq!(status, 400, "{body}");
    Ok(())
}
