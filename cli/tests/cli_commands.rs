#![expect(
    clippy::panic_in_result_fn,
    reason = "tests assert inside Result-returning fns"
)]

mod support;

use support::{exit_code, record_json, serve, stderr, stdout, tempo};

#[test]
fn agents_prints_tab_separated_roster() -> anyhow::Result<()> {
    let srv = serve(vec![(
        200,
        r#"{"agents":[
        {"id":"builder","state":"idle","pending_asks":0,"exit_code":null},
        {"id":"planner","state":"working","pending_asks":2,"exit_code":null}]}"#
            .replace('\n', "")
            .replace("    ", ""),
    )])?;
    let out = tempo(&["agents"], srv.port, None)?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "builder\tidle\t0\nplanner\tworking\t2\n");
    let reqs = srv.requests();
    assert_eq!(reqs[0].path, "/v1/agents");
    assert_eq!(
        reqs[0].header("authorization"),
        Some(format!("Bearer {}", "t".repeat(64)).as_str())
    );
    Ok(())
}

#[test]
fn send_posts_and_prints_message_id() -> anyhow::Result<()> {
    let srv = serve(vec![(
        201,
        record_json("m-a3f91c2e", "send", "queued", None, None),
    )])?;
    let out = tempo(&["send", "builder", "build it"], srv.port, Some("planner"))?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "m-a3f91c2e\n");
    let reqs = srv.requests();
    assert_eq!(
        (reqs[0].method.as_str(), reqs[0].path.as_str()),
        ("POST", "/v1/messages")
    );
    assert_eq!(reqs[0].header("x-coretempo-agent"), Some("planner"));
    let body: serde_json::Value = serde_json::from_str(&reqs[0].body)?;
    assert_eq!(
        body,
        serde_json::json!({"to":"builder","kind":"send","body":"build it"})
    );
    Ok(())
}

#[test]
fn reply_posts_code_and_body() -> anyhow::Result<()> {
    let srv = serve(vec![(
        200,
        record_json("m-a3f91c2e", "ask", "replied", Some(1), Some("no")),
    )])?;
    let out = tempo(
        &["reply", "m-a3f91c2e", "--code", "1", "did not work"],
        srv.port,
        Some("builder"),
    )?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let reqs = srv.requests();
    assert_eq!(reqs[0].path, "/v1/messages/m-a3f91c2e/reply");
    let body: serde_json::Value = serde_json::from_str(&reqs[0].body)?;
    assert_eq!(body, serde_json::json!({"code":1,"body":"did not work"}));
    Ok(())
}

#[test]
fn reply_code_out_of_range_is_usage_error_3() -> anyhow::Result<()> {
    let out = tempo(&["reply", "m-a3f91c2e", "--code", "2", "x"], 1, None)?;
    assert_eq!(exit_code(&out), 3);
    Ok(())
}

#[test]
fn status_prints_record_json_and_caps_wait() -> anyhow::Result<()> {
    let rec = record_json("m-a3f91c2e", "ask", "working", None, None);
    let srv = serve(vec![(200, rec.clone()), (200, rec.clone())])?;
    let out = tempo(&["status", "m-a3f91c2e"], srv.port, None)?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let parsed: serde_json::Value = serde_json::from_str(stdout(&out).trim())?;
    assert_eq!(parsed["id"], "m-a3f91c2e");
    let out = tempo(&["status", "m-a3f91c2e", "--wait", "999"], srv.port, None)?;
    assert_eq!(exit_code(&out), 0);
    let reqs = srv.requests();
    assert_eq!(reqs[0].path, "/v1/messages/m-a3f91c2e");
    assert_eq!(reqs[1].path, "/v1/messages/m-a3f91c2e?wait=300");
    Ok(())
}

