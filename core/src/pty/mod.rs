//! PTY layer: spawn recipe, per-agent output pipeline, reported agent state, and
//! the serialized injection queue. The `InjectionQueue`/`ClearGate` traits are
//! the ONLY boundary the router uses to write into a PTY.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};

use crate::bus::EventBus;
use crate::pty::detector::spawn_debouncer;
use crate::pty::queue::{QueueCmd, QueueWorker};
use crate::pty::ring::{RING_CAPACITY, ReplayRing, coalesce};
use crate::pty::spawn::{SpawnInputs, spawn_spec, to_command};
use crate::time::Timestamp;
use crate::types::agent::{AgentExit, AgentState};
use crate::types::event::{EventPayload, LifecyclePhase};
use crate::types::id::{AgentId, Token};

pub mod detector;
pub(crate) mod hooks;
pub(crate) mod queue;
pub(crate) mod ring;
mod roster;
pub(crate) mod spawn;

pub use roster::{McpPolicy, PtyRoster, RosterEntry};

/// Monotonic byte offset in an agent's output stream. Survives restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Cursor(pub u64);

/// One coalesced flush of PTY output. `start` is the cursor of `bytes[0]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyChunk {
    pub start: Cursor,
    pub bytes: Vec<u8>,
}

/// Process-wide values injected into every agent PTY. Per-agent files live on
/// each [`RosterEntry`] (amendment 46).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEnv {
    pub port: u16,
    pub token: Token,
    /// Prepended to PATH so agents can exec `tempo`.
    pub tempo_bin_dir: PathBuf,
    /// The operator's credential store, exported as
    /// `CLAUDE_SECURESTORAGE_CONFIG_DIR` to agents that have a `config_dir` so an
    /// isolated session reads and refreshes the same `.credentials.json` as
    /// the operator (never a copy or symlink — see `claude_config`). `None`
    /// when the daemon knows no home; the var is then left alone.
    pub credential_store: Option<PathBuf>,
}

/// Resolution of a successful injection. `cursor` = injection marker position
/// (end-of-stream cursor at the moment the bytes hit the PTY).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Injected {
    pub at: Timestamp,
    pub cursor: Cursor,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InjectError {
    #[error("agent '{0}' has exited")]
    AgentExited(AgentId),
    /// The target sat on a permission dialog past [`BLOCKED_GRACE`] while the
    /// injection waited (#63): typing into the dialog would answer it, and
    /// `send`s have no TTL to bound the wait otherwise.
    #[error("agent '{agent}' has been blocked on a permission dialog for {tool:?} for {} s",
            waited.as_secs())]
    Blocked {
        agent: AgentId,
        tool: Option<String>,
        waited: std::time::Duration,
    },
    #[error("agent '{0}' was restarted")]
    AgentRestarted(AgentId),
    #[error("unknown agent '{0}'")]
    UnknownAgent(AgentId),
}

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("unknown agent '{0}'; not in the roster")]
    UnknownAgent(AgentId),
    #[error("agent '{0}' has exited; restart it first")]
    AgentExited(AgentId),
    #[error("failed to spawn agent '{agent}': {reason}")]
    Spawn { agent: AgentId, reason: String },
    #[error("pty i/o for agent '{agent}' failed: {reason}")]
    Io { agent: AgentId, reason: String },
    #[error("agent '{0}' already exists in the roster")]
    AgentExists(AgentId),
}

/// Implemented by `PtyManager`; the ONLY write path the router uses.
pub trait InjectionQueue: Send + Sync + 'static {
    /// Enqueue on the target's serialized queue. Injection happens only when the
    /// target is debounced-idle. Receiver resolves when bytes hit the PTY
    /// (=> `injected_at`) or on failure.
    fn enqueue(
        &self,
        target: AgentId,
        text: String,
    ) -> tokio::sync::oneshot::Receiver<Result<Injected, InjectError>>;

    /// Ask the target's queue worker to re-run the idle gate now. A no-op for
    /// implementations that have no worker (test stubs).
    fn reconsider(&self, _target: &AgentId) {}
}

/// What the gate wants the queue worker to do at a stable idle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdleDecision {
    /// Nothing blocks clearing; the worker still honors `auto_clear`.
    AllowClear,
    /// Type this text (+ separate Enter) instead of clearing.
    Nudge(String),
    /// Neither clear nor nudge — pending ask or already-nudged stall.
    HoldQuiet,
}

/// Implemented by `Router`. Consulted by the queue worker on each debounced
/// working→idle transition — the single serialized decision point for
/// drain/nudge/clear ordering (spec §2). A drained injection short-circuits the
/// consult entirely, and `auto_clear = false` agents skip only the clear.
pub trait ClearGate: Send + Sync + 'static {
    fn on_stable_idle(&self, agent: &AgentId) -> IdleDecision;
}

/// Consulted immediately before every spawn — initial and restart — with the
/// agent's frozen `dir`. `Err(reason)` fails that spawn as
/// [`PtyError::Spawn`] instead of letting the agent start and park (spec
/// 2026-08-17 §1: trust must be re-checked at each spawn).
pub trait SpawnGate: Send + Sync + 'static {
    /// # Errors
    /// A human/LLM-readable reason the agent must not be spawned right now.
    fn before_spawn(&self, agent: &AgentId, dir: &Path) -> Result<(), String>;
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Backpressure gate for the blocking reader thread (UI signals >~1 MB unparsed).
struct PauseFlag {
    paused: Mutex<bool>,
    cv: Condvar,
}

impl PauseFlag {
    fn new() -> PauseFlag {
        PauseFlag {
            paused: Mutex::new(false),
            cv: Condvar::new(),
        }
    }

    fn set(&self, paused: bool) {
        *lock(&self.paused) = paused;
        if !paused {
            self.cv.notify_all();
        }
    }

