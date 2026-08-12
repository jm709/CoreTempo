//! PTY layer: spawn recipe, per-agent output pipeline, reported agent state, and
//! the serialized injection queue. The `InjectionQueue`/`ClearGate` traits are
//! the ONLY boundary the router uses to write into a PTY.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};

use crate::bus::EventBus;
use crate::pty::detector::spawn_debouncer;
use crate::pty::queue::{QueueCmd, QueueWorker};
use crate::pty::ring::{RING_CAPACITY, ReplayRing, coalesce};
use crate::pty::spawn::{SpawnInputs, spawn_spec, to_command};
use crate::time::Timestamp;
use crate::types::agent::AgentState;
use crate::types::config::{AgentConfig, FrozenWorkflow};
use crate::types::event::{EventPayload, LifecyclePhase};
use crate::types::id::{AgentId, Token};

pub mod detector;
pub(crate) mod hooks;
pub(crate) mod queue;
pub(crate) mod ring;
pub(crate) mod spawn;

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

/// Injected into every agent PTY at spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEnv {
    pub port: u16,
    pub token: Token,
    /// Prepended to PATH so agents can exec `tempo`.
    pub tempo_bin_dir: PathBuf,
    /// Per-agent generated settings (turn-boundary hooks + Bash allowlist),
    /// passed as `--settings`. Missing entry leaves that agent's settings alone.
    pub settings_paths: BTreeMap<AgentId, PathBuf>,
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
    #[error("agent '{0}' was restarted")]
    AgentRestarted(AgentId),
    #[error("unknown agent '{0}'")]
    UnknownAgent(AgentId),
}

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("unknown agent '{0}'; the roster is frozen in tempo.toml")]
    UnknownAgent(AgentId),
    #[error("agent '{0}' has exited; restart it first")]
    AgentExited(AgentId),
    #[error("failed to spawn agent '{agent}': {reason}")]
    Spawn { agent: AgentId, reason: String },
    #[error("pty i/o for agent '{agent}' failed: {reason}")]
    Io { agent: AgentId, reason: String },
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
}

/// A freshly opened PTY pair with the agent process running in it.
struct OpenedPty {
    session: Session,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
}

struct AgentHandle {
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
    session: Option<Session>,
    exit_code: Option<i32>,
}

pub struct PtyManager {
    workflow: Arc<FrozenWorkflow>,
    bus: EventBus,
    env: AgentEnv,
    program: String,
    me: Weak<PtyManager>,
    clear_gate: Arc<OnceLock<Weak<dyn ClearGate>>>,
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

impl PtyManager {
    /// Must be called inside a tokio runtime (spawns per-agent workers).
    #[must_use]
    pub fn new(workflow: Arc<FrozenWorkflow>, bus: EventBus, env: AgentEnv) -> Arc<PtyManager> {
        PtyManager::new_with_program(workflow, bus, env, "claude")
    }

