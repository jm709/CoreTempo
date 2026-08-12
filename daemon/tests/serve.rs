#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]
#![expect(clippy::unwrap_used, reason = "assertions are the vocabulary of tests")]
#![expect(clippy::expect_used, reason = "assertions are the vocabulary of tests")]
#![expect(clippy::panic, reason = "assertions are the vocabulary of tests")]

//! `coretempod serve` end to end (spec triggers §3): a standing listener that
//! cold-starts one run per trigger, FIFO, against a scripted fake agent.

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const DAEMON: &str = env!("CARGO_BIN_EXE_coretempod");

/// A fake `claude`: reports turn boundaries like the real hooks do and answers
/// every ask it is given. Speaks HTTP over bash's `/dev/tcp` so the test needs
/// no client binary on PATH.
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
  sleep "${FAKE_AGENT_DELAY:-0}"
  post "/v1/messages/$id/reply" '{"code":0,"body":"ok"}'
  post "/v1/agents/$me/state" '{"state":"idle"}'
done
"#;

const TOKEN: &str = "ab12cd34ef56ab78cd90ef12ab34cd56ef78ab90cd12ef34ab56cd78ef90ab12";

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
        .timeout_global(Some(Duration::from_mins(1)))
        .build();
    ureq::Agent::new_with_config(cfg)
}

struct Scratch {
    root: PathBuf,
    config: PathBuf,
    home: PathBuf,
    bin: PathBuf,
}

