#![expect(
    clippy::panic_in_result_fn,
    reason = "tests assert inside Result-returning fns"
)]

mod support;

use support::{exit_code, serve, stderr, stdout, tempo, tempo_with_stdin};

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
    assert_eq!(
        body,
        serde_json::json!({"state": "working", "claude_session_id": null})
    );
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
    assert_eq!(
        body,
        serde_json::json!({"state": "idle", "claude_session_id": null})
    );
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

#[test]
fn state_blocked_forwards_the_tool_name_from_hook_stdin() -> anyhow::Result<()> {
    let srv = serve(vec![(
        200,
        r#"{"agent":"builder","state":"working"}"#.to_string(),
    )])?;
    let stdin = concat!(
        r#"{"hook_event_name":"PermissionRequest","tool_name":"Read","#,
        r#""tool_input":{"file_path":"/etc/hostname"}}"#
    );
    let out = tempo_with_stdin(&["state", "blocked"], srv.port, Some("builder"), stdin)?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let reqs = srv.requests();
    let body: serde_json::Value = serde_json::from_str(&reqs[0].body)?;
    assert_eq!(
        body,
        serde_json::json!({
            "state": "blocked", "tool": "Read", "agent_id": null, "claude_session_id": null
        }),
        "a main-session payload carries no agent_id"
    );
    Ok(())
}

/// A subagent's dialog fires the parent's `PermissionRequest` hook with the
/// subagent's `agent_id`; the server needs it to scope the later clear
/// (live payload 2026-08-18).
#[test]
fn state_blocked_forwards_the_hook_agent_id() -> anyhow::Result<()> {
    let srv = serve(vec![(
        200,
        r#"{"agent":"builder","state":"idle"}"#.to_string(),
    )])?;
    let stdin = concat!(
        r#"{"hook_event_name":"PermissionRequest","agent_id":"a9c81c1e4a5cf2bbe","#,
        r#""tool_name":"Bash","tool_input":{"command":"ls"}}"#
    );
    let out = tempo_with_stdin(&["state", "blocked"], srv.port, Some("builder"), stdin)?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let body: serde_json::Value = serde_json::from_str(&srv.requests()[0].body)?;
    assert_eq!(
        body,
        serde_json::json!({
            "state": "blocked",
            "tool": "Bash",
            "agent_id": "a9c81c1e4a5cf2bbe",
            "claude_session_id": null,
        })
    );
    Ok(())
}

