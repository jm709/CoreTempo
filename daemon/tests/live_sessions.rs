#![expect(clippy::unwrap_used, reason = "assertions are the vocabulary of tests")]
#![expect(clippy::expect_used, reason = "assertions are the vocabulary of tests")]
#![expect(clippy::panic, reason = "assertions are the vocabulary of tests")]
#![expect(
    clippy::print_stdout,
    reason = "`--nocapture` is how a live run reports what it actually probed"
)]

//! `coretempod sessions` against a real `claude` (spec 2026-08-27 §10).
//!
//! The scripted fake agent the rest of the sessions suite uses answers the
//! hook route over HTTP and echoes lines; it cannot falsify the two claims
//! this file exists for. Both are properties of the real binary:
//!
//! * a stopped session resumes its *conversation* — `--resume
//!   <claude_session_id>` with the id the `SessionStart` hook reported, and the
//!   model still knows what it was told before the stop;
//! * a `claude_session_id` Claude Code does not recognise makes the resumed
//!   process **exit**, rather than silently starting a fresh conversation under
//!   a row that claims to be resumed (spec §2).
//!
//! It also probes the decision the scripted suite can only assert against its
//! own fake `.claude.json`: a derived worktree grant copies the project root's
//! `.mcp.json` approvals (spec §5). The repository carries a committed
//! `.mcp.json` approved for the root, and the keys the grant wrote for the
//! worktree are read back out of the operator's real `.claude.json`. Reaching
//! `idle` is *not* additional evidence about the dialog: these sessions run in
//! `bypassPermissions`, which skips the "New MCP server found" prompt anyway.
//!
//! `#[ignore]`d because it needs a logged-in `claude` on PATH and spends
//! tokens (two Haiku turns). Run it with `./dev live`, which builds the
//! workspace first so `tempo` sits beside `coretempod` — the daemon hands each
//! session the `tempo` found next to its own executable.
//!
//! The project is `~/.coretempo/live-test/repo`, a fixed directory so Claude
//! Code trust is granted once and reused. Everything else (root, db, token,
//! worktrees, logs) is per-run scratch under the system temp dir, so the
//! operator's own `~/.coretempo/sessions` daemon and lock are never touched.
//! HOME is deliberately *not* overridden: `isolated_config` sessions share the
//! operator's login through `CLAUDE_SECURESTORAGE_CONFIG_DIR`, and trust is
//! mirrored from the operator's `~/.claude.json`.
//!
//! Each run leaves one `projects["/tmp/coretempo-live-sessions-…"]` entry in
//! that file: worktree paths are per-run (random project id and slug), and the
//! derived grants a session writes are not removed on delete — the known gap
//! recorded in contracts amendment 47. The worktrees themselves, their
//! `session/*` branches and the scratch root are cleaned up on the way out.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use coretempo_core::sessions::SessionStore;
use coretempo_core::sessions::trust::MCP_APPROVAL_KEYS;
use coretempo_core::trust::TrustStore;
use coretempo_core::types::{AgentId, SessionsApiFile};

const DAEMON: &str = env!("CARGO_BIN_EXE_coretempod");

/// A cold spawn draws its welcome box before any hook fires, and an
/// `isolated_config` session seeds a managed config dir on the way.
const SPAWN_DEADLINE: Duration = Duration::from_secs(90);
/// One Haiku turn, with headroom for a slow API.
const TURN_DEADLINE: Duration = Duration::from_secs(120);
/// A rejected `--resume` claim is a startup failure, not a turn.
const EXIT_DEADLINE: Duration = Duration::from_secs(30);
/// The Enter-as-separate-write gotcha applies to raw writes too.
const SUBMIT_DELAY: Duration = Duration::from_millis(400);
/// How long a submitted prompt has to move the session off `idle`.
const SUBMIT_VERIFY: Duration = Duration::from_secs(5);
/// Extra Enters a swallowed one is worth before the write is called a failure.
const MAX_ENTER_RESENDS: u32 = 3;
/// The `.mcp.json` server the live repository declares.
const MCP_SERVER: &str = "livecheck";