    fn wait_unpaused(&self) {
        let mut guard = lock(&self.paused);
        while *guard {
            guard = self.cv.wait(guard).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

struct OutputHub {
    ring: ReplayRing,
    subscribers: Vec<mpsc::Sender<PtyChunk>>,
}

struct Session {
    master: Box<dyn portable_pty::MasterPty + Send>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    pid: Option<u32>,
    /// Completed by the reaper thread once `wait` has returned and the exit
    /// is recorded, so a stop or restart can hold until the process is gone.
    exited: oneshot::Receiver<()>,
}

/// A freshly opened PTY pair with the agent process running in it.
struct OpenedPty {
    session: Session,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    exited_tx: oneshot::Sender<()>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
}

/// How long a signalled agent gets to exit on its own before it is killed
/// outright. Claude Code handles SIGHUP by writing its session-end records
/// first; removing the run dir or respawning into the same managed config dir
/// while that write is in flight races it (#94).
pub const EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Signals the session's process and waits for the reaper to confirm it has
/// exited, escalating to SIGKILL after [`EXIT_GRACE`].
async fn reap(agent: &AgentId, mut session: Session, context: &str) {
    if let Err(err) = session.killer.kill() {
        tracing::warn!(agent = %agent, error = %err, "kill during {context} failed");
    }
    let mut exited = session.exited;
    if tokio::time::timeout(EXIT_GRACE, &mut exited).await.is_ok() {
        tracing::debug!(agent = %agent, "agent process exited on SIGHUP during {context}");
        return;
    }
    tracing::warn!(
        agent = %agent,
        grace = ?EXIT_GRACE,
        "agent ignored SIGHUP during {context}; killing it"
    );
    if let Some(pid) = session.pid.and_then(|pid| i32::try_from(pid).ok()) {
        // SAFETY: `kill(2)` has no memory-safety preconditions; the pid is the
        // one portable-pty spawned and the reaper thread has not yet returned
        // from `wait`, so it has not been recycled.
        if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
            let err = std::io::Error::last_os_error();
            tracing::warn!(agent = %agent, error = %err, "SIGKILL during {context} failed");
        }
    }
    let _ = exited.await;
}

/// How long an agent may sit on a permission dialog before its owed asks are
/// failed and its parked injections rejected `blocked_on_permission`
/// (spec 2026-08-17 §4.2, #63). The router's sweeper and the queue worker
/// share this one clock, both measured from [`Blocked::since`].
pub const BLOCKED_GRACE: std::time::Duration = std::time::Duration::from_secs(90);

/// A permission dialog the agent's `PermissionRequest` hook reported, with
/// when it went up and which tool it is for (spec 2026-08-17 §4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    pub since: tokio::time::Instant,
    pub tool: Option<String>,
    /// The Claude Code (sub)agent the dialog belongs to, from the hook payload's
    /// `agent_id`; `None` for the main session. Only an `unblocked` report
    /// carrying the same value clears this dialog.
    pub agent_id: Option<String>,
}

/// What [`PtyManager::report_unblocked`] did, decided under the agents lock and
/// acted on after it is dropped.
enum UnblockOutcome {
    Cleared,
    /// The dialog belongs to a different (sub)agent than the reporter it carries.
    OtherAgent(Option<String>),
    NothingSet,
}

struct AgentHandle {
    /// The spawn inputs for this agent; `resume` is consumed by the next
    /// spawn that succeeds.
    entry: RosterEntry,
    raw_tx: watch::Sender<AgentState>,
    debounced_rx: watch::Receiver<AgentState>,
    /// Session epoch; bumping it fails queued/in-flight injections.
    epoch_tx: watch::Sender<u64>,
    hub: Arc<Mutex<OutputHub>>,
    /// Mirrors `hub.ring.end()` for lock-free reads by the queue worker.
    end_cursor: Arc<AtomicU64>,
    queue_tx: mpsc::UnboundedSender<QueueCmd>,
    /// Injections enqueued and not yet delivered or failed. Incremented here on
    /// enqueue; decremented by the `QueueWorker` once per resolved command.
    queue_depth: Arc<AtomicU64>,
    write_tx: mpsc::Sender<Vec<u8>>,
    writer_slot: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    pause: Arc<PauseFlag>,
    /// Set by the agent's `PermissionRequest` hook, cleared by
    /// `PostToolBatch`/raw state changes/restart/exit. A watch so the queue
    /// worker can park injections and hold the gate while it is `Some` (#63).
    blocked_tx: watch::Sender<Option<Blocked>>,
    session: Option<Session>,
    exit: Option<AgentExit>,
    /// The pane size last reported through [`PtyManager::resize`], or the
    /// spawn default. A respawn opens at it: the desktop only reports a size
    /// when xterm's own dimensions change, which a restart never does.
    size: portable_pty::PtySize,
}

pub struct PtyManager {
    /// The roster's debounce, kept for agents added after construction.
    idle_debounce: Duration,
    bus: EventBus,
    env: AgentEnv,
    program: String,
    me: Weak<PtyManager>,
    clear_gate: Arc<OnceLock<Weak<dyn ClearGate>>>,
    spawn_gate: OnceLock<Arc<dyn SpawnGate>>,
    agents: Mutex<BTreeMap<AgentId, AgentHandle>>,
}

/// Blocking PTY reader → raw byte channel. Honors the backpressure pause flag
/// between reads. Exits on EOF/error (child gone) or when the pipeline closes.
fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    tx: mpsc::Sender<Vec<u8>>,
    pause: Arc<PauseFlag>,
) {
    std::thread::spawn(move || {
        let mut buf = [0_u8; 8192];
        loop {
            pause.wait_unpaused();
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        return;
                    }
                }
            }
        }
    });
}

/// Serialized PTY write pump: user keystrokes AND queue injections both land
/// here, so the kernel never interleaves two writers.
async fn write_pump(
    mut rx: mpsc::Receiver<Vec<u8>>,
    slot: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    agent: AgentId,
) {
    while let Some(bytes) = rx.recv().await {
        let ok = {
            let mut guard = lock(&slot);
            match guard.as_mut() {
                Some(writer) => {
                    let res = writer.write_all(&bytes).and_then(|()| writer.flush());
                    if res.is_err() {
                        *guard = None;
                    }
                    res.is_ok()
                }
                None => false,
            }
        };
        if !ok {
            tracing::debug!(agent = %agent, "dropped pty write: no live session");
        }
    }
}