#[test]
fn state_blocked_tolerates_missing_or_invalid_stdin() -> anyhow::Result<()> {
    let srv = serve(vec![
        (200, r#"{"agent":"builder","state":"working"}"#.to_string()),
        (200, r#"{"agent":"builder","state":"working"}"#.to_string()),
    ])?;
    let out = tempo_with_stdin(&["state", "blocked"], srv.port, Some("builder"), "not json")?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let out = tempo_with_stdin(&["state", "blocked"], srv.port, Some("builder"), "")?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    for req in srv.requests() {
        let body: serde_json::Value = serde_json::from_str(&req.body)?;
        assert_eq!(
            body,
            serde_json::json!({
                "state": "blocked", "tool": null, "agent_id": null, "claude_session_id": null
            })
        );
    }
    Ok(())
}

/// `unblocked` carries the reporting agent's id and nothing else: a helper
/// agent's `PostToolBatch` names a tool it never ran, so forwarding `tool_name`
/// here would be a lie, but the server needs `agent_id` to refuse the clear.
#[test]
fn state_unblocked_forwards_only_the_hook_agent_id() -> anyhow::Result<()> {
    let srv = serve(vec![
        (200, r#"{"agent":"builder","state":"working"}"#.to_string()),
        (200, r#"{"agent":"builder","state":"working"}"#.to_string()),
    ])?;
    let helper = concat!(
        r#"{"hook_event_name":"PostToolBatch","agent_id":"ac3cef2916066bf6d","#,
        r#""tool_name":"X","tool_response":"No tools needed for summary"}"#
    );
    let out = tempo_with_stdin(&["state", "unblocked"], srv.port, Some("builder"), helper)?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let out = tempo_with_stdin(
        &["state", "unblocked"],
        srv.port,
        Some("builder"),
        r#"{"hook_event_name":"PostToolBatch","tool_name":"X"}"#,
    )?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let reqs = srv.requests();
    let first: serde_json::Value = serde_json::from_str(&reqs[0].body)?;
    assert_eq!(
        first,
        serde_json::json!({
            "state": "unblocked", "agent_id": "ac3cef2916066bf6d", "claude_session_id": null
        })
    );
    let second: serde_json::Value = serde_json::from_str(&reqs[1].body)?;
    assert_eq!(
        second,
        serde_json::json!({
            "state": "unblocked", "agent_id": null, "claude_session_id": null
        }),
        "a main-session payload has no agent_id"
    );
    Ok(())
}

/// The hook payload Claude Code pipes to a `PermissionRequest` hook.
const BASH_PROMPT: &str = concat!(
    r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash","#,
    r#""tool_input":{"command":"mkdir probe"},"permission_mode":"default"}"#
);

fn decision(stdout: &str) -> anyhow::Result<serde_json::Value> {
    let parsed: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|e| anyhow::anyhow!("stdout is not JSON ({e}): {stdout}"))?;
    Ok(parsed["hookSpecificOutput"].clone())
}

#[test]
fn state_refused_prints_a_deny_decision_and_reports_the_tool() -> anyhow::Result<()> {
    let srv = serve(vec![(
        200,
        r#"{"agent":"builder","state":"working"}"#.to_string(),
    )])?;
    let out = tempo_with_stdin(
        &["state", "refused"],
        srv.port,
        Some("builder"),
        BASH_PROMPT,
    )?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let decision = decision(&stdout(&out))?;
    assert_eq!(decision["hookEventName"], "PermissionRequest");
    assert_eq!(decision["decision"]["behavior"], "deny");
    let message = decision["decision"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("Bash"),
        "names the refused tool: {message}"
    );
    assert!(message.contains("allow"), "points at the fix: {message}");
    let reqs = srv.requests();
    assert_eq!(reqs[0].path, "/v1/agents/builder/state");
    let body: serde_json::Value = serde_json::from_str(&reqs[0].body)?;
    assert_eq!(
        body,
        serde_json::json!({
            "state": "refused", "tool": "Bash", "input": "mkdir probe", "agent_id": null,
            "claude_session_id": null
        }),
        "a Bash refusal carries the command"
    );
    Ok(())
}

#[test]
fn state_refused_summarises_the_input_per_tool() -> anyhow::Result<()> {
    let srv = serve(vec![
        (200, r#"{"agent":"builder","state":"working"}"#.to_string()),
        (200, r#"{"agent":"builder","state":"working"}"#.to_string()),
        (200, r#"{"agent":"builder","state":"working"}"#.to_string()),
    ])?;
    let read = concat!(
        r#"{"hook_event_name":"PermissionRequest","tool_name":"Read","#,
        r#""tool_input":{"file_path":"/etc/passwd","limit":10}}"#
    );
    let mcp = concat!(
        r#"{"hook_event_name":"PermissionRequest","tool_name":"mcp__gh__create_issue","#,
        r#""tool_input":{"title":"t","body":"b"}}"#
    );
    let long = format!(
        r#"{{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{{"command":"{}"}}}}"#,
        "x".repeat(500)
    );
    for payload in [read, mcp, long.as_str()] {
        let out = tempo_with_stdin(&["state", "refused"], srv.port, Some("builder"), payload)?;
        assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    }
    let reqs = srv.requests();
    let inputs: Vec<serde_json::Value> = reqs
        .iter()
        .map(|r| serde_json::from_str::<serde_json::Value>(&r.body).map(|b| b["input"].clone()))
        .collect::<Result<_, _>>()?;
    assert_eq!(inputs[0], "/etc/passwd", "file tools carry the path");
    assert_eq!(
        inputs[1], r#"{"body":"b","title":"t"}"#,
        "other tools carry their input as compact JSON"
    );
    let capped = inputs[2].as_str().unwrap_or_default();
    assert!(
        capped.len() <= 200 && capped.ends_with('…'),
        "capped: {}",
        capped.len()
    );
    Ok(())
}

#[test]
fn state_refused_still_denies_when_the_server_is_unreachable() -> anyhow::Result<()> {
    let srv = serve(vec![])?; // accepts nothing: the report cannot land
    let out = tempo_with_stdin(
        &["state", "refused"],
        srv.port,
        Some("builder"),
        BASH_PROMPT,
    )?;
    assert_eq!(
        exit_code(&out),
        0,
        "a hook must exit 0 for its decision to count"
    );
    assert_eq!(decision(&stdout(&out))?["decision"]["behavior"], "deny");
    assert!(
        stderr(&out).contains("could not report"),
        "the failed report is mentioned: {}",
        stderr(&out)
    );
    Ok(())
}

#[test]
fn state_refused_without_a_payload_still_denies() -> anyhow::Result<()> {
    let srv = serve(vec![(
        200,
        r#"{"agent":"builder","state":"working"}"#.to_string(),
    )])?;
    let out = tempo_with_stdin(&["state", "refused"], srv.port, Some("builder"), "")?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(decision(&stdout(&out))?["decision"]["behavior"], "deny");
    Ok(())
}

#[test]
fn state_forwards_the_hook_payloads_session_id() -> anyhow::Result<()> {
    let srv = serve(vec![(
        200,
        r#"{"agent":"s-1f2e3d4c","state":"idle"}"#.to_string(),
    )])?;
    let stdin = r#"{"hook_event_name":"SessionStart","session_id":"0f9c1b2a-4d5e","cwd":"/w"}"#;
    let out = tempo_with_stdin(&["state", "idle"], srv.port, Some("s-1f2e3d4c"), stdin)?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let reqs = srv.requests();
    let body: serde_json::Value = serde_json::from_str(&reqs[0].body)?;
    assert_eq!(body["state"], "idle");
    assert_eq!(body["claude_session_id"], "0f9c1b2a-4d5e");
    Ok(())
}

#[test]
fn state_without_a_payload_sends_a_null_session_id() -> anyhow::Result<()> {
    let srv = serve(vec![(
        200,
        r#"{"agent":"builder","state":"working"}"#.to_string(),
    )])?;
    let out = tempo(&["state", "working"], srv.port, Some("builder"))?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let body: serde_json::Value = serde_json::from_str(&srv.requests()[0].body)?;
    assert_eq!(body["claude_session_id"], serde_json::Value::Null);
    Ok(())
}