#[test]
fn api_error_message_is_printed_verbatim_exit_3() -> anyhow::Result<()> {
    let msg = "no agent named 'buidler'; roster: planner, builder";
    let srv = serve(vec![(
        404,
        format!(r#"{{"error":{{"code":"unknown_agent","message":"{msg}"}}}}"#),
    )])?;
    let out = tempo(&["send", "buidler", "hi"], srv.port, Some("planner"))?;
    assert_eq!(exit_code(&out), 3);
    assert!(stderr(&out).contains(msg), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "");
    Ok(())
}

#[test]
fn connection_refused_is_exit_3() -> anyhow::Result<()> {
    // Bind then drop a listener so the port is closed.
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let out = tempo(&["agents"], port, None)?;
    assert_eq!(exit_code(&out), 3);
    assert!(!stderr(&out).is_empty());
    Ok(())
}

#[test]
fn done_posts_loop_done_for_the_calling_agent() -> anyhow::Result<()> {
    let srv = serve(vec![(
        200,
        r#"{"owner":"planner","target":"builder","loop":"done"}"#.to_string(),
    )])?;
    let out = tempo(&["done", "builder"], srv.port, Some("planner"))?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let reqs = srv.requests();
    assert_eq!(reqs[0].path, "/v1/agents/builder/loop-done");
    assert_eq!(reqs[0].header("x-coretempo-agent"), Some("planner"));
    Ok(())
}

#[test]
fn done_outside_an_agent_session_is_an_error() -> anyhow::Result<()> {
    let srv = serve(Vec::new())?;
    let out = tempo(&["done", "builder"], srv.port, None)?;
    assert_ne!(exit_code(&out), 0);
    assert!(
        stderr(&out).contains("CORETEMPO_AGENT_ID"),
        "error explains the missing identity: {}",
        stderr(&out)
    );
    Ok(())
}

#[test]
fn reply_json_file_posts_the_file_contents() -> anyhow::Result<()> {
    let srv = serve(vec![(
        200,
        r#"{"id":"m-1","status":"replied"}"#.to_string(),
    )])?;
    let dir = std::env::temp_dir().join(format!("tempo-reply-json-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("reply.json");
    std::fs::write(&path, r#"{"name":"x"}"#)?;
    let path_str = path.to_str().expect("temp path is valid utf-8");
    let out = tempo(
        &["reply", "m-1", "--code", "0", "--json-file", path_str],
        srv.port,
        Some("builder"),
    )?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let reqs = srv.requests();
    assert_eq!(reqs[0].path, "/v1/messages/m-1/reply");
    let body: serde_json::Value = serde_json::from_str(&reqs[0].body)?;
    assert_eq!(body["body"], r#"{"name":"x"}"#);
    Ok(())
}

#[test]
fn reply_rejects_both_message_and_json_file() -> anyhow::Result<()> {
    let srv = serve(Vec::new())?;
    let out = tempo(
        &[
            "reply",
            "m-1",
            "--code",
            "0",
            "inline body",
            "--json-file",
            "/nonexistent/reply.json",
        ],
        srv.port,
        Some("builder"),
    )?;
    assert_eq!(exit_code(&out), 3);
    assert!(
        stderr(&out).contains("json-file") || stderr(&out).contains("json_file"),
        "stderr mentions the conflict: {}",
        stderr(&out)
    );
    assert_eq!(srv.requests().len(), 0);
    Ok(())
}

#[test]
fn reply_rejects_neither_body_source() -> anyhow::Result<()> {
    let srv = serve(Vec::new())?;
    let out = tempo(&["reply", "m-1", "--code", "0"], srv.port, Some("builder"))?;
    assert_eq!(exit_code(&out), 3);
    assert_eq!(srv.requests().len(), 0);
    Ok(())
}

#[test]
fn schema_rejection_text_reaches_stderr() -> anyhow::Result<()> {
    let msg = "tempo reply rejected: at /name: required";
    let srv = serve(vec![(
        422,
        format!(r#"{{"error":{{"code":"schema_validation_failed","message":"{msg}"}}}}"#),
    )])?;
    let out = tempo(
        &["reply", "m-1", "--code", "0", "{}"],
        srv.port,
        Some("builder"),
    )?;
    assert_ne!(exit_code(&out), 0);
    assert!(stderr(&out).contains(msg), "stderr: {}", stderr(&out));
    Ok(())
}

#[test]
fn missing_json_file_errors_before_any_request() -> anyhow::Result<()> {
    let srv = serve(Vec::new())?;
    let out = tempo(
        &[
            "reply",
            "m-1",
            "--code",
            "0",
            "--json-file",
            "/nonexistent/reply.json",
        ],
        srv.port,
        Some("builder"),
    )?;
    assert_ne!(exit_code(&out), 0);
    assert!(
        stderr(&out).contains("/nonexistent/reply.json"),
        "stderr names the path: {}",
        stderr(&out)
    );
    assert_eq!(srv.requests().len(), 0);
    Ok(())
}