    /// Test seam: substitute the spawned program (e.g. a scripted fake agent).
    #[must_use]
    pub fn new_with_program(
        workflow: Arc<FrozenWorkflow>,
        bus: EventBus,
        env: AgentEnv,
        program: &str,
    ) -> Arc<PtyManager> {
        let clear_gate: Arc<OnceLock<Weak<dyn ClearGate>>> = Arc::new(OnceLock::new());
        let mut agents = BTreeMap::new();
        for (id, cfg) in &workflow.agents {
            let (raw_tx, raw_rx) = watch::channel(AgentState::Starting);
            let debounced_rx = spawn_debouncer(raw_rx, workflow.idle_debounce);
            let (epoch_tx, epoch_rx) = watch::channel(0_u64);
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
                    writer: write_tx.clone(),
                    end_cursor: Arc::clone(&end_cursor),
                    auto_clear: cfg.auto_clear,
                    clear_gate: Arc::clone(&clear_gate),
                    prev: AgentState::Starting,
                    depth: Arc::clone(&queue_depth),
                }
                .run(),
            );
            agents.insert(
                id.clone(),
                AgentHandle {
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
                    session: None,
                    exit_code: None,
                },
            );
        }
        Arc::new_cyclic(|me| PtyManager {
            workflow,
            bus,
            env,
            program: program.to_string(),
            me: me.clone(),
            clear_gate,
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

    /// Spawns every agent in the frozen roster, in lexicographic order.
    ///
    /// # Errors
    /// Propagates the first [`PtyError`] from [`PtyManager::spawn`].
    pub async fn spawn_all(&self) -> Result<(), PtyError> {
        let ids: Vec<AgentId> = self.workflow.agents.keys().cloned().collect();
        for id in ids {
            self.spawn(&id).await?;
        }
        Ok(())
    }

    /// Starts one agent's PTY session. No-op if it already has a live session.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] off-roster, [`PtyError::Spawn`] if the pty or
    /// child cannot be created, [`PtyError::Io`] if the pty handles are lost.
    #[expect(
        clippy::unused_async,
        reason = "async signature frozen in contracts §3; awaited by spawn_all/restart"
    )]
    pub async fn spawn(&self, agent: &AgentId) -> Result<(), PtyError> {
        let cfg = self
            .workflow
            .agents
            .get(agent)
            .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?
            .clone();
        {
            let agents = lock(&self.agents);
            let handle = agents
                .get(agent)
                .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
            if handle.session.is_some() {
                return Ok(());
            }
        }
        let OpenedPty {
            session,
            mut child,
            reader,
            writer,
        } = self.open_pty(agent, &cfg)?;

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
            handle.exit_code = None;
            handle.session = Some(session);
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
            exit_code: None,
        });

        let me = self.me.clone();
        let exit_agent = agent.clone();
        std::thread::spawn(move || {
            let code = child.wait().map_or(-1, |status| {
                i32::try_from(status.exit_code()).unwrap_or(i32::MAX)
            });
            if let Some(mgr) = me.upgrade() {
                mgr.on_child_exit(&exit_agent, my_epoch, code);
            }
        });
        tracing::info!(agent = %agent, "spawned agent pty");
        Ok(())
    }

    /// Opens a PTY pair and starts the agent process in it with the frozen spawn
    /// recipe. The `--append-system-prompt` value is the role prompt plus the
    /// protocol primer from [`FrozenWorkflow::system_prompt`] (contracts amendment 3).
    fn open_pty(&self, agent: &AgentId, cfg: &AgentConfig) -> Result<OpenedPty, PtyError> {
        let spawn_err = |e: &dyn std::fmt::Display| PtyError::Spawn {
            agent: agent.clone(),
            reason: e.to_string(),
        };
        let io_err = |e: &dyn std::fmt::Display| PtyError::Io {
            agent: agent.clone(),
            reason: e.to_string(),
        };
        let system_prompt = self
            .workflow
            .system_prompt(agent)
            .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
        let size = portable_pty::PtySize {
            rows: PTY_ROWS,
            cols: PTY_COLS,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = portable_pty::native_pty_system()
            .openpty(size)
            .map_err(|e| spawn_err(&e))?;
        let cmd = to_command(&spawn_spec(&SpawnInputs {
            id: agent,
            cfg,
            env: &self.env,
            program: &self.program,
            system_prompt: &system_prompt,
        }));
        let child = pair.slave.spawn_command(cmd).map_err(|e| spawn_err(&e))?;
        drop(pair.slave);
        let killer = child.clone_killer();
        let reader = pair.master.try_clone_reader().map_err(|e| io_err(&e))?;
        let writer = pair.master.take_writer().map_err(|e| io_err(&e))?;
        Ok(OpenedPty {
            session: Session {
                master: pair.master,
                killer,
            },
            child,
            reader,
            writer,
        })
    }

    fn on_child_exit(&self, agent: &AgentId, epoch: u64, code: i32) {
        {
            let mut agents = lock(&self.agents);
            let Some(handle) = agents.get_mut(agent) else {
                return;
            };
            if *handle.epoch_tx.borrow() != epoch {
                return; // superseded by a restart
            }
            handle.session = None;
            *lock(&handle.writer_slot) = None;
            handle.exit_code = Some(code);
            let _ = handle.raw_tx.send(AgentState::Exited);
        }
        self.bus.publish(EventPayload::AgentStateChanged {
            agent: agent.clone(),
            state: AgentState::Exited,
        });
        self.bus.publish(EventPayload::AgentLifecycle {
            agent: agent.clone(),
            phase: LifecyclePhase::Exited,
            exit_code: Some(code),
        });
        tracing::info!(agent = %agent, code, "agent exited");
    }

    /// Kill + respawn from the same frozen config. Fails queued/in-flight
    /// injections with `InjectError::AgentRestarted` (via the epoch bump).
    /// Emits agent.lifecycle restarting → spawned.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] off-roster, plus anything
    /// [`PtyManager::spawn`] reports for the fresh session.
    pub async fn restart(&self, agent: &AgentId) -> Result<(), PtyError> {
        let session = {
            let mut agents = lock(&self.agents);
            let handle = agents
                .get_mut(agent)
                .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
            let next = *handle.epoch_tx.borrow() + 1;
            let _ = handle.epoch_tx.send(next);
            let _ = handle.raw_tx.send(AgentState::Restarting);
            *lock(&handle.writer_slot) = None;
            handle.exit_code = None;
            handle.session.take()
        };
        self.bus.publish(EventPayload::AgentStateChanged {
            agent: agent.clone(),
            state: AgentState::Restarting,
        });
        self.bus.publish(EventPayload::AgentLifecycle {
            agent: agent.clone(),
            phase: LifecyclePhase::Restarting,
            exit_code: None,
        });
        if let Some(mut session) = session
            && let Err(err) = session.killer.kill()
        {
            tracing::warn!(agent = %agent, error = %err, "kill during restart failed");
        }
        self.spawn(agent).await
    }

    /// Kills every agent PTY, stops the reader/coalescer/queue tasks, and fails
    /// queued injections with `InjectError::AgentExited`. Idempotent.
    #[expect(
        clippy::unused_async,
        reason = "contract-frozen async signature; Run::stop awaits it"
    )]
    pub async fn shutdown(&self) {
        let sessions: Vec<(AgentId, Option<Session>)> = {
            let mut agents = lock(&self.agents);
            agents
                .iter_mut()
                .map(|(id, handle)| {
                    let _ = handle.raw_tx.send(AgentState::Exited);
                    *lock(&handle.writer_slot) = None;
                    (id.clone(), handle.session.take())
                })
                .collect()
        };
        for (agent, session) in sessions {
            if let Some(mut session) = session
                && let Err(err) = session.killer.kill()
            {
                tracing::warn!(agent = %agent, error = %err, "kill during shutdown failed");
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
        reason = "async signature frozen in contracts §3"
    )]
    pub async fn resize(&self, agent: &AgentId, cols: u16, rows: u16) -> Result<(), PtyError> {
        let agents = lock(&self.agents);
        let handle = agents
            .get(agent)
            .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
        let Some(session) = &handle.session else {
            return Err(PtyError::AgentExited(agent.clone()));
        };
        session
            .master
            .resize(portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::Io {
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
    /// [`PtyError::UnknownAgent`] if the agent is not in the frozen roster.
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
    /// [`PtyError::UnknownAgent`] if the agent is not in the frozen roster.
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

    /// Current RAW (undebounced) state.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the frozen roster.
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
    /// no-op, so a repeated report emits no duplicate `agent.state` event.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the frozen roster.
    pub fn report_state(&self, agent: &AgentId, state: AgentState) -> Result<(), PtyError> {
        {
            let agents = lock(&self.agents);
            let handle = agents
                .get(agent)
                .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
            let current = *handle.raw_tx.borrow();
            if current == state {
                return Ok(());
            }
            // A hook from a dying session can fire after the PTY is gone.
            // Reviving the agent would let the queue inject into a dead PTY.
            if current == AgentState::Exited {
                tracing::debug!(agent = %agent, ?state, "ignoring report for exited agent");
                return Ok(());
            }
            let _ = handle.raw_tx.send(state);
        }
        self.bus.publish(EventPayload::AgentStateChanged {
            agent: agent.clone(),
            state,
        });
        tracing::debug!(agent = %agent, ?state, "agent reported state");
        Ok(())
    }

    /// Exit code of the last session, set only while the agent is `exited`.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the frozen roster.
    pub fn exit_code(&self, agent: &AgentId) -> Result<Option<i32>, PtyError> {
        let agents = lock(&self.agents);
        let handle = agents
            .get(agent)
            .ok_or_else(|| PtyError::UnknownAgent(agent.clone()))?;
        Ok(handle.exit_code)
    }

    /// Feeds `agent.state` events; the UI shows this truth undebounced.
    ///
    /// # Errors
    /// [`PtyError::UnknownAgent`] if the agent is not in the frozen roster.
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
    /// [`PtyError::UnknownAgent`] if the agent is not in the frozen roster.
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
    /// [`PtyError::UnknownAgent`] if the agent is not in the frozen roster.
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