/// Per-session flush pipeline: ring append + cursor bump + subscriber fan-out.
/// State is reported externally (`PtyManager::report_state`), never scraped
/// from these bytes.
async fn pipeline(
    hub: Arc<Mutex<OutputHub>>,
    end_cursor: Arc<AtomicU64>,
    mut flushed: mpsc::UnboundedReceiver<Vec<u8>>,
    agent: AgentId,
) {
    while let Some(bytes) = flushed.recv().await {
        let (start, senders) = {
            let mut hub = lock(&hub);
            let start = hub.ring.end();
            hub.ring.push(&bytes);
            end_cursor.store(hub.ring.end().0, Ordering::SeqCst);
            (start, hub.subscribers.clone())
        };
        let mut dropped: Vec<mpsc::Sender<PtyChunk>> = Vec::new();
        for tx in senders {
            match tx.try_send(PtyChunk {
                start,
                bytes: bytes.clone(),
            }) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    // A subscriber that stopped draining must not stall the ring or
                    // its peers. Contiguity is contractual, so no holes: close the
                    // channel instead, which obliges the consumer to resubscribe by
                    // cursor (see `subscribe_output`) and have the ring replay the gap.
                    tracing::warn!(
                        agent = %agent,
                        "pty subscriber lagged; dropping it to keep the fan-out live"
                    );
                    dropped.push(tx);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => dropped.push(tx),
            }
        }
        if !dropped.is_empty() {
            lock(&hub)
                .subscribers
                .retain(|s| !s.is_closed() && !dropped.iter().any(|d| d.same_channel(s)));
        }
    }
}

const PTY_COLS: u16 = 120;
const PTY_ROWS: u16 = 40;

