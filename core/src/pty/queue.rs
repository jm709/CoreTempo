//! Per-agent serialized injection queue. The ONLY writer of message text into a
//! PTY. Gating, drain-then-`/clear` ordering, and restart failure semantics
//! all live in this single worker task — serialization IS the correctness story.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use tokio::sync::{mpsc, oneshot, watch};

use crate::pty::{ClearGate, Cursor, IdleDecision, InjectError, Injected};
use crate::time::Timestamp;
use crate::types::agent::AgentState;
use crate::types::id::AgentId;

/// Gap between typing an injection and pressing Enter. Claude Code drops an
/// Enter that arrives in the same write while its input box is being rebuilt.
const SUBMIT_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

pub(crate) enum QueueCmd {
    Inject {
        text: String,
        done: oneshot::Sender<Result<Injected, InjectError>>,
    },
}

pub(crate) struct QueueWorker {
    pub(crate) agent: AgentId,
    pub(crate) cmds: mpsc::UnboundedReceiver<QueueCmd>,
    /// Debounced state signal (Task 12). Actions key off this, never raw.
    pub(crate) debounced: watch::Receiver<AgentState>,
    /// Bumped by `PtyManager::restart`; any change fails in-flight + queued.
    pub(crate) epoch: watch::Receiver<u64>,
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
}

impl QueueWorker {
    pub(crate) async fn run(mut self) {
        loop {
            tokio::select! {
                cmd = self.cmds.recv() => {
                    let Some(QueueCmd::Inject { text, done }) = cmd else {
                        return;
                    };
                    self.handle_inject(text, done).await;
                }
                changed = self.debounced.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let state = *self.debounced.borrow_and_update();
                    let was = self.prev;
                    self.prev = state;
                    match state {
                        AgentState::Idle if was == AgentState::Working => {
                            self.drain_then_maybe_clear().await;
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
            }
        }
    }

    async fn handle_inject(
        &mut self,
        text: String,
        done: oneshot::Sender<Result<Injected, InjectError>>,
    ) {
        match self.wait_debounced_idle().await {
            Ok(()) => {
                // The Enter must be a separate, later write. Glued to the text
                // it is swallowed when Claude Code is still rebuilding its
                // input box (right after spawn, or after the session restart
                // that /clear triggers), leaving the prompt typed but never
                // submitted.
                if self.writer.send(text.into_bytes()).await.is_err() {
                    let _ = done.send(Err(InjectError::AgentExited(self.agent.clone())));
                    self.depth.fetch_sub(1, Ordering::SeqCst);
                    return;
                }
                tokio::time::sleep(SUBMIT_DELAY).await;
                if self.writer.send(vec![b'\r']).await.is_err() {
                    let _ = done.send(Err(InjectError::AgentExited(self.agent.clone())));
                    self.depth.fetch_sub(1, Ordering::SeqCst);
                    return;
                }
                let injected = Injected {
                    at: Timestamp::now(),
                    cursor: Cursor(self.end_cursor.load(Ordering::SeqCst)),
                };
                let _ = done.send(Ok(injected));
                self.depth.fetch_sub(1, Ordering::SeqCst);
            }
            Err(err) => {
                let _ = done.send(Err(err.clone()));
                self.depth.fetch_sub(1, Ordering::SeqCst);
                self.fail_queued(&err);
            }
        }
    }

    async fn wait_debounced_idle(&mut self) -> Result<(), InjectError> {
        loop {
            let state = *self.debounced.borrow_and_update();
            self.prev = state;
            match state {
                AgentState::Idle => return Ok(()),
                AgentState::Exited => {
                    return Err(InjectError::AgentExited(self.agent.clone()));
                }
                AgentState::Restarting => {
                    return Err(InjectError::AgentRestarted(self.agent.clone()));
                }
                AgentState::Starting | AgentState::Working => {}
            }
            tokio::select! {
                changed = self.debounced.changed() => {
                    if changed.is_err() {
                        return Err(InjectError::AgentExited(self.agent.clone()));
                    }
                }
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
    /// transition, first inject everything already queued — a drained message
    /// arms or continues a turn, so nothing else happens this transition.
    /// Otherwise the gate decides: nudge (typed like any injection, Enter as a
    /// separate write), allow `/clear` (still subject to `auto_clear`), or
    /// hold quiet.
    async fn drain_then_maybe_clear(&mut self) {
        let mut drained = false;
        while let Ok(QueueCmd::Inject { text, done }) = self.cmds.try_recv() {
            drained = true;
            self.handle_inject(text, done).await;
        }
        if drained {
            return;
        }
        let Some(gate) = self.clear_gate.get().and_then(Weak::upgrade) else {
            return;
        };
        if *self.debounced.borrow() != AgentState::Idle {
            return;
        }
        match gate.on_stable_idle(&self.agent) {
            IdleDecision::Nudge(text) => {
                if self.writer.send(text.into_bytes()).await.is_err() {
                    tracing::warn!(agent = %self.agent, "nudge write failed: pty gone");
                    return;
                }
                tokio::time::sleep(SUBMIT_DELAY).await;
                if self.writer.send(vec![b'\r']).await.is_err() {
                    tracing::warn!(agent = %self.agent, "nudge Enter failed: pty gone");
                }
            }
            IdleDecision::AllowClear => {
                if !self.auto_clear {
                    return;
                }
                if self.writer.send(b"/clear\r".to_vec()).await.is_err() {
                    tracing::warn!(agent = %self.agent, "auto-/clear write failed: pty gone");
                }
            }
            IdleDecision::HoldQuiet => {}
        }
    }

    fn fail_queued(&mut self, err: &InjectError) {
        while let Ok(QueueCmd::Inject { done, .. }) = self.cmds.try_recv() {
            let _ = done.send(Err(err.clone()));
            self.depth.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, OnceLock, Weak};
    use std::time::Duration;

    use tokio::sync::{mpsc, oneshot, watch};

    use crate::pty::queue::{QueueCmd, QueueWorker};
    use crate::pty::{ClearGate, Cursor, IdleDecision, InjectError};
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
        let (writer, written) = mpsc::channel(16);
        let depth = Arc::new(AtomicU64::new(0));
        let worker = QueueWorker {
            agent: AgentId("builder".into()),
            cmds,
            debounced,
            epoch,
            writer,
            end_cursor: Arc::new(AtomicU64::new(7)),
            auto_clear,
            clear_gate: gate,
            prev: initial,
            depth: Arc::clone(&depth),
        };
        tokio::spawn(worker.run());
        Harness {
            cmd_tx,
            state_tx,
            epoch_tx,
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
        assert!(d2.await.unwrap().is_ok());
        assert_eq!(recv_injection(&mut h.written).await, b"first\r".to_vec());
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
}
