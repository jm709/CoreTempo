#![expect(
    clippy::panic_in_result_fn,
    reason = "tests assert inside Result-returning fns"
)]

mod support;

use std::path::PathBuf;

use support::{exit_code, serve, stderr, stdout};

const PROJECTS_JSON: &str =
    r#"[{"id":"p-1","path":"/w/proj","name":"proj","created_at":"2026-08-27T10:00:00Z"}]"#;

/// `tempo session …` against the stub with a sessions api.json under a
/// scratch root, and CORETEMPO_* set to nonsense so discovery is proven to
/// ignore it.
fn tempo_session(root: &std::path::Path, args: &[&str]) -> anyhow::Result<std::process::Output> {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_tempo"));
    cmd.arg("session")
        .arg("--root")
        .arg(root)
        .args(args)
        .env("CORETEMPO_PORT", "1")
        .env("CORETEMPO_TOKEN", "wrong")
        .env("CORETEMPO_AGENT_ID", "planner")
        .stdin(std::process::Stdio::null());
    Ok(cmd.output()?)
}

fn root_with_api(port: u16) -> anyhow::Result<PathBuf> {
    let root = std::env::temp_dir().join(format!("tempo-session-{}-{port}", std::process::id()));
    std::fs::create_dir_all(&root)?;
    std::fs::write(
        root.join("api.json"),
        format!(
            r#"{{"port":{port},"token":"{}","pid":{}}}"#,
            "t".repeat(64),
            std::process::id()
        ),
    )?;
    Ok(root)
}

fn session_json(id: &str, state: &str, branch: Option<&str>, blocked: bool) -> String {
    let branch = branch.map_or("null".to_string(), |b| format!("\"{b}\""));
    let blocked = if blocked {
        r#"{"tool":"Bash","since":"2026-08-27T10:00:00Z"}"#
    } else {
        "null"
    };
    format!(
        r#"{{"id":"{id}","project":"p-1","cwd":"/w/proj","worktree":null,"title":"fix it",
            "claude_session_id":null,"model":null,"permission_mode":null,"isolated_config":false,
            "prompt":null,"created_at":"2026-08-27T10:00:00Z","stopped_at":null,"state":"{state}",
            "blocked":{blocked},"exit":null,"pty_cursor":0,"branch":{branch},"changed_files":2,
            "ahead":null,"worktree_status":"none"}}"#
    )
}

#[test]
fn discovery_ignores_the_run_environment_and_uses_the_sessions_api_file() -> anyhow::Result<()> {
    let srv = serve(vec![
        (200, PROJECTS_JSON.to_string()),
        (
            200,
            format!("[{}]", session_json("s-1", "working", Some("main"), true)),
        ),
    ])?;
    let root = root_with_api(srv.port)?;
    let out = tempo_session(&root, &["list"])?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "s-1\tproj\tmain\tblocked\t2\t-\tfix it\n");
    let reqs = srv.requests();
    assert_eq!(
        reqs[0].header("authorization"),
        Some(&*format!("Bearer {}", "t".repeat(64)))
    );
    assert_eq!(
        reqs[0].header("x-coretempo-agent"),
        None,
        "never an agent identity"
    );
    Ok(())
}

