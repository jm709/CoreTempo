//! `coretempod sessions` (spec 2026-08-27 §3): one process per user, its
//! root `~/.coretempo/sessions/`, loopback only, a generated operator token
//! published in `api.json` beside the pid, `sessions.lock` held with
//! `flock` for the daemon's life, `daemon.log` appended.
//!
//! Everything here is process work — the root, the lock, the log, `api.json`,
//! and the signal path. The HTTP handlers are `core`'s.

use std::fs::File;
use std::net::{IpAddr, Ipv4Addr};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use clap::{Args, Subcommand};
use coretempo_core::api::auth::write_private_file;
use coretempo_core::api::sessions::{SessionsApi, build_sessions_router};
use coretempo_core::api::{ApiCore, PtyManagerSource, Roster, TokenAuth, serve_app};
use coretempo_core::bus::EventBus;
use coretempo_core::claude_config::operator_credential_store;
use coretempo_core::pid::pid_alive;
use coretempo_core::pty::{AgentEnv, PtyManager, PtyRoster};
use coretempo_core::sessions::{SessionManager, SessionManagerInputs, SessionStore, SessionsRoot};
use coretempo_core::time::Timestamp;
use coretempo_core::trust::{TrustPolicy, TrustStore};
use coretempo_core::types::{SessionsApiFile, Token};
use coretempo_core::user_config::UserConfig;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriterExt;

/// Every session's state channel debounce — the workflow default.
const IDLE_DEBOUNCE: Duration = Duration::from_secs(2);

/// The daemon serves the operator's own machine and nothing else: its token is
/// generated, so a routable bind would publish an API nobody provisioned a
/// secret for.
const BIND: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

#[derive(Args)]
pub struct SessionsArgs {
    /// Sessions root (default ~/.coretempo/sessions); worktrees go under
    /// ~/.coretempo/worktrees, or <root>/worktrees with an explicit root.
    #[arg(long)]
    root: Option<PathBuf>,
    /// Loopback port to bind (default: ephemeral; api.json records it).
    #[arg(long, default_value_t = 0)]
    port: u16,
    #[command(subcommand)]
    action: Option<SessionsAction>,
}

#[derive(Subcommand)]
enum SessionsAction {
    /// Stop the running daemon (SIGTERM to the pid in api.json).
    Stop,
}

fn root_from(args: &SessionsArgs) -> anyhow::Result<SessionsRoot> {
    if let Some(dir) = &args.root {
        return Ok(SessionsRoot::at(dir.clone()));
    }
    let home = std::env::home_dir().context("cannot locate HOME for ~/.coretempo/sessions")?;
    Ok(SessionsRoot::from_home(&home))
}

pub async fn main(
    args: SessionsArgs,
    interrupt: crate::signal::Interrupt,
) -> anyhow::Result<ExitCode> {
    let root = root_from(&args)?;
    match args.action {
        Some(SessionsAction::Stop) => stop(&root),
        None => run(root, args.port, interrupt).await,
    }
}

/// The running daemon's `api.json`: `sessions.lock` held by another process
/// *and* the pid the file names still alive.
///
/// Neither alone is evidence. `api.json` is removed only on the clean-exit
/// path, so a daemon killed with SIGKILL, killed by the OOM reaper, or crashed leaves it behind
/// naming a pid the kernel eventually recycles — and `kill(pid, 0)` succeeds on
/// a zombie too. Signalling on that would SIGTERM an unrelated process of the
/// operator's. The lock is what says a daemon owns this root.
fn live_api_file(root: &SessionsRoot) -> Option<SessionsApiFile> {
    if !lock_is_held(root) {
        return None;
    }
    let text = std::fs::read_to_string(root.api_file()).ok()?;
    let file: SessionsApiFile = serde_json::from_str(&text).ok()?;
    pid_alive(file.pid).then_some(file)
}

/// Whether another process holds `sessions.lock`. A probe that cannot be made
/// answers "no": both callers then refuse to signal, or fall back to a message
/// that names no pid, and either is the safe answer.
fn lock_is_held(root: &SessionsRoot) -> bool {
    if !root.lock_file().exists() {
        return false;
    }
    // Taking the lock proves nobody else has it; the guard drops at the end of
    // the arm, releasing it again before any caller acts on the answer.
    match take_lock(root) {
        Ok(Some(_lock)) => false,
        Ok(None) => true,
        Err(error) => {
            tracing::debug!(%error, "could not probe sessions.lock");
            false
        }
    }
}

#[expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI output; `sessions stop` never initialises tracing"
)]
fn stop(root: &SessionsRoot) -> anyhow::Result<ExitCode> {
    let Some(file) = live_api_file(root) else {
        eprintln!("no session daemon running; start it with 'coretempod sessions'");
        return Ok(ExitCode::from(1));
    };
    let pid = i32::try_from(file.pid).context("pid out of range")?;
    // SAFETY: kill(2) has no memory-safety preconditions.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot signal the daemon");
    }
    println!(
        "sent SIGTERM to session daemon pid {} (port {})",
        file.pid, file.port
    );
    Ok(ExitCode::SUCCESS)
}

/// Held for the daemon's life; dropping the `File` releases the lock.
struct RootLock(#[expect(dead_code, reason = "held for its Drop")] File);

fn take_lock(root: &SessionsRoot) -> anyhow::Result<Option<RootLock>> {
    std::fs::create_dir_all(&root.dir)
        .with_context(|| format!("cannot create {}", root.dir.display()))?;
    let file = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(root.lock_file())
        .with_context(|| format!("cannot open {}", root.lock_file().display()))?;
    // SAFETY: flock on a valid open fd.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(RootLock(file)));
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(None);
    }
    Err(err).context("flock on sessions.lock failed")
}