fn default_pty_size() -> portable_pty::PtySize {
    portable_pty::PtySize {
        rows: PTY_ROWS,
        cols: PTY_COLS,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// One agent's channels, ring, queue worker and write pump, before any spawn.
/// Shared by construction and, once it lands, [`PtyManager::add_agent`].
fn new_handle(
    id: &AgentId,
    entry: RosterEntry,
    idle_debounce: Duration,
    clear_gate: &Arc<OnceLock<Weak<dyn ClearGate>>>,
) -> AgentHandle {
    let (raw_tx, raw_rx) = watch::channel(AgentState::Starting);
    let debounced_rx = spawn_debouncer(raw_rx, idle_debounce);
    let (epoch_tx, epoch_rx) = watch::channel(0_u64);
    let (blocked_tx, blocked_rx) = watch::channel(None);
    let hub = Arc::new(Mutex::new(OutputHub {
        ring: ReplayRing::new(RING_CAPACITY),
        subscribers: Vec::new(),
    }));
    let end_cursor = Arc::new(AtomicU64::new(0));
    let (queue_tx, queue_rx) = mpsc::unbounded_channel();
    let (write_tx, write_rx) = mpsc::channel(64);
    let writer_slot: Arc<Mutex<Option<Box<dyn Write + Send>>>> = Arc::new(Mutex::new(None));
    let queue_depth = Arc::new(AtomicU64::new(0));
    tokio::spawn(write_pump(write_rx, Arc::clone(&writer_slot), id.clone()));
    tokio::spawn(
        QueueWorker {
            agent: id.clone(),
            cmds: queue_rx,
            debounced: debounced_rx.clone(),
            epoch: epoch_rx,
            blocked: blocked_rx,
            writer: write_tx.clone(),
            end_cursor: Arc::clone(&end_cursor),
            auto_clear: entry.cfg.auto_clear,
            clear_gate: Arc::clone(clear_gate),
            prev: AgentState::Starting,
            depth: Arc::clone(&queue_depth),
            served_inject_since_idle: false,
        }
        .run(),
    );
    AgentHandle {
        entry,
        raw_tx,
        debounced_rx,
        epoch_tx,
        hub,
        end_cursor,
        queue_tx,
        queue_depth,
        write_tx,
        writer_slot,
        pause: Arc::new(PauseFlag::new()),
        blocked_tx,
        session: None,
        exit: None,
        size: default_pty_size(),
    }
}

impl PtyManager {
    /// Must be called inside a tokio runtime (spawns per-agent workers).
    #[must_use]
    pub fn new(roster: PtyRoster, bus: EventBus, env: AgentEnv) -> Arc<PtyManager> {
        PtyManager::new_with_program(roster, bus, env, "claude")
    }

    /// Test seam: substitute the spawned program (e.g. a scripted fake agent).
    #[must_use]
    pub fn new_with_program(
        roster: PtyRoster,
        bus: EventBus,
        env: AgentEnv,
        program: &str,
    ) -> Arc<PtyManager> {
        let clear_gate: Arc<OnceLock<Weak<dyn ClearGate>>> = Arc::new(OnceLock::new());
        let mut agents = BTreeMap::new();
        for (id, entry) in roster.agents {
            let handle = new_handle(&id, entry, roster.idle_debounce, &clear_gate);
            agents.insert(id, handle);
        }
        Arc::new_cyclic(|me| PtyManager {
            idle_debounce: roster.idle_debounce,
            bus,
            env,
            program: program.to_string(),
            me: me.clone(),
            clear_gate,
            spawn_gate: OnceLock::new(),
            agents: Mutex::new(agents),
        })
    }

    /// Wiring break for the `PtyManager`⇄Router cycle; called once in `Run::start`
    /// before spawn (workflow-run plan). Stores a `Weak`, not the `Arc` itself:
    /// `PtyManager` must not be a strong owner of the `Router`, or the two hold
    /// each other alive forever and the whole run graph leaks on stop. The
    /// caller's own strong reference (`Run::router`) keeps it upgradeable for
    /// the run's life.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "by-value signature is frozen at the one call site (run.rs); \
                  downgrading a borrowed Arc would force an extra clone there"
    )]
    pub fn set_clear_gate(&self, gate: Arc<dyn ClearGate>) {
        if self.clear_gate.set(Arc::downgrade(&gate)).is_err() {
            tracing::warn!("clear gate already set; ignoring");
        }
    }

    /// Installs the pre-spawn check. Set once; a second call is a `CoreTempo`
    /// bug and is logged, not honoured.
    pub fn set_spawn_gate(&self, gate: Arc<dyn SpawnGate>) {
        if self.spawn_gate.set(gate).is_err() {
            tracing::error!("spawn gate installed twice; keeping the first");
        }
    }

    /// Adds an agent to the roster without spawning it: creates its channels,
    /// ring, queue worker and write pump. Call [`PtyManager::spawn`] next.
    /// Must be called inside a tokio runtime (spawns per-agent workers).
    ///
    /// # Errors
    /// [`PtyError::AgentExists`] if the id is already in the roster.
    pub fn add_agent(&self, id: AgentId, entry: RosterEntry) -> Result<(), PtyError> {
        let mut agents = lock(&self.agents);
        if agents.contains_key(&id) {
            return Err(PtyError::AgentExists(id));
        }
        let handle = new_handle(&id, entry, self.idle_debounce, &self.clear_gate);
        agents.insert(id, handle);
        Ok(())
    }

    /// Sets (or clears) the `--resume <claude_session_id>` the next spawn of
    /// `agent` passes. Consumed by the next spawn that succeeds; a refused or
    /// failed spawn leaves it armed, and a later respawn never reuses it.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] off-roster.
    pub fn set_resume(&self, agent: &AgentId, resume: Option<String>) -> Result<(), PtyError> {
        let mut agents = lock(&self.agents);
        let handle = agents
            .get_mut(agent)
            .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
        handle.entry.resume = resume;
        Ok(())
    }

    /// Spawns every agent in the roster, in lexicographic order.
    ///
    /// # Errors
    /// Propagates the first [`PtyError`] from [`PtyManager::spawn`].
    pub async fn spawn_all(&self) -> Result<(), PtyError> {
        let ids: Vec<AgentId> = lock(&self.agents).keys().cloned().collect();
        for id in ids {
            self.spawn(&id).await?;
        }
        Ok(())
    }

    /// Starts one agent's PTY session. No-op if it already has a live session.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] off-roster, [`PtyError::Spawn`] if an installed
    /// [`SpawnGate`] refuses or the pty or child cannot be created,
    /// [`PtyError::Io`] if the pty handles are lost.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "async signature frozen in contracts §3; awaited by spawn_all/restart"
    )]
    pub async fn spawn(&self, agent: &AgentId) -> Result<(), PtyError> {
        let (entry, size) = {
            let mut agents = lock(&self.agents);
            let handle = agents
                .get_mut(agent)
                .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
            if handle.session.is_some() {
                return Ok(());
            }
            // `resume` is for this spawn only; only cleared once this spawn
            // actually lands a session (below) so a refused or failed attempt
            // leaves it armed for the next one.
            let entry = RosterEntry {
                resume: handle.entry.resume.clone(),
                ..handle.entry.clone()
            };
            (entry, handle.size)
        };
        if let Some(gate) = self.spawn_gate.get() {
            gate.before_spawn(agent, &entry.cfg.dir)
                .map_err(|reason| PtyError::Spawn {
                    agent: agent.clone(),
                    reason,
                })?;
        }
        let OpenedPty {
            session,
            mut child,
            exited_tx,
            reader,
            writer,
        } = self.open_pty(agent, &entry, size)?;

        let (raw_bytes_tx, raw_bytes_rx) = mpsc::channel::<Vec<u8>>(64);
        let (flushed_tx, flushed_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(coalesce(raw_bytes_rx, move |chunk| {
            let _ = flushed_tx.send(chunk);
        }));

        let my_epoch;
        {
            let mut agents = lock(&self.agents);
            let handle = agents
                .get_mut(agent)
                .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
            my_epoch = *handle.epoch_tx.borrow();
            *lock(&handle.writer_slot) = Some(writer);
            handle.exit = None;
            handle.session = Some(session);
            // This spawn landed a session; the resume id (if any) is consumed.
            handle.entry.resume = None;
            spawn_reader(reader, raw_bytes_tx, Arc::clone(&handle.pause));
            tokio::spawn(pipeline(
                Arc::clone(&handle.hub),
                Arc::clone(&handle.end_cursor),
                flushed_rx,
                agent.clone(),
            ));
            let _ = handle.raw_tx.send(AgentState::Starting);
        }
        self.bus.publish(EventPayload::AgentStateChanged {
            agent: agent.clone(),
            state: AgentState::Starting,
        });
        self.bus.publish(EventPayload::AgentLifecycle {
            agent: agent.clone(),
            phase: LifecyclePhase::Spawned,
            exit: None,
        });

        let me = self.me.clone();
        let exit_agent = agent.clone();
        std::thread::spawn(move || {
            let exit = child
                .wait()
                .map_or(AgentExit::Code(-1), |status| match status.signal() {
                    Some(signal) => AgentExit::Signal(signal.to_string()),
                    None => AgentExit::Code(i32::try_from(status.exit_code()).unwrap_or(i32::MAX)),
                });
            if let Some(mgr) = me.upgrade() {
                mgr.on_child_exit(&exit_agent, my_epoch, exit);
            }
            let _ = exited_tx.send(());
        });
        tracing::info!(agent = %agent, "spawned agent pty");
        Ok(())
    }

    /// Opens a PTY pair and starts the agent process in it with the spawn
    /// recipe. The `--append-system-prompt` value, when the entry has one, is
    /// the role prompt plus the protocol primer from
    /// `FrozenWorkflow::system_prompt` (contracts amendment 3); sessions have
    /// none (amendment 46).
    fn open_pty(
        &self,
        agent: &AgentId,
        entry: &RosterEntry,
        size: portable_pty::PtySize,
    ) -> Result<OpenedPty, PtyError> {
        let spawn_err = |e: &dyn std::fmt::Display| PtyError::Spawn {
            agent: agent.clone(),
            reason: e.to_string(),
        };
        let io_err = |e: &dyn std::fmt::Display| PtyError::Io {
            agent: agent.clone(),
            reason: e.to_string(),
        };
        let pair = portable_pty::native_pty_system()
            .openpty(size)
            .map_err(|e| spawn_err(&e))?;
        let cmd = to_command(&spawn_spec(&SpawnInputs {
            id: agent,
            entry,
            env: &self.env,
            program: &self.program,
        }));
        let child = pair.slave.spawn_command(cmd).map_err(|e| spawn_err(&e))?;
        drop(pair.slave);
        let killer = child.clone_killer();
        let pid = child.process_id();
        let (exited_tx, exited) = oneshot::channel();
        let reader = pair.master.try_clone_reader().map_err(|e| io_err(&e))?;
        let writer = pair.master.take_writer().map_err(|e| io_err(&e))?;
        Ok(OpenedPty {
            session: Session {
                master: pair.master,
                killer,
                pid,
                exited,
            },
            child,
            exited_tx,
            reader,
            writer,
        })
    }

    fn on_child_exit(&self, agent: &AgentId, epoch: u64, exit: AgentExit) {
        let was_blocked = {
            let mut agents = lock(&self.agents);
            let Some(handle) = agents.get_mut(agent) else {
                return;
            };
            if *handle.epoch_tx.borrow() != epoch {
                return; // superseded by a restart
            }
            handle.session = None;
            *lock(&handle.writer_slot) = None;
            handle.exit = Some(exit.clone());
            let _ = handle.raw_tx.send(AgentState::Exited);
            Self::take_blocked(handle)
        };
        if was_blocked {
            self.publish_blocked(agent, false, None);
        }
        self.bus.publish(EventPayload::AgentStateChanged {
            agent: agent.clone(),
            state: AgentState::Exited,
        });
        tracing::info!(agent = %agent, ?exit, "agent exited");
        self.bus.publish(EventPayload::AgentLifecycle {
            agent: agent.clone(),
            phase: LifecyclePhase::Exited,
            exit: Some(exit),
        });
    }

    /// The old session is already dead and the fresh one never started, so the
    /// agent would otherwise sit in `Restarting` with no session forever —
    /// nothing else moves it, and the API's restart handler only logs the
    /// error. Report the truth: this agent is gone (spec 2026-08-17 §1).
    fn mark_exited_after_failed_spawn(&self, agent: &AgentId, reason: &PtyError) {
        {
            let mut agents = lock(&self.agents);
            let Some(handle) = agents.get_mut(agent) else {
                return;
            };
            let _ = handle.raw_tx.send(AgentState::Exited);
        }
        self.bus.publish(EventPayload::AgentStateChanged {
            agent: agent.clone(),
            state: AgentState::Exited,
        });
        self.bus.publish(EventPayload::AgentLifecycle {
            agent: agent.clone(),
            phase: LifecyclePhase::Exited,
            exit: None,
        });
        tracing::error!(
            agent = %agent,
            error = %reason,
            "respawn during restart failed; the agent has no session and is marked exited"
        );
    }

    /// Kill + respawn from the same roster entry. Fails queued/in-flight
    /// injections with `InjectError::AgentRestarted` (via the epoch bump).
    /// Emits agent.lifecycle restarting → spawned, or restarting → exited when
    /// the respawn is refused.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] off-roster, plus anything
    /// [`PtyManager::spawn`] reports for the fresh session.
    pub async fn restart(&self, agent: &AgentId) -> Result<(), PtyError> {
        let (session, was_blocked) = {
            let mut agents = lock(&self.agents);
            let handle = agents
                .get_mut(agent)
                .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
            let next = *handle.epoch_tx.borrow() + 1;
            let _ = handle.epoch_tx.send(next);
            let _ = handle.raw_tx.send(AgentState::Restarting);
            *lock(&handle.writer_slot) = None;
            handle.exit = None;
            (handle.session.take(), Self::take_blocked(handle))
        };
        if was_blocked {
            self.publish_blocked(agent, false, None);
        }
        self.bus.publish(EventPayload::AgentStateChanged {
            agent: agent.clone(),
            state: AgentState::Restarting,
        });
        self.bus.publish(EventPayload::AgentLifecycle {
            agent: agent.clone(),
            phase: LifecyclePhase::Restarting,
            exit: None,
        });
        if let Some(session) = session {
            reap(agent, session, "restart").await;
        }
        if let Err(err) = self.spawn(agent).await {
            self.mark_exited_after_failed_spawn(agent, &err);
            return Err(err);
        }
        Ok(())
    }

    /// Kills one agent's process and waits for it to exit (SIGHUP, then SIGKILL
    /// after [`EXIT_GRACE`]) — `shutdown` for one handle. Unlike `restart`
    /// there is no epoch bump: the bump exists to fail queued injections with
    /// `AgentRestarted`, and here the queue fails them itself on the raw
    /// `Exited`; because `reap` awaits the reaper thread's oneshot, which it
    /// sends only after `on_child_exit`, [`PtyManager::exit`] is recorded when
    /// this returns. The handle, ring and output subscribers survive, so a
    /// later [`PtyManager::spawn`] continues the same stream (spec 2026-08-27
    /// §4, amendment 46).
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] off-roster; [`PtyError::AgentExited`] when
    /// there is no live session to stop.
    pub async fn stop(&self, agent: &AgentId) -> Result<(), PtyError> {
        let (session, was_blocked) = {
            let mut agents = lock(&self.agents);
            let handle = agents
                .get_mut(agent)
                .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
            let Some(session) = handle.session.take() else {
                return Err(PtyError::AgentExited(agent.clone()));
            };
            let _ = handle.raw_tx.send(AgentState::Exited);
            *lock(&handle.writer_slot) = None;
            (session, Self::take_blocked(handle))
        };
        if was_blocked {
            self.publish_blocked(agent, false, None);
        }
        reap(agent, session, "stop").await;
        Ok(())
    }

    /// `stop()` if live, then drop the handle. Output subscribers are closed
    /// explicitly; the queue worker, write pump and state subscribers end
    /// when their senders drop with the handle. The id becomes unknown.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] off-roster.
    pub async fn remove_agent(&self, agent: &AgentId) -> Result<(), PtyError> {
        match self.stop(agent).await {
            Ok(()) | Err(PtyError::AgentExited(_)) => {}
            Err(err) => return Err(err),
        }
        let handle = lock(&self.agents)
            .remove(agent)
            .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
        lock(&handle.hub).subscribers.clear();
        drop(handle);
        tracing::info!(agent = %agent, "removed agent from roster");
        Ok(())
    }

    /// Kills every agent PTY, stops the reader/coalescer/queue tasks, and fails
    /// queued injections with `InjectError::AgentExited`. Returns once every
    /// agent process has actually exited (SIGHUP, then SIGKILL after
    /// [`EXIT_GRACE`]), so the caller may remove the run dir. Idempotent.
    pub async fn shutdown(&self) {
        let sessions: Vec<(AgentId, Option<Session>, bool)> = {
            let mut agents = lock(&self.agents);
            agents
                .iter_mut()
                .map(|(id, handle)| {
                    let _ = handle.raw_tx.send(AgentState::Exited);
                    *lock(&handle.writer_slot) = None;
                    let was_blocked = Self::take_blocked(handle);
                    (id.clone(), handle.session.take(), was_blocked)
                })
                .collect()
        };
        let mut reapers = Vec::new();
        for (agent, session, was_blocked) in sessions {
            if was_blocked {
                self.publish_blocked(&agent, false, None);
            }
            if let Some(session) = session {
                reapers.push(tokio::spawn(async move {
                    reap(&agent, session, "shutdown").await;
                }));
            }
        }
        for reaper in reapers {
            if let Err(err) = reaper.await {
                tracing::warn!(error = %err, "shutdown reaper task failed");
            }
        }
    }

    /// Raw user keystrokes — bypasses the injection queue entirely (but shares
    /// the serialized write pump with it).
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] off-roster, [`PtyError::AgentExited`] when
    /// the agent has no live session.
    pub async fn write(&self, agent: &AgentId, bytes: &[u8]) -> Result<(), PtyError> {
        let tx = {
            let agents = lock(&self.agents);
            let handle = agents
                .get(agent)
                .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
            if handle.session.is_none() {
                return Err(PtyError::AgentExited(agent.clone()));
            }
            handle.write_tx.clone()
        };
        tx.send(bytes.to_vec())
            .await
            .map_err(|_| PtyError::AgentExited(agent.clone()))
    }

    /// Resizes the agent's pty window.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] off-roster, [`PtyError::AgentExited`] with no
    /// live session, [`PtyError::Io`] if the resize ioctl fails.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "async signature frozen in contracts §3"
    )]
    pub async fn resize(&self, agent: &AgentId, cols: u16, rows: u16) -> Result<(), PtyError> {
        let size = portable_pty::PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut agents = lock(&self.agents);
        let handle = agents
            .get_mut(agent)
            .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
        handle.size = size;
        let Some(session) = &handle.session else {
            return Err(PtyError::AgentExited(agent.clone()));
        };
        session.master.resize(size).map_err(|e| PtyError::Io {
            agent: agent.clone(),
            reason: e.to_string(),
        })
    }

    /// Live output. Guarantee: chunks are contiguous from `max(since, ring_start)`;
    /// consumer detects aged-out data by `first_chunk.start > since`.
    /// `since = None` → full ring tail.
    ///
    /// A consumer that stops draining is dropped whole — contiguity is never
    /// broken by a hole. A dropped consumer must resubscribe, passing the cursor
    /// after its last received byte (`chunk.start + chunk.bytes.len()`);
    /// `read_since` is inclusive of `since`, so resubscribing at the last
    /// received `start` re-delivers that chunk.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the roster.
    pub fn subscribe_output(
        &self,
        agent: &AgentId,
        since: Option<Cursor>,
    ) -> Result<mpsc::Receiver<PtyChunk>, PtyError> {
        let agents = lock(&self.agents);
        let handle = agents
            .get(agent)
            .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
        let mut hub = lock(&handle.hub);
        let (tx, rx) = mpsc::channel(256);
        let (end, tail) = hub.ring.read_since(since);
        if !tail.is_empty() {
            let start = Cursor(end.0 - tail.len() as u64);
            let _ = tx.try_send(PtyChunk { start, bytes: tail });
        }
        hub.subscribers.push(tx);
        Ok(rx)
    }

    /// One-shot ring read: (cursor after last byte, bytes from `max(since, start)`).
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the roster.
    pub fn read_ring(
        &self,
        agent: &AgentId,
        since: Option<Cursor>,
    ) -> Result<(Cursor, Vec<u8>), PtyError> {
        let agents = lock(&self.agents);
        let handle = agents
            .get(agent)
            .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
        let hub = lock(&handle.hub);
        Ok(hub.ring.read_since(since))
    }

    /// Backpressure from the UI (>~1 MB unparsed): pause/resume reading this PTY.
    pub fn pause_output(&self, agent: &AgentId, paused: bool) {
        let agents = lock(&self.agents);
        if let Some(handle) = agents.get(agent) {
            handle.pause.set(paused);
        } else {
            tracing::warn!(agent = %agent, "pause_output for unknown agent ignored");
        }
    }

    /// Whether this agent is parked on a permission dialog (spec 2026-08-17 §3):
    /// its `PermissionRequest` hook fired and nothing has cleared it yet.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the roster.
    pub fn blocked(&self, agent: &AgentId) -> Result<bool, PtyError> {
        let agents = lock(&self.agents);
        let handle = agents
            .get(agent)
            .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
        Ok(handle.blocked_tx.borrow().is_some())
    }

    /// When the current dialog went up and for which tool; `None` when clear.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the roster.
    pub fn blocked_since(&self, agent: &AgentId) -> Result<Option<Blocked>, PtyError> {
        let agents = lock(&self.agents);
        let handle = agents
            .get(agent)
            .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
        Ok(handle.blocked_tx.borrow().clone())
    }

    /// Agents currently parked on a permission dialog; `/v1/health` reports it.
    #[must_use]
    pub fn blocked_count(&self) -> usize {
        lock(&self.agents)
            .values()
            .filter(|h| h.blocked_tx.borrow().is_some())
            .count()
    }

    /// Clears the blocked flag under the lock; returns whether it was set so the
    /// caller can publish the clearing event outside the lock.
    fn take_blocked(handle: &mut AgentHandle) -> bool {
        // `send_if_modified`, not `send_replace`: a clear on an already-clear
        // flag must not wake the queue worker (it re-runs the gate on a flip).
        handle.blocked_tx.send_if_modified(|b| b.take().is_some())
    }

    fn publish_blocked(&self, agent: &AgentId, blocked: bool, tool: Option<String>) {
        self.bus.publish(EventPayload::AgentBlocked {
            agent: agent.clone(),
            blocked,
            tool,
        });
    }

    /// The agent's `PermissionRequest` hook fired: a permission dialog is up.
    /// Accepted at `working` and — because a subagent's dialog fires the parent's
    /// hook after the parent's `Stop` — at `idle`. A report at any other raw
    /// state is logged and dropped; a repeat while already flagged publishes
    /// nothing and keeps the original `since`.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the roster.
    pub fn report_blocked(
        &self,
        agent: &AgentId,
        tool: Option<String>,
        agent_id: Option<String>,
    ) -> Result<(), PtyError> {
        {
            let mut agents = lock(&self.agents);
            let handle = agents
                .get_mut(agent)
                .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
            let state = *handle.raw_tx.borrow();
            if state != AgentState::Working && state != AgentState::Idle {
                tracing::debug!(
                    agent = %agent,
                    ?state,
                    "ignoring blocked report: no live turn or session"
                );
                return Ok(());
            }
            if handle.blocked_tx.borrow().is_some() {
                return Ok(());
            }
            handle.blocked_tx.send_replace(Some(Blocked {
                since: tokio::time::Instant::now(),
                tool: tool.clone(),
                agent_id,
            }));
        }
        tracing::warn!(
            agent = %agent,
            tool = tool.as_deref().unwrap_or("?"),
            "agent is waiting on a permission dialog"
        );
        self.publish_blocked(agent, true, tool);
        Ok(())
    }

    /// The agent's `PermissionRequest` hook refused a tool call on `CoreTempo`'s
    /// behalf (`on_permission_prompt = "deny"`, amendment 44). No dialog is up
    /// and the turn continues, so nothing is flagged; the refused tool and a
    /// summary of its input are logged and published as
    /// `agent.permission_refused` — the operator's signal for which allow rule
    /// is missing.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the roster.
    pub fn report_refused(
        &self,
        agent: &AgentId,
        tool: Option<String>,
        input: Option<String>,
    ) -> Result<(), PtyError> {
        {
            let agents = lock(&self.agents);
            agents
                .get(agent)
                .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
        }
        tracing::warn!(
            agent = %agent,
            tool = tool.as_deref().unwrap_or("?"),
            input = input.as_deref().unwrap_or(""),
            "permission refused: the tool call had no allow rule and the agent runs unattended"
        );
        self.bus.publish(EventPayload::AgentPermissionRefused {
            agent: agent.clone(),
            tool,
            input,
        });
        Ok(())
    }

    /// The agent's `PostToolBatch` hook fired: the dialog was answered.
    /// Clearing an already-clear flag publishes nothing.
    ///
    /// Scoped to the reporting (sub)agent: a Claude Code helper agent fires
    /// `PostToolBatch` for tools it did not run, so a report whose `agent_id`
    /// is not the one the dialog was raised with is logged and dropped — it
    /// would otherwise clear a dialog still on screen and disarm the
    /// `blocked_on_permission` fail-fast (spec 2026-08-17 §4.2 amendment).
    /// Turn boundaries, restart, exit and shutdown clear regardless.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the roster.
    pub fn report_unblocked(
        &self,
        agent: &AgentId,
        agent_id: Option<String>,
    ) -> Result<(), PtyError> {
        let outcome = {
            let mut agents = lock(&self.agents);
            let handle = agents
                .get_mut(agent)
                .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
            let dialog_owner = handle
                .blocked_tx
                .borrow()
                .as_ref()
                .map(|b| b.agent_id.clone());
            match dialog_owner {
                None => UnblockOutcome::NothingSet,
                Some(owner) if owner != agent_id => UnblockOutcome::OtherAgent(agent_id),
                Some(_) => {
                    handle.blocked_tx.send_replace(None);
                    UnblockOutcome::Cleared
                }
            }
        };
        match outcome {
            UnblockOutcome::Cleared => {
                tracing::debug!(agent = %agent, "agent reported unblocked");
                self.publish_blocked(agent, false, None);
            }
            UnblockOutcome::OtherAgent(reporter) => tracing::debug!(
                agent = %agent,
                reporter = reporter.as_deref().unwrap_or("<main>"),
                "unblocked report from another agent; dialog still up"
            ),
            UnblockOutcome::NothingSet => {
                tracing::debug!(agent = %agent, "unblocked report with no flag set");
            }
        }
        Ok(())
    }

    /// Current RAW (undebounced) state.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the roster.
    pub fn state(&self, agent: &AgentId) -> Result<AgentState, PtyError> {
        let agents = lock(&self.agents);
        let handle = agents
            .get(agent)
            .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
        Ok(*handle.raw_tx.borrow())
    }

    /// Publishes an externally reported state (Claude Code hooks: session start
    /// and turn end → `Idle`, prompt submit → `Working`) onto the raw state
    /// channel every downstream consumer already reads. Unchanged state is a
    /// no-op, so a repeated report emits no duplicate `agent.state` event —
    /// and it leaves the blocked flag alone, because only a raw state *change*
    /// is a turn boundary.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the roster.
    pub fn report_state(&self, agent: &AgentId, state: AgentState) -> Result<(), PtyError> {
        let (was_blocked, changed) = {
            let mut agents = lock(&self.agents);
            let handle = agents
                .get_mut(agent)
                .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
            // A turn boundary ends any dialog of this session's own turn; a
            // report at the same raw state is not a boundary (a subagent's
            // dialog can be up while the parent is idle), so the flag survives
            // it.
            let current = *handle.raw_tx.borrow();
            if current == state {
                (false, false)
            } else if current == AgentState::Exited {
                // A hook from a dying session can fire after the PTY is gone.
                // Reviving the agent would let the queue inject into a dead PTY.
                tracing::debug!(agent = %agent, ?state, "ignoring report for exited agent");
                (false, false)
            } else {
                let was_blocked = Self::take_blocked(handle);
                let _ = handle.raw_tx.send(state);
                (was_blocked, true)
            }
        };
        if was_blocked {
            self.publish_blocked(agent, false, None);
        }
        if changed {
            self.bus.publish(EventPayload::AgentStateChanged {
                agent: agent.clone(),
                state,
            });
            tracing::debug!(agent = %agent, ?state, "agent reported state");
        }
        Ok(())
    }

    /// How the last session ended, set only while the agent is `exited`.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the roster.
    pub fn exit(&self, agent: &AgentId) -> Result<Option<AgentExit>, PtyError> {
        let agents = lock(&self.agents);
        let handle = agents
            .get(agent)
            .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
        Ok(handle.exit.clone())
    }

    /// Feeds `agent.state` events; the UI shows this truth undebounced.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the roster.
    pub fn subscribe_state_raw(
        &self,
        agent: &AgentId,
    ) -> Result<watch::Receiver<AgentState>, PtyError> {
        let agents = lock(&self.agents);
        let handle = agents
            .get(agent)
            .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
        Ok(handle.raw_tx.subscribe())
    }

    /// 2 s stable idle (tunable via `idle_debounce_seconds`). All actions key
    /// off this signal.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the roster.
    pub fn subscribe_state_debounced(
        &self,
        agent: &AgentId,
    ) -> Result<watch::Receiver<AgentState>, PtyError> {
        let agents = lock(&self.agents);
        let handle = agents
            .get(agent)
            .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
        Ok(handle.debounced_rx.clone())
    }

    /// Number of injections enqueued for `agent` and not yet delivered or failed.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the roster.
    pub fn queue_depth(&self, agent: &AgentId) -> Result<u64, PtyError> {
        let agents = lock(&self.agents);
        let handle = agents
            .get(agent)
            .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
        Ok(handle.queue_depth.load(Ordering::SeqCst))
    }
}

