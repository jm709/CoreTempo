//! Per-agent serialized injection queue. The ONLY writer of message text into a
//! PTY. Gating, drain-then-`/clear` ordering, and restart failure semantics
//! all live in this single worker task — serialization IS the correctness story.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use tokio::sync::{mpsc, oneshot, watch};

use crate::pty::{BLOCKED_GRACE, Blocked, ClearGate, Cursor, IdleDecision, InjectError, Injected};
use crate::time::Timestamp;
use crate::types::agent::AgentState;
use crate::types::id::AgentId;

/// Gap between typing an injection and pressing Enter. Claude Code drops an
/// Enter that arrives in the same write while its input box is being rebuilt.
const SUBMIT_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

/// How long after Enter the worker waits for the agent's `UserPromptSubmit`
/// hook (debounced idle → working) before pressing Enter again (#54). Claude
/// Code drops an Enter that lands while it is still drawing its welcome box at
/// spawn; no hook announces "prompt ready", so the submit itself is verified.
/// The trade-off behind 2 s: hooks are a `tempo` process plus an HTTP call, so
/// under heavy load a submit *and* a permission dialog could both report later
/// than this, and the resend would then take the dialog's default.
const SUBMIT_VERIFY: std::time::Duration = std::time::Duration::from_secs(2);
/// Enter is resent at most this many times per injection; then a warning.
const MAX_ENTER_RESENDS: u32 = 2;

/// Settle time between a blocked→clear flip and the gate re-run it triggers.
/// `report_state` clears the flag and moves the raw state in one call, but the
/// debounced Working reaches this worker one debouncer hop later; poking in
/// that window would nudge an agent that just woke to answer its dialog.
const UNBLOCK_SETTLE: std::time::Duration = std::time::Duration::from_millis(250);

pub(crate) enum QueueCmd {
    Inject {
        text: String,
        done: oneshot::Sender<Result<Injected, InjectError>>,
    },
    /// The router's sweeper asks the worker to re-run the idle gate now
    /// (spec 2026-08-17 §4.1). Not an injection: no depth accounting, and it
    /// may only deliver a nudge — a poke that finds `AllowClear` holds quiet,
    /// because `/clear` stays the working→idle transition path's alone.
    Reconsider,
}

pub(crate) struct QueueWorker {
    pub(crate) agent: AgentId,
    pub(crate) cmds: mpsc::UnboundedReceiver<QueueCmd>,
    /// Debounced state signal (Task 12). Actions key off this, never raw.
    pub(crate) debounced: watch::Receiver<AgentState>,
    /// Bumped by `PtyManager::restart`; any change fails in-flight + queued.
    pub(crate) epoch: watch::Receiver<u64>,
    /// The permission dialog the agent is parked on, if any (#63). While set,
    /// nothing is typed: injections park (bounded by [`BLOCKED_GRACE`] from
    /// the dialog's `since`), nudges and `/clear` hold. Text into a dialog
    /// answers it — a leading digit picks an option, Enter takes the default.
    pub(crate) blocked: watch::Receiver<Option<Blocked>>,
    /// Serialized PTY write channel (owned by the manager's write pump).
    pub(crate) writer: mpsc::Sender<Vec<u8>>,
    /// Mirrors the agent ring's end cursor (updated by the flush pipeline).
    pub(crate) end_cursor: Arc<AtomicU64>,
    pub(crate) auto_clear: bool,
    pub(crate) clear_gate: Arc<OnceLock<Weak<dyn ClearGate>>>,
    /// Last debounced state this worker observed (transition detection).
    pub(crate) prev: AgentState,
    /// Shared with `PtyManager::enqueue`, which increments on send; this worker
    /// decrements exactly once per resolved command (spec triggers §2).
    pub(crate) depth: Arc<AtomicU64>,
    /// An injection has been delivered and the debounced state has not moved
    /// since. The agent still reads idle — its `UserPromptSubmit` hook has not
    /// fired yet — so a poke that arrives in this window would nudge it for the
    /// message it was just handed. Set on a delivered injection, cleared on any
    /// debounced state change.
    pub(crate) served_inject_since_idle: bool,
}

/// Which path reached [`QueueWorker::consult_gate`]. Only the working→idle
/// transition may type `/clear`: it alone knows the agent just finished a turn.
/// A poke arrives on the sweeper's cadence, so clearing on one would wipe an
/// idle session at an arbitrary moment; a poke may deliver a nudge only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateEntry {
    Transition,
    Poke,
}

