//! Shared `coretempod serve` harness: a scratch home with a scripted fake
//! `claude` on PATH, a daemon child, and an HTTP client for its public port.
#![expect(
    dead_code,
    reason = "each integration-test crate uses a subset of this harness"
)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) const DAEMON: &str = env!("CARGO_BIN_EXE_coretempod");

/// A fake `claude`: reports turn boundaries like the real hooks do and answers
/// every ask it is given. Speaks HTTP over bash's `/dev/tcp` so the test needs
/// no client binary on PATH.
///
/// It replies `ok` unless `FAKE_AGENT_REPLY` overrides the body, and retries
/// once with `FAKE_AGENT_REPAIR` when the server rejects a reply off-schema —
/// the agent-side half of the in-turn repair loop. Both are spliced into a JSON
/// string, so a JSON body must arrive already escaped.
///
/// Every prompt it sees lands in `prompts.log` in its working directory, so a
/// test can assert on what was actually typed at the agent.
pub(crate) const FAKE_AGENT: &str = r#"#!/bin/bash
me="$CORETEMPO_AGENT_ID"
post() {
  exec 3<>"/dev/tcp/127.0.0.1/$CORETEMPO_PORT" || return 1
  printf 'POST %s HTTP/1.1\r\nHost: 127.0.0.1\r\n' "$1" >&3
  printf 'Authorization: Bearer %s\r\n' "$CORETEMPO_TOKEN" >&3
  printf 'X-CoreTempo-Agent: %s\r\n' "$me" >&3
  printf 'Content-Type: application/json\r\nContent-Length: %d\r\n' "${#2}" >&3
  printf 'Connection: close\r\n\r\n%s' "$2" >&3
  cat <&3
  exec 3>&-
}
reply() {
  res=$(post "/v1/messages/$1/reply" "{\"code\":0,\"body\":\"$2\"}")
  case "$res" in
    *schema_validation_failed*)
      [ -n "$FAKE_AGENT_REPAIR" ] &&
        post "/v1/messages/$1/reply" "{\"code\":0,\"body\":\"$FAKE_AGENT_REPAIR\"}" >/dev/null
      ;;
  esac
}
post "/v1/agents/$me/state" '{"state":"idle"}' >/dev/null
last=""
while IFS= read -r line; do
  [[ "$line" =~ (m-[0-9a-f]+) ]] || continue
  printf '%s\n' "$line" >>"$PWD/prompts.log"
  id="${BASH_REMATCH[1]}"
  [ "$id" = "$last" ] && continue
  last="$id"
  post "/v1/agents/$me/state" '{"state":"working"}' >/dev/null
  sleep "${FAKE_AGENT_DELAY:-0}"
  reply "$id" "${FAKE_AGENT_REPLY:-ok}"
  post "/v1/agents/$me/state" '{"state":"idle"}' >/dev/null
done
"#;

pub(crate) const TOKEN: &str = "ab12cd34ef56ab78cd90ef12ab34cd56ef78ab90cd12ef34ab56cd78ef90ab12";

/// Waits for the daemon to report the port it bound, reading its startup log.
///
/// Picking a free port here and handing it over is a TOCTOU: the probe socket
/// is closed again before the daemon binds, so anything on the box — a peer
/// test doing the same thing included — can take that port in the gap, and the
/// daemon dies with "Address already in use". The tests bind `--port 0` and let
/// the kernel pick one that is genuinely free, then read back which.
fn wait_for_bound_port(root: &Path) -> Option<u16> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let log = std::fs::read_to_string(root.join("out.log")).unwrap_or_default();
        if let Some(port) = strip_ansi(&log).lines().find_map(listening_port) {
            return Some(port);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// The port out of `serve listening; ... addr=127.0.0.1:41235 workflow=...`.
fn listening_port(line: &str) -> Option<u16> {
    if !line.contains("serve listening") {
        return None;
    }
    line.split("addr=")
        .nth(1)?
        .split_whitespace()
        .next()?
        .rsplit(':')
        .next()?
        .parse()
        .ok()
}

/// Drops the colour escapes tracing writes even into a file, so `addr=` is one
/// substring rather than three.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        for c in chars.by_ref() {
            if c.is_ascii_alphabetic() {
                break;
            }
        }
    }
    out
}

