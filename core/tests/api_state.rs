#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]

mod support;

use coretempo_core::types::AgentId;
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
fn unparseable_state_is_400_naming_every_reported_word() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    for body in [
        json!({"state": "starting"}),
        json!({"state": "busy"}),
        json!({"state": "asleep"}),
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
        for word in ["working", "idle", "blocked", "unblocked"] {
            assert!(message.contains(word), "{word} missing from: {message}");
        }
    }
    Ok(())
}

/// `blocked` in the roster comes from the `PtySource`, so this reads the whole
/// list the UI reads rather than the flag the fake stores.
fn roster_blocked(srv: &support::TestServer, id: &str) -> anyhow::Result<bool> {
    let (status, body) = srv.get("/v1/agents", None)?;
    assert_eq!(status, 200, "body: {body}");
    let agents = body["agents"].as_array().cloned().unwrap_or_default();
    let agent = agents
        .iter()
        .find(|a| a["id"] == json!(id))
        .cloned()
        .unwrap_or(json!({}));
    Ok(agent["blocked"] == json!(true))
}

#[test]
fn blocked_and_unblocked_reports_flag_the_agent() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let (status, body) = srv.post(
        "/v1/agents/builder/state",
        Some("builder"),
        &json!({"state": "blocked", "tool": "Read"}),
    )?;
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(body["agent"], "builder");
    // blocked is orthogonal to raw state: the turn is still open.
    assert_eq!(body["state"], "idle");
    assert_eq!(
        srv.handles.fake_pty.blocked_reports(),
        vec![(AgentId("builder".into()), Some("Read".into()))],
        "the tool name from the hook reaches the PtySource"
    );
    assert!(roster_blocked(&srv, "builder")?);

    let (status, body) = srv.post(
        "/v1/agents/builder/state",
        Some("builder"),
        &json!({"state": "unblocked"}),
    )?;
    assert_eq!(status, 200, "body: {body}");
    assert!(!roster_blocked(&srv, "builder")?);
    Ok(())
}

/// A `PermissionRequest` payload with no `tool_name` forwards an empty string
/// rather than omitting the field; the roster badge must not title itself "  ".
#[test]
fn a_blank_tool_is_normalised_to_none() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    for tool in [json!("  "), json!("")] {
        let (status, body) = srv.post(
            "/v1/agents/builder/state",
            Some("builder"),
            &json!({"state": "blocked", "tool": tool}),
        )?;
        assert_eq!(status, 200, "body: {body}");
    }
    assert_eq!(
        srv.handles.fake_pty.blocked_reports(),
        vec![
            (AgentId("builder".into()), None),
            (AgentId("builder".into()), None)
        ]
    );
    Ok(())
}

/// A tool name past 128 chars (a hostile or malformed hook payload) is stored
/// truncated rather than growing the roster response without bound.
#[test]
fn a_long_tool_name_is_truncated_to_128_chars() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let long_tool = "x".repeat(300);
    let (status, body) = srv.post(
        "/v1/agents/builder/state",
        Some("builder"),
        &json!({"state": "blocked", "tool": long_tool}),
    )?;
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(
        srv.handles.fake_pty.blocked_reports(),
        vec![(AgentId("builder".into()), Some("x".repeat(128)))]
    );
    Ok(())
}

/// The hook payload's `agent_id` must survive the handler on both reports, or
/// the pty cannot tell a sibling helper agent's `PostToolBatch` from the one
/// that owns the dialog. Blank normalises to `None` (a hook with no value sends
/// `""`) and an over-long value is capped.
#[test]
fn the_hook_agent_id_reaches_the_pty_on_both_reports() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    for agent_id in [
        json!("a9c81c1e4a5cf2bbe"),
        json!("  "),
        json!("z".repeat(200)),
    ] {
        let (status, body) = srv.post(
            "/v1/agents/builder/state",
            Some("builder"),
            &json!({"state": "blocked", "tool": "Bash", "agent_id": agent_id}),
        )?;
        assert_eq!(status, 200, "body: {body}");
    }
    // No agent_id field at all: the main session's payload shape.
    let (status, body) = srv.post(
        "/v1/agents/builder/state",
        Some("builder"),
        &json!({"state": "blocked", "tool": "Bash"}),
    )?;
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(
        srv.handles.fake_pty.blocked_agent_ids(),
        vec![
            Some("a9c81c1e4a5cf2bbe".to_string()),
            None,
            Some("z".repeat(64)),
            None
        ]
    );

    for agent_id in [json!("ac3cef2916066bf6d"), json!(null)] {
        let (status, body) = srv.post(
            "/v1/agents/builder/state",
            Some("builder"),
            &json!({"state": "unblocked", "agent_id": agent_id}),
        )?;
        assert_eq!(status, 200, "body: {body}");
    }
    assert_eq!(
        srv.handles.fake_pty.unblocked_reports(),
        vec![
            (
                AgentId("builder".into()),
                Some("ac3cef2916066bf6d".to_string())
            ),
            (AgentId("builder".into()), None),
        ]
    );
    Ok(())
}

#[test]
fn only_the_agent_itself_may_report_blocked() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let (status, body) = srv.post(
        "/v1/agents/builder/state",
        Some("planner"),
        &json!({"state": "blocked", "tool": "Read"}),
    )?;
    assert_eq!(
        (status, body["error"]["code"].as_str().unwrap_or_default()),
        (403, "wrong_agent")
    );
    assert!(srv.handles.fake_pty.blocked_reports().is_empty());
    assert!(!roster_blocked(&srv, "builder")?);
    Ok(())
}

#[test]
fn refused_report_reaches_the_pty_source_without_flagging_the_agent() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let (status, body) = srv.post(
        "/v1/agents/builder/state",
        Some("builder"),
        &json!({"state": "refused", "tool": "Bash", "input": "python3 -c 'print(1)'"}),
    )?;
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(body["agent"], "builder");
    assert_eq!(
        body["state"], "idle",
        "a refusal leaves the raw state alone"
    );
    assert_eq!(
        srv.handles.fake_pty.refused_reports(),
        vec![(
            AgentId("builder".into()),
            Some("Bash".into()),
            Some("python3 -c 'print(1)'".into())
        )],
        "tool and input both reach the PtySource"
    );
    assert!(srv.handles.fake_pty.blocked_reports().is_empty());
    assert!(
        !roster_blocked(&srv, "builder")?,
        "nothing is waiting on a dialog"
    );
    Ok(())
}

#[test]
fn only_the_agent_itself_may_report_refused() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let (status, body) = srv.post(
        "/v1/agents/builder/state",
        Some("planner"),
        &json!({"state": "refused", "tool": "Bash"}),
    )?;
    assert_eq!(
        (status, body["error"]["code"].as_str().unwrap_or_default()),
        (403, "wrong_agent")
    );
    assert!(srv.handles.fake_pty.refused_reports().is_empty());
    Ok(())
}
