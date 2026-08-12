#![expect(
    clippy::panic_in_result_fn,
    reason = "tests assert inside Result-returning fns"
)]

mod support;

use support::{exit_code, record_json, serve, stderr, stdout, tempo};

#[test]
fn agent_ask_is_async_and_prints_id() -> anyhow::Result<()> {
    let srv = serve(vec![(
        201,
        record_json("m-a3f91c2e", "ask", "queued", None, None),
    )])?;
    let out = tempo(&["ask", "builder", "done?"], srv.port, Some("planner"))?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "m-a3f91c2e\n");
    let reqs = srv.requests();
    assert_eq!(reqs.len(), 1, "agent ask must not poll");
    assert_eq!(reqs[0].header("x-coretempo-agent"), Some("planner"));
    Ok(())
}

#[test]
fn human_ask_polls_wait_30_until_replied_and_prints_body() -> anyhow::Result<()> {
    let srv = serve(vec![
        (201, record_json("m-a3f91c2e", "ask", "queued", None, None)),
        (
            200,
            record_json("m-a3f91c2e", "ask", "injected", None, None),
        ),
        (
            200,
            record_json(
                "m-a3f91c2e",
                "ask",
                "replied",
                Some(0),
                Some("yes, shipped"),
            ),
        ),
    ])?;
    let out = tempo(&["ask", "builder", "done?"], srv.port, None)?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "yes, shipped\n");
    let reqs = srv.requests();
    assert_eq!(reqs.len(), 3);
    assert_eq!(reqs[1].path, "/v1/messages/m-a3f91c2e?wait=30");
    assert_eq!(reqs[2].path, "/v1/messages/m-a3f91c2e?wait=30");
    Ok(())
}

#[test]
fn reply_code_1_exits_1_failed_exits_2() -> anyhow::Result<()> {
    let srv = serve(vec![
        (201, record_json("m-a3f91c2e", "ask", "queued", None, None)),
        (
            200,
            record_json("m-a3f91c2e", "ask", "replied", Some(1), Some("broke")),
        ),
    ])?;
    let out = tempo(&["ask", "builder", "done?"], srv.port, None)?;
    assert_eq!(exit_code(&out), 1);
    assert_eq!(stdout(&out), "broke\n");
    let srv = serve(vec![
        (201, record_json("m-b7c2aaaa", "ask", "queued", None, None)),
        (200, record_json("m-b7c2aaaa", "ask", "failed", None, None)),
    ])?;
    let out = tempo(&["ask", "builder", "done?"], srv.port, None)?;
    assert_eq!(exit_code(&out), 2);
    assert!(
        stderr(&out).contains("m-b7c2aaaa"),
        "stderr: {}",
        stderr(&out)
    );
    Ok(())
}

#[test]
fn wait_flags_override_the_default() -> anyhow::Result<()> {
    // --no-wait as a human: single request, id printed.
    let srv = serve(vec![(
        201,
        record_json("m-a3f91c2e", "ask", "queued", None, None),
    )])?;
    let out = tempo(&["ask", "builder", "q", "--no-wait"], srv.port, None)?;
    assert_eq!(
        (exit_code(&out), stdout(&out).as_str()),
        (0, "m-a3f91c2e\n")
    );
    assert_eq!(srv.requests().len(), 1);
    // --wait as an agent: polls despite CORETEMPO_AGENT_ID.
    let srv = serve(vec![
        (201, record_json("m-a3f91c2e", "ask", "queued", None, None)),
        (
            200,
            record_json("m-a3f91c2e", "ask", "replied", Some(0), Some("ok")),
        ),
    ])?;
    let out = tempo(
        &["ask", "builder", "q", "--wait"],
        srv.port,
        Some("planner"),
    )?;
    assert_eq!((exit_code(&out), stdout(&out).as_str()), (0, "ok\n"));
    assert_eq!(srv.requests().len(), 2);
    // both flags together is a usage error (3).
    let out = tempo(&["ask", "builder", "q", "--wait", "--no-wait"], 1, None)?;
    assert_eq!(exit_code(&out), 3);
    Ok(())
}

#[test]
fn api_json_fallback_supplies_port_and_token() -> anyhow::Result<()> {
    let srv = serve(vec![(200, r#"{"agents":[]}"#.to_string())])?;
    let home = std::env::temp_dir().join(format!("tempo-home-{}-{}", std::process::id(), srv.port));
    let run_dir = home.join(".coretempo/runs/r-11223344");
    std::fs::create_dir_all(&run_dir)?;
    std::fs::write(
        run_dir.join("api.json"),
        format!(
            r#"{{"port":{},"token":"filetok","run_id":"r-11223344"}}"#,
            srv.port
        ),
    )?;
    std::os::unix::fs::symlink("r-11223344", home.join(".coretempo/runs/current"))?;
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tempo"))
        .arg("agents")
        .env_remove("CORETEMPO_PORT")
        .env_remove("CORETEMPO_TOKEN")
        .env_remove("CORETEMPO_AGENT_ID")
        .env("HOME", &home)
        .output()?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let reqs = srv.requests();
    assert_eq!(reqs[0].header("authorization"), Some("Bearer filetok"));
    Ok(())
}