impl InjectionQueue for PtyManager {
    fn enqueue(
        &self,
        target: AgentId,
        text: String,
    ) -> oneshot::Receiver<Result<Injected, InjectError>> {
        let (done, rx) = oneshot::channel();
        let agents = lock(&self.agents);
        match agents.get(&target) {
            Some(handle) => {
                handle.queue_depth.fetch_add(1, Ordering::SeqCst);
                let _ = handle.queue_tx.send(QueueCmd::Inject { text, done });
            }
            None => {
                let _ = done.send(Err(InjectError::UnknownAgent(target)));
            }
        }
        rx
    }

    fn reconsider(&self, target: &AgentId) {
        let agents = lock(&self.agents);
        if let Some(handle) = agents.get(target) {
            let _ = handle.queue_tx.send(QueueCmd::Reconsider);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::sync::mpsc;

    use crate::pty::ring::{RING_CAPACITY, ReplayRing};
    use crate::pty::{Cursor, InjectError, OutputHub, PtyChunk, lock, pipeline};
    use crate::types::id::AgentId;

    #[test]
    fn cursor_serializes_transparent() {
        assert_eq!(serde_json::to_string(&Cursor(183_462)).unwrap(), "183462");
        let c: Cursor = serde_json::from_str("7").unwrap();
        assert_eq!(c, Cursor(7));
    }

    #[test]
    fn inject_error_messages_name_the_agent() {
        let err = InjectError::AgentRestarted(AgentId("builder".into()));
        assert_eq!(err.to_string(), "agent 'builder' was restarted");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn full_subscriber_is_dropped_without_stalling_others() {
        let hub = Arc::new(Mutex::new(OutputHub {
            ring: ReplayRing::new(RING_CAPACITY),
            subscribers: Vec::new(),
        }));
        let end_cursor = Arc::new(AtomicU64::new(0));
        let (flush_tx, flush_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (stalled_tx, mut stalled_rx) = mpsc::channel::<PtyChunk>(1); // tiny cap: fills fast
        let (healthy_tx, mut healthy_rx) = mpsc::channel::<PtyChunk>(1024);
        lock(&hub).subscribers.push(stalled_tx);
        lock(&hub).subscribers.push(healthy_tx);
        let pipe = tokio::spawn(pipeline(
            Arc::clone(&hub),
            Arc::clone(&end_cursor),
            flush_rx,
            AgentId("builder".into()),
        ));

        for i in 0..10_u8 {
            flush_tx.send(vec![i; 8]).expect("flush channel open");
        }
        drop(flush_tx);
        tokio::time::timeout(Duration::from_secs(5), pipe)
            .await
            .expect("pipeline must finish rather than park on the full subscriber")
            .expect("pipeline task");

        // The healthy subscriber saw every chunk even though the stalled one filled.
        let mut received = 0;
        while healthy_rx.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, 10, "healthy subscriber must not be starved");
        assert_eq!(
            end_cursor.load(Ordering::SeqCst),
            80,
            "ring must keep advancing"
        );
        // The stalled subscriber was pruned, and every sender clone was dropped
        // with it — the consumer sees a closed channel and knows to resubscribe.
        assert_eq!(lock(&hub).subscribers.len(), 1);
        loop {
            match stalled_rx.try_recv() {
                Ok(_) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => break,
                Err(mpsc::error::TryRecvError::Empty) => {
                    panic!("pruned subscriber's channel must actually be closed")
                }
            }
        }
    }
}
