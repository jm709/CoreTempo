#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]
#![expect(clippy::unwrap_used, reason = "assertions are the vocabulary of tests")]
#![expect(clippy::expect_used, reason = "assertions are the vocabulary of tests")]
#![expect(clippy::panic, reason = "assertions are the vocabulary of tests")]

//! `tempo session …` against a real `coretempod sessions` (spec 2026-08-27
//! §10). Runs the `tempo` beside `coretempod`: `cargo test --workspace` builds
//! both; running this file alone needs `cargo build -p coretempo-cli` first.

mod support;

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use support::{
    DAEMON, SessionsDaemon, SessionsScratch, sessions_daemon, sessions_daemon_on, sessions_scratch,
};

/// A fake `claude` that reports its state through the real `tempo` binary, the
/// way the generated hooks do: a `SessionStart`-shaped payload on stdin, whose
/// `session_id` the daemon must store as the session's `claude_session_id`.
/// Its stderr goes to a file because its stdout is the PTY.
const FAKE_HOOK_AGENT: &str = r#"#!/bin/bash
payload="{\"session_id\":\"hook-sid-42\",\"hook_event_name\":\"SessionStart\",\"cwd\":\"$PWD\"}"
printf '%s' "$payload" | tempo state idle >"$HOME/tempo-state.log" 2>&1
printf 'booted\n'
while IFS= read -r line; do
  case "$line" in
    quit) exit 3 ;;
    *) printf 'got:%s\n' "$line" ;;
  esac
done
"#;

fn tempo_bin() -> PathBuf {
    let tempo = Path::new(DAEMON).with_file_name("tempo");
    assert!(
        tempo.is_file(),
        "no `tempo` next to {DAEMON}; run `cargo build -p coretempo-cli` \
         (or `cargo test --workspace`)"
    );
    tempo
}

fn tempo(d: &SessionsDaemon, args: &[&str]) -> Command {
    let mut cmd = Command::new(tempo_bin());
    cmd.arg("session")
        .arg("--root")
        .arg(d.root())
        .args(args)
        .env("HOME", &d.scratch.home)
        .env_remove("CORETEMPO_PORT")
        .env_remove("CORETEMPO_TOKEN")
        .env_remove("CORETEMPO_AGENT_ID");
    cmd
}

fn run(d: &SessionsDaemon, args: &[&str]) -> (i32, String, String) {
    let out = tempo(d, args)
        .stdin(Stdio::null())
        .output()
        .expect("run tempo");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// What [`FAKE_HOOK_AGENT`]'s `tempo state` printed, if that agent is the one
/// running — the only place its failure would otherwise be visible.
fn hook_log(d: &SessionsDaemon) -> String {
    std::fs::read_to_string(d.scratch.home.join("tempo-state.log")).unwrap_or_default()
}

fn wait_line(d: &SessionsDaemon, id: &str, state: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let (_, out, _) = run(d, &["list"]);
        if out
            .lines()
            .any(|l| l.starts_with(id) && l.split('\t').nth(3) == Some(state))
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "session {id} never listed as {state}; logs:\n{}{}",
        d.logs(),
        hook_log(d)
    );
}

/// Reads `stdout` until `want` shows up, or panics with everything it saw.
fn read_until(d: &SessionsDaemon, stdout: &mut impl Read, want: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    while !String::from_utf8_lossy(&seen).contains(want) {
        assert!(
            Instant::now() < deadline,
            "attach never showed {want}; got {:?}; logs:\n{}",
            String::from_utf8_lossy(&seen),
            d.logs()
        );
        // A blocking pipe read returns 0 only at EOF: attach is gone, and no
        // amount of waiting will produce `want`.
        let n = stdout.read(&mut buf).expect("read attach stdout");
        assert!(
            n > 0,
            "attach's stdout ended before {want}; got {:?}; logs:\n{}",
            String::from_utf8_lossy(&seen),
            d.logs()
        );
        seen.extend_from_slice(&buf[..n]);
    }
}

/// A live `tempo session attach`, with its three pipes taken. Reaped on drop,
/// so an assertion between attaching and detaching leaves nothing behind.
struct Attached {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
}