#[test]
fn no_daemon_is_a_clear_message() -> anyhow::Result<()> {
    let root = std::env::temp_dir().join(format!("tempo-session-none-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let out = tempo_session(&root, &["list"])?;
    assert_eq!(exit_code(&out), 3);
    assert!(
        stderr(&out).contains("no session daemon running"),
        "{}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("coretempod sessions"),
        "{}",
        stderr(&out)
    );
    // A dead pid counts as no daemon.
    std::fs::create_dir_all(&root)?;
    std::fs::write(
        root.join("api.json"),
        r#"{"port":1,"token":"t","pid":2147483646}"#,
    )?;
    let out = tempo_session(&root, &["list"])?;
    assert_eq!(exit_code(&out), 3);
    assert!(
        stderr(&out).contains("no session daemon running"),
        "{}",
        stderr(&out)
    );
    Ok(())
}

#[test]
fn new_registers_the_project_when_unknown_and_prints_id_and_branch() -> anyhow::Result<()> {
    let repo = std::env::temp_dir().join(format!("tempo-session-new-{}", std::process::id()));
    std::fs::create_dir_all(&repo)?;
    let canonical = std::fs::canonicalize(&repo)?;
    let mut view = session_json("s-2", "starting", Some("session/brisk-otter-3f1a"), false);
    view = view.replace(
        r#""worktree":null"#,
        r#""worktree":{"path":"/w/wt","branch":"session/brisk-otter-3f1a","base":"abc"}"#,
    );
    let srv = serve(vec![
        (200, "[]".into()),
        (
            201,
            format!(
                r#"{{"id":"p-9","path":"{}","name":"x","created_at":"2026-08-27T10:00:00Z"}}"#,
                canonical.display()
            ),
        ),
        (201, view),
    ])?;
    let root = root_with_api(srv.port)?;
    let repo_arg = repo.display().to_string();
    let out = tempo_session(
        &root,
        &[
            "new",
            repo_arg.as_str(),
            "--worktree",
            "--prompt",
            "fix it",
            "--model",
            "haiku",
        ],
    )?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "s-2\nsession/brisk-otter-3f1a\n");
    let reqs = srv.requests();
    assert_eq!(
        (reqs[0].method.as_str(), reqs[0].path.as_str()),
        ("GET", "/v1/projects")
    );
    assert_eq!(
        (reqs[1].method.as_str(), reqs[1].path.as_str()),
        ("POST", "/v1/projects")
    );
    let registered: serde_json::Value = serde_json::from_str(&reqs[1].body)?;
    assert_eq!(registered["path"], canonical.display().to_string());
    let created: serde_json::Value = serde_json::from_str(&reqs[2].body)?;
    assert_eq!(
        created,
        serde_json::json!({
            "project": "p-9", "worktree": true, "cwd": null, "title": null, "prompt": "fix it",
            "model": "haiku", "permission_mode": null, "isolated_config": false
        })
    );
    Ok(())
}

#[test]
fn stop_resume_rm_and_show_map_to_their_routes() -> anyhow::Result<()> {
    let srv = serve(vec![
        (200, session_json("s-1", "stopped", None, false)),
        (
            200,
            format!(
                r#"{{"session":{},"resumed":true}}"#,
                session_json("s-1", "starting", None, false).replace(
                    r#""claude_session_id":null"#,
                    r#""claude_session_id":"0f9c""#
                )
            ),
        ),
        (200, r#"{"branch_kept":true}"#.into()),
        (200, session_json("s-1", "idle", None, false)),
    ])?;
    let root = root_with_api(srv.port)?;
    let out = tempo_session(&root, &["stop", "s-1"])?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let out = tempo_session(&root, &["resume", "s-1"])?;
    assert_eq!(stdout(&out), "resumed conversation 0f9c\n");
    let out = tempo_session(&root, &["rm", "s-1", "--remove-worktree", "--force"])?;
    assert_eq!(stdout(&out), "branch kept\n");
    let out = tempo_session(&root, &["show", "s-1"])?;
    let shown: serde_json::Value = serde_json::from_str(&stdout(&out))?;
    assert_eq!(shown["state"], "idle");
    let reqs = srv.requests();
    let calls: Vec<(String, String)> = reqs
        .iter()
        .map(|r| (r.method.clone(), r.path.clone()))
        .collect();
    assert_eq!(
        calls,
        [
            ("POST".to_string(), "/v1/sessions/s-1/stop".to_string()),
            ("POST".to_string(), "/v1/sessions/s-1/resume".to_string()),
            (
                "DELETE".to_string(),
                "/v1/sessions/s-1?remove_worktree=true&force=true".to_string()
            ),
            ("GET".to_string(), "/v1/sessions/s-1".to_string()),
        ]
    );
    Ok(())
}

#[test]
fn an_api_refusal_prints_the_servers_message_and_exits_3() -> anyhow::Result<()> {
    let srv = serve(vec![(
        409,
        concat!(
            r#"{"error":{"code":"wrong_state","message":"session 's-1' is Idle "#,
            r#"— cannot resume; valid now: stop"}}"#
        )
        .into(),
    )])?;
    let root = root_with_api(srv.port)?;
    let out = tempo_session(&root, &["resume", "s-1"])?;
    assert_eq!(exit_code(&out), 3);
    assert!(stderr(&out).contains("valid now: stop"), "{}", stderr(&out));
    Ok(())
}

#[test]
fn projects_lists_and_forgets() -> anyhow::Result<()> {
    let srv = serve(vec![(200, PROJECTS_JSON.to_string()), (204, String::new())])?;
    let root = root_with_api(srv.port)?;
    let out = tempo_session(&root, &["projects"])?;
    assert_eq!(stdout(&out), "p-1\tproj\t/w/proj\n");
    let out = tempo_session(&root, &["projects", "rm", "p-1"])?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(srv.requests()[1].path, "/v1/projects/p-1");
    Ok(())
}

/// A repository registered inside another registered one: the cwd belongs to
/// the innermost project, whatever order `GET /v1/projects` returns them in.
#[test]
fn a_nested_project_wins_over_the_enclosing_one() -> anyhow::Result<()> {
    let base = std::env::temp_dir().join(format!("tempo-session-nested-{}", std::process::id()));
    let inner = base.join("outer/inner");
    std::fs::create_dir_all(inner.join("pkg"))?;
    let outer = std::fs::canonicalize(base.join("outer"))?;
    let inner = std::fs::canonicalize(&inner)?;
    // Outer first, so a `find` would take it.
    let projects = format!(
        r#"[{{"id":"p-outer","path":"{}","name":"outer","created_at":"2026-08-27T10:00:00Z"}},
            {{"id":"p-inner","path":"{}","name":"inner","created_at":"2026-08-27T10:00:00Z"}}]"#,
        outer.display(),
        inner.display()
    );
    let srv = serve(vec![
        (200, projects),
        (201, session_json("s-3", "starting", Some("main"), false)),
    ])?;
    let root = root_with_api(srv.port)?;
    let arg = inner.join("pkg").display().to_string();
    let out = tempo_session(&root, &["new", arg.as_str()])?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let reqs = srv.requests();
    assert_eq!(
        reqs.len(),
        2,
        "the project was already registered; paths: {:?}",
        reqs.iter().map(|r| r.path.clone()).collect::<Vec<_>>()
    );
    let created: serde_json::Value = serde_json::from_str(&reqs[1].body)?;
    assert_eq!(created["project"], "p-inner");
    Ok(())
}

/// The daemon was alive when `api.json` was read and gone by the time the
/// request went out: the run API's "is a run active?" is the wrong advice.
#[test]
fn a_daemon_that_stops_mid_command_names_itself_and_its_api_file() -> anyhow::Result<()> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let root = root_with_api(port)?;
    let out = tempo_session(&root, &["list"])?;
    assert_eq!(exit_code(&out), 3);
    let err = stderr(&out);
    assert!(err.contains("sessions daemon"), "{err}");
    assert!(
        err.contains(&root.join("api.json").display().to_string()),
        "{err}"
    );
    assert!(!err.contains("is a run active?"), "{err}");
    Ok(())
}
