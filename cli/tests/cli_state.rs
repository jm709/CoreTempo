#![expect(
    clippy::panic_in_result_fn,
    reason = "tests assert inside Result-returning fns"
)]

mod support;

use support::{exit_code, serve, stderr, stdout, tempo};

#[test]
fn state_posts_the_reported_state_for_the_calling_agent() -> anyhow::Result<()> {
    let srv = serve(vec![(
        200,
        r#"{"agent":"builder","state":"working"}"#.to_string(),
    )])?;
    let out = tempo(&["state", "working"], srv.port, Some("builder"))?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "");
    let reqs = srv.requests();
    assert_eq!(
        (reqs[0].method.as_str(), reqs[0].path.as_str()),
        ("POST", "/v1/agents/builder/state")
    );
    assert_eq!(reqs[0].header("x-coretempo-agent"), Some("builder"));
    let body: serde_json::Value = serde_json::from_str(&reqs[0].body)?;
    assert_eq!(body, serde_json::json!({"state": "working"}));
    Ok(())
}

#[test]
fn state_idle_is_accepted() -> anyhow::Result<()> {
    let srv = serve(vec![(
        200,
        r#"{"agent":"planner","state":"idle"}"#.to_string(),
    )])?;
    let out = tempo(&["state", "idle"], srv.port, Some("planner"))?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let reqs = srv.requests();
    assert_eq!(reqs[0].path, "/v1/agents/planner/state");
    let body: serde_json::Value = serde_json::from_str(&reqs[0].body)?;
    assert_eq!(body, serde_json::json!({"state": "idle"}));
    Ok(())
}

#[test]
fn state_without_agent_id_errors_without_calling_the_server() -> anyhow::Result<()> {
    // Bind then drop a listener so any request would fail loudly.
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let out = tempo(&["state", "working"], port, None)?;
    assert_eq!(exit_code(&out), 3);
    let err = stderr(&out);
    assert!(err.contains("CORETEMPO_AGENT_ID"), "stderr: {err}");
    assert_eq!(stdout(&out), "");
    Ok(())
}

#[test]
fn unknown_state_word_is_a_usage_error_3() -> anyhow::Result<()> {
    let out = tempo(&["state", "busy"], 1, Some("builder"))?;
    assert_eq!(exit_code(&out), 3);
    Ok(())
}
