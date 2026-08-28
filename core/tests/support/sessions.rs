//! `SessionManager` harness shared by the manager suite (`sessions.rs`) and
//! the HTTP suite (`sessions_api.rs`): a temp git repository, an explicit
//! trust store, an explicit sessions root, and a scripted fake `claude`.
//! No HOME mutation — every path is an input (spec 2026-08-27 §10).
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns are not covered by allow-*-in-tests"
)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use coretempo_core::bus::EventBus;
use coretempo_core::pty::{AgentEnv, PtyManager, PtyRoster};
use coretempo_core::sessions::manager::{SessionManager, SessionManagerInputs};
use coretempo_core::sessions::{SessionStore, SessionsRoot};
use coretempo_core::trust::{TrustPolicy, TrustStore};
use coretempo_core::types::{AgentId, AgentState, CreateSessionRequest, ProjectId, Token};

pub const DEADLINE: Duration = Duration::from_secs(10);

/// The operator token every harness serves under.
pub const OPERATOR: &str = "abababababababababababababababababababababababababababababababab";

/// Runs `git` in `dir`, asserting success; returns trimmed stdout.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub struct Harness {
    pub root: PathBuf,
    pub repo: PathBuf,
    pub argv_log: PathBuf,
    pub trust: TrustStore,
    pub bus: EventBus,
    pub mgr: Arc<SessionManager>,
}

/// Every fake `claude` logs its argv and cwd, then echoes `got:<line>` and
/// exits 3 on `quit`.
const FAKE_ARGV: &str = "#!/usr/bin/env bash\n\
    { printf 'cwd=%s\\n' \"$PWD\"; printf '%s\\n' \"$@\"; printf -- '===ARGV===\\n'; } \
    >> '@ARGV_LOG@'\n";

/// The HTTP fake's `SessionStart`: it reports through the real hook route with
/// its own hook token, and sends no `X-CoreTempo-Agent` — the token is its
/// identity (contracts amendment 47). Mirrors `daemon/tests/support`'s `post`.
const FAKE_POST: &str = r#"post() {
  exec 3<>"/dev/tcp/127.0.0.1/$CORETEMPO_PORT" || return 1
  printf 'POST %s HTTP/1.1\r\nHost: 127.0.0.1\r\n' "$1" >&3
  printf 'Authorization: Bearer %s\r\n' "$CORETEMPO_TOKEN" >&3
  printf 'Content-Type: application/json\r\nContent-Length: %d\r\n' "${#2}" >&3
  printf 'Connection: close\r\n\r\n%s' "$2" >&3
  cat <&3 >/dev/null
  exec 3>&-
}
start="{\"state\":\"idle\",\"claude_session_id\":\"${FAKE_SESSION_ID:-fake-sid}\"}"
post "/v1/agents/$CORETEMPO_AGENT_ID/state" "$start"
"#;

const FAKE_ECHO: &str = "printf 'booted\\n'\n\
    while IFS= read -r line; do\n\
    \x20 case \"$line\" in quit) exit 3 ;; *) printf 'got:%s\\n' \"$line\" ;; esac\n\
    done\n";

/// Writes the fake `claude`. `http` adds the `SessionStart` report; without it
/// a test drives state through [`Harness::hook_idle`].
fn write_fake(dir: &Path, argv_log: &Path, http: bool) -> PathBuf {
    let path = dir.join("fake-claude.sh");
    let mut script = FAKE_ARGV.replace("@ARGV_LOG@", &argv_log.display().to_string());
    if http {
        script.push_str(FAKE_POST);
    }
    script.push_str(FAKE_ECHO);
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

pub struct HarnessOptions<'a> {
    /// Distinguishes this harness's temp root from its peers'.
    pub name: &'a str,
    /// `false` leaves the repo root without a trust key.
    pub trusted: bool,
    /// The port the agent environment advertises as `CORETEMPO_PORT`.
    pub port: u16,
    /// Use the fake that reports state over HTTP.
    pub http: bool,
}

/// Trusted, no HTTP reporting, on a port nothing listens on.
pub async fn harness(name: &str) -> Harness {
    harness_with(name, true).await
}