/// Sleeps until `at`; pends forever on `None`, so it is inert in a select
/// unless there is a dialog whose grace can run out.
async fn sleep_until_or_forever(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

impl QueueWorker {
    pub(crate) async fn run(mut self) {
        loop {
            tokio::select! {
                cmd = self.cmds.recv() => {
                    match cmd {
                        Some(QueueCmd::Inject { text, done }) => {
                            self.handle_inject(text, done).await;
                        }
                        Some(QueueCmd::Reconsider) => {
                            self.consult_gate(GateEntry::Poke).await;
                        }
                        None => return,
                    }
                }
                changed = self.debounced.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let state = *self.debounced.borrow_and_update();
                    let was = self.prev;
                    self.prev = state;
                    self.served_inject_since_idle = false;
                    match state {
                        AgentState::Idle if was == AgentState::Working => {
                            self.drain_then_consult(GateEntry::Transition).await;
                        }
                        AgentState::Exited => {
                            self.fail_queued(&InjectError::AgentExited(self.agent.clone()));
                        }
                        AgentState::Restarting => {
                            self.fail_queued(&InjectError::AgentRestarted(self.agent.clone()));
                        }
                        AgentState::Starting | AgentState::Working | AgentState::Idle => {}
                    }
                }
                changed = self.epoch.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    self.epoch.borrow_and_update();
                    self.fail_queued(&InjectError::AgentRestarted(self.agent.clone()));
                }
                changed = self.blocked.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let cleared = self.blocked.borrow_and_update().is_none();
                    if cleared {
                        self.reconsider_after_unblock().await;
                    }
                }
            }
        }
    }

    async fn handle_inject(
        &mut self,
        text: String,
        done: oneshot::Sender<Result<Injected, InjectError>>,
    ) {
        // `depth` is decremented before the caller's oneshot is resolved, in
        // every arm: a caller that has seen its injection acked must never
        // read a queue_depth that still counts it (spec triggers §2), and the
        // reverse order is a real race for anyone polling GET /v1/agents/{id}.
        match self.wait_debounced_idle().await {
            Ok(()) => {
                // The Enter must be a separate, later write. Glued to the text
                // it is swallowed when Claude Code is still rebuilding its
                // input box (right after spawn, or after the session restart
                // that /clear triggers), leaving the prompt typed but never
                // submitted.
                if self.writer.send(text.into_bytes()).await.is_err() {
                    self.depth.fetch_sub(1, Ordering::SeqCst);
                    let _ = done.send(Err(InjectError::AgentExited(self.agent.clone())));
                    return;
                }
                tokio::time::sleep(SUBMIT_DELAY).await;
                if let Err(err) = self.press_enter().await {
                    self.depth.fetch_sub(1, Ordering::SeqCst);
                    let _ = done.send(Err(err.clone()));
                    self.fail_queued(&err);
                    return;
                }
                let injected = Injected {
                    at: Timestamp::now(),
                    cursor: Cursor(self.end_cursor.load(Ordering::SeqCst)),
                };
                self.depth.fetch_sub(1, Ordering::SeqCst);
                self.served_inject_since_idle = true;
                let _ = done.send(Ok(injected));
                self.confirm_submission().await;
            }
            Err(err) => {
                self.depth.fetch_sub(1, Ordering::SeqCst);
                let _ = done.send(Err(err.clone()));
                self.fail_queued(&err);
            }
        }
    }

    /// When the current dialog's grace runs out, if there is one; `Err` once
    /// it already has.
    fn dialog_grace(&self) -> Result<Option<tokio::time::Instant>, InjectError> {
        let Some(dialog) = self.blocked.borrow().clone() else {
            return Ok(None);
        };
        let waited = tokio::time::Instant::now().duration_since(dialog.since);
        if waited >= BLOCKED_GRACE {
            return Err(InjectError::Blocked {
                agent: self.agent.clone(),
                tool: dialog.tool,
                waited,
            });
        }
        tracing::debug!(agent = %self.agent, tool = dialog.tool.as_deref().unwrap_or("?"),
                        "agent is on a permission dialog; parking injection");
        Ok(Some(dialog.since + BLOCKED_GRACE))
    }

    /// The Enter that submits typed text, as its own write after
    /// `SUBMIT_DELAY`. Withheld if a dialog opened in that gap: Enter would
    /// take its default. The text is left in the input box unsubmitted —
    /// the injection fails `Blocked` and the caller re-fires.
    async fn press_enter(&mut self) -> Result<(), InjectError> {
        if let Some(dialog) = self.blocked.borrow().clone() {
            tracing::warn!(agent = %self.agent, tool = dialog.tool.as_deref().unwrap_or("?"),
                           "dialog opened between text and Enter; Enter withheld, text stranded");
            return Err(InjectError::Blocked {
                agent: self.agent.clone(),
                tool: dialog.tool,
                waited: tokio::time::Instant::now().duration_since(dialog.since),
            });
        }
        if self.writer.send(vec![b'\r']).await.is_err() {
            return Err(InjectError::AgentExited(self.agent.clone()));
        }
        Ok(())
    }

    /// Waits for the submit to be confirmed — the debounced state moving off
    /// idle (`UserPromptSubmit` → working) — and presses Enter again, up to
    /// [`MAX_ENTER_RESENDS`] times, while it is not (#54). Anything else that
    /// moves ends the wait: an exit, restart or epoch bump makes the resend
    /// moot, and a permission dialog (#63) makes it dangerous — Enter would
    /// accept its default. Reads cloned receivers so the run loop still sees
    /// every change itself. An Enter on an already-submitted prompt is a
    /// no-op, so a slow hook costs nothing.
    async fn confirm_submission(&mut self) {
        let mut states = self.debounced.clone();
        let mut blocked = self.blocked.clone();
        let mut epoch = self.epoch.clone();
        for resend in 1..=MAX_ENTER_RESENDS + 1 {
            tokio::select! {
                _ = states.changed() => return,
                _ = epoch.changed() => return,
                // Only real flips bump this watch, and the flag was clear
                // when the text went in: a change means a dialog opened.
                _ = blocked.changed() => return,
                () = tokio::time::sleep(SUBMIT_VERIFY) => {}
            }
            if resend > MAX_ENTER_RESENDS {
                tracing::warn!(agent = %self.agent, resends = MAX_ENTER_RESENDS,
                               "prompt still idle after Enter was resent; giving up");
                return;
            }
            tracing::debug!(agent = %self.agent, resend, "no submit hook after Enter; resending");
            if self.writer.send(vec![b'\r']).await.is_err() {
                return;
            }
        }
    }

    /// The dialog went away: if the agent is still idle, the decision the
    /// transition held is due now — poke-style, so it may nudge but never
    /// `/clear`. A state change during the settle means the agent woke; the
    /// main loop owns that transition, so this does nothing.
    async fn reconsider_after_unblock(&mut self) {
        tokio::time::sleep(UNBLOCK_SETTLE).await;
        if self.debounced.has_changed().unwrap_or(true) {
            return;
        }
        self.drain_then_consult(GateEntry::Poke).await;
    }

    /// Resolves once the agent is debounced-idle **and** not parked on a
    /// permission dialog. A blocked idle agent parks the injection until the
    /// flag clears; once the dialog is [`BLOCKED_GRACE`] old the injection
    /// fails [`InjectError::Blocked`] instead (#63).
    async fn wait_debounced_idle(&mut self) -> Result<(), InjectError> {
        loop {
            let state = *self.debounced.borrow_and_update();
            self.prev = state;
            match state {
                AgentState::Exited => {
                    return Err(InjectError::AgentExited(self.agent.clone()));
                }
                AgentState::Restarting => {
                    return Err(InjectError::AgentRestarted(self.agent.clone()));
                }
                AgentState::Starting | AgentState::Working | AgentState::Idle => {}
            }
            // A dialog past the grace fails the injection at any state — the
            // in-turn dialog leaves the agent working forever, and a message
            // queued behind it must not park without bound.
            let grace_over = self.dialog_grace()?;
            if state == AgentState::Idle && grace_over.is_none() {
                return Ok(());
            }
            self.blocked.mark_unchanged();
            tokio::select! {
                changed = self.debounced.changed() => {
                    if changed.is_err() {
                        return Err(InjectError::AgentExited(self.agent.clone()));
                    }
                    self.served_inject_since_idle = false;
                }
                changed = self.blocked.changed() => {
                    if changed.is_err() {
                        return Err(InjectError::AgentExited(self.agent.clone()));
                    }
                }
                () = sleep_until_or_forever(grace_over) => {}
                changed = self.epoch.changed() => {
                    self.epoch.borrow_and_update();
                    if changed.is_err() {
                        return Err(InjectError::AgentExited(self.agent.clone()));
                    }
                    return Err(InjectError::AgentRestarted(self.agent.clone()));
                }
            }
        }
    }

    /// Strict drain-then-decide (spec §2): on a debounced working→idle
    /// transition — and on an unblock at idle, poke-style — first inject
    /// everything already queued; a drained message arms or continues a turn,
    /// so nothing else happens this time. Otherwise the gate decides: nudge
    /// (typed like any injection, Enter as a separate write), allow `/clear`
    /// (still subject to `auto_clear`; a poke never clears), or hold quiet.
    async fn drain_then_consult(&mut self, entry: GateEntry) {
        let mut drained = false;
        while let Ok(cmd) = self.cmds.try_recv() {
            match cmd {
                QueueCmd::Inject { text, done } => {
                    drained = true;
                    self.handle_inject(text, done).await;
                }
                QueueCmd::Reconsider => {}
            }
        }
        if drained {
            return;
        }
        self.consult_gate(entry).await;
    }

    /// Re-run the idle gate now: at a debounced idle, ask `ClearGate` what to
    /// do and act on the verdict. Not idle, do nothing. This is the shared
    /// remainder of a working→idle transition (after the drain) and of a
    /// direct `QueueCmd::Reconsider` poke (spec 2026-08-17 §4.1) — which is
    /// allowed to nudge and nothing else, see [`GateEntry`].
    async fn consult_gate(&mut self, entry: GateEntry) {
        let Some(gate) = self.clear_gate.get().and_then(Weak::upgrade) else {
            return;
        };
        if *self.debounced.borrow() != AgentState::Idle {
            return;
        }
        if self.blocked.borrow().is_some() {
            tracing::debug!(agent = %self.agent,
                            "idle on a permission dialog; holding nudge and /clear");
            return;
        }
        if entry == GateEntry::Poke && self.served_inject_since_idle {
            tracing::debug!(agent = %self.agent,
                            "poke arrived behind a just-delivered injection; holding quiet");
            return;
        }
        match gate.on_stable_idle(&self.agent) {
            IdleDecision::Nudge(text) => {
                if self.writer.send(text.into_bytes()).await.is_err() {
                    tracing::warn!(agent = %self.agent, "nudge write failed: pty gone");
                    return;
                }
                tokio::time::sleep(SUBMIT_DELAY).await;
                if self.press_enter().await.is_err() {
                    return;
                }
                self.confirm_submission().await;
            }
            IdleDecision::AllowClear => {
                if entry == GateEntry::Poke {
                    tracing::debug!(agent = %self.agent,
                                    "poke found nothing owed; /clear is the transition's alone");
                    return;
                }
                if !self.auto_clear {
                    return;
                }
                if self.writer.send(b"/clear\r".to_vec()).await.is_err() {
                    tracing::warn!(agent = %self.agent, "auto-/clear write failed: pty gone");
                } else {
                    tracing::info!(agent = %self.agent, "typed /clear");
                }
            }
            IdleDecision::HoldQuiet => {}
        }
    }

    fn fail_queued(&mut self, err: &InjectError) {
        while let Ok(cmd) = self.cmds.try_recv() {
            match cmd {
                QueueCmd::Inject { done, .. } => {
                    self.depth.fetch_sub(1, Ordering::SeqCst);
                    let _ = done.send(Err(err.clone()));
                }
                QueueCmd::Reconsider => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, OnceLock, Weak};
    use std::time::Duration;

    use tokio::sync::{mpsc, oneshot, watch};

    use crate::pty::queue::{MAX_ENTER_RESENDS, QueueCmd, QueueWorker, SUBMIT_VERIFY};
    use crate::pty::{BLOCKED_GRACE, Blocked, ClearGate, Cursor, IdleDecision, InjectError};
    use crate::types::agent::AgentState;
    use crate::types::id::AgentId;

    struct FixedGate(IdleDecision);

    impl ClearGate for FixedGate {
        fn on_stable_idle(&self, _: &AgentId) -> IdleDecision {
            self.0.clone()
        }
    }

    struct Harness {
        cmd_tx: mpsc::UnboundedSender<QueueCmd>,
        state_tx: watch::Sender<AgentState>,
        /// Held so the worker's epoch channel stays open; bumped by the restart
        /// tests to fail in-flight and queued injections.
        epoch_tx: watch::Sender<u64>,
        /// The permission-dialog flag `PtyManager` keeps per agent (#63): the
        /// worker parks injections and holds the gate while it is `Some`.
        blocked_tx: watch::Sender<Option<Blocked>>,
        written: mpsc::Receiver<Vec<u8>>,
        /// Shared with the worker; mirrors `PtyManager::enqueue`'s increment so the
        /// harness can assert queue depth without a real manager.
        depth: Arc<AtomicU64>,
        /// Keeps the gate alive: the worker only holds a `Weak` (production
        /// wiring breaks a real `Router`<->`PtyManager` cycle the same way).
        /// `None` for the dead-gate harness, which drops its strong ref before
        /// the worker ever gets to decide (see `start_dead_gate`).
        _gate: Option<Arc<dyn ClearGate>>,
    }

    fn start(initial: AgentState, auto_clear: bool, decision: IdleDecision) -> Harness {
        let strong_gate: Arc<dyn ClearGate> = Arc::new(FixedGate(decision));
        let gate: Arc<OnceLock<Weak<dyn ClearGate>>> = Arc::new(OnceLock::new());
        let _ = gate.set(Arc::downgrade(&strong_gate));
        start_with_gate(initial, auto_clear, gate, Some(strong_gate))
    }

    /// A harness whose clear gate is already dead by the time the worker makes
    /// its first idle decision: exercises the "hold quiet" branch a live
    /// `Weak::upgrade` failure takes (production's guard against typing
    /// `/clear` into a PTY mid-teardown, when the `Router` may already be gone).
    fn start_dead_gate(initial: AgentState, auto_clear: bool) -> Harness {
        let gate: Arc<OnceLock<Weak<dyn ClearGate>>> = Arc::new(OnceLock::new());
        {
            let strong: Arc<dyn ClearGate> = Arc::new(FixedGate(IdleDecision::AllowClear));
            let _ = gate.set(Arc::downgrade(&strong));
            // `strong` drops here: the slot now holds a `Weak` that never upgrades.
        }
        start_with_gate(initial, auto_clear, gate, None)
    }

    fn start_with_gate(
        initial: AgentState,
        auto_clear: bool,
        gate: Arc<OnceLock<Weak<dyn ClearGate>>>,
        keep_alive: Option<Arc<dyn ClearGate>>,
    ) -> Harness {
        let (cmd_tx, cmds) = mpsc::unbounded_channel();
        let (state_tx, debounced) = watch::channel(initial);
        let (epoch_tx, epoch) = watch::channel(0_u64);
        let (blocked_tx, blocked) = watch::channel(None);
        let (writer, written) = mpsc::channel(16);
        let depth = Arc::new(AtomicU64::new(0));
        let worker = QueueWorker {
            agent: AgentId("builder".into()),
            cmds,
            debounced,
            epoch,
            blocked,
            writer,
            end_cursor: Arc::new(AtomicU64::new(7)),
            auto_clear,
            clear_gate: gate,
            prev: initial,
            depth: Arc::clone(&depth),
            served_inject_since_idle: false,
        };
        tokio::spawn(worker.run());
        Harness {
            cmd_tx,
            state_tx,
            epoch_tx,
            blocked_tx,
            written,
            depth,
            _gate: keep_alive,
        }
    }

    fn inject(
        h: &Harness,
        text: &str,
    ) -> oneshot::Receiver<Result<crate::pty::Injected, InjectError>> {
        let (done, rx) = oneshot::channel();
        h.depth.fetch_add(1, Ordering::SeqCst);
        h.cmd_tx
            .send(QueueCmd::Inject {
                text: text.into(),
                done,
            })
            .unwrap();
        rx
    }

    /// One injection is two writes now: the typed text, then Enter.
    async fn recv_injection(written: &mut mpsc::Receiver<Vec<u8>>) -> Vec<u8> {
        let mut text = written.recv().await.unwrap();
        let enter = written.recv().await.unwrap();
        assert_eq!(enter, b"\r".to_vec(), "Enter must be a separate write");
        text.extend_from_slice(&enter);
        text
    }

    #[tokio::test(start_paused = true)]
    async fn inject_waits_for_debounced_idle() {
        let mut h = start(AgentState::Working, false, IdleDecision::AllowClear);
        let done = inject(&h, "[CoreTempo m-aaaaaaaa from planner] hi");

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            h.written.try_recv().is_err(),
            "must not inject while working"
        );

        h.state_tx.send(AgentState::Idle).unwrap();
        let injected = done.await.unwrap().unwrap();
        assert_eq!(injected.cursor, Cursor(7));
        assert_eq!(
            recv_injection(&mut h.written).await,
            b"[CoreTempo m-aaaaaaaa from planner] hi\r".to_vec(),
            "text is submitted with a trailing carriage return"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn injections_are_serialized_in_fifo_order() {
        let mut h = start(AgentState::Idle, false, IdleDecision::AllowClear);
        let d1 = inject(&h, "first");
        let d2 = inject(&h, "second");
        assert!(d1.await.unwrap().is_ok());
        assert_eq!(recv_injection(&mut h.written).await, b"first\r".to_vec());
        // The agent takes the first (its submit hook fires) and finishes the
        // turn; only then is the second typed.
        h.state_tx.send(AgentState::Working).unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        h.state_tx.send(AgentState::Idle).unwrap();
        assert!(d2.await.unwrap().is_ok());
        assert_eq!(recv_injection(&mut h.written).await, b"second\r".to_vec());
    }

    #[tokio::test(start_paused = true)]
    async fn exited_agent_fails_fast() {
        let h = start(AgentState::Exited, false, IdleDecision::AllowClear);
        let done = inject(&h, "hello");
        assert_eq!(
            done.await.unwrap().unwrap_err(),
            InjectError::AgentExited(AgentId("builder".into()))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn exit_while_waiting_fails_inflight_and_queued() {
        let h = start(AgentState::Working, false, IdleDecision::AllowClear);
        let d1 = inject(&h, "one");
        let d2 = inject(&h, "two");
        tokio::time::sleep(Duration::from_millis(10)).await;
        h.state_tx.send(AgentState::Exited).unwrap();
        let expected = InjectError::AgentExited(AgentId("builder".into()));
        assert_eq!(d1.await.unwrap().unwrap_err(), expected);
        assert_eq!(d2.await.unwrap().unwrap_err(), expected);
    }

    #[tokio::test(start_paused = true)]
    async fn epoch_bump_fails_inflight_and_queued_with_restarted() {
        let h = start(AgentState::Working, false, IdleDecision::AllowClear);
        let d1 = inject(&h, "in-flight");
        let d2 = inject(&h, "queued-behind");
        tokio::time::sleep(Duration::from_millis(10)).await;

        h.epoch_tx.send(1).unwrap();

        let expected = InjectError::AgentRestarted(AgentId("builder".into()));
        assert_eq!(d1.await.unwrap().unwrap_err(), expected);
        assert_eq!(d2.await.unwrap().unwrap_err(), expected);
    }

    #[tokio::test(start_paused = true)]
    async fn queue_recovers_after_epoch_bump() {
        let mut h = start(AgentState::Working, false, IdleDecision::AllowClear);
        let dead = inject(&h, "doomed");
        tokio::time::sleep(Duration::from_millis(10)).await;
        h.epoch_tx.send(1).unwrap();
        assert!(dead.await.unwrap().is_err());

        // respawned session comes back idle; new enqueues must succeed
        h.state_tx.send(AgentState::Starting).unwrap();
        h.state_tx.send(AgentState::Idle).unwrap();
        let alive = inject(&h, "alive");
        assert!(alive.await.unwrap().is_ok());
        assert_eq!(recv_injection(&mut h.written).await, b"alive\r".to_vec());
    }

    #[tokio::test(start_paused = true)]
    async fn auto_clear_fires_on_debounced_working_to_idle() {
        let mut h = start(AgentState::Working, true, IdleDecision::AllowClear);
        h.state_tx.send(AgentState::Idle).unwrap();
        assert_eq!(h.written.recv().await.unwrap(), b"/clear\r".to_vec());
    }

    /// THE dedicated race test (spec §4.3): a reply arriving at the idle
    /// transition must be injected and `/clear` must NOT be typed, regardless
    /// of which select branch the worker wakes on first.
    #[tokio::test(start_paused = true)]
    async fn reply_racing_idle_transition_beats_auto_clear() {
        for _ in 0..50 {
            let mut h = start(AgentState::Working, true, IdleDecision::AllowClear);
            let done = inject(
                &h,
                "[CoreTempo reply to m-a3f91c2e from builder — code 0] yes",
            );
            h.state_tx.send(AgentState::Idle).unwrap();

            assert!(done.await.unwrap().is_ok(), "reply must inject, not fail");
            assert_eq!(
                recv_injection(&mut h.written).await,
                "[CoreTempo reply to m-a3f91c2e from builder — code 0] yes\r"
                    .as_bytes()
                    .to_vec(),
                "the reply is the first write"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                h.written.try_recv().is_err(),
                "/clear must never follow a drained injection"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn hold_quiet_blocks_auto_clear() {
        let mut h = start(AgentState::Working, true, IdleDecision::HoldQuiet);
        h.state_tx.send(AgentState::Idle).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            h.written.try_recv().is_err(),
            "pending ask must suppress /clear"
        );
    }

    /// A dead gate (its `Router` already freed — teardown may be underway) must
    /// hold quiet, the same as `HoldQuiet`: typing `/clear` into a dying PTY is
    /// worse than doing nothing (CLAUDE.md). Regression coverage for the
    /// `Weak::upgrade` branch introduced when `clear_gate` stopped being a
    /// strong `Arc`.
    #[tokio::test(start_paused = true)]
    async fn dead_gate_holds_quiet_instead_of_clearing() {
        let mut h = start_dead_gate(AgentState::Working, true);
        h.state_tx.send(AgentState::Idle).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            h.written.try_recv().is_err(),
            "a dead gate must never type /clear"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn nudge_is_typed_with_separate_enter() {
        let mut h = start(
            AgentState::Working,
            true,
            IdleDecision::Nudge("[CoreTempo] finish your steps".into()),
        );
        h.state_tx.send(AgentState::Idle).unwrap();
        assert_eq!(
            recv_injection(&mut h.written).await,
            b"[CoreTempo] finish your steps\r".to_vec(),
            "nudge text then Enter as a separate write"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(h.written.try_recv().is_err(), "no /clear after a nudge");
    }

    #[tokio::test(start_paused = true)]
    async fn nudge_fires_even_for_auto_clear_false_agents() {
        let mut h = start(
            AgentState::Working,
            false,
            IdleDecision::Nudge("[CoreTempo] finish your steps".into()),
        );
        h.state_tx.send(AgentState::Idle).unwrap();
        assert_eq!(
            recv_injection(&mut h.written).await,
            b"[CoreTempo] finish your steps\r".to_vec()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn auto_clear_false_agents_are_never_cleared() {
        let mut h = start(AgentState::Working, false, IdleDecision::AllowClear);
        h.state_tx.send(AgentState::Idle).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(h.written.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn starting_to_idle_does_not_clear() {
        // First prompt paint after spawn is starting→idle, not working→idle:
        // clearing here would wipe a fresh session for no reason.
        let mut h = start(AgentState::Starting, true, IdleDecision::AllowClear);
        h.state_tx.send(AgentState::Idle).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(h.written.try_recv().is_err());
    }

    fn poke(h: &Harness) {
        h.cmd_tx.send(QueueCmd::Reconsider).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn reconsider_at_idle_types_the_gates_nudge() {
        let mut h = start(
            AgentState::Idle,
            true,
            IdleDecision::Nudge("[CoreTempo] still owed".into()),
        );
        poke(&h);
        let typed = recv_injection(&mut h.written).await;
        assert_eq!(typed, b"[CoreTempo] still owed\r".to_vec());
    }

    /// A poke may deliver a nudge and nothing else: `/clear` belongs to the
    /// working→idle transition path, which alone knows a turn just ended. A
    /// poke that found `AllowClear` would clear an idle agent at an arbitrary
    /// moment — the sweeper's cadence, not the agent's.
    #[tokio::test(start_paused = true)]
    async fn reconsider_at_idle_never_types_clear() {
        let mut h = start(AgentState::Idle, true, IdleDecision::AllowClear);
        poke(&h);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            h.written.try_recv().is_err(),
            "a poke must never type /clear"
        );
    }

    /// A poke buffered behind an injection must not nudge for the message that
    /// was just delivered. The agent still reads debounced-idle until its
    /// `UserPromptSubmit` hook fires, so the gate would find the ask owed and
    /// type a second prompt into a pane that has not started the first.
    #[tokio::test(start_paused = true)]
    async fn reconsider_right_after_an_injection_types_nothing() {
        let mut h = start(
            AgentState::Idle,
            true,
            IdleDecision::Nudge("[CoreTempo] still owed".into()),
        );
        let done = inject(&h, "msg");
        assert!(done.await.unwrap().is_ok());
        assert_eq!(recv_injection(&mut h.written).await, b"msg\r".to_vec());

        poke(&h);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            h.written.try_recv().is_err(),
            "no nudge behind a just-delivered injection"
        );

        // The agent picks the message up and finishes the turn: the debounced
        // state moved, so the next poke is free to nudge again.
        h.state_tx.send(AgentState::Working).unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        h.state_tx.send(AgentState::Idle).unwrap();
        assert_eq!(
            recv_injection(&mut h.written).await,
            b"[CoreTempo] still owed\r".to_vec(),
            "the working->idle transition consults the gate itself"
        );
        poke(&h);
        // Nothing here plays the submit hook for that nudge, so the worker
        // resends its Enter (#54) before it serves the poke.
        for _ in 0..MAX_ENTER_RESENDS {
            assert_eq!(h.written.recv().await.unwrap(), b"\r".to_vec());
        }
        assert_eq!(
            recv_injection(&mut h.written).await,
            b"[CoreTempo] still owed\r".to_vec(),
            "and the poke nudges once the state has moved"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reconsider_while_working_is_ignored() {
        let mut h = start(AgentState::Working, true, IdleDecision::AllowClear);
        poke(&h);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(h.written.try_recv().is_err(), "nothing typed while working");
    }

    #[tokio::test(start_paused = true)]
    async fn queue_depth_tracks_enqueue_deliver_and_fail() {
        let mut h = start(AgentState::Working, false, IdleDecision::AllowClear);
        assert_eq!(h.depth.load(Ordering::SeqCst), 0);
        let d1 = inject(&h, "one");
        let d2 = inject(&h, "two");
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            h.depth.load(Ordering::SeqCst),
            2,
            "both queued while working"
        );
        h.state_tx.send(AgentState::Idle).unwrap();
        assert!(d1.await.unwrap().is_ok());
        assert!(d2.await.unwrap().is_ok());
        recv_injection(&mut h.written).await;
        recv_injection(&mut h.written).await;
        assert_eq!(
            h.depth.load(Ordering::SeqCst),
            0,
            "delivered injections drain the depth"
        );

        // Failure path drains it too.
        let h2 = start(AgentState::Working, false, IdleDecision::AllowClear);
        let doomed = inject(&h2, "doomed");
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(h2.depth.load(Ordering::SeqCst), 1);
        h2.state_tx.send(AgentState::Exited).unwrap();
        assert!(doomed.await.unwrap().is_err());
        assert_eq!(h2.depth.load(Ordering::SeqCst), 0);
    }

    fn block(h: &Harness, tool: &str) {
        h.blocked_tx
            .send(Some(Blocked {
                since: tokio::time::Instant::now(),
                tool: Some(tool.into()),
                agent_id: None,
            }))
            .unwrap();
    }

    fn unblock(h: &Harness) {
        h.blocked_tx.send(None).unwrap();
    }

    /// #63: an idle agent whose pane is a permission dialog must not be typed
    /// into — text + Enter would answer the dialog. The injection parks until
    /// the flag clears, then goes out as usual.
    #[tokio::test(start_paused = true)]
    async fn inject_parks_while_blocked_and_delivers_once_unblocked() {
        let mut h = start(AgentState::Idle, false, IdleDecision::HoldQuiet);
        block(&h, "Bash");
        let rx = inject(&h, "hello");
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert!(h.written.try_recv().is_err(), "nothing typed into a dialog");
        assert_eq!(h.depth.load(Ordering::SeqCst), 1, "still queued");
        unblock(&h);
        assert_eq!(recv_injection(&mut h.written).await, b"hello\r".to_vec());
        assert!(rx.await.unwrap().is_ok());
        assert_eq!(h.depth.load(Ordering::SeqCst), 0);
    }

    /// A parked injection is bounded by the same grace the owed-ask sweeper
    /// uses, measured from when the dialog went up: `send`s have no TTL, so an
    /// unbounded park would hang them forever.
    #[tokio::test(start_paused = true)]
    async fn parked_inject_fails_blocked_once_the_dialog_is_grace_old() {
        let mut h = start(AgentState::Idle, false, IdleDecision::HoldQuiet);
        block(&h, "Bash");
        let rx = inject(&h, "hello");
        tokio::time::sleep(BLOCKED_GRACE.saturating_sub(Duration::from_secs(1))).await;
        assert!(h.written.try_recv().is_err());
        tokio::time::sleep(Duration::from_secs(2)).await;
        match rx.await.unwrap() {
            Err(InjectError::Blocked { agent, tool, .. }) => {
                assert_eq!(agent, AgentId("builder".into()));
                assert_eq!(tool.as_deref(), Some("Bash"));
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
        assert!(
            h.written.try_recv().is_err(),
            "a failed injection types nothing"
        );
        assert_eq!(h.depth.load(Ordering::SeqCst), 0);
    }

    /// The grace is the dialog's age, not the injection's wait: a message
    /// aimed at an agent already parked past the grace fails at once.
    #[tokio::test(start_paused = true)]
    async fn inject_to_an_agent_blocked_past_the_grace_fails_immediately() {
        let h = start(AgentState::Idle, false, IdleDecision::HoldQuiet);
        block(&h, "Bash");
        tokio::time::sleep(BLOCKED_GRACE + Duration::from_secs(1)).await;
        let rx = inject(&h, "hello");
        assert!(matches!(
            rx.await.unwrap(),
            Err(InjectError::Blocked { .. })
        ));
    }

    /// The gate is never consulted while blocked, so neither a nudge nor
    /// `/clear` reaches the dialog. Once the flag clears at idle the gate is
    /// re-run poke-style: the deferred nudge goes out.
    #[tokio::test(start_paused = true)]
    async fn nudge_is_held_while_blocked_and_typed_once_unblocked() {
        let mut h = start(
            AgentState::Working,
            true,
            IdleDecision::Nudge("[CoreTempo] finish your steps".into()),
        );
        block(&h, "Bash");
        h.state_tx.send(AgentState::Idle).unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(h.written.try_recv().is_err(), "no nudge into a dialog");
        unblock(&h);
        assert_eq!(
            recv_injection(&mut h.written).await,
            b"[CoreTempo] finish your steps\r".to_vec()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn clear_is_held_while_blocked_and_never_typed_by_the_unblock() {
        let mut h = start(AgentState::Working, true, IdleDecision::AllowClear);
        block(&h, "Bash");
        h.state_tx.send(AgentState::Idle).unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(h.written.try_recv().is_err(), "no /clear into a dialog");
        unblock(&h);
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(
            h.written.try_recv().is_err(),
            "unblocking re-runs the gate as a poke, which may not /clear"
        );
    }

    /// `report_state` clears the flag and moves the raw state in one call, but
    /// the debounced Working reaches the worker a hop later. The unblock poke
    /// must not run against that stale idle and nudge an agent that just woke.
    #[tokio::test(start_paused = true)]
    async fn unblock_racing_a_working_transition_types_nothing() {
        let mut h = start(
            AgentState::Idle,
            true,
            IdleDecision::Nudge("[CoreTempo] finish your steps".into()),
        );
        block(&h, "Bash");
        tokio::time::sleep(Duration::from_secs(1)).await;
        unblock(&h);
        tokio::task::yield_now().await; // the debouncer's forwarding hop
        h.state_tx.send(AgentState::Working).unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(
            h.written.try_recv().is_err(),
            "the agent is working; no nudge"
        );
    }

    /// #54: Claude Code drops the Enter when it is still drawing its welcome
    /// box at spawn. `UserPromptSubmit` → working is the only confirmation of a
    /// submit, so a debounced state that never moves means Enter again — a
    /// bounded number of times.
    #[tokio::test(start_paused = true)]
    async fn enter_is_resent_while_the_agent_stays_idle_after_an_injection() {
        let mut h = start(AgentState::Idle, false, IdleDecision::HoldQuiet);
        let rx = inject(&h, "kickoff");
        assert_eq!(recv_injection(&mut h.written).await, b"kickoff\r".to_vec());
        assert!(rx.await.unwrap().is_ok(), "ack is not held for the verify");
        for _ in 0..MAX_ENTER_RESENDS {
            tokio::time::sleep(SUBMIT_VERIFY.saturating_sub(Duration::from_millis(100))).await;
            assert!(h.written.try_recv().is_err(), "verify window not over yet");
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(
                h.written.try_recv().unwrap(),
                b"\r".to_vec(),
                "Enter resent"
            );
        }
        tokio::time::sleep(SUBMIT_VERIFY * 3).await;
        assert!(h.written.try_recv().is_err(), "resends are bounded");
    }

    #[tokio::test(start_paused = true)]
    async fn no_enter_is_resent_once_the_agent_goes_working() {
        let mut h = start(AgentState::Idle, false, IdleDecision::HoldQuiet);
        let _rx = inject(&h, "kickoff");
        assert_eq!(recv_injection(&mut h.written).await, b"kickoff\r".to_vec());
        tokio::time::sleep(Duration::from_millis(300)).await;
        h.state_tx.send(AgentState::Working).unwrap();
        tokio::time::sleep(SUBMIT_VERIFY * 4).await;
        assert!(h.written.try_recv().is_err(), "the submit was confirmed");
    }

    /// A dialog that comes up during the verify window (#63) ends it: a resent
    /// Enter would accept the dialog's default.
    #[tokio::test(start_paused = true)]
    async fn no_enter_is_resent_into_a_dialog_that_appears_after_the_injection() {
        let mut h = start(AgentState::Idle, false, IdleDecision::HoldQuiet);
        let _rx = inject(&h, "kickoff");
        assert_eq!(recv_injection(&mut h.written).await, b"kickoff\r".to_vec());
        tokio::time::sleep(Duration::from_millis(300)).await;
        block(&h, "Bash");
        tokio::time::sleep(SUBMIT_VERIFY * 4).await;
        assert!(h.written.try_recv().is_err(), "never Enter into a dialog");
    }

    /// Nudges are typed like injections and lose their Enter the same way.
    #[tokio::test(start_paused = true)]
    async fn nudge_enter_is_resent_while_the_agent_stays_idle() {
        let mut h = start(
            AgentState::Working,
            true,
            IdleDecision::Nudge("[CoreTempo] finish your steps".into()),
        );
        h.state_tx.send(AgentState::Idle).unwrap();
        assert_eq!(
            recv_injection(&mut h.written).await,
            b"[CoreTempo] finish your steps\r".to_vec()
        );
        tokio::time::sleep(SUBMIT_VERIFY + Duration::from_millis(100)).await;
        assert_eq!(
            h.written.try_recv().unwrap(),
            b"\r".to_vec(),
            "Enter resent"
        );
    }

    /// The common dialog: the agent is *working* when its tool call parks on
    /// permission. A message queued behind it must fail on the same grace,
    /// not park forever (a `send` has no TTL) — and the grace timer must not
    /// spin the worker once it has passed.
    #[tokio::test(start_paused = true)]
    async fn inject_behind_a_working_blocked_agent_fails_once_the_grace_is_spent() {
        let mut h = start(AgentState::Working, false, IdleDecision::HoldQuiet);
        let rx = inject(&h, "queued");
        block(&h, "Bash");
        tokio::time::sleep(BLOCKED_GRACE + Duration::from_secs(1)).await;
        assert!(matches!(
            rx.await.unwrap(),
            Err(InjectError::Blocked { .. })
        ));
        assert!(h.written.try_recv().is_err());
        assert_eq!(h.depth.load(Ordering::SeqCst), 0);
    }

    /// One dialog outlasting the grace fails everything queued behind it with
    /// the same reason, so no caller waits on text that will never be typed.
    #[tokio::test(start_paused = true)]
    async fn a_blocked_failure_fails_the_rest_of_the_queue_blocked() {
        let h = start(AgentState::Idle, false, IdleDecision::HoldQuiet);
        block(&h, "Bash");
        let first = inject(&h, "one");
        let second = inject(&h, "two");
        tokio::time::sleep(BLOCKED_GRACE + Duration::from_secs(1)).await;
        assert!(matches!(
            first.await.unwrap(),
            Err(InjectError::Blocked { .. })
        ));
        assert!(matches!(
            second.await.unwrap(),
            Err(InjectError::Blocked { .. })
        ));
        assert_eq!(h.depth.load(Ordering::SeqCst), 0);
    }

    /// A restart during the verify window makes the resend moot: the session
    /// that lost the Enter is gone.
    #[tokio::test(start_paused = true)]
    async fn no_enter_is_resent_after_an_epoch_bump() {
        let mut h = start(AgentState::Idle, false, IdleDecision::HoldQuiet);
        let _rx = inject(&h, "kickoff");
        assert_eq!(recv_injection(&mut h.written).await, b"kickoff\r".to_vec());
        tokio::time::sleep(Duration::from_millis(300)).await;
        h.epoch_tx.send(1).unwrap();
        tokio::time::sleep(SUBMIT_VERIFY * 4).await;
        assert!(
            h.written.try_recv().is_err(),
            "the session restarted; nothing to resend into"
        );
    }

    /// A dialog that opens in the `SUBMIT_DELAY` gap between the text and its
    /// Enter: the Enter is withheld (it would take the dialog's default) and
    /// the injection fails `Blocked` at once.
    #[tokio::test(start_paused = true)]
    async fn enter_is_withheld_when_a_dialog_opens_between_text_and_enter() {
        let mut h = start(AgentState::Idle, false, IdleDecision::HoldQuiet);
        let rx = inject(&h, "kickoff");
        assert_eq!(h.written.recv().await.unwrap(), b"kickoff".to_vec());
        block(&h, "Bash");
        match rx.await.unwrap() {
            Err(InjectError::Blocked { .. }) => {}
            other => panic!("expected Blocked, got {other:?}"),
        }
        tokio::time::sleep(SUBMIT_VERIFY * 4).await;
        assert!(h.written.try_recv().is_err(), "no Enter into the dialog");
        assert_eq!(h.depth.load(Ordering::SeqCst), 0);
    }

    /// The unblock re-run is drain-then-decide like the transition: a message
    /// queued while the dialog was up goes out first, and it arms or continues
    /// a turn, so the gate is not consulted at all.
    #[tokio::test(start_paused = true)]
    async fn unblock_drains_queued_injections_before_consulting_the_gate() {
        let mut h = start(
            AgentState::Idle,
            true,
            IdleDecision::Nudge("[CoreTempo] still owed".into()),
        );
        block(&h, "Bash");
        let rx = inject(&h, "queued");
        tokio::time::sleep(Duration::from_secs(1)).await;
        unblock(&h);
        assert_eq!(recv_injection(&mut h.written).await, b"queued\r".to_vec());
        assert!(rx.await.unwrap().is_ok());
        h.state_tx.send(AgentState::Working).unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(
            h.written.try_recv().is_err(),
            "the drained message opened the turn; no nudge"
        );
    }
}
