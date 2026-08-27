#![expect(clippy::unwrap_used, reason = "assertions are the vocabulary of tests")]
#![expect(clippy::expect_used, reason = "assertions are the vocabulary of tests")]
#![expect(clippy::panic, reason = "assertions are the vocabulary of tests")]

//! `coretempod run` against a real `claude` (#3). The scripted fake agent the
//! rest of the suite uses cannot falsify assumptions about the real binary —
//! every bug that reached real usage (Enter glued to the text, vanished TUI
//! markers, leaked `CLAUDE_CODE_*`, the startup dialogs) was a property of
//! Claude Code or of PTY timing, not of `CoreTempo`'s logic. This test is the
//! one place those assumptions meet the binary.
//!
//! It is `#[ignore]`d because it needs a logged-in `claude` on PATH and spends
//! tokens (two Haiku turns). Run it with `./dev live`, which also builds the
//! workspace so `tempo` sits next to `coretempod` — the run hands agents the
//! `tempo` found beside its own executable.
//!
//! The agent works in `~/.coretempo/live-test/agent`, a fixed directory so
//! Claude Code trust is granted once (`trust_agent_dirs = true`) and reused,
//! instead of a fresh temp dir leaving one more trust entry in
//! `~/.claude.json` per run. Everything else (db, token, logs, tempo.toml) is
//! per-run scratch under the system temp dir. HOME is deliberately *not*
//! overridden: `isolated_config` agents share the operator's login through
//! `CLAUDE_SECURESTORAGE_CONFIG_DIR`, and trust is mirrored from the operator's
//! `~/.claude.json`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const DAEMON: &str = env!("CARGO_BIN_EXE_coretempod");
const TOKEN: &str = "12ab34cd56ef78ab90cd12ef34ab56cd78ef90ab12cd34ef56ab78cd90ef12ab";
const AGENT: &str = "solver";

/// A cold spawn draws the welcome box and may lose its first Enter (#54); the
/// hook that reports `idle` fires only once the session is up.
const SPAWN_DEADLINE: Duration = Duration::from_mins(1);
/// One Haiku turn plus the `tempo reply` it runs, with headroom for a slow API.
const ASK_WAIT_SECS: u64 = 90;
/// Auto-`/clear` fires after `idle_debounce_seconds` of stable idle.
const CLEAR_DEADLINE: Duration = Duration::from_secs(20);

struct Live {
    scratch: PathBuf,
    config: PathBuf,
    port: u16,
}

