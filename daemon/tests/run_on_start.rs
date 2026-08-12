#![expect(clippy::unwrap_used, reason = "assertions are the vocabulary of tests")]
#![expect(clippy::expect_used, reason = "assertions are the vocabulary of tests")]
#![expect(clippy::panic, reason = "assertions are the vocabulary of tests")]

//! `coretempod run` on an `on_start` workflow (spec triggers §2, task 10): the
//! kickoff fires right after `Run::start`, and the process exits on its own once
//! the kickoff settles — no separate trigger client, no standing listener.

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const DAEMON: &str = env!("CARGO_BIN_EXE_coretempod");

/// A fake `claude`: reports turn boundaries like the real hooks do. Replies with
/// `$FAKE_AGENT_CODE` (default 0) unless `FAKE_AGENT_REPLY=0`, in which case it
/// reports `working` and then never replies — simulating a stuck turn for the
/// SIGINT test. Speaks HTTP over bash's `/dev/tcp`, matching `daemon/tests/serve.rs`.
const FAKE_AGENT: &str = r#"#!/bin/bash
me="$CORETEMPO_AGENT_ID"
post() {
  exec 3<>"/dev/tcp/127.0.0.1/$CORETEMPO_PORT" || return 1
  printf 'POST %s HTTP/1.1\r\nHost: 127.0.0.1\r\n' "$1" >&3
  printf 'Authorization: Bearer %s\r\n' "$CORETEMPO_TOKEN" >&3
  printf 'X-CoreTempo-Agent: %s\r\n' "$me" >&3
  printf 'Content-Type: application/json\r\nContent-Length: %d\r\n' "${#2}" >&3
  printf 'Connection: close\r\n\r\n%s' "$2" >&3
  cat <&3 >/dev/null
  exec 3>&-
}
post "/v1/agents/$me/state" '{"state":"idle"}'
last=""
while IFS= read -r line; do
  [[ "$line" =~ (m-[0-9a-f]+) ]] || continue
  id="${BASH_REMATCH[1]}"
  [ "$id" = "$last" ] && continue
  last="$id"
  post "/v1/agents/$me/state" '{"state":"working"}'
  if [ "${FAKE_AGENT_REPLY:-1}" = "1" ]; then
    post "/v1/messages/$id/reply" "{\"code\":${FAKE_AGENT_CODE:-0},\"body\":\"ok\"}"
    post "/v1/agents/$me/state" '{"state":"idle"}'
  fi
done
"#;

const TOKEN: &str = "12ab34cd56ef78ab90cd12ef34ab56cd78ef90ab12cd34ef56ab78cd90ef12ab";

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn agent() -> ureq::Agent {
    let cfg = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build();
    ureq::Agent::new_with_config(cfg)
}

struct Scratch {
    root: PathBuf,
    config: PathBuf,
    home: PathBuf,
    bin: PathBuf,
}

