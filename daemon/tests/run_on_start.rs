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
/// Every prompt it sees lands in `prompts.log` in its working directory, which is
/// how the flow-label test reads what was actually typed at it.
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
  printf '%s\n' "$line" >>"$PWD/prompts.log"
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
/// `extra` is appended verbatim after the `on_start` flow block, with `{dir}`
/// replaced by the scratch root — how the mixed-file test adds a second flow.
fn scratch(name: &str, extra: &str) -> Scratch {
    let root = std::env::temp_dir().join(format!("coretempo-run-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    let bin = root.join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    // The run preflights Claude Code trust for the agent dir; a scratch root
    // has never been opened in `claude`, so grant it through the user config.
    std::fs::create_dir_all(home.join(".coretempo")).unwrap();
    std::fs::write(
        home.join(".coretempo/config.toml"),
        "trust_agent_dirs = true\n",
    )
    .unwrap();

    let fake = bin.join("claude");
    std::fs::write(&fake, FAKE_AGENT).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(root.join("token"), TOKEN).unwrap();

    let config = root.join("tempo.toml");
    let mut toml = format!(
        "[workflow]\nname = \"run-{name}\"\ndb = \"{db}\"\n\
         ask_timeout_minutes = 1\nidle_debounce_seconds = 0.3\n\
         [agents.worker]\ndir = \"{dir}\"\nprompt = \"You reply.\"\n\
         [flows.main]\nagents = [\"worker\"]\n\
         trigger = {{ type = \"on_start\", edge = {{ to = \"worker\", \
         kind = \"ask\" }}, message = \"begin\" }}\n",
        db = root.join("tempo.db").display(),
        dir = root.display(),
    );
    toml.push_str(&extra.replace("{dir}", &root.display().to_string()));
    std::fs::write(&config, toml).unwrap();
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
/// `flow` maps to `--flow <name>`; `None` is a bare (whole-pool) run.
fn spawn_run(scratch: &Scratch, port: u16, code: u8, reply: bool, flow: Option<&str>) -> Child {
    let path = std::env::var("PATH").unwrap_or_default();
    let mut cmd = Command::new(DAEMON);
    cmd.arg("run").arg(&scratch.config);
    if let Some(flow) = flow {
        cmd.arg("--flow").arg(flow);
    }
    cmd.arg("--port")
        .arg(port.to_string())
        .arg("--token-file")
        .arg(scratch.root.join("token"))
        .env("HOME", &scratch.home)
        .env("PATH", format!("{}:{path}", scratch.bin.display()))
        .env("FAKE_AGENT_CODE", code.to_string())
        .env("FAKE_AGENT_REPLY", if reply { "1" } else { "0" })
        .env("RUST_LOG", "info")
        // The scratch config is the only one this run may read.
        .env_remove("CORETEMPO_CONFIG")
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
    let scratch = scratch("code-zero", "");
    let port = free_port();
    let mut child = spawn_run(&scratch, port, 0, true, Some("main"));
    let status = wait_for_exit(&mut child, Duration::from_secs(30));
    assert!(
        status.success(),
        "expected exit 0, got {status:?}; stderr:\n{}",
        stderr_text(&scratch)
    );
}

/// Amendment 31 (#42): an `on_start` kickoff names its flow too. The scope rule
/// is "a flow kickoff always says which flow it belongs to", so an unlabelled
/// ask is never one — leaving batch kickoffs bare would put the only ambiguous
/// case back.
#[test]
fn the_on_start_kickoff_names_its_flow() {
    let scratch = scratch("flow-label", "");
    let port = free_port();
    let mut child = spawn_run(&scratch, port, 0, true, Some("main"));
    let status = wait_for_exit(&mut child, Duration::from_secs(30));
    assert!(
        status.success(),
        "expected exit 0, got {status:?}; stderr:\n{}",
        stderr_text(&scratch)
    );
    let prompts = std::fs::read_to_string(scratch.root.join("prompts.log")).unwrap_or_default();
    assert!(
        prompts.contains("from http, flow main"),
        "the kickoff typed at the agent names its flow; prompts:\n{prompts}"
    );
}

#[test]
fn a_reply_of_code_one_exits_one() {
    let scratch = scratch("code-one", "");
    let port = free_port();
    let mut child = spawn_run(&scratch, port, 1, true, Some("main"));
    let status = wait_for_exit(&mut child, Duration::from_secs(30));
    assert_eq!(status.code(), Some(1), "stderr:\n{}", stderr_text(&scratch));
}

#[test]
fn an_on_start_kickoff_ignores_a_webhook_flows_output_contract() {
    // The webhook flow declares a schema the fake agent's "ok" reply cannot
    // satisfy. Under `--flow main` the derived subset excludes the webhook
    // flow entirely, so its contract cannot even load — this pins that a
    // mixed file's batch run exits 0 despite a contract declared elsewhere
    // in the file. (Until phase 3 this file was refused outright at
    // startup_kickoff; commit e70a3f4.)
    let extra = "[agents.other]\ndir = \"{dir}\"\nprompt = \"You reply.\"\n\
        [flows.hook]\nagents = [\"other\"]\n\
        trigger = { type = \"webhook\", edge = { to = \"other\", kind = \"ask\" } }\n\
        [flows.hook.output]\nschema = { type = \"object\", required = [\"name\"] }\n";
    let scratch = scratch("mixed-contract", extra);
    let port = free_port();
    let mut child = spawn_run(&scratch, port, 0, true, Some("main"));
    let status = wait_for_exit(&mut child, Duration::from_mins(1));
    assert_eq!(
        status.code(),
        Some(0),
        "the batch reply must not be validated against the webhook flow's \
         schema; stderr:\n{}",
        stderr_text(&scratch)
    );
}

/// Every message body the run has recorded, newest first.
fn message_bodies(port: u16) -> Vec<String> {
    let mut res = agent()
        .get(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("Authorization", format!("Bearer {TOKEN}"))
        .call()
        .unwrap();
    let text = res.body_mut().read_to_string().unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    json["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter_map(|m| m["body"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
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
    let scratch = scratch(&format!("never-replies-{}", signal.to_lowercase()), "");
    let port = free_port();
    let mut child = spawn_run(&scratch, port, 0, false, Some("main"));
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

#[test]
fn a_bare_run_fires_nothing_and_stays_warm() {
    let scratch = scratch("bare-warm", "");
    let port = free_port();
    let mut child = spawn_run(&scratch, port, 0, true, None);
    wait_for_health(&scratch, port, Duration::from_secs(20));
    // Nothing auto-fires: the message log stays empty (multi-flow spec §6).
    // Sampled over a settle window — health answering does not mean a
    // regression's kickoff has had time to reach the log.
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        assert_eq!(
            message_bodies(port),
            Vec::<String>::new(),
            "a bare run must not fire the on_start flow; stderr:\n{}",
            stderr_text(&scratch)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // And it does not exit on its own — interrupt it. Ctrl-c on a warm run
    // with no kickoff in flight is a clean stop: exit 0, the landed
    // no-kickoff semantics (130 is for interrupting a live kickoff).
    let ok = Command::new("kill")
        .arg("-INT")
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(ok.success(), "could not signal the daemon");
    let status = wait_for_exit(&mut child, Duration::from_secs(20));
    assert_eq!(status.code(), Some(0), "stderr:\n{}", stderr_text(&scratch));
}

const HOOK_FLOW: &str = "[flows.hook]\nagents = [\"worker\"]\n\
    trigger = { type = \"webhook\", edge = { to = \"worker\", kind = \"ask\" } }\n";

#[test]
fn run_flow_with_an_unknown_name_exits_naming_the_flows() {
    let scratch = scratch("unknown-flow", HOOK_FLOW);
    let port = free_port();
    let mut child = spawn_run(&scratch, port, 0, true, Some("nope"));
    let status = wait_for_exit(&mut child, Duration::from_secs(20));
    assert!(!status.success());
    let err = stderr_text(&scratch);
    assert!(err.contains("nope"), "names the input: {err}");
    assert!(
        err.contains("hook") && err.contains("main"),
        "names the flows: {err}"
    );
}

#[test]
fn run_flow_webhook_is_warm_with_the_flow_armed_and_the_subset_spawned() {
    let extra =
        format!("[agents.bystander]\ndir = \"{{dir}}\"\nprompt = \"You wait.\"\n{HOOK_FLOW}");
    let scratch = scratch("webhook-flow", &extra);
    let port = free_port();
    let mut child = spawn_run(&scratch, port, 0, true, Some("hook"));
    wait_for_health(&scratch, port, Duration::from_secs(20));
    // Subset roster: the API's workflow view holds only the flow's members
    // (Task 1's narrowing).
    let mut res = agent()
        .get(format!("http://127.0.0.1:{port}/v1/workflow"))
        .header("Authorization", format!("Bearer {TOKEN}"))
        .call()
        .expect("workflow");
    let body: serde_json::Value =
        serde_json::from_str(&res.body_mut().read_to_string().expect("body")).expect("json");
    let agents = body["workflow"]["agents"].as_object().expect("agents map");
    assert!(
        agents.contains_key("worker") && !agents.contains_key("bystander"),
        "{body}"
    );
    // The armed route round-trips.
    let res = agent()
        .post(format!(
            "http://127.0.0.1:{port}/v1/flows/hook/trigger?wait=25"
        ))
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Content-Type", "text/plain")
        .send("do the thing")
        .expect("trigger");
    assert_eq!(
        res.status().as_u16(),
        200,
        "stderr:\n{}",
        stderr_text(&scratch)
    );
    let ok = Command::new("kill")
        .arg("-INT")
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(ok.success());
    let _ = wait_for_exit(&mut child, Duration::from_secs(20));
}