/// Both tests drive real `claude` processes against the same live repository,
/// and `git worktree add` takes that repository's index lock. Serialize them
/// however the file is invoked, not only under `--test-threads=1`.
static SERIAL: Mutex<()> = Mutex::new(());

// --- fixtures ---------------------------------------------------------------

/// Refuses to start unless the binaries the daemon depends on are where it
/// will look for them; each message says how to fix it.
fn preflight() {
    let tempo = Path::new(DAEMON).with_file_name("tempo");
    assert!(
        tempo.is_file(),
        "no `tempo` next to {DAEMON}; the daemon hands sessions the tempo beside its own \
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

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args([
            "-c",
            "user.name=coretempo-live",
            "-c",
            "user.email=live@coretempo.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git")
}

fn git_ok(dir: &Path, args: &[&str]) {
    let out = git(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// One server definition from the operator's `~/.claude.json`, reused verbatim
/// so the probe declares something that actually starts. `None` when the
/// operator has no user-scoped MCP server to borrow — the probe is then
/// skipped rather than faked with `{"mcpServers": {}}`, which would prove
/// nothing.
fn mcp_probe_server() -> Option<serde_json::Value> {
    let path = coretempo_core::claude_config::operator_claude_json()?;
    let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let servers = doc.get("mcpServers")?.as_object()?;
    let (_, server) = servers.iter().next()?;
    Some(server.clone())
}

/// `~/.coretempo/live-test/repo`: a fixed one-commit repository, so Claude
/// Code trust for it is granted once and reused instead of one more entry in
/// `~/.claude.json` per run. Its committed `.mcp.json` is the approval probe.
fn live_repo() -> (PathBuf, bool) {
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"));
    let repo = home.join(".coretempo/live-test/repo");
    std::fs::create_dir_all(&repo).unwrap();
    if !repo.join(".git").exists() {
        git_ok(&repo, &["init", "-q", "-b", "main"]);
    }
    std::fs::write(repo.join("README"), "CoreTempo live session fixture\n").unwrap();
    let probe = mcp_probe_server();
    if let Some(server) = &probe {
        let doc = serde_json::json!({ "mcpServers": { MCP_SERVER: server } });
        std::fs::write(
            repo.join(".mcp.json"),
            serde_json::to_string_pretty(&doc).unwrap(),
        )
        .unwrap();
    }
    git_ok(&repo, &["add", "-A"]);
    // `diff --cached --quiet` fails when something is staged; on every run
    // after the first there is nothing, and an empty commit would pile up.
    if !git(&repo, &["diff", "--cached", "--quiet"])
        .status
        .success()
    {
        git_ok(&repo, &["commit", "-q", "-m", "live session fixture"]);
    }
    let repo = std::fs::canonicalize(&repo).unwrap();
    // The operator-side half of the probe: the server enabled for the project
    // root, exactly as answering the dialog once would leave it.
    let approved = probe.is_some() && approve_mcp_for(&repo);
    (repo, approved)
}

fn approve_mcp_for(repo: &Path) -> bool {
    let Some(store) = TrustStore::from_env() else {
        return false;
    };
    let mut values = serde_json::Map::new();
    values.insert(
        "enabledMcpjsonServers".to_string(),
        serde_json::json!([MCP_SERVER]),
    );
    store.grant_with_keys(repo, &values).is_ok()
}

// --- the live daemon --------------------------------------------------------

struct Live {
    /// Held for the test's life; released once [`Drop`] has stopped the
    /// daemon, so the next test never overlaps this one's `claude`.
    #[expect(dead_code, reason = "held for its Drop")]
    serial: MutexGuard<'static, ()>,
    scratch: PathBuf,
    root: PathBuf,
    repo: PathBuf,
    child: Child,
    api: SessionsApiFile,
    project: String,
}

/// Stops the daemon (and with it every `claude` it owns), then leaves the live
/// repository as it found it: worktrees under the deleted scratch pruned, and
/// the `session/*` branches they were created on removed.
impl Drop for Live {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            // Interrupt rather than kill: the daemon owns PTY children, and
            // SIGKILL would orphan them.
            let _ = Command::new("kill")
                .arg("-INT")
                .arg(self.child.id().to_string())
                .stderr(Stdio::null())
                .status();
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                if matches!(self.child.try_wait(), Ok(Some(_)) | Err(_)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.scratch);
        let _ = git(&self.repo, &["worktree", "prune"]);
        let out = git(
            &self.repo,
            &[
                "for-each-ref",
                "--format=%(refname:short)",
                "refs/heads/session",
            ],
        );
        for branch in String::from_utf8_lossy(&out.stdout).lines() {
            let _ = git(&self.repo, &["branch", "-D", branch.trim()]);
        }
    }
}

fn http() -> ureq::Agent {
    let cfg = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build();
    ureq::Agent::new_with_config(cfg)
}

/// `api.json` under `root`, but only once it parses *and* names this daemon:
/// the truncated window `write_private_file` opens while it writes must never
/// be mistaken for a published port.
fn read_api_file(root: &Path, pid: u32) -> Option<SessionsApiFile> {
    let text = std::fs::read_to_string(root.join("api.json")).ok()?;
    let file: SessionsApiFile = serde_json::from_str(&text).ok()?;
    (file.pid == pid).then_some(file)
}

fn live(name: &str) -> Live {
    let serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    preflight();
    let (repo, mcp_approved) = live_repo();
    println!(
        "live repo {} (mcp probe: {})",
        repo.display(),
        if mcp_approved {
            MCP_SERVER
        } else {
            "skipped — no user-scoped mcpServers in the operator's .claude.json"
        }
    );

    let scratch = std::env::temp_dir().join(format!(
        "coretempo-live-sessions-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    // The daemon reads `trust_agent_dirs` from the user config alone (no
    // workflow can opt in), and this scratch file is the only one it may read.
    std::fs::write(scratch.join("config.toml"), "trust_agent_dirs = true\n").unwrap();
    let root = scratch.join("sessions");

    let log = |n: &str| Stdio::from(std::fs::File::create(scratch.join(n)).unwrap());
    let child = Command::new(DAEMON)
        .args([
            "sessions",
            "--root",
            &root.display().to_string(),
            "--port",
            "0",
        ])
        .env("CORETEMPO_CONFIG", scratch.join("config.toml"))
        .env("RUST_LOG", "info")
        .env_remove("CORETEMPO_TOKEN")
        .env_remove("CORETEMPO_TOKEN_FILE")
        .env_remove("CORETEMPO_PORT")
        .env_remove("CORETEMPO_AGENT_ID")
        .stdout(log("out.log"))
        .stderr(log("err.log"))
        .spawn()
        .unwrap();

    // Owned from here on, so every assertion below still stops the daemon.
    let mut live = Live {
        serial,
        scratch,
        root,
        repo,
        child,
        api: SessionsApiFile {
            port: 0,
            token: coretempo_core::types::Token(String::new()),
            pid: 0,
        },
        project: String::new(),
    };
    let pid = live.child.id();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(file) = read_api_file(&live.root, pid) {
            live.api = file;
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the sessions daemon never wrote its api.json; logs:\n{}",
            live.logs()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let (200, body) = live.get("/v1/health") {
            assert_eq!(body["ok"], true, "health: {body}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the sessions daemon never became healthy; logs:\n{}",
            live.logs()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let path = live.repo.display().to_string();
    let (status, project) = live.post("/v1/projects", &serde_json::json!({ "path": path }));
    assert_eq!(status, 201, "register the live repo: {project}");
    live.project = project["id"].as_str().unwrap().to_string();
    live
}

impl Live {
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.api.port)
    }

    fn logs(&self) -> String {
        let read = |path: PathBuf| std::fs::read_to_string(path).unwrap_or_default();
        format!(
            "{}{}{}",
            read(self.scratch.join("out.log")),
            read(self.scratch.join("err.log")),
            read(self.root.join("daemon.log")),
        )
    }

    fn get(&self, path: &str) -> (u16, serde_json::Value) {
        let mut res = http()
            .get(self.url(path))
            .header("Authorization", format!("Bearer {}", self.api.token.0))
            .call()
            .expect("GET the sessions daemon");
        let status = res.status().as_u16();
        let body = res.body_mut().read_to_string().unwrap_or_default();
        (status, serde_json::from_str(&body).unwrap_or_default())
    }

    fn post(&self, path: &str, body: &serde_json::Value) -> (u16, serde_json::Value) {
        let mut res = http()
            .post(self.url(path))
            .header("Authorization", format!("Bearer {}", self.api.token.0))
            .send_json(body)
            .expect("POST the sessions daemon");
        let status = res.status().as_u16();
        let body = res.body_mut().read_to_string().unwrap_or_default();
        (status, serde_json::from_str(&body).unwrap_or_default())
    }

    /// `POST /v1/sessions` with this run's project filled in; returns the id.
    fn create(&self, mut body: serde_json::Value) -> String {
        body["project"] = serde_json::json!(self.project);
        let (status, view) = self.post("/v1/sessions", &body);
        assert_eq!(status, 201, "create: {view}; logs:\n{}", self.logs());
        view["id"].as_str().unwrap().to_string()
    }

    fn view(&self, id: &str) -> serde_json::Value {
        let (status, view) = self.get(&format!("/v1/sessions/{id}"));
        assert_eq!(status, 200, "get {id}: {view}");
        view
    }

    fn stop(&self, id: &str) {
        let (status, view) = self.post(&format!("/v1/sessions/{id}/stop"), &serde_json::json!({}));
        assert_eq!(status, 200, "stop {id}: {view}; logs:\n{}", self.logs());
    }

    fn resume(&self, id: &str) -> serde_json::Value {
        let (status, body) =
            self.post(&format!("/v1/sessions/{id}/resume"), &serde_json::json!({}));
        assert_eq!(status, 200, "resume {id}: {body}; logs:\n{}", self.logs());
        body
    }

    /// Deletes the session and its worktree, so the live repository does not
    /// collect one `session/*` branch per run.
    fn delete(&self, id: &str) {
        let mut res = http()
            .delete(self.url(&format!(
                "/v1/sessions/{id}?remove_worktree=true&force=true"
            )))
            .header("Authorization", format!("Bearer {}", self.api.token.0))
            .call()
            .expect("DELETE the session");
        let status = res.status().as_u16();
        let body = res.body_mut().read_to_string().unwrap_or_default();
        assert_eq!(status, 200, "delete {id}: {body}");
    }

    fn post_pty(&self, id: &str, body: &str) {
        let res = http()
            .post(self.url(&format!("/v1/sessions/{id}/pty")))
            .header("Authorization", format!("Bearer {}", self.api.token.0))
            .header("Content-Type", "application/octet-stream")
            .send(body)
            .expect("write to the session PTY");
        assert_eq!(res.status().as_u16(), 204);
    }

    /// Types into the session's PTY and does not return until the turn has
    /// actually started.
    ///
    /// The text and the Enter are separate writes: glued together, the prompt
    /// is left typed but unsubmitted whenever Claude Code is rebuilding its
    /// input box. Even a separate Enter is dropped by a session still drawing
    /// itself (#54), which a `--resume` spawn does at length, so the Enter is
    /// resent while the session still reads `idle`. `POST …/pty` is raw by
    /// contract — the injection queue's submit verification is not on this
    /// path, and an attached human is the one who presses Enter again.
    fn write_and_submit(&self, id: &str, text: &str) {
        self.post_pty(id, text);
        std::thread::sleep(SUBMIT_DELAY);
        for _ in 0..=MAX_ENTER_RESENDS {
            self.post_pty(id, "\r");
            let deadline = Instant::now() + SUBMIT_VERIFY;
            while Instant::now() < deadline {
                if self.view(id)["state"] != "idle" {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        panic!(
            "the session never left idle after {} Enters; logs:\n{}",
            MAX_ENTER_RESENDS + 1,
            self.logs()
        );
    }

    fn wait_state(&self, id: &str, want: &str, within: Duration) {
        let deadline = Instant::now() + within;
        let mut last = serde_json::Value::Null;
        while Instant::now() < deadline {
            let view = self.view(id);
            if view["state"] == want {
                return;
            }
            last = view;
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!(
            "session {id} never reported `{want}` within {within:?}; last seen {last}; logs:\n{}",
            self.logs()
        );
    }

    /// Everything the PTY emitted since `cursor`, decoded from the SSE stream.
    /// The replay arrives as soon as the stream opens; the read then blocks
    /// until the agent's global timeout, which is the signal to stop.
    fn ring_tail(&self, id: &str, cursor: u64) -> String {
        let cfg = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(5)))
            .build();
        let mut res = ureq::Agent::new_with_config(cfg)
            .get(self.url(&format!("/v1/sessions/{id}/pty?since={cursor}")))
            .header("Authorization", format!("Bearer {}", self.api.token.0))
            .call()
            .expect("open the PTY stream");
        assert_eq!(res.status().as_u16(), 200);
        let mut reader = res.body_mut().as_reader();
        let mut raw = Vec::new();
        let mut buf = [0u8; 8192];
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
            }
        }
        let text = String::from_utf8_lossy(&raw).into_owned();
        let mut out = Vec::new();
        for line in text.lines() {
            let Some(json) = line.strip_prefix("data:") else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<serde_json::Value>(json.trim()) else {
                continue;
            };
            if let Some(b64) = event["b64"].as_str() {
                out.extend(b64_decode(b64));
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// The row the daemon reads on the next resume, edited behind its back —
    /// the only way to hand Claude Code a session id it has never issued.
    fn set_claude_session_id(&self, id: &str, sid: &str) {
        let db = self.root.join("sessions.db");
        let (id, sid) = (AgentId(id.to_string()), sid.to_string());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let store = SessionStore::open(&db).unwrap();
                assert!(
                    store.set_claude_session_id(&id, sid).await.unwrap(),
                    "no session row to edit"
                );
            });
    }
}

/// Standard base64, padding and any stray character ignored: enough to decode
/// the PTY stream's chunks without a dependency the daemon does not have.
fn b64_decode(text: &str) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let (mut acc, mut bits, mut out) = (0u32, 0u32, Vec::new());
    for byte in text.bytes() {
        let Some(value) = ALPHABET.iter().position(|c| *c == byte) else {
            continue;
        };
        acc = (acc << 6) | u32::try_from(value).unwrap_or(0);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xff).unwrap_or(0));
        }
    }
    out
}

// --- the tests --------------------------------------------------------------

/// A worktree session remembers across stop and resume: the conversation the
/// `SessionStart` hook identified is the one `--resume` reopens.
///
/// The question asks for the word *in lowercase*, which no earlier screen ever
/// showed — a redraw of the replayed transcript could satisfy an uppercase
/// match without the model having remembered anything.
#[test]
#[ignore = "spawns a real claude and spends tokens; run ./dev live"]
fn a_real_session_remembers_across_stop_and_resume() {
    let live = live("resume");
    let id = live.create(serde_json::json!({
        "worktree": true,
        "model": "haiku",
        "isolated_config": true,
        "permission_mode": "bypassPermissions",
        "prompt": "Remember the word PELICAN. Reply with just: ok"
    }));
    live.wait_state(&id, "working", SPAWN_DEADLINE); // the prompt was submitted
    live.wait_state(&id, "idle", TURN_DEADLINE); //     and answered

    let view = live.view(&id);
    let worktree = PathBuf::from(view["worktree"]["path"].as_str().expect("a worktree"));
    println!("worktree {} on {}", worktree.display(), view["branch"]);
    assert_mcp_approvals_copied(&live.repo, &worktree);
    let sid = view["claude_session_id"]
        .as_str()
        .expect("SessionStart delivered a session id")
        .to_string();

    live.stop(&id);
    let resumed = live.resume(&id);
    assert_eq!(resumed["resumed"], true, "{resumed}");
    live.wait_state(&id, "idle", SPAWN_DEADLINE);

    let cursor = live.view(&id)["pty_cursor"].as_u64().expect("a pty cursor");
    live.write_and_submit(
        &id,
        "What word did I ask you to remember? Reply with just that word, in lowercase.",
    );
    live.wait_state(&id, "idle", TURN_DEADLINE);

    let tail = live.ring_tail(&id, cursor);
    let unwrapped: String = tail.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        tail.contains("pelican") || unwrapped.contains("pelican"),
        "resume did not carry the conversation; PTY tail since {cursor}:\n{tail}"
    );
    let after = live.view(&id);
    assert_eq!(
        after["claude_session_id"], sid,
        "resume keeps the same id (latest SessionStart)"
    );
    println!("resumed conversation {sid}, and the SessionStart after it reported the same id");

    live.stop(&id);
    live.delete(&id);
}

/// The rejected `--resume` claim (spec §2): a `claude_session_id` Claude Code
/// does not know makes the process exit within seconds. The row must not read
/// `idle` afterwards — a fresh conversation under a row that says `resumed`
/// would lose whatever the session was for.
#[test]
#[ignore = "spawns a real claude; run ./dev live"]
fn a_bogus_claude_session_id_makes_the_resume_exit() {
    let live = live("bogus");
    let id = live.create(serde_json::json!({
        "model": "haiku",
        "isolated_config": true,
        "permission_mode": "bypassPermissions"
    }));
    live.wait_state(&id, "idle", SPAWN_DEADLINE);
    live.stop(&id);

    live.set_claude_session_id(&id, "00000000-0000-0000-0000-000000000000");
    let started = Instant::now();
    let resumed = live.resume(&id);
    assert_eq!(resumed["resumed"], true, "{resumed}");
    live.wait_state(&id, "exited", EXIT_DEADLINE);
    println!(
        "a bogus --resume exited after {:?} with {}",
        started.elapsed(),
        live.view(&id)["exit"]
    );

    live.delete(&id);
}

/// The decision under test (spec §5): the derived worktree grant copies the
/// project root's `.mcp.json` approvals into the worktree's own entry, so the
/// worktree is not "new" to Claude Code. Skipped, loudly, when the operator
/// has no user-scoped MCP server for the fixture to borrow.
fn assert_mcp_approvals_copied(repo: &Path, worktree: &Path) {
    if !repo.join(".mcp.json").exists() {
        println!("mcp approval copy: not probed (no .mcp.json in the live repo)");
        return;
    }
    let store = TrustStore::from_env().expect("the operator's .claude.json");
    let root = store.project_keys(repo, &MCP_APPROVAL_KEYS).unwrap();
    let derived = store.project_keys(worktree, &MCP_APPROVAL_KEYS).unwrap();
    assert_eq!(
        derived.get("enabledMcpjsonServers"),
        root.get("enabledMcpjsonServers"),
        "the worktree grant did not copy the project root's MCP approvals \
         (root {root:?}, worktree {derived:?})"
    );
    println!("mcp approval copy: {derived:?} (read back from the operator's .claude.json)");
}
