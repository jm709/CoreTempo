#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]

mod support;

use coretempo_core::types::{AgentExit, AgentState};
use serde_json::json;

fn seeded() -> anyhow::Result<support::TestServer> {
    let (ctx, handles) = support::test_ctx()?;
    handles
        .fake_pty
        .set_agent("builder", AgentState::Working, None, vec![(0, b"hello ")]);
    handles.fake_pty.set_agent(
        "planner",
        AgentState::Exited,
        Some(AgentExit::Code(1)),
        Vec::new(),
    );
    support::start(ctx, handles)
}

#[test]
fn roster_is_lexicographic_with_state_and_pending_asks() -> anyhow::Result<()> {
    let srv = seeded()?;
    srv.post(
        "/v1/messages",
        Some("planner"),
        &json!({"to": "builder", "kind": "ask", "body": "q"}),
    )?;
    let (status, body) = srv.get("/v1/agents", None)?;
    assert_eq!(status, 200);
    let agents = body["agents"].as_array().cloned().unwrap_or_default();
    assert_eq!(agents.len(), 2);
    assert_eq!(agents[0]["id"], "builder");
    assert_eq!(agents[0]["state"], "working");
    assert_eq!(agents[0]["pending_asks"], 0);
    assert!(agents[0]["exit"].is_null());
    assert_eq!(agents[1]["id"], "planner");
    assert_eq!(agents[1]["state"], "exited");
    assert_eq!(agents[1]["exit"], serde_json::json!({"code": 1}));
    assert_eq!(agents[1]["pending_asks"], 1);
    Ok(())
}

#[test]
fn detail_flattens_info_and_adds_frozen_config() -> anyhow::Result<()> {
    let srv = seeded()?;
    let (status, body) = srv.get("/v1/agents/builder", None)?;
    assert_eq!(status, 200);
    assert_eq!(body["id"], "builder");
    assert_eq!(body["state"], "working");
    assert_eq!(body["dir"], "/tmp");
    assert_eq!(body["auto_clear"], true);
    assert!(body["model"].is_null());
    assert_eq!(body["pty_cursor"], 6);
    assert_eq!(body["isolated_config"], false);
    assert_eq!(body["skills"], serde_json::json!([]));
    Ok(())
}

#[test]
fn unknown_agent_paths_are_404_with_roster() -> anyhow::Result<()> {
    let srv = seeded()?;
    for path in ["/v1/agents/ghost", "/v1/agents/ghost/restart"] {
        let (status, body) = if path.ends_with("restart") {
            srv.post(path, None, &json!({}))?
        } else {
            srv.get(path, None)?
        };
        assert_eq!(status, 404, "{path}");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("builder, planner")
        );
    }
    Ok(())
}

#[test]
fn restart_returns_202_and_kicks_pty() -> anyhow::Result<()> {
    let srv = seeded()?;
    let (status, body) = srv.post("/v1/agents/planner/restart", None, &json!({}))?;
    assert_eq!(status, 202);
    assert_eq!(body["agent"], "planner");
    assert_eq!(body["state"], "restarting");
    let restarts = srv
        .handles
        .fake_pty
        .restarts
        .lock()
        .map_err(|_| anyhow::anyhow!("poisoned"))?
        .clone();
    assert_eq!(restarts.len(), 1);
    assert_eq!(restarts[0].0, "planner");
    Ok(())
}

#[test]
fn roster_reports_the_blocked_flag() -> anyhow::Result<()> {
    let (ctx, handles) = support::test_ctx()?;
    handles
        .fake_pty
        .set_agent("builder", AgentState::Working, None, Vec::new());
    handles.fake_pty.set_blocked("builder", true);
    let srv = support::start(ctx, handles)?;
    let (_, body) = srv.get("/v1/agents", None)?;
    let agents = body["agents"].as_array().cloned().unwrap_or_default();
    assert_eq!(agents[0]["id"], "builder");
    assert_eq!(agents[0]["blocked"], true);
    assert_eq!(agents[1]["blocked"], false);
    let (_, detail) = srv.get("/v1/agents/builder", None)?;
    assert_eq!(detail["blocked"], true, "detail flattens info");
    Ok(())
}
