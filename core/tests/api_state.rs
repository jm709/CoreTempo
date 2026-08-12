#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]

mod support;

use serde_json::json;

#[test]
fn agent_reports_its_own_state_200() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let (status, body) = srv.post(
        "/v1/agents/builder/state",
        Some("builder"),
        &json!({"state": "working"}),
    )?;
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(body["agent"], "builder");
    assert_eq!(body["state"], "working");
    let (status, body) = srv.get("/v1/agents/builder", None)?;
    assert_eq!((status, &body["state"]), (200, &json!("working")));

    let (status, body) = srv.post(
        "/v1/agents/builder/state",
        Some("builder"),
        &json!({"state": "idle"}),
    )?;
    assert_eq!((status, &body["state"]), (200, &json!("idle")));
    let (_, body) = srv.get("/v1/agents/builder", None)?;
    assert_eq!(body["state"], "idle");
    Ok(())
}

#[test]
fn reporting_for_another_agent_is_403_wrong_agent() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let (status, body) = srv.post(
        "/v1/agents/builder/state",
        Some("planner"),
        &json!({"state": "working"}),
    )?;
    assert_eq!(
        (status, body["error"]["code"].as_str().unwrap_or_default()),
        (403, "wrong_agent")
    );
    // A caller with no agent identity (human/script) is not the agent either.
    let (status, body) = srv.post("/v1/agents/builder/state", None, &json!({"state": "idle"}))?;
    assert_eq!(
        (status, body["error"]["code"].as_str().unwrap_or_default()),
        (403, "wrong_agent")
    );
    // The reports were rejected, so the state never moved.
    let (_, body) = srv.get("/v1/agents/builder", None)?;
    assert_eq!(body["state"], "idle");
    Ok(())
}

#[test]
fn reporting_for_an_unknown_agent_is_404() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let (status, body) = srv.post(
        "/v1/agents/ghost/state",
        Some("builder"),
        &json!({"state": "working"}),
    )?;
    assert_eq!(
        (status, body["error"]["code"].as_str().unwrap_or_default()),
        (404, "unknown_agent")
    );
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("roster"), "message: {message}");
    Ok(())
}

#[test]
fn unparseable_state_is_400() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    for body in [
        json!({"state": "starting"}),
        json!({"state": "busy"}),
        json!({"state": 7}),
        json!({}),
    ] {
        let (status, answer) = srv.post("/v1/agents/builder/state", Some("builder"), &body)?;
        assert_eq!(
            (status, answer["error"]["code"].as_str().unwrap_or_default()),
            (400, "invalid_request"),
            "body: {body}"
        );
        let message = answer["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("working"), "message: {message}");
    }
    Ok(())
}