/// A scratch home, a fake `claude` on PATH, and a tempo.toml with `trigger`.
fn scratch(name: &str, trigger: &str) -> Scratch {
    let root = std::env::temp_dir().join(format!("coretempo-serve-{}-{name}", std::process::id()));
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
            // The file's port is never bound in serve mode — the listener takes
            // --port and each triggered run binds an ephemeral one — so this is
            // a literal rather than another racy free_port().
            "[workflow]\nname = \"serve-{name}\"\nport = 4820\ndb = \"{db}\"\n\
             ask_timeout_minutes = 1\nidle_debounce_seconds = 0.3\n\
             [agents.worker]\ndir = \"{dir}\"\nprompt = \"You reply.\"\n{trigger}",
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

const WEBHOOK: &str = "[trigger]\ntype = \"webhook\"\nedge = { to = \"worker\", kind = \"ask\" }\n";

struct Serving {
    child: Child,
    port: u16,
    scratch: Scratch,
}

impl Drop for Serving {
    fn drop(&mut self) {
        // Interrupt rather than kill: the daemon owns PTY children, and SIGKILL
        // would orphan them.
        let _ = Command::new("kill")
            .arg("-INT")
            .arg(self.child.id().to_string())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_)) | Err(_)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn daemon_command(scratch: &Scratch, sub: &str, port: u16, delay: &str) -> Command {
    let mut cmd = Command::new(DAEMON);
    let path = std::env::var("PATH").unwrap_or_default();
    cmd.arg(sub)
        .arg(&scratch.config)
        .arg("--port")
        .arg(port.to_string())
        .arg("--token-file")
        .arg(scratch.root.join("token"))
        .env("HOME", &scratch.home)
        .env("PATH", format!("{}:{path}", scratch.bin.display()))
        .env("FAKE_AGENT_DELAY", delay)
        .env("RUST_LOG", "info");
    cmd
}

fn log_file(root: &Path, name: &str) -> std::fs::File {
    std::fs::File::create(root.join(name)).unwrap()
}

/// Starts `coretempod serve` and waits for its health endpoint.
fn serving(name: &str, delay: &str) -> Serving {
    let scratch = scratch(name, WEBHOOK);
    let port = free_port();
    let child = daemon_command(&scratch, "serve", port, delay)
        .stdout(Stdio::from(log_file(&scratch.root, "out.log")))
        .stderr(Stdio::from(log_file(&scratch.root, "err.log")))
        .spawn()
        .unwrap();
    let serving = Serving {
        child,
        port,
        scratch,
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok((200, body)) = serving.get("/v1/health") {
            assert_eq!(body["status"], "ok", "health: {body}");
            return serving;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "serve never became healthy; stderr:\n{}",
        serving.stderr_text()
    );
}

impl Serving {
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Both daemon logs: tracing goes to stdout, anyhow's exit error to stderr.
    fn stderr_text(&self) -> String {
        let read =
            |name: &str| std::fs::read_to_string(self.scratch.root.join(name)).unwrap_or_default();
        format!("{}{}", read("out.log"), read("err.log"))
    }

    fn get(&self, path: &str) -> anyhow::Result<(u16, serde_json::Value)> {
        self.get_as(path, Some(TOKEN))
    }

    /// GET with an explicit bearer token, or none at all.
    fn get_as(&self, path: &str, token: Option<&str>) -> anyhow::Result<(u16, serde_json::Value)> {
        let mut req = agent().get(self.url(path));
        if let Some(token) = token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let mut res = req.call()?;
        let status = res.status().as_u16();
        Ok((status, json_of(res.body_mut().read_to_string()?)))
    }

    fn fire(&self, query: &str, body: &str) -> anyhow::Result<(u16, serde_json::Value)> {
        self.fire_as(query, body, Some(TOKEN))
    }

    /// POST a trigger with an explicit bearer token, or none at all.
    fn fire_as(
        &self,
        query: &str,
        body: &str,
        token: Option<&str>,
    ) -> anyhow::Result<(u16, serde_json::Value)> {
        let mut req = agent()
            .post(self.url(&format!("/v1/trigger{query}")))
            .header("Content-Type", "text/plain");
        if let Some(token) = token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let mut res = req.send(body)?;
        let status = res.status().as_u16();
        Ok((status, json_of(res.body_mut().read_to_string()?)))
    }

    /// GET with a `Host` the daemon should not answer to.
    fn get_with_host(&self, path: &str, host: &str) -> anyhow::Result<(u16, serde_json::Value)> {
        let mut res = agent()
            .get(self.url(path))
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header("Host", host)
            .call()?;
        let status = res.status().as_u16();
        Ok((status, json_of(res.body_mut().read_to_string()?)))
    }

    /// Fires a trigger, asserting it was accepted, and returns its id.
    fn fire_ok(&self, body: &str) -> String {
        let (status, json) = self.fire("", body).unwrap();
        assert_eq!(status, 202, "trigger rejected: {json}");
        json["trigger_id"].as_str().unwrap().to_string()
    }

    fn status_of(&self, id: &str) -> serde_json::Value {
        self.get(&format!("/v1/trigger/{id}")).unwrap().1
    }

    /// Polls until `id` is no longer queued or running.
    fn settled(&self, id: &str) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            let body = self.status_of(id);
            let status = body["status"].as_str().unwrap_or_default();
            if status != "queued" && status != "running" {
                return body;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "trigger {id} never settled; stderr:\n{}",
            self.stderr_text()
        );
    }

    fn interrupt(&self) {
        let ok = Command::new("kill")
            .arg("-INT")
            .arg(self.child.id().to_string())
            .status()
            .unwrap();
        assert!(ok.success(), "could not signal the daemon");
    }
}

fn json_of(text: String) -> serde_json::Value {
    serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
}

#[test]
fn a_trigger_cold_starts_a_run_and_reports_the_reply() -> anyhow::Result<()> {
    let serve = serving("basic", "0");
    let (status, health) = serve.get("/v1/health")?;
    assert_eq!(status, 200);
    assert_eq!(health["queue_depth"], 0);
    assert!(health["current_run_id"].is_null(), "health: {health}");

    let id = serve.fire_ok("please do the thing");
    assert!(id.starts_with("t-"), "id: {id}");
    let done = serve.settled(&id);
    assert_eq!(done["status"], "completed", "final: {done}");
    assert_eq!(done["result"], "replied");
    assert_eq!(done["code"], 0);
    assert_eq!(done["reply"], "ok");

    // The run was torn down: no agents are held between triggers.
    let (_, health) = serve.get("/v1/health")?;
    assert!(health["current_run_id"].is_null(), "health: {health}");
    // And its artifact directory was cleaned up, so a long-lived daemon does not
    // accumulate one per trigger.
    let runs = serve.scratch.home.join(".coretempo/runs");
    let leftovers: Vec<_> = std::fs::read_dir(&runs)
        .map(|dir| {
            dir.filter_map(Result::ok)
                .map(|e| e.file_name())
                .filter(|name| name != "current")
                .collect()
        })
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "run dirs left behind: {leftovers:?}");
    // Serve-mode runs never steal the `current` symlink from an interactive run.
    assert!(!runs.join("current").exists(), "serve repointed current");
    Ok(())
}