/// Names the daemon already holding `root`, so the operator can stop it. It
/// goes through the same [`live_api_file`] rule `stop` does — a pid is only
/// reported when the lock backs it up — so the two never disagree about
/// whether a daemon is there.
///
/// Runs before [`init_logging`]: the lock is what decides which process owns
/// `daemon.log`, so the loser must not write to it.
#[expect(
    clippy::print_stderr,
    reason = "CLI refusal before tracing is initialised"
)]
fn refuse_locked(root: &SessionsRoot) {
    match live_api_file(root) {
        Some(file) => eprintln!(
            "a session daemon is already running (pid {}, port {}); stop it with \
             'coretempod sessions stop'",
            file.pid, file.port
        ),
        None => eprintln!(
            "{} is locked by another process; stop it with 'coretempod sessions stop'",
            root.lock_file().display()
        ),
    }
}

fn init_logging(root: &SessionsRoot) -> anyhow::Result<()> {
    let log = File::options()
        .create(true)
        .append(true)
        .open(root.log_file())
        .with_context(|| format!("cannot open {}", root.log_file().display()))?;
    let filter = std::env::var("RUST_LOG").ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            filter
                .and_then(|f| EnvFilter::try_new(f).ok())
                .unwrap_or_else(|| EnvFilter::new("info")),
        )
        .with_ansi(false)
        .with_writer(std::io::stderr.and(Mutex::new(log)))
        .init();
    Ok(())
}

/// Opens the store, builds the `PtyManager` every session spawns through, and
/// loads the roster. `port` is the one already bound: agents are told about it
/// through `CORETEMPO_PORT`, so the listener has to exist first.
async fn boot(
    root: &SessionsRoot,
    port: u16,
    token: &Token,
) -> anyhow::Result<(Arc<SessionManager>, ApiCore)> {
    let user = UserConfig::load_default().context("invalid ~/.coretempo/config.toml")?;
    // No workflow, so nothing can opt in per-run: the user config alone decides
    // whether the daemon may grant Claude Code trust for a session's dirs.
    let policy = TrustPolicy::resolve(user.trust_agent_dirs, false);
    let trust_store = TrustStore::from_env().context("cannot locate HOME for .claude.json")?;
    let tempo_bin_dir = std::env::current_exe()
        .context("cannot locate the running coretempod")?
        .parent()
        .map(Path::to_path_buf)
        .context("cannot locate the directory containing coretempod")?;

    let db = root.db();
    let store = tokio::task::spawn_blocking(move || SessionStore::open(&db))
        .await
        .context("store open task failed")??;
    let bus = EventBus::new();
    let pty = PtyManager::new(
        PtyRoster::empty(IDLE_DEBOUNCE),
        bus.clone(),
        AgentEnv {
            port,
            token: token.clone(),
            tempo_bin_dir: tempo_bin_dir.clone(),
            credential_store: operator_credential_store(),
        },
    );
    let sessions = SessionManager::boot(SessionManagerInputs {
        root: root.clone(),
        store,
        pty: Arc::clone(&pty),
        bus: bus.clone(),
        trust_store,
        policy,
        tempo_bin: tempo_bin_dir.join("tempo"),
        operator_token: token.clone(),
    })
    .await
    .context("cannot load the sessions store")?;
    let core = ApiCore {
        pty: Arc::new(PtyManagerSource(pty)),
        bus,
        roster: Arc::clone(&sessions) as Arc<dyn Roster>,
        auth: Arc::clone(&sessions) as Arc<dyn TokenAuth>,
        // Generated, not provisioned: `check_bind` must keep refusing a
        // routable bind if this daemon ever grows one.
        token_provisioned: false,
        bind: BIND,
        port,
        started_at: Timestamp::now(),
        started: std::time::Instant::now(),
    };
    Ok((sessions, core))
}

async fn run(
    root: SessionsRoot,
    port: u16,
    mut interrupt: crate::signal::Interrupt,
) -> anyhow::Result<ExitCode> {
    let Some(_lock) = take_lock(&root)? else {
        refuse_locked(&root);
        return Ok(ExitCode::from(1));
    };
    init_logging(&root)?;
    let listener = tokio::net::TcpListener::bind((BIND, port))
        .await
        .with_context(|| format!("cannot bind 127.0.0.1:{port}"))?;
    let port = listener
        .local_addr()
        .context("cannot read the bound address")?
        .port();
    let token = Token::generate();
    let (sessions, core) = boot(&root, port, &token).await?;
    let api = serve_app(
        listener,
        build_sessions_router(SessionsApi {
            core,
            sessions: Arc::clone(&sessions),
        }),
        BIND,
        false,
    )?;
    let file = SessionsApiFile {
        port,
        token,
        pid: std::process::id(),
    };
    write_private_file(&root.api_file(), &serde_json::to_string_pretty(&file)?)
        .with_context(|| format!("cannot write {}", root.api_file().display()))?;
    tracing::info!(port, root = %root.dir.display(), "sessions daemon listening");

    interrupt.wait().await?;
    tracing::info!("interrupt received; stopping every session");
    // API first (its abort is bounded to 500 ms), then the sessions: a create
    // that is still mid-flight when the manager stops is refused by its
    // `stopping` flag under the session lock, never orphaned.
    api.shutdown().await;
    sessions.shutdown().await;
    if let Err(error) = std::fs::remove_file(root.api_file()) {
        tracing::warn!(%error, "could not remove api.json");
    }
    Ok(ExitCode::SUCCESS)
}