/// A scratch home, a fake `claude` on PATH, and an `on_start` tempo.toml.
fn scratch(name: &str) -> Scratch {
    let root = std::env::temp_dir().join(format!("coretempo-run-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    let bin = root.join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin).unwrap();

    let fake = bin.join("claude");
    std::fs::write(&fake, FAKE_AGENT).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(root.join("token"), TOKEN).unwrap();

    let config = root.join("tempo.toml");
    std::fs::write(
        &config,
        format!(
            "[workflow]\nname = \"run-{name}\"\ndb = \"{db}\"\n\
             ask_timeout_minutes = 1\nidle_debounce_seconds = 0.3\n\
             [agents.worker]\ndir = \"{dir}\"\nprompt = \"You reply.\"\n\
             [trigger]\ntype = \"on_start\"\n\
             edge = {{ to = \"worker\", kind = \"ask\" }}\nmessage = \"begin\"\n",
            db = root.join("tempo.db").display(),
            dir = root.display(),
        ),
    )
    .unwrap();
    Scratch {
        root,
        config,
        home,
        bin,
    }
}

fn log_file(root: &std::path::Path, name: &str) -> std::fs::File {
    std::fs::File::create(root.join(name)).unwrap()
}

/// Spawns `coretempod run` against `scratch`, with the fake agent's behavior
/// driven by `code` (reply code) and `reply` (whether it replies at all).
fn spawn_run(scratch: &Scratch, port: u16, code: u8, reply: bool) -> Child {
    let path = std::env::var("PATH").unwrap_or_default();
    Command::new(DAEMON)
        .arg("run")
        .arg(&scratch.config)
        .arg("--port")
        .arg(port.to_string())
        .arg("--token-file")
        .arg(scratch.root.join("token"))
        .env("HOME", &scratch.home)
        .env("PATH", format!("{}:{path}", scratch.bin.display()))
        .env("FAKE_AGENT_CODE", code.to_string())
        .env("FAKE_AGENT_REPLY", if reply { "1" } else { "0" })
        .env("RUST_LOG", "info")
        .stdout(Stdio::from(log_file(&scratch.root, "out.log")))
        .stderr(Stdio::from(log_file(&scratch.root, "err.log")))
        .spawn()
        .unwrap()
}

fn stderr_text(scratch: &Scratch) -> String {
    let read = |name: &str| std::fs::read_to_string(scratch.root.join(name)).unwrap_or_default();
    format!("{}{}", read("out.log"), read("err.log"))
}

/// Polls the run's own `/v1/health` until it answers — proof `Run::start`
/// finished and the kickoff race (watcher vs. ctrl-c) has begun.
fn wait_for_health(scratch: &Scratch, port: u16, within: Duration) {
    let url = format!("http://127.0.0.1:{port}/v1/health");
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Ok(res) = agent().get(&url).call()
            && res.status().as_u16() == 200
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "run never became healthy; stderr:\n{}",
        stderr_text(scratch)
    );
}

fn wait_for_exit(child: &mut Child, within: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("wait on the daemon") {
            return status;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("the daemon did not exit within {within:?}");
}

#[test]
fn a_reply_of_code_zero_exits_zero() {
    let scratch = scratch("code-zero");
    let port = free_port();
    let mut child = spawn_run(&scratch, port, 0, true);
    let status = wait_for_exit(&mut child, Duration::from_secs(30));
    assert!(
        status.success(),
        "expected exit 0, got {status:?}; stderr:\n{}",
        stderr_text(&scratch)
    );
}

#[test]
fn a_reply_of_code_one_exits_one() {
    let scratch = scratch("code-one");
    let port = free_port();
    let mut child = spawn_run(&scratch, port, 1, true);
    let status = wait_for_exit(&mut child, Duration::from_secs(30));
    assert_eq!(status.code(), Some(1), "stderr:\n{}", stderr_text(&scratch));
}

#[test]
fn sigint_during_a_never_replying_kickoff_exits_130() {
    signal_during_a_never_replying_kickoff_exits_130("INT");
}

#[test]
fn sigterm_during_a_never_replying_kickoff_exits_130() {
    // `systemctl stop` and `docker stop` send SIGTERM, not SIGINT — the daemon
    // must treat both the same way (same drain/stop/exit-130 semantics).
    signal_during_a_never_replying_kickoff_exits_130("TERM");
}

fn signal_during_a_never_replying_kickoff_exits_130(signal: &str) {
    let scratch = scratch(&format!("never-replies-{}", signal.to_lowercase()));
    let port = free_port();
    let mut child = spawn_run(&scratch, port, 0, false);
    wait_for_health(&scratch, port, Duration::from_secs(20));

    let ok = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(ok.success(), "could not signal the daemon");

    let status = wait_for_exit(&mut child, Duration::from_secs(30));
    assert_eq!(
        status.code(),
        Some(130),
        "stderr:\n{}",
        stderr_text(&scratch)
    );
}