#[test]
fn health_is_open_but_triggers_need_the_token() -> anyhow::Result<()> {
    // Serve mode authenticates its own requests, without core's ApiContext, so
    // the rule is implemented twice and has to be pinned twice.
    let serve = serving("auth", "0");

    // Health carries no secret and is what a supervisor polls: no auth.
    let (status, body) = serve.get_as("/v1/health", None)?;
    assert_eq!(status, 200, "health must answer unauthenticated: {body}");
    assert_eq!(body["status"], "ok");

    // Firing a workflow is not open, with no token or with the wrong one.
    for token in [None, Some("not-the-token"), Some("")] {
        let (status, body) = serve.fire_as("", "let me in", token)?;
        assert_eq!(status, 401, "token {token:?} was accepted: {body}");
        assert_eq!(body["error"]["code"], "unauthorized", "token {token:?}");
        let (status, body) = serve.get_as("/v1/trigger/t-deadbeef", token)?;
        assert_eq!(status, 401, "token {token:?} was accepted: {body}");
    }
    // None of the refused requests queued anything.
    let (_, health) = serve.get("/v1/health")?;
    assert_eq!(
        health["queue_depth"], 0,
        "a refused trigger queued: {health}"
    );
    assert!(health["current_run_id"].is_null(), "health: {health}");

    // A Host the daemon does not answer to is refused even with the token:
    // this is what blocks DNS-rebinding at the public port.
    let (status, body) = serve.get_with_host("/v1/health", "evil.example.com")?;
    assert_eq!(status, 403, "foreign Host accepted: {body}");
    assert_eq!(body["error"]["code"], "invalid_host");

    // And the real token still works, so the checks are not refusing everything.
    let id = serve.fire_ok("do the thing");
    assert_eq!(serve.settled(&id)["status"], "completed");
    Ok(())
}

#[test]
fn triggers_run_one_at_a_time_in_order() -> anyhow::Result<()> {
    let serve = serving("fifo", "2");
    let first = serve.fire_ok("one");
    let (status, second) = serve.fire("", "two")?;
    assert_eq!(status, 202, "second trigger: {second}");
    assert_eq!(
        second["position"], 1,
        "the second trigger queues behind the first: {second}"
    );
    let second = second["trigger_id"].as_str().unwrap().to_string();

    // The second must never be running while the first still is.
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut second_started = false;
    while Instant::now() < deadline {
        // Sample the second trigger before the first. run_worker settles a
        // trigger before it begins the next one, and statuses only move toward
        // terminal, so observing `two == running` here already implies `one` is
        // terminal — reading the other order across two HTTP round trips would
        // let the worker settle the first and start the second in between,
        // catching `one` mid-flight and false-failing this assertion.
        let two = serve.status_of(&second);
        let one = serve.status_of(&first);
        let one_status = one["status"].as_str().unwrap_or_default().to_string();
        let two_status = two["status"].as_str().unwrap_or_default().to_string();
        if two_status == "running" {
            second_started = true;
            assert_eq!(
                one_status, "completed",
                "the second trigger started while the first was {one_status}"
            );
        }
        if two_status == "completed" || two_status == "failed" {
            assert_eq!(one["status"], "completed", "first: {one}");
            assert_eq!(two["status"], "completed", "second: {two}");
            assert!(second_started, "never observed the second trigger running");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "the queued trigger never ran; stderr:\n{}",
        serve.stderr_text()
    );
}

/// How many distinct 64-hex words a message carries — a hash-mismatch reason
/// names both the frozen hash and the one on disk.
fn hash_words(text: &str) -> usize {
    let mut hashes: Vec<&str> = text
        .split(|c: char| !c.is_ascii_hexdigit())
        .filter(|word| word.len() == 64)
        .collect();
    hashes.sort_unstable();
    hashes.dedup();
    hashes.len()
}

#[test]
fn an_edited_workflow_fails_the_trigger_not_the_daemon() -> anyhow::Result<()> {
    let serve = serving("hash", "2");
    let first = serve.fire_ok("one");
    let queued = serve.fire_ok("two");
    // Edit while the first trigger holds the worker: serve froze the workflow at
    // startup, so the queued trigger must refuse to adopt the edit.
    let text = std::fs::read_to_string(&serve.scratch.config)?;
    std::fs::write(&serve.scratch.config, format!("{text}# edited\n"))?;

    let done = serve.settled(&first);
    assert_eq!(done["status"], "completed", "first: {done}");
    let failed = serve.settled(&queued);
    assert_eq!(failed["status"], "failed", "queued: {failed}");
    let reason = failed["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("restart the daemon"),
        "the reason must say how to adopt the edit: {reason}"
    );
    assert_eq!(
        hash_words(reason),
        2,
        "the reason must name both the frozen hash and the one on disk: {reason}"
    );

    // The daemon itself is unharmed.
    let (status, health) = serve.get("/v1/health")?;
    assert_eq!(status, 200, "health after a hash mismatch: {health}");
    assert_eq!(health["status"], "ok");
    Ok(())
}

#[test]
fn serve_refuses_a_workflow_without_a_webhook_trigger() {
    let scratch = scratch("no-trigger", "");
    let out = daemon_command(&scratch, "serve", free_port(), "0")
        .output()
        .unwrap();
    assert!(!out.status.success(), "serve accepted a plain workflow");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("webhook") && stderr.contains("coretempod run"),
        "the error must point at run mode: {stderr}"
    );
}