impl Drop for Attached {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

impl Attached {
    /// The exit status, on a deadline: an attachment that never lets go must
    /// fail this test, not hang it.
    fn wait(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("wait on tempo session attach") {
                return status;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("tempo session attach never exited");
    }
}

fn attach(d: &SessionsDaemon, id: &str) -> Attached {
    let mut child = tempo(d, &["attach", id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tempo session attach");
    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");
    Attached {
        child,
        stdin,
        stdout,
        stderr,
    }
}

#[test]
fn new_list_attach_detach_stop_resume_rm() -> anyhow::Result<()> {
    let d = sessions_daemon("cli");
    let repo = d.scratch.repo.display().to_string();
    let (code, out, err) = run(&d, &["new", &repo, "--worktree", "--title", "cli test"]);
    assert_eq!(code, 0, "{err}");
    let mut lines = out.lines();
    let id = lines.next().unwrap_or_default().to_string();
    assert!(id.starts_with("s-"), "{out}");
    assert!(
        lines.next().unwrap_or_default().starts_with("session/"),
        "{out}"
    );
    wait_line(&d, &id, "idle");
    let (_, out, _) = run(&d, &["list"]);
    assert!(out.contains("\tcli test"), "{out}");

    // attach over a pipe: type, see the echo, Ctrl-] out with status 0.
    let mut session = attach(&d, &id);
    session.stdin.write_all(b"ping\n")?;
    session.stdin.flush()?;
    read_until(&d, &mut session.stdout, "got:ping");
    session.stdin.write_all(&[0x1d])?;
    session.stdin.flush()?;
    assert_eq!(session.wait().code(), Some(0), "detach exits 0");

    // attach again, then make the session exit under it: status 1, message.
    let mut session = attach(&d, &id);
    read_until(&d, &mut session.stdout, "booted");
    session.stdin.write_all(b"quit\n")?;
    session.stdin.flush()?;
    let status = session.wait();
    let mut err = String::new();
    session.stderr.read_to_string(&mut err)?;
    assert_eq!(status.code(), Some(1), "exit while attached is 1: {err}");
    // The agent's last line comes over the PTY stream while the exit comes
    // over the event stream: attach must drain the one before honouring the
    // other, or the operator never sees why the session ended.
    let mut last = String::new();
    session.stdout.read_to_string(&mut last)?;
    assert!(
        last.contains("bye"),
        "last output before the exit: {last:?}"
    );
    assert!(
        err.contains(&id) && err.contains("exited (code 3)"),
        "the message names the exit: {err}"
    );
    wait_line(&d, &id, "exited");

    let (code, out, err) = run(&d, &["resume", &id]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out.trim(), "resumed conversation fake-sid");
    wait_line(&d, &id, "idle");
    let (code, _, _) = run(&d, &["stop", &id]);
    assert_eq!(code, 0);
    wait_line(&d, &id, "stopped");
    let (code, out, err) = run(&d, &["rm", &id, "--remove-worktree"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "", "an unmoved branch is deleted silently");
    let (code, _, err) = run(&d, &["show", &id]);
    assert_eq!(code, 3);
    assert!(err.contains("no session"), "{err}");
    Ok(())
}

#[test]
fn attach_to_a_stopped_session_exits_1_immediately() {
    let d = sessions_daemon("attach-stopped");
    let repo = d.scratch.repo.display().to_string();
    let (_, out, _) = run(&d, &["new", &repo]);
    let id = out.lines().next().unwrap_or_default().to_string();
    wait_line(&d, &id, "idle");
    run(&d, &["stop", &id]);
    wait_line(&d, &id, "stopped");
    let (code, _, err) = run(&d, &["attach", &id]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("not live"), "{err}");
}

/// [`sessions_scratch`] whose fake `claude` is [`FAKE_HOOK_AGENT`],
/// with the real `tempo` on the session's PATH for it to call.
fn hook_agent_scratch(name: &str) -> SessionsScratch {
    let scratch = sessions_scratch(name);
    let fake = scratch.bin.join("claude");
    std::fs::write(&fake, FAKE_HOOK_AGENT).expect("write fake claude");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::os::unix::fs::symlink(tempo_bin(), scratch.bin.join("tempo")).expect("link tempo");
    scratch
}

/// The hook path end to end: a session's `tempo state` carries the Claude
/// Code `session_id` from its hook payload, and the daemon stores it as the
/// row's `claude_session_id` (spec §4, §10).
#[test]
fn tempo_state_from_a_hook_records_the_claude_session_id() -> anyhow::Result<()> {
    let d = sessions_daemon_on(hook_agent_scratch("cli-hook"));
    let repo = d.scratch.repo.display().to_string();
    let (code, out, err) = run(&d, &["new", &repo]);
    assert_eq!(code, 0, "{err}");
    let id = out.lines().next().unwrap_or_default().to_string();
    wait_line(&d, &id, "idle");
    let (code, out, err) = run(&d, &["show", &id]);
    assert_eq!(code, 0, "{err}");
    let view: serde_json::Value = serde_json::from_str(out.trim())?;
    assert_eq!(
        view["claude_session_id"],
        "hook-sid-42",
        "tempo state did not forward the hook payload's session_id: {}",
        hook_log(&d)
    );
    Ok(())
}