pub(crate) fn agent() -> ureq::Agent {
    let cfg = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_mins(1)))
        .build();
    ureq::Agent::new_with_config(cfg)
}

pub(crate) struct Scratch {
    pub(crate) root: PathBuf,
    pub(crate) config: PathBuf,
    pub(crate) home: PathBuf,
    pub(crate) bin: PathBuf,
}

/// A scratch home, a fake `claude` on PATH, and a tempo.toml whose agents and
/// flows come from `tail` — `{dir}` in it is replaced with the scratch root.
pub(crate) fn scratch(name: &str, tail: &str) -> Scratch {
    let root = std::env::temp_dir().join(format!("coretempo-serve-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    let bin = root.join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    // The daemon preflights Claude Code trust for every agent dir before it
    // spawns anything, and a scratch root has never been opened in `claude`.
    // Granting through the user config keeps the generated tempo.toml free of
    // a [server] table several tails already open for themselves.
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
    std::fs::write(
        &config,
        format!(
            // The file's port is never bound in serve mode — the listener takes
            // --port and each triggered run binds an ephemeral one — so this is
            // a literal rather than another racy free_port().
            "[workflow]\nname = \"serve-{name}\"\nport = 4820\ndb = \"{db}\"\n\
             ask_timeout_minutes = 1\nidle_debounce_seconds = 0.3\n{tail}",
            db = root.join("tempo.db").display(),
            tail = tail.replace("{dir}", &root.display().to_string()),
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

/// [`scratch`] with the trust grant taken away again: the shape the trust
/// preflight must refuse.
pub(crate) fn scratch_without_trust(name: &str, tail: &str) -> Scratch {
    let scratch = scratch(name, tail);
    std::fs::remove_file(scratch.home.join(".coretempo/config.toml")).unwrap();
    scratch
}

/// One agent, one webhook flow: the single-flow shape the serve tests assume.
pub(crate) const WEBHOOK: &str = "[agents.worker]\ndir = \"{dir}\"\nprompt = \"You reply.\"\n\
                                  [flows.main]\nagents = [\"worker\"]\n\
                                  trigger = { type = \"webhook\", \
                                  edge = { to = \"worker\", kind = \"ask\" } }\n";

pub(crate) struct Serving {
    pub(crate) child: Child,
    pub(crate) port: u16,
    pub(crate) scratch: Scratch,
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

pub(crate) fn daemon_command(scratch: &Scratch, sub: &str, port: u16, delay: &str) -> Command {
    let mut cmd = daemon_command_without_token(scratch, sub, port, delay);
    cmd.arg("--token-file").arg(scratch.root.join("token"));
    cmd
}

/// [`daemon_command`] with nothing provisioning a token: no `--token-file`, and
/// no `CORETEMPO_TOKEN*` inherited from the developer's shell. This is the
/// shape serve mode has to refuse (#57).
pub(crate) fn daemon_command_without_token(
    scratch: &Scratch,
    sub: &str,
    port: u16,
    delay: &str,
) -> Command {
    let mut cmd = Command::new(DAEMON);
    let path = std::env::var("PATH").unwrap_or_default();
    cmd.arg(sub)
        .arg(&scratch.config)
        .arg("--port")
        .arg(port.to_string())
        .env("HOME", &scratch.home)
        .env("PATH", format!("{}:{path}", scratch.bin.display()))
        .env("FAKE_AGENT_DELAY", delay)
        .env("RUST_LOG", "info")
        // A developer whose own CORETEMPO_CONFIG grants trust would otherwise
        // make the refusal tests pass for the wrong reason — or fail. Same for
        // a token in their environment: these tests provision one through
        // `--token-file`, or deliberately not at all.
        .env_remove("CORETEMPO_CONFIG")
        .env_remove("CORETEMPO_TOKEN")
        .env_remove("CORETEMPO_TOKEN_FILE");
    cmd
}

pub(crate) fn log_file(root: &Path, name: &str) -> std::fs::File {
    std::fs::File::create(root.join(name)).unwrap()
}

/// Starts `coretempod serve` on the single-webhook-flow fixture.
pub(crate) fn serving(name: &str, delay: &str) -> Serving {
    serving_flows(name, WEBHOOK, delay)
}

/// Starts `coretempod serve` on `tail`'s agents and flows, waiting for health.
pub(crate) fn serving_flows(name: &str, tail: &str, delay: &str) -> Serving {
    serving_flows_env(name, tail, delay, &[])
}

/// [`serving_flows`] with extra environment — the fake agent inherits it, so
/// this is how a test scripts what that agent replies.
pub(crate) fn serving_flows_env(
    name: &str,
    tail: &str,
    delay: &str,
    env: &[(&str, &str)],
) -> Serving {
    serving_scratch(scratch(name, tail), delay, env)
}

/// [`serving_flows`] on a scratch that grants no trust of its own — for the
/// test that watches the daemon grant it from the workflow key instead.
pub(crate) fn serving_flows_without_user_config(name: &str, tail: &str, delay: &str) -> Serving {
    serving_scratch(scratch_without_trust(name, tail), delay, &[])
}

/// [`serving_flows_env`] on a scratch that grants no trust of its own — for the
/// test that points `CORETEMPO_CONFIG` at a user config that does.
pub(crate) fn serving_flows_env_without_user_config(
    name: &str,
    tail: &str,
    delay: &str,
    env: &[(&str, &str)],
) -> Serving {
    serving_scratch(scratch_without_trust(name, tail), delay, env)
}

/// Boots `serve` on an already-built scratch and waits until it is healthy.
fn serving_scratch(scratch: Scratch, delay: &str, env: &[(&str, &str)]) -> Serving {
    let mut command = daemon_command(&scratch, "serve", 0, delay);
    for (key, value) in env {
        command.env(key, value);
    }
    let child = command
        .stdout(Stdio::from(log_file(&scratch.root, "out.log")))
        .stderr(Stdio::from(log_file(&scratch.root, "err.log")))
        .spawn()
        .unwrap();
    // Port 0 until the daemon says otherwise; the child is already owned so a
    // panic below still stops it.
    let mut serving = Serving {
        child,
        port: 0,
        scratch,
    };
    serving.port = wait_for_bound_port(&serving.scratch.root).unwrap_or_else(|| {
        panic!(
            "serve never logged the port it bound; stderr:\n{}",
            serving.stderr_text()
        )
    });
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
    pub(crate) fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Both daemon logs: tracing goes to stdout, anyhow's exit error to stderr.
    pub(crate) fn stderr_text(&self) -> String {
        let read =
            |name: &str| std::fs::read_to_string(self.scratch.root.join(name)).unwrap_or_default();
        format!("{}{}", read("out.log"), read("err.log"))
    }

    pub(crate) fn get(&self, path: &str) -> anyhow::Result<(u16, serde_json::Value)> {
        self.get_as(path, Some(TOKEN))
    }

    /// GET with an explicit bearer token, or none at all.
    pub(crate) fn get_as(
        &self,
        path: &str,
        token: Option<&str>,
    ) -> anyhow::Result<(u16, serde_json::Value)> {
        let mut req = agent().get(self.url(path));
        if let Some(token) = token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let mut res = req.call()?;
        let status = res.status().as_u16();
        Ok((status, json_of(res.body_mut().read_to_string()?)))
    }

    /// POSTs the [`WEBHOOK`] fixture's only flow, `main`.
    pub(crate) fn fire(&self, query: &str, body: &str) -> anyhow::Result<(u16, serde_json::Value)> {
        self.fire_as(query, body, Some(TOKEN))
    }

    /// POST `main`'s trigger with an explicit bearer token, or none at all.
    pub(crate) fn fire_as(
        &self,
        query: &str,
        body: &str,
        token: Option<&str>,
    ) -> anyhow::Result<(u16, serde_json::Value)> {
        let mut req = agent()
            .post(self.url(&format!("/v1/flows/main/trigger{query}")))
            .header("Content-Type", "text/plain");
        if let Some(token) = token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let mut res = req.send(body)?;
        let status = res.status().as_u16();
        Ok((status, json_of(res.body_mut().read_to_string()?)))
    }

    /// POSTs a named flow's trigger endpoint with the daemon token.
    pub(crate) fn fire_flow(
        &self,
        flow: &str,
        body: &str,
    ) -> anyhow::Result<(u16, serde_json::Value)> {
        let mut res = agent()
            .post(self.url(&format!("/v1/flows/{flow}/trigger")))
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header("Content-Type", "text/plain")
            .send(body)?;
        let status = res.status().as_u16();
        Ok((status, json_of(res.body_mut().read_to_string()?)))
    }

    /// Fires a named flow, asserting acceptance, and returns the trigger id.
    pub(crate) fn fire_flow_ok(&self, flow: &str, body: &str) -> String {
        let (status, json) = self.fire_flow(flow, body).unwrap();
        assert_eq!(status, 202, "flow '{flow}' trigger rejected: {json}");
        json["trigger_id"].as_str().unwrap().to_string()
    }

    /// GET with a `Host` the daemon should not answer to.
    pub(crate) fn get_with_host(
        &self,
        path: &str,
        host: &str,
    ) -> anyhow::Result<(u16, serde_json::Value)> {
        let mut res = agent()
            .get(self.url(path))
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header("Host", host)
            .call()?;
        let status = res.status().as_u16();
        Ok((status, json_of(res.body_mut().read_to_string()?)))
    }

    /// Fires `main`, asserting it was accepted, and returns its id.
    pub(crate) fn fire_ok(&self, body: &str) -> String {
        let (status, json) = self.fire("", body).unwrap();
        assert_eq!(status, 202, "trigger rejected: {json}");
        json["trigger_id"].as_str().unwrap().to_string()
    }

    pub(crate) fn status_of(&self, id: &str) -> serde_json::Value {
        self.get(&format!("/v1/trigger/{id}")).unwrap().1
    }

    /// Polls until `id` is no longer queued or running.
    pub(crate) fn settled(&self, id: &str) -> serde_json::Value {
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

    pub(crate) fn interrupt(&self) {
        let ok = Command::new("kill")
            .arg("-INT")
            .arg(self.child.id().to_string())
            .status()
            .unwrap();
        assert!(ok.success(), "could not signal the daemon");
    }
}

pub(crate) fn json_of(text: String) -> serde_json::Value {
    serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
}

pub(crate) fn wait_for_exit(serve: &mut Serving, within: Duration) -> std::process::ExitStatus {
    let Some(status) = poll_exit(&mut serve.child, within) else {
        panic!(
            "the daemon did not exit within {within:?}; stderr:\n{}",
            serve.stderr_text()
        );
    };
    status
}

/// Waits for a daemon child with no [`Serving`] wrapper around it: a boot
/// refusal never binds a port, so there is nothing to build one from. Kills the
/// child before panicking, so a daemon that wrongly stayed up cannot outlive
/// the test.
pub(crate) fn wait_for_child_exit(child: &mut Child, within: Duration) -> std::process::ExitStatus {
    let Some(status) = poll_exit(child, within) else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("the daemon did not exit within {within:?}");
    };
    status
}

fn poll_exit(child: &mut Child, within: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("wait on the daemon") {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}