#[test]
fn interrupting_the_daemon_fails_the_queued_triggers() -> anyhow::Result<()> {
    let mut serve = serving("shutdown", "6");
    let _running = serve.fire_ok("one");

    // A long-poll on the queued trigger: ctrl-c must answer it rather than drop
    // the connection.
    let url = serve.url("/v1/trigger?wait=45");
    let waiter = std::thread::spawn(move || {
        let mut res = agent()
            .post(url)
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header("Content-Type", "text/plain")
            .send("two")?;
        let status = res.status().as_u16();
        Ok::<_, anyhow::Error>((status, json_of(res.body_mut().read_to_string()?)))
    });
    // Let the queued trigger reach the queue before interrupting.
    std::thread::sleep(Duration::from_secs(2));
    let interrupted = Instant::now();
    serve.interrupt();

    let (status, body) = waiter.join().expect("waiter panicked")?;
    assert_eq!(
        status, 200,
        "the long-poll must report the shutdown: {body}"
    );
    assert_eq!(body["status"], "failed", "queued trigger: {body}");
    let reason = body["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("daemon_shutdown"),
        "the reason must name the shutdown: {reason}"
    );

    let exit = wait_for_exit(&mut serve, Duration::from_secs(30));
    assert!(
        exit.success(),
        "ctrl-c is a clean exit, got {exit:?}; stderr:\n{}",
        serve.stderr_text()
    );
    // The in-flight run had a six-second turn ahead of it: shutdown does not
    // wait it out.
    assert!(
        interrupted.elapsed() < Duration::from_secs(20),
        "shutdown was not bounded: {:?}",
        interrupted.elapsed()
    );
    // A stopping daemon cold-starts nothing. `Run::start_with` brings up the
    // whole roster and cannot be cancelled once underway, so a queued trigger
    // picked up after the interrupt would spend the shutdown grace on work
    // nobody collects — and leave PTY children behind if it overran.
    let logs = serve.stderr_text();
    assert_eq!(
        logs.matches("starting run").count(),
        1,
        "only the in-flight trigger may have started a run; logs:\n{logs}"
    );
    Ok(())
}

fn wait_for_exit(serve: &mut Serving, within: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Some(status) = serve.child.try_wait().expect("wait on the daemon") {
            return status;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "the daemon did not exit within {within:?}; stderr:\n{}",
        serve.stderr_text()
    );
}

#[test]
fn the_queue_and_payload_limits_are_enforced() -> anyhow::Result<()> {
    let serve = serving("limits", "6");
    let (status, body) = serve.fire("", &"x".repeat(64 * 1024 + 1))?;
    assert_eq!(status, 413, "body: {body}");

    // One runs, QUEUE_CAP wait, the next is refused.
    for _ in 0..40_u32 {
        let (status, body) = serve.fire("", "queued")?;
        if status == 429 {
            let message = body["error"]["message"].as_str().unwrap_or_default();
            assert_eq!(body["error"]["code"], "queue_full");
            assert!(
                message.contains("32"),
                "the refusal must state the depth: {message}"
            );
            return Ok(());
        }
        assert_eq!(status, 202, "unexpected: {body}");
    }
    panic!("the queue never filled; stderr:\n{}", serve.stderr_text());
}
