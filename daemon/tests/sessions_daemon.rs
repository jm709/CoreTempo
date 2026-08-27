#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]
#![expect(clippy::unwrap_used, reason = "assertions are the vocabulary of tests")]
#![expect(clippy::expect_used, reason = "assertions are the vocabulary of tests")]
#![expect(clippy::panic, reason = "assertions are the vocabulary of tests")]

//! `coretempod sessions` as a process (spec 2026-08-27 §3): its root, the
//! `flock`ed `sessions.lock`, the 0600 `api.json` that names the live pid,
//! `daemon.log`, and the signal path that marks every row exited on the way
//! out.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::{
    sessions_command, sessions_daemon, sessions_daemon_on, sessions_scratch, wait_for_child_exit,
    wait_for_exit_of,
};

#[test]
fn api_json_is_private_names_the_pid_and_goes_away_on_a_clean_stop() -> anyhow::Result<()> {
    let mut d = sessions_daemon("apifile");
    let api_json = d.root().join("api.json");
    let mode = std::fs::metadata(&api_json)?.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(d.api.pid, d.child.id());
    assert_eq!(d.api.token.0.len(), 64);
    let db_mode = std::fs::metadata(d.root().join("sessions.db"))?
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(db_mode, 0o600);
    let (status, health) = d.get("/v1/health")?;
    assert_eq!(status, 200);
    assert_eq!(health["ok"], true);
    assert!(d.root().join("sessions.lock").exists());
    assert!(d.root().join("daemon.log").exists());

    // SIGTERM: clean exit, api.json removed, rows marked, lock released.
    let repo = d.scratch.repo.clone();
    let (status, project) = d.post("/v1/projects", &serde_json::json!({ "path": repo }))?;
    assert_eq!(status, 201, "{project}");
    let (status, view) = d.post(
        "/v1/sessions",
        &serde_json::json!({"project": project["id"]}),
    )?;
    assert_eq!(status, 201, "{view}");
    let id = view["id"].as_str().unwrap_or_default().to_string();
    std::process::Command::new("kill")
        .arg("-TERM")
        .arg(d.child.id().to_string())
        .status()?;
    let exit = wait_for_exit_of(&mut d, Duration::from_secs(20));
    assert!(exit.success(), "logs:\n{}", d.logs());
    assert!(!api_json.exists(), "api.json removed on clean exit");
    let log = std::fs::read_to_string(d.root().join("daemon.log"))?;
    assert!(log.contains("sessions marked exited"), "{log}");

    // A second daemon over the same root sees the row exited.
    let d2 = sessions_daemon_on(d.scratch_clone());
    let (status, view) = d2.get(&format!("/v1/sessions/{id}"))?;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["state"], "exited");
    Ok(())
}

#[test]
fn a_second_daemon_on_the_same_root_is_refused_naming_pid_and_port() -> anyhow::Result<()> {
    let d = sessions_daemon("second");
    let root = d.root().display().to_string();
    let mut second = sessions_command(&d.scratch, &["sessions", "--root", &root])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let status = wait_for_child_exit(&mut second, Duration::from_secs(10));
    assert_eq!(status.code(), Some(1));
    let mut err = String::new();
    std::io::Read::read_to_string(&mut second.stderr.take().unwrap(), &mut err)?;
    assert!(err.contains(&d.api.pid.to_string()), "{err}");
    assert!(err.contains(&d.api.port.to_string()), "{err}");
    assert!(err.contains("coretempod sessions stop"), "{err}");
    Ok(())
}

#[test]
fn a_stale_api_json_from_a_dead_pid_is_overwritten() -> anyhow::Result<()> {
    let scratch = sessions_scratch("stale");
    let root = scratch.root.join("sessions");
    std::fs::create_dir_all(&root)?;
    std::fs::write(
        root.join("api.json"),
        r#"{"port":1,"token":"00","pid":2147483646}"#,
    )?;
    let d = sessions_daemon_on(scratch);
    assert_ne!(d.api.pid, 2_147_483_646);
    assert_ne!(d.api.port, 1);
    Ok(())
}

#[test]
fn sessions_stop_sends_sigterm_and_reports_no_daemon_afterwards() -> anyhow::Result<()> {
    let mut d = sessions_daemon("stopcmd");
    let root = d.root().display().to_string();
    let out = sessions_command(&d.scratch, &["sessions", "--root", &root, "stop"]).output()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let exit = wait_for_exit_of(&mut d, Duration::from_secs(20));
    assert!(exit.success(), "logs:\n{}", d.logs());
    let out = sessions_command(&d.scratch, &["sessions", "--root", &root, "stop"]).output()?;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no session daemon running"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

/// `api.json` outlives a daemon killed with SIGKILL, reaped by the OOM killer,
/// or crashed — and the kernel recycles pids, so a live pid in that file is not
/// evidence a daemon is running. `sessions.lock` is: `stop` must probe it and
/// refuse rather than SIGTERM whatever now holds the number.
#[test]
fn stop_never_signals_an_unrelated_process_named_by_a_stale_api_json() -> anyhow::Result<()> {
    let scratch = sessions_scratch("stalestop");
    let root = scratch.root.join("sessions");
    std::fs::create_dir_all(&root)?;
    // Alive, ours, and emphatically not a session daemon. Nothing holds the
    // lock — no daemon ever started against this root.
    let mut bystander = Command::new("sleep").arg("60").spawn()?;
    std::fs::write(
        root.join("api.json"),
        format!(r#"{{"port":1,"token":"00","pid":{}}}"#, bystander.id()),
    )?;

    let out = sessions_command(
        &scratch,
        &["sessions", "--root", &root.display().to_string(), "stop"],
    )
    .output()?;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no session daemon running"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Proving a non-event, so it needs a window: a SIGTERM sent before `stop`
    // exited would land and be reapable well inside this one.
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        assert!(
            bystander.try_wait()?.is_none(),
            "stop signalled an unrelated process"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    bystander.kill()?;
    bystander.wait()?;
    Ok(())
}

#[test]
fn the_host_guard_and_bearer_apply() -> anyhow::Result<()> {
    let d = sessions_daemon("guard");
    let (status, _) = d.get_with_host("/v1/sessions", "evil.example.com")?;
    assert_eq!(status, 403);
    let (status, body) = d.get_as("/v1/sessions", None)?;
    assert_eq!(status, 401, "{body}");
    Ok(())
}
