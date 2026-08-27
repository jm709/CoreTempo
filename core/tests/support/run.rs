//! Boot scaffold shared by the `run_*` integration tests: a scratch root with
//! its own HOME, a fake `claude` on PATH, and a `tempo.toml` to freeze and
//! start a real `Run` against (issue #81).
#![expect(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns are not covered by allow-*-in-tests"
)]

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use coretempo_core::run::{Run, RunError, RunOptions};
use coretempo_core::trust::TrustPolicy;
use coretempo_core::types::config::{FrozenWorkflow, ServerOverrides, WorkflowFile};
use coretempo_core::workflow::{load_workflow, resolve_server};
use tokio::sync::{Mutex, MutexGuard};

/// HOME/PATH are process-global; a scaffold holds this for as long as those
/// vars matter (through spawn and stop), even in tests that only read them
/// indirectly — `set_var`'s safety contract forbids concurrent reads too, and
/// cargo runs one binary's tests on separate threads.
static ENV_LOCK: Mutex<()> = Mutex::const_new(());

/// Scaffolded runs put their agents in fresh temp dirs Claude Code has never
/// seen, so the trust preflight would refuse them; granting is what an
/// interactive run with `trust_agent_dirs = true` does. Trust itself is covered
/// by `run_trust.rs`.
pub const GRANT_TRUST: RunOptions = RunOptions {
    ephemeral_port: false,
    repoint_current: true,
    cleanup_run_dir: false,
    trust: TrustPolicy { grant: true },
};

/// The fake `claude` every scaffold starts with: prints a prompt marker, then
/// idles.
const IDLE_CLAUDE: &str = "printf '> '\nsleep 300\n";

pub fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// A scratch run rooted in its own directory. Holding one holds `ENV_LOCK`.
///
/// HOME is `<root>/home`, so `~/.coretempo/runs` and `~/.claude.json` are this
/// test's; a fake `claude` in `<root>/bin` heads PATH; the single agent `echo`
/// lives in `<root>/agent` — untrusted, because nothing has written a trust key
/// for it. `CLAUDE_CONFIG_DIR` and `CLAUDE_SECURESTORAGE_CONFIG_DIR` are removed
/// so the operator's own relocation never receives a test's trust grant.
pub struct RunScaffold {
    _env: MutexGuard<'static, ()>,
    pub name: String,
    pub root: PathBuf,
    pub home: PathBuf,
    pub agent_dir: PathBuf,
    pub config: PathBuf,
    /// The `[workflow] port` `write_workflow` declares; a free one by default.
    pub port: u16,
}

impl RunScaffold {
    pub async fn new(name: &str) -> RunScaffold {
        let env = ENV_LOCK.lock().await;
        let root = std::env::temp_dir().join(format!("coretempo-{name}-{}", std::process::id()));
        // /tmp survives between `cargo test` runs; a stale store or db would
        // make a test's assertions meaningless.
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        let bin = root.join("bin");
        let agent_dir = root.join("agent");
        for dir in [&home, &bin, &agent_dir] {
            std::fs::create_dir_all(dir).unwrap();
        }

        // SAFETY: `env` is held for the scaffold's whole life; HOME/PATH are
        // set before any core code in the test runs.
        unsafe {
            std::env::set_var("HOME", &home);
            let path = std::env::var("PATH").unwrap();
            std::env::set_var("PATH", format!("{}:{path}", bin.display()));
            std::env::remove_var("CLAUDE_CONFIG_DIR");
            std::env::remove_var("CLAUDE_SECURESTORAGE_CONFIG_DIR");
        }

        let scaffold = RunScaffold {
            _env: env,
            name: name.to_string(),
            config: root.join("tempo.toml"),
            port: free_port(),
            root,
            home,
            agent_dir,
        };
        scaffold.fake_claude(IDLE_CLAUDE);
        scaffold.write_workflow(&format!(
            "[agents.echo]\ndir = \"{}\"\nprompt = \"You echo.\"\n",
            scaffold.agent_dir.display()
        ));
        scaffold
    }

    /// Replaces the fake `claude` with a `/bin/sh` script running `body`.
    pub fn fake_claude(&self, body: &str) {
        let fake = self.root.join("bin").join("claude");
        std::fs::write(&fake, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Writes `tempo.toml`: a `[workflow]` header naming this scaffold, on
    /// `self.port`, with the db under the root — then `body` (further
    /// `[workflow]` keys, agents, flows).
    pub fn write_workflow(&self, body: &str) {
        std::fs::write(
            &self.config,
            format!(
                "[workflow]\nname = \"{}\"\nport = {}\ndb = \"{}\"\n{body}",
                self.name,
                self.port,
                self.root.join("tempo.db").display(),
            ),
        )
        .unwrap();
    }

    pub fn load(&self) -> (WorkflowFile, FrozenWorkflow) {
        load_workflow(&self.config).unwrap()
    }

    /// Freezes `tempo.toml` and starts a run with `options`.
    pub async fn start(&self, options: RunOptions) -> Result<Arc<Run>, RunError> {
        let loaded = self.load();
        RunScaffold::start_loaded(loaded, options).await
    }

    /// Starts a run from an already-frozen workflow, for tests that inspect or
    /// perturb things between the freeze and the start.
    pub async fn start_loaded(
        (file, frozen): (WorkflowFile, FrozenWorkflow),
        options: RunOptions,
    ) -> Result<Arc<Run>, RunError> {
        let server = resolve_server(
            ServerOverrides::default(),
            ServerOverrides::default(),
            &file,
        )
        .unwrap();
        Run::start_with(frozen, server, options).await
    }

    pub fn runs_dir(&self) -> PathBuf {
        self.home.join(".coretempo").join("runs")
    }

    pub fn run_dir(&self, run: &Run) -> PathBuf {
        self.runs_dir().join(&run.run_id().0)
    }

    /// The parsed `api.json` a run wrote under this HOME.
    pub fn api_json(&self, run: &Run) -> serde_json::Value {
        let text = std::fs::read_to_string(self.run_dir(run).join("api.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }
}