/// `trusted = false` leaves the repo root without a trust key.
pub async fn harness_with(name: &str, trusted: bool) -> Harness {
    build(HarnessOptions {
        name,
        trusted,
        port: 4821,
        http: false,
    })
    .await
}

/// A harness whose fake agent reports its state to `port` — the API suite's,
/// which must know its listener's port before anything spawns.
pub async fn harness_http(name: &str, port: u16) -> Harness {
    harness_http_with(name, port, true).await
}

pub async fn harness_http_with(name: &str, port: u16, trusted: bool) -> Harness {
    build(HarnessOptions {
        name,
        trusted,
        port,
        http: true,
    })
    .await
}

async fn build(opts: HarnessOptions<'_>) -> Harness {
    let HarnessOptions {
        name,
        trusted,
        port,
        http,
    } = opts;
    let root =
        std::env::temp_dir().join(format!("coretempo-sessions-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join("pkg")).unwrap();
    // Git tracks no empty directories: without a file under pkg/ a worktree
    // of this repo has no pkg/, and a `cwd = "pkg"` worktree session fails.
    std::fs::write(repo.join("pkg/.keep"), "").unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README"), "hi\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let repo = std::fs::canonicalize(&repo).unwrap();
    let argv_log = root.join("argv.txt");
    let fake = write_fake(&root, &argv_log, http);
    let trust = TrustStore::at(root.join("claude.json"));
    if trusted {
        trust.grant(std::slice::from_ref(&repo)).unwrap();
    }
    let sessions_root = SessionsRoot::at(root.join("sessions"));
    std::fs::create_dir_all(&sessions_root.dir).unwrap();
    let store = SessionStore::open(&sessions_root.db()).unwrap();
    let bus = EventBus::new();
    let pty = PtyManager::new_with_program(
        PtyRoster::empty(Duration::from_millis(100)),
        bus.clone(),
        AgentEnv {
            port,
            token: Token(OPERATOR.to_string()),
            tempo_bin_dir: PathBuf::from("/usr/bin"),
            credential_store: None,
        },
        fake.to_str().unwrap(),
    );
    let mgr = SessionManager::boot(SessionManagerInputs {
        root: sessions_root,
        store,
        pty,
        bus: bus.clone(),
        trust_store: trust.clone(),
        policy: TrustPolicy { grant: false },
        tempo_bin: PathBuf::from("/usr/bin/tempo"),
        operator_token: Token(OPERATOR.to_string()),
    })
    .await
    .unwrap();
    Harness {
        root,
        repo,
        argv_log,
        trust,
        bus,
        mgr,
    }
}

/// A session in the project root: no worktree, no prompt, all defaults.
pub fn plain_req(project: &ProjectId) -> CreateSessionRequest {
    CreateSessionRequest {
        project: project.clone(),
        worktree: false,
        cwd: None,
        title: None,
        prompt: None,
        model: None,
        permission_mode: None,
        isolated_config: false,
    }
}

impl Harness {
    pub async fn project(&self) -> ProjectId {
        self.mgr
            .register_project(&self.repo, None)
            .await
            .unwrap()
            .id
    }

    /// Simulates the `SessionStart` hook.
    pub fn hook_idle(&self, id: &AgentId) {
        self.mgr.pty().report_state(id, AgentState::Idle).unwrap();
    }

    pub async fn wait_state(&self, id: &AgentId, want: AgentState) {
        let mut rx = self.mgr.pty().subscribe_state_debounced(id).unwrap();
        tokio::time::timeout(DEADLINE, rx.wait_for(|s| *s == want))
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {want:?}"))
            .unwrap();
    }

    pub fn argv(&self) -> Vec<Vec<String>> {
        let text = std::fs::read_to_string(&self.argv_log).unwrap_or_default();
        text.split("===ARGV===\n")
            .filter(|b| !b.is_empty())
            .map(|b| {
                b.lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .collect()
    }

    pub async fn wait_argv(&self, spawns: usize) {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        while self.argv().len() < spawns {
            assert!(
                tokio::time::Instant::now() < deadline,
                "spawn {spawns} never recorded"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