/// The spawned `coretempod`, killed on drop so a failed assertion cannot leave
/// it (and its `claude`) running after the test process moves on.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Refuses to start unless the binaries the run depends on are where it will
/// look for them; each message says how to fix it.
fn preflight() {
    let tempo = Path::new(DAEMON).with_file_name("tempo");
    assert!(
        tempo.is_file(),
        "no `tempo` next to {DAEMON}; the run hands agents the tempo beside its own \
         executable. Run `./dev live`, or `cargo build --workspace` first."
    );
    let on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("claude").is_file()));
    assert!(
        on_path,
        "no `claude` on PATH; this test spawns the real binary. Install Claude Code and \
         log in, then run `./dev live`."
    );
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn live() -> Live {
    preflight();
    let home = std::env::var_os("HOME").expect("HOME is set");
    let agent_dir = PathBuf::from(home).join(".coretempo/live-test/agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let scratch = std::env::temp_dir().join(format!("coretempo-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::write(scratch.join("token"), TOKEN).unwrap();

    let config = scratch.join("tempo.toml");
    std::fs::write(
        &config,
        format!(
            "[workflow]\nname = \"live\"\ndb = \"{db}\"\n\
             ask_timeout_minutes = 2\nidle_debounce_seconds = 1.0\n\
             [server]\ntrust_agent_dirs = true\n\
             [agents.{AGENT}]\ndir = \"{dir}\"\nmodel = \"haiku\"\n\
             permission_mode = \"bypassPermissions\"\nisolated_config = true\n\
             prompt = \"You answer arithmetic questions. Reply with just the number.\"\n",
            db = scratch.join("tempo.db").display(),
            dir = agent_dir.display(),
        ),
    )
    .unwrap();
    Live {
        scratch,
        config,
        port: free_port(),
    }
}

impl Live {
    fn spawn(&self) -> Daemon {
        let log = |name: &str| std::fs::File::create(self.scratch.join(name)).unwrap();
        let child = Command::new(DAEMON)
            .arg("run")
            .arg(&self.config)
            .arg("--port")
            .arg(self.port.to_string())
            .arg("--token-file")
            .arg(self.scratch.join("token"))
            // The scratch config is the only one this run may read.
            .env_remove("CORETEMPO_CONFIG")
            .stdout(Stdio::from(log("out.log")))
            .stderr(Stdio::from(log("err.log")))
            .spawn()
            .unwrap();
        Daemon(child)
    }

    fn logs(&self) -> String {
        let read =
            |name: &str| std::fs::read_to_string(self.scratch.join(name)).unwrap_or_default();
        format!("{}{}", read("out.log"), read("err.log"))
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn get(&self, path: &str) -> Option<serde_json::Value> {
        let mut res = http()
            .get(self.url(path))
            .header("Authorization", format!("Bearer {TOKEN}"))
            .call()
            .ok()?;
        (res.status().as_u16() == 200)
            .then(|| res.body_mut().read_json::<serde_json::Value>().ok())
            .flatten()
    }

    /// Polls `GET /v1/agents/<AGENT>` until its hook-reported state is `want`.
    fn wait_for_state(&self, want: &str, within: Duration) {
        let deadline = Instant::now() + within;
        let mut last = None;
        while Instant::now() < deadline {
            if let Some(info) = self.get(&format!("/v1/agents/{AGENT}")) {
                if info["state"] == want {
                    return;
                }
                last = Some(info);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "agent never reported `{want}` within {within:?}; last seen {last:?}; logs:\n{}",
            self.logs()
        );
    }

    /// An HTTP-origin ask, long-polled to its terminal status.
    fn ask(&self, body: &str) -> serde_json::Value {
        let mut res = http()
            .post(self.url(&format!("/v1/messages?wait={ASK_WAIT_SECS}")))
            .header("Authorization", format!("Bearer {TOKEN}"))
            .send_json(serde_json::json!({ "to": AGENT, "kind": "ask", "body": body }))
            .unwrap();
        let status = res.status().as_u16();
        let record: serde_json::Value = res.body_mut().read_json().unwrap();
        assert_eq!(
            status,
            200,
            "ask did not settle: {record}; logs:\n{}",
            self.logs()
        );
        record
    }

    /// The queue logs each auto-`/clear` it types; wait for the `n`th.
    fn wait_for_clears(&self, n: usize, within: Duration) {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.logs().matches("typed /clear").count() >= n {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "auto-/clear #{n} was not typed within {within:?}; logs:\n{}",
            self.logs()
        );
    }

    fn wait_for_health(&self, within: Duration) {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.get("/v1/health").is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("run never became healthy; logs:\n{}", self.logs());
    }
}

fn http() -> ureq::Agent {
    let cfg = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(ASK_WAIT_SECS + 10)))
        .build();
    ureq::Agent::new_with_config(cfg)
}

fn stop(daemon: &mut Daemon, live: &Live) {
    let child = &mut daemon.0;
    let ok = Command::new("kill")
        .arg("-INT")
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(ok.success(), "could not signal the daemon");
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "expected a clean exit after ctrl-c, got {status:?}; logs:\n{}",
                live.logs()
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    panic!(
        "the daemon did not exit within 30s of SIGINT; logs:\n{}",
        live.logs()
    );
}

fn assert_reply(record: &serde_json::Value, want: &str, live: &Live) {
    assert_eq!(
        record["status"],
        "replied",
        "ask did not get a reply: {record}; logs:\n{}",
        live.logs()
    );
    assert_eq!(record["code"], 0, "reply code: {record}");
    let reply = record["reply"].as_str().unwrap_or_default();
    assert!(
        reply.contains(want),
        "expected the reply to contain {want:?}, got {reply:?}"
    );
}

/// One real agent: it reports `idle` through its hooks, answers an ask via
/// `tempo reply`, gets auto-`/clear`ed once its turn settles, and answers a
/// second ask typed into the freshly cleared prompt (the path that regressed
/// once already).
#[test]
#[ignore = "spawns a real claude and spends tokens; run ./dev live"]
fn a_real_claude_round_trips_and_survives_auto_clear() {
    let live = live();
    let mut daemon = live.spawn();
    live.wait_for_health(SPAWN_DEADLINE);

    live.wait_for_state("idle", SPAWN_DEADLINE);

    let first = live.ask("What is 2+2? Reply with just the number.");
    assert_reply(&first, "4", &live);

    live.wait_for_clears(1, CLEAR_DEADLINE);
    live.wait_for_state("idle", SPAWN_DEADLINE);

    let second = live.ask("What is 3+4? Reply with just the number.");
    assert_reply(&second, "7", &live);

    stop(&mut daemon, &live);
}
