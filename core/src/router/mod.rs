//! Message router (contracts §4): message lifecycle state machine, pending-asks accounting,
//! reply idempotency, per-origin sinks, restart handling, ask TTL.
//!
//! `from` is NEVER read from a request body: callers (API auth layer, UI, CLI header
//! mapping) derive `Origin` from auth context and pass it in (spec §3.2).

pub(crate) mod sinks;
pub(crate) mod ttl;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::Duration;

use tokio::sync::{broadcast, watch};

use crate::bus::EventBus;
use crate::pty::{ClearGate, IdleDecision, InjectError, InjectionQueue};
use crate::store::{Store, StoreError};
use crate::time::Timestamp;
use crate::types::agent::AgentState;
use crate::types::config::{AgentConfig, Edge, EdgeKind, FrozenWorkflow};
use crate::types::event::EventPayload;
use crate::types::id::{AgentId, FlowName, MessageId};
use crate::types::message::{MessageKind, MessageRecord, MessageStatus, Origin};

/// `GET /v1/messages` default page size.
pub const DEFAULT_LIST_LIMIT: u32 = 100;
/// `GET /v1/messages` maximum page size.
pub const MAX_LIST_LIMIT: u32 = 1000;

/// Maps 1:1 to the `GET /v1/messages` query string (contracts §4).
#[derive(Debug, Clone, PartialEq)]
pub struct MessageFilter {
    pub to: Option<AgentId>,
    pub from: Option<Origin>,
    pub status: Option<MessageStatus>,
    pub kind: Option<MessageKind>,
    /// Matches `created_at >= since` (RFC 3339 UTC strings order lexicographically).
    pub since: Option<Timestamp>,
    /// 0 is treated as the default (100); values above 1000 are clamped.
    pub limit: u32,
}

impl Default for MessageFilter {
    fn default() -> MessageFilter {
        MessageFilter {
            to: None,
            from: None,
            status: None,
            kind: None,
            since: None,
            limit: DEFAULT_LIST_LIMIT,
        }
    }
}

/// A flow's kickoff message (amendment 31). The flow name is not part of the
/// [`MessageRecord`]: it exists to be rendered into the injected header, which
/// is where the target agent reads which output contract applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowKickoff {
    pub flow: FlowName,
    pub from: Origin,
    pub to: AgentId,
    pub kind: MessageKind,
    pub body: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    #[error("no agent named '{}'", agent_name(.0))]
    UnknownAgent(AgentId),
    #[error("no message with id '{}'", message_name(.0))]
    UnknownMessage(MessageId),
    #[error("message '{}' already has a different reply", message_name(.0))]
    AlreadyReplied(MessageId),
    #[error("message '{}' is a send; only asks take replies", message_name(.0))]
    NotAnAsk(MessageId),
    #[error("only the addressee of message '{}' may reply to it", message_name(.0))]
    WrongReplier(MessageId),
    #[error("invalid reply code {0}; valid codes: 0, 1")]
    InvalidCode(u8),
    /// The reply body does not match the workflow's output schema and the
    /// repair budget is not spent (design 2026-08-06). `rendered` is written
    /// for the agent to read as CLI stderr.
    #[error("{rendered}")]
    OutputSchema { rendered: String },
    #[error(
        "agent '{owner}' has no loop edge to '{target}' (its edges: {edges}); \
         `tempo done` only ends a loop edge from your workflow config"
    )]
    NoLoopEdge {
        owner: AgentId,
        target: AgentId,
        edges: String,
    },
    #[error("store error: {0}")]
    Store(#[from] StoreError),
}

fn agent_name(id: &AgentId) -> &str {
    &id.0
}

fn message_name(id: &MessageId) -> &str {
    &id.0
}

/// Why a message is being failed; written onto the record (spec 2026-08-17
/// §4.3) so callers and the trigger watcher read the cause instead of guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailReason {
    pub code: &'static str,
    pub reason: String,
}

impl FailReason {
    #[must_use]
    pub fn timeout(id: &MessageId, ttl: Duration) -> FailReason {
        FailReason {
            code: "timeout",
            reason: format!(
                "ask '{}' got no reply within ask_timeout ({} s); the target never \
                 replied — check its pane, then fire again or raise \
                 [workflow] ask_timeout_minutes",
                id.0,
                ttl.as_secs()
            ),
        }
    }

    #[must_use]
    pub fn restarted(agent: &AgentId) -> FailReason {
        FailReason {
            code: "agent_restarted",
            reason: format!(
                "agent '{}' was restarted before it completed this message; fire again",
                agent.0
            ),
        }
    }

    #[must_use]
    pub fn exited(agent: &AgentId) -> FailReason {
        FailReason {
            code: "agent_exited",
            reason: format!(
                "agent '{}' exited before it completed this message; check its pane, \
                 restart it, then fire again",
                agent.0
            ),
        }
    }

    #[must_use]
    pub fn blocked(agent: &AgentId, tool: Option<&str>, grace: Duration) -> FailReason {
        FailReason {
            code: "blocked_on_permission",
            reason: format!(
                "agent '{}' has been waiting on a Claude Code permission dialog for {} \
                 for {} s and cannot reply; add `tools = [...]`/`allow = [...]` for it \
                 in tempo.toml (or answer the dialog in the pane) and fire again",
                agent.0,
                tool.unwrap_or("an undeclared tool"),
                grace.as_secs()
            ),
        }
    }

    #[must_use]
    pub fn from_inject(err: &InjectError) -> FailReason {
        match err {
            InjectError::AgentRestarted(agent) => FailReason::restarted(agent),
            InjectError::AgentExited(agent) => FailReason::exited(agent),
            InjectError::Blocked {
                agent,
                tool,
                waited,
            } => FailReason::blocked(agent, tool.as_deref(), *waited),
            InjectError::UnknownAgent(agent) => FailReason {
                code: "agent_exited",
                reason: format!("agent '{}' is not in this run's roster", agent.0),
            },
        }
    }
}

/// Debounced agent-state signal used to drive `injected → working` and send completion
/// (`working → idle` ⇒ `done`). Contract ADDITION: implemented in `Run::start` by an
/// adapter over `PtyManager::subscribe_state_debounced`, wired via
/// [`Router::set_state_source`] next to `PtyManager::set_clear_gate`.
pub trait StateSource: Send + Sync + 'static {
    /// `None` for unknown agents.
    fn subscribe_debounced(&self, agent: &AgentId) -> Option<watch::Receiver<AgentState>>;
    /// The dialog the agent is parked on, if any (spec 2026-08-17 §4.2).
    fn blocked_since(&self, _agent: &AgentId) -> Option<crate::pty::Blocked> {
        None
    }
}

/// One open obligation turn (spec §2). Keyed off the agent in `turns`.
#[derive(Default)]
struct TurnState {
    /// (target, kind) pairs the agent has emitted since the turn opened.
    met: HashSet<(AgentId, MessageKind)>,
    /// Loop targets whose round cap was reached this turn (edge-semantics spec):
    /// the step reads met, and the one nudge left names `tempo done`.
    capped: HashSet<AgentId>,
    nudged: bool,
    stalled: bool,
}

/// Owed-ask watchdog backoff between re-nudges (spec 2026-08-17 §4.1):
/// 60/120/240/240 s, the last entry repeating for every round past the fourth.
pub const DEFAULT_REPLY_NUDGE_BACKOFF: [Duration; 4] = [
    Duration::from_mins(1),
    Duration::from_mins(2),
    Duration::from_mins(4),
    Duration::from_mins(4),
];
/// How long an agent may sit on a permission dialog before its owed asks are
/// failed `blocked_on_permission` (spec 2026-08-17 §4.2). Shared with the
/// queue worker's parked-injection bound (#63).
pub const DEFAULT_BLOCKED_GRACE: Duration = crate::pty::BLOCKED_GRACE;

/// Owed-ask watchdog timing (spec 2026-08-17 §4). Constants in production;
/// tests shrink them through [`Router::set_watchdog_timing`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchdogTiming {
    pub reply_nudge_backoff: [Duration; 4],
    pub blocked_grace: Duration,
}

impl Default for WatchdogTiming {
    fn default() -> WatchdogTiming {
        WatchdogTiming {
            reply_nudge_backoff: DEFAULT_REPLY_NUDGE_BACKOFF,
            blocked_grace: DEFAULT_BLOCKED_GRACE,
        }
    }
}

impl WatchdogTiming {
    /// Wait owed after `nudges` nudges have been sent; the table's last entry
    /// is the steady-state cadence.
    fn backoff_after(&self, nudges: u32) -> Duration {
        let idx = usize::try_from(nudges.max(1)).unwrap_or(1).min(4) - 1;
        self.reply_nudge_backoff[idx]
    }
}

/// Re-nudge bookkeeping for an agent idling with an unanswered incoming ask
/// (spec 2026-08-17 §4.1). Lives beside `owed` and is dropped with it.
#[derive(Debug, Clone, Copy)]
struct ReplyNudgeState {
    nudges: u32,
    last_nudge_at: tokio::time::Instant,
    /// True once `agent.stalled` has been published for the current round.
    stalled: bool,
}

pub struct Router {
    store: Store,
    bus: EventBus,
    injector: Arc<dyn InjectionQueue>,
    workflow: Arc<FrozenWorkflow>,
    state_source: OnceLock<Arc<dyn StateSource>>,
    self_ref: OnceLock<Weak<Router>>,
    /// Asks SENT BY each agent, not yet terminal (spec §3.2 / §4.3 auto-clear gate).
    counts: Mutex<HashMap<AgentId, u64>>,
    /// Ask ids whose asker restarted since asking: replies are logged + evented only.
    suppressed: Mutex<HashSet<MessageId>>,
    /// Ask id → TTL deadline (spec §3.2, default 30 min via `FrozenWorkflow::ask_timeout`).
    deadlines: Mutex<HashMap<MessageId, tokio::time::Instant>>,
    /// Open obligation turns, one per armed agent (spec §2).
    turns: Mutex<HashMap<AgentId, TurnState>>,
    /// Asks addressed TO each agent, not yet terminal: owed replies. An agent
    /// idle with one gets nudged, never cleared (design 2026-08-06 companion
    /// fix — pre-existing hole, made likelier by schema rejection).
    owed: Mutex<HashMap<AgentId, HashSet<MessageId>>>,
    /// Re-nudge bookkeeping per owed-reply idle; cleared with `owed`.
    owed_nudges: Mutex<HashMap<AgentId, ReplyNudgeState>>,
    /// Owed-ask watchdog timing; the defaults except where a test shrinks them.
    timing: Mutex<WatchdogTiming>,
    /// Loops ended by `tempo done`, keyed (owner, target); cleared when a new
    /// arming turn opens for the owner (edge-semantics spec).
    loops_done: Mutex<HashSet<(AgentId, AgentId)>>,
    /// Rounds asked per loop, keyed (owner, target); in-memory by design —
    /// restart resets it, matching restart's disarm semantics.
    loop_rounds: Mutex<HashMap<(AgentId, AgentId), u32>>,
    /// Output-schema rejections per kickoff ask (design 2026-08-06); dropped in
    /// `settle`. In-memory by design: restart clears it with everything else.
    repairs: Mutex<HashMap<MessageId, u32>>,
    /// Output contracts by kickoff origin (multi-flow spec §5): the in-turn
    /// repair gate reads the kickoff's own flow contract, never a workflow-wide
    /// one. Bound before `create_kickoff` so no reply can beat the
    /// registration; dropped in `settle`.
    contracts: Mutex<HashMap<String, Arc<crate::schema::OutputContract>>>,
    /// Serializes status transitions (read-modify-write against the store).
    transition: tokio::sync::Mutex<()>,
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// True when `from` is an agent the receiver delegates to via any edge.
fn is_downstream_of(from: &Origin, receiver: &AgentConfig) -> bool {
    let Origin::Agent(sender) = from else {
        return false;
    };
    receiver.edges.iter().any(|e| e.to == *sender)
}

impl Router {
    /// Must be called within a tokio runtime (spawns background drivers).
    #[must_use]
    pub fn new(
        store: Store,
        bus: EventBus,
        injector: Arc<dyn InjectionQueue>,
        workflow: Arc<FrozenWorkflow>,
    ) -> Arc<Router> {
        let router = Arc::new(Router {
            store,
            bus,
            injector,
            workflow,
            state_source: OnceLock::new(),
            self_ref: OnceLock::new(),
            counts: Mutex::new(HashMap::new()),
            suppressed: Mutex::new(HashSet::new()),
            deadlines: Mutex::new(HashMap::new()),
            turns: Mutex::new(HashMap::new()),
            owed: Mutex::new(HashMap::new()),
            owed_nudges: Mutex::new(HashMap::new()),
            timing: Mutex::new(WatchdogTiming::default()),
            loops_done: Mutex::new(HashSet::new()),
            loop_rounds: Mutex::new(HashMap::new()),
            repairs: Mutex::new(HashMap::new()),
            contracts: Mutex::new(HashMap::new()),
            transition: tokio::sync::Mutex::new(()),
        });
        let _ = router.self_ref.set(Arc::downgrade(&router));
        ttl::spawn_sweeper(&router);
        router
    }

    /// Wiring break, called once in `Run::start`. Later calls are ignored.
    pub fn set_state_source(&self, source: Arc<dyn StateSource>) {
        let _ = self.state_source.set(source);
    }

    /// Test knob: shrink the backoff and grace so timing tests run in
    /// milliseconds. Production keeps the defaults.
    pub fn set_watchdog_timing(&self, timing: WatchdogTiming) {
        *lock(&self.timing) = timing;
    }

    /// Validates target, assigns `MessageId`, persists (`queued`), emits `message.created`,
    /// enqueues injection, and drives `queued → injected → working` (and `done` for sends)
    /// in a background task. `from` comes from auth context only.
    ///
    /// # Errors
    ///
    /// [`RouterError::UnknownAgent`] if `to` is not in the frozen roster;
    /// [`RouterError::Store`] if the record cannot be persisted.
    pub async fn create_message(
        &self,
        from: Origin,
        to: AgentId,
        kind: MessageKind,
        body: String,
    ) -> Result<MessageRecord, RouterError> {
        self.create(from, to, kind, body, None).await
    }

    /// A flow's kickoff message (amendment 31): [`Router::create_message`] with
    /// the flow name rendered into the injected header, so a target holding more
    /// than one flow's output contract can tell which schema its reply owes.
    ///
    /// The label is a prompt-format detail only — it is not persisted on the
    /// [`MessageRecord`], whose shape stays as contracts §2.2 fixes it.
    ///
    /// # Errors
    ///
    /// As [`Router::create_message`].
    pub async fn create_kickoff(&self, kickoff: FlowKickoff) -> Result<MessageRecord, RouterError> {
        let FlowKickoff {
            flow,
            from,
            to,
            kind,
            body,
        } = kickoff;
        self.create(from, to, kind, body, Some(flow)).await
    }

    async fn create(
        &self,
        from: Origin,
        to: AgentId,
        kind: MessageKind,
        body: String,
        flow: Option<FlowName>,
    ) -> Result<MessageRecord, RouterError> {
        if !self.workflow.agents.contains_key(&to) {
            return Err(RouterError::UnknownAgent(to));
        }
        // Met-recording: an agent-origin message is the SENDER completing a step.
        if let Origin::Agent(sender) = &from
            && let Some(turn) = lock(&self.turns).get_mut(sender)
        {
            turn.met.insert((to.clone(), kind));
        }
        // Loop round counting (edge-semantics spec): every round ask counts
        // toward the soft cap, whatever the turn state.
        if let Origin::Agent(sender) = &from
            && kind == MessageKind::Ask
            && self.loop_edge(sender, &to).is_some()
        {
            *lock(&self.loop_rounds)
                .entry((sender.clone(), to.clone()))
                .or_insert(0) += 1;
        }
        let record = MessageRecord {
            id: MessageId::generate(),
            kind,
            from: from.clone(),
            to: to.clone(),
            body: body.clone(),
            status: MessageStatus::Queued,
            code: None,
            reply: None,
            created_at: Timestamp::now(),
            injected_at: None,
            completed_at: None,
            reason: None,
            reason_code: None,
        };
        self.store.insert_message(&record).await?;
        if kind == MessageKind::Ask {
            if let Origin::Agent(asker) = &from {
                *lock(&self.counts).entry(asker.clone()).or_insert(0) += 1;
            }
            let deadline = tokio::time::Instant::now() + self.workflow.ask_timeout;
            lock(&self.deadlines).insert(record.id.clone(), deadline);
        }
        self.bus.publish(EventPayload::MessageCreated {
            message: record.clone(),
        });
        let text = match kind {
            MessageKind::Ask => sinks::render_ask(&record.id, &from, flow.as_ref(), &body),
            MessageKind::Send => sinks::render_send(&record.id, &from, flow.as_ref(), &body),
        };
        let rx = self.injector.enqueue(to.clone(), text);
        // Owed replies (design 2026-08-06 companion fix): the addressee owes a
        // reply whatever the origin — a UI or HTTP asker increments nobody's
        // outgoing count, so this is the only thing holding the idle gate off
        // `/clear` while they wait.
        //
        // Recorded after the enqueue for the same reason arming is: the gate
        // runs concurrently on the queue worker, and an obligation that is
        // visible before the agent has been handed the ask spends the one-shot
        // nudge budget on a message it never saw.
        if kind == MessageKind::Ask {
            lock(&self.owed)
                .entry(to.clone())
                .or_default()
                .insert(record.id.clone());
        }
        // Obligation turns (spec §2). Arming: an incoming ask/send opens (or
        // merges into) the TARGET's turn when it has edges; merge resets the
        // nudge budget but keeps met steps. Replies bypass create_message
        // entirely, so they can never arm — the loop-prevention rule.
        //
        // This must follow the enqueue above. A turn opens when the agent
        // RECEIVES the message, and the gate runs concurrently on the queue
        // worker: arming earlier (across the `insert_message` await) exposes a
        // window where an open turn coexists with an empty queue, and the
        // worker nudges for a message the agent has not been given.
        //
        // Downstream-feedback exemption (edge-semantics spec, 2026-08-05): a
        // message FROM an agent the target has an edge TO is feedback on
        // delegated work, not a new workflow instance — it never arms. Chains
        // still propagate: an upstream sender is nobody's delegate here.
        if let Some(cfg) = self.workflow.agents.get(&to)
            && !cfg.edges.is_empty()
            && !is_downstream_of(&from, cfg)
        {
            {
                let mut turns = lock(&self.turns);
                let turn = turns.entry(to.clone()).or_default();
                turn.nudged = false;
                turn.stalled = false;
                turn.capped.clear();
            }
            // A fresh kickoff restarts completed loops and their round counts.
            lock(&self.loops_done).retain(|(owner, _)| owner != &to);
            lock(&self.loop_rounds).retain(|(owner, _), _| owner != &to);
        }
        if let Some(this) = self.strong() {
            tokio::spawn(drive_message(this, record.id.clone(), to, kind, rx));
        }
        Ok(record)
    }

    /// # Errors
    ///
    /// [`RouterError::UnknownMessage`] if no row exists; [`RouterError::Store`] if the
    /// lookup fails.
    pub async fn get_message(&self, id: &MessageId) -> Result<MessageRecord, RouterError> {
        self.store
            .get_message(id)
            .await?
            .ok_or_else(|| RouterError::UnknownMessage(id.clone()))
    }

    /// # Errors
    ///
    /// [`RouterError::Store`] if the query fails.
    pub async fn list_messages(
        &self,
        filter: MessageFilter,
    ) -> Result<Vec<MessageRecord>, RouterError> {
        Ok(self.store.list_messages(&filter).await?)
    }

    /// Long-poll (spec §6.1): subscribe to the bus filtered by id, race a timeout, then
    /// read the record from `SQLite`. Always returns the current record — callers branch on
    /// `status`, never on how this returned.
    ///
    /// # Errors
    ///
    /// [`RouterError::UnknownMessage`] if no row exists; [`RouterError::Store`] if the
    /// lookup fails.
    pub async fn wait_terminal(
        &self,
        id: &MessageId,
        timeout: Duration,
    ) -> Result<MessageRecord, RouterError> {
        let mut events = self.bus.subscribe();
        let current = self.get_message(id).await?;
        if current.status.is_terminal() {
            return Ok(current);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            match tokio::time::timeout(deadline - now, events.recv()).await {
                Err(_elapsed) => break,
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                    let rec = self.get_message(id).await?;
                    if rec.status.is_terminal() {
                        return Ok(rec);
                    }
                }
                Ok(Ok(event)) => {
                    if let EventPayload::MessageStatusChanged { message } = &event.payload
                        && message.id == *id
                        && message.status.is_terminal()
                    {
                        return Ok(message.clone());
                    }
                }
            }
        }
        self.get_message(id).await
    }

    fn strong(&self) -> Option<Arc<Router>> {
        self.self_ref.get().and_then(Weak::upgrade)
    }

    /// Serialized status transition: loads the record, applies `mutate` only if the current
    /// status is in `allowed_from`, persists, and publishes `message.status`. Returns the
    /// updated record, or `None` if the row is missing or not in an allowed state.
    async fn transition(
        &self,
        id: &MessageId,
        allowed_from: &[MessageStatus],
        mutate: impl FnOnce(&mut MessageRecord),
    ) -> Result<Option<MessageRecord>, RouterError> {
        let _guard = self.transition.lock().await;
        let Some(mut rec) = self.store.get_message(id).await? else {
            return Ok(None);
        };
        if !allowed_from.contains(&rec.status) {
            return Ok(None);
        }
        mutate(&mut rec);
        self.store.update_message(&rec).await?;
        self.bus.publish(EventPayload::MessageStatusChanged {
            message: rec.clone(),
        });
        Ok(Some(rec))
    }

    /// Idempotency (spec §6.2): first reply fires the sink exactly once; identical replay
    /// (same `code` + `body`) is a no-op `Ok` (Bash-retry safe); conflicting replay →
    /// `AlreadyReplied`; reply to a send → `NotAnAsk`; replier ≠ addressee → `WrongReplier`.
    ///
    /// # Errors
    ///
    /// [`RouterError::InvalidCode`] if `code` is not 0 or 1;
    /// [`RouterError::UnknownMessage`] if no row exists;
    /// [`RouterError::NotAnAsk`] if the message is a send;
    /// [`RouterError::WrongReplier`] if `replier` is not the addressee;
    /// [`RouterError::AlreadyReplied`] on a conflicting replay;
    /// [`RouterError::OutputSchema`] if the body misses the workflow's output
    /// schema and the repair budget is not spent;
    /// [`RouterError::Store`] if the record cannot be persisted.
    pub async fn reply(
        &self,
        replier: Origin,
        id: &MessageId,
        code: u8,
        body: String,
    ) -> Result<MessageRecord, RouterError> {
        if code > 1 {
            return Err(RouterError::InvalidCode(code));
        }
        let guard = self.transition.lock().await;
        let Some(rec) = self.store.get_message(id).await? else {
            return Err(RouterError::UnknownMessage(id.clone()));
        };
        if rec.kind == MessageKind::Send {
            return Err(RouterError::NotAnAsk(id.clone()));
        }
        let Origin::Agent(replier_id) = &replier else {
            return Err(RouterError::WrongReplier(id.clone()));
        };
        if *replier_id != rec.to {
            return Err(RouterError::WrongReplier(id.clone()));
        }
        if rec.status.is_terminal() {
            let identical = rec.status == MessageStatus::Replied
                && rec.code == Some(code)
                && rec.reply.as_deref() == Some(body.as_str());
            if identical {
                return Ok(rec);
            }
            return Err(RouterError::AlreadyReplied(id.clone()));
        }
        if let Some(rejection) = self.reject_off_schema(&rec, code, &body) {
            return Err(rejection);
        }
        let mut updated = rec;
        updated.status = MessageStatus::Replied;
        updated.code = Some(code);
        updated.reply = Some(body.clone());
        updated.completed_at = Some(Timestamp::now());
        self.store.update_message(&updated).await?;
        self.settle(&updated);
        self.bus.publish(EventPayload::MessageStatusChanged {
            message: updated.clone(),
        });
        drop(guard);
        self.fire_reply_sink(&updated, replier_id, code, &body);
        Ok(updated)
    }

    /// Registers `contract` for the kickoff about to be created under
    /// `Origin::Trigger(origin)` (multi-flow spec §5). Call before
    /// `create_kickoff`: a scripted replier can answer off a bus event, so
    /// binding afterwards races the reply. If creation fails, call
    /// [`Router::unbind_kickoff_contract`] — no settle will.
    pub fn bind_kickoff_contract(
        &self,
        origin: &str,
        contract: Arc<crate::schema::OutputContract>,
    ) {
        lock(&self.contracts).insert(origin.to_string(), contract);
    }

    /// Drops a binding whose kickoff was never created.
    pub fn unbind_kickoff_contract(&self, origin: &str) {
        lock(&self.contracts).remove(origin);
    }

    /// Output-schema gate (design 2026-08-06). A code-0 reply to a kickoff ask
    /// whose origin bound a contract must match that contract's schema — the
    /// kickoff's own flow's, never another flow's, so a kickoff that bound none
    /// (every `on_start` kickoff) is ungated. Returns the rejection to send back
    /// while the repair budget lasts; `None` when no contract applies, the body
    /// conforms, or the budget is spent (the trigger fails it at the boundary
    /// instead of leaving the caller waiting on an agent that cannot comply).
    ///
    /// Runs under the `transition` guard: synchronous throughout, no `.await`.
    fn reject_off_schema(&self, rec: &MessageRecord, code: u8, body: &str) -> Option<RouterError> {
        let Origin::Trigger(origin) = &rec.from else {
            return None;
        };
        let contract = lock(&self.contracts).get(origin).cloned()?;
        if code != 0 || rec.kind != MessageKind::Ask {
            return None;
        }
        let Err(errors) = contract.check(body) else {
            return None;
        };
        let rejections = {
            let mut repairs = lock(&self.repairs);
            let n = repairs.entry(rec.id.clone()).or_insert(0);
            *n += 1;
            *n
        };
        if rejections > contract.max_repairs {
            tracing::warn!(
                message = %rec.id.0,
                rejections,
                "output schema budget exhausted; accepting the reply as-is"
            );
            return None;
        }
        let rendered = crate::schema::render_rejection(
            &errors,
            contract.max_repairs - rejections,
            &contract.flow,
        );
        self.bus.publish(EventPayload::ReplyRejected {
            message: rec.id.clone(),
            agent: rec.to.clone(),
            errors: rendered.clone(),
        });
        Some(RouterError::OutputSchema { rendered })
    }

    /// Reply sinks by origin (spec §3.3). `SQLite` + `message.status` already happened — the
    /// log is the floor. `agent:*` → inject into the asker's PTY (unless suppressed by
    /// restart); `user` → the bus event IS the sink; `http:*` → the bus event wakes that
    /// caller's `?wait` long-poll (`wait_terminal`).
    fn fire_reply_sink(&self, rec: &MessageRecord, replier: &AgentId, code: u8, body: &str) {
        let Origin::Agent(asker) = &rec.from else {
            return;
        };
        if lock(&self.suppressed).remove(&rec.id) {
            tracing::info!(
                message = %rec.id.0,
                asker = %asker.0,
                "asker restarted since ask; reply logged + evented, not injected"
            );
            return;
        }
        let text = sinks::render_reply(&rec.id, replier, code, body);
        let rx = self.injector.enqueue(asker.clone(), text);
        self.rearm_loop_after_reply(asker, replier);
        let message_id = rec.id.0.clone();
        tokio::spawn(async move {
            match rx.await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    tracing::warn!(message = %message_id, error = %e,
                                   "reply injection failed");
                }
                Err(_) => {
                    tracing::warn!(message = %message_id, "reply injection dropped");
                }
            }
        });
    }

    /// Terminal bookkeeping: drop TTL deadline, repair count, bound output
    /// contract and suppression entry, and
    /// decrement the asker's pending count for agent-origin asks. A `replied` record keeps
    /// its suppression entry
    /// until [`Router::fire_reply_sink`] consumes it — settling runs first (the auto-clear
    /// gate must see the decremented count when `message.status` publishes).
    fn settle(&self, rec: &MessageRecord) {
        lock(&self.deadlines).remove(&rec.id);
        lock(&self.repairs).remove(&rec.id);
        if let Origin::Trigger(origin) = &rec.from {
            lock(&self.contracts).remove(origin);
        }
        if rec.status != MessageStatus::Replied {
            lock(&self.suppressed).remove(&rec.id);
        }
        if rec.kind == MessageKind::Ask
            && let Origin::Agent(asker) = &rec.from
            && let Some(n) = lock(&self.counts).get_mut(asker)
        {
            *n = n.saturating_sub(1);
        }
        if rec.kind == MessageKind::Ask {
            let mut owed = lock(&self.owed);
            if let Some(set) = owed.get_mut(&rec.to) {
                set.remove(&rec.id);
                if set.is_empty() {
                    owed.remove(&rec.to);
                    lock(&self.owed_nudges).remove(&rec.to);
                }
            }
        }
    }

    /// Restart semantics (spec §4.3): in-flight and queued messages TO the restarted agent
    /// become `failed`; pending asks FROM it are marked so a later reply is logged +
    /// evented but never injected into the fresh session (spec §3.3). Idempotent; races
    /// with `InjectError::AgentRestarted` in the drive task are harmless because
    /// `fail_message` no-ops on terminal rows.
    pub async fn on_agent_restarted(&self, agent: &AgentId) {
        // Disarm (spec §2): a restarted session has no memory of the arming
        // message — nor of an owed reply it was already nudged about. The
        // sweep below drops `owed` itself by failing the asks.
        lock(&self.turns).remove(agent);
        lock(&self.owed_nudges).remove(agent);
        match self.store.pending_asks().await {
            Ok(asks) => {
                let mut suppressed = lock(&self.suppressed);
                for ask in &asks {
                    if ask.from == Origin::Agent(agent.clone()) {
                        suppressed.insert(ask.id.clone());
                    }
                }
            }
            Err(e) => {
                tracing::error!(agent = %agent.0, error = %e,
                                "restart sweep: pending_asks query failed");
            }
        }
        match self.store.pending_to_agent(agent).await {
            Ok(records) => {
                for rec in records {
                    self.fail_message(&rec.id, FailReason::restarted(agent))
                        .await;
                }
            }
            Err(e) => {
                tracing::error!(agent = %agent.0, error = %e,
                                "restart sweep: pending_to_agent query failed");
            }
        }
    }

    /// Idempotent: no-op if the message is already terminal (or missing).
    pub(crate) async fn fail_message(&self, id: &MessageId, reason: FailReason) {
        let result = self
            .transition(
                id,
                &[
                    MessageStatus::Queued,
                    MessageStatus::Injected,
                    MessageStatus::Working,
                ],
                |rec| {
                    rec.status = MessageStatus::Failed;
                    rec.completed_at = Some(Timestamp::now());
                    rec.reason = Some(reason.reason.clone());
                    rec.reason_code = Some(reason.code.to_string());
                },
            )
            .await;
        match result {
            Ok(Some(rec)) => self.settle(&rec),
            Ok(None) => {}
            Err(e) => {
                tracing::error!(message = %id.0, error = %e, "failed to mark message failed");
            }
        }
    }
}

impl Router {
    /// Asks SENT BY `agent` that are not yet terminal (roster/detail endpoints
    /// and the idle gate both read this).
    /// The owner's loop edge to `target`, if one exists.
    fn loop_edge(&self, owner: &AgentId, target: &AgentId) -> Option<Edge> {
        self.workflow
            .agents
            .get(owner)?
            .edges
            .iter()
            .find(|e| e.kind == EdgeKind::Loop && e.to == *target)
            .cloned()
    }

    /// Ends the loop `owner` runs with `target` (edge-semantics spec): the loop
    /// step reads met from here on and replies from `target` stop re-arming,
    /// until a fresh arming turn restarts the loop.
    ///
    /// # Errors
    ///
    /// [`RouterError::NoLoopEdge`] when `owner` has no loop edge to `target` —
    /// the message lists the owner's actual edges.
    pub fn mark_loop_done(&self, owner: &AgentId, target: &AgentId) -> Result<(), RouterError> {
        if self.loop_edge(owner, target).is_none() {
            let edges = self.workflow.agents.get(owner).map_or_else(
                || "(unknown agent)".to_string(),
                |cfg| {
                    if cfg.edges.is_empty() {
                        "(none)".to_string()
                    } else {
                        cfg.edges
                            .iter()
                            .map(|e| format!("{} {}", e.kind.as_str(), e.to.0))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                },
            );
            return Err(RouterError::NoLoopEdge {
                owner: owner.clone(),
                target: target.clone(),
                edges,
            });
        }
        lock(&self.loops_done).insert((owner.clone(), target.clone()));
        if let Some(turn) = lock(&self.turns).get_mut(owner) {
            turn.capped.remove(target);
        }
        Ok(())
    }

    /// Loop re-arm (edge-semantics spec): a reply from the loop target is the
    /// round coming back — the owner must fire the next round or end the loop.
    /// This is the one scoped exception to "replies never open a turn". At the
    /// round cap the loop stops re-arming and the owner gets one cap nudge.
    fn rearm_loop_after_reply(&self, owner: &AgentId, replier: &AgentId) {
        let Some(edge) = self.loop_edge(owner, replier) else {
            return;
        };
        if lock(&self.loops_done).contains(&(owner.clone(), replier.clone())) {
            return;
        }
        let rounds = lock(&self.loop_rounds)
            .get(&(owner.clone(), replier.clone()))
            .copied()
            .unwrap_or(0);
        let mut turns = lock(&self.turns);
        let turn = turns.entry(owner.clone()).or_default();
        if rounds >= edge.effective_max_rounds() {
            tracing::warn!(
                owner = %owner.0, target = %replier.0, cap = edge.effective_max_rounds(),
                "loop round cap reached; not re-arming — owner should run tempo done"
            );
            turn.capped.insert(replier.clone());
        } else {
            // Demand a fresh round: the previous ask no longer meets the step.
            turn.met.remove(&(replier.clone(), MessageKind::Ask));
        }
        turn.nudged = false;
        turn.stalled = false;
    }

    /// Owed-reply gate (design 2026-08-06 companion fix, spec 2026-08-17 §4).
    /// An agent that idles without answering an ask addressed to it is nudged,
    /// then re-nudged on a backoff for as long as the reply is owed — `/clear`
    /// would destroy the very context the asker is blocked on, and after a
    /// schema rejection it would destroy the work being repaired. `None` means
    /// nothing is owed and the caller carries on to turn logic.
    fn owed_reply_decision(&self, agent: &AgentId) -> Option<IdleDecision> {
        let mut owed: Vec<MessageId> = lock(&self.owed)
            .get(agent)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default();
        if owed.is_empty() {
            return None;
        }
        owed.sort_by(|a, b| a.0.cmp(&b.0)); // the set's order is not stable
        if self.blocked_since(agent).is_some() {
            // Never type into a dialog: a leading digit would answer it. The
            // sweeper fails the ask once the blocked grace is spent.
            return Some(IdleDecision::HoldQuiet);
        }
        let (round, publish_stalled) = self.advance_nudge_round(agent);
        match round {
            None => {
                if publish_stalled {
                    self.bus.publish(EventPayload::AgentStalled {
                        agent: agent.clone(),
                    });
                }
                Some(IdleDecision::HoldQuiet)
            }
            Some(round) => {
                self.bus.publish(EventPayload::AgentNudged {
                    agent: agent.clone(),
                });
                Some(IdleDecision::Nudge(sinks::render_reply_nudge(&owed, round)))
            }
        }
    }

    /// The check-and-bump half of [`Router::owed_reply_decision`], under one
    /// `owed_nudges` guard so a poke and a state transition cannot both nudge
    /// the same round. Returns `Some(round)` to nudge or `None` to hold quiet,
    /// plus whether `agent.stalled` is still owed for the current round.
    fn advance_nudge_round(&self, agent: &AgentId) -> (Option<u32>, bool) {
        let now = tokio::time::Instant::now();
        let timing = *lock(&self.timing);
        let mut nudges = lock(&self.owed_nudges);
        match nudges.get_mut(agent) {
            None => {
                nudges.insert(
                    agent.clone(),
                    ReplyNudgeState {
                        nudges: 1,
                        last_nudge_at: now,
                        stalled: false,
                    },
                );
                (Some(1), false)
            }
            Some(state)
                if now.duration_since(state.last_nudge_at) < timing.backoff_after(state.nudges) =>
            {
                let publish = !state.stalled;
                state.stalled = true;
                (None, publish)
            }
            Some(state) => {
                state.nudges += 1;
                state.last_nudge_at = now;
                state.stalled = false;
                (Some(state.nudges), false)
            }
        }
    }

    /// The dialog this agent is parked on, if any (spec 2026-08-17 §4.2).
    fn blocked_since(&self, agent: &AgentId) -> Option<crate::pty::Blocked> {
        self.state_source
            .get()
            .and_then(|source| source.blocked_since(agent))
    }

    /// One owed-ask watchdog pass (spec 2026-08-17 §4), run by the sweeper
    /// right after expiry so a dead ask is never poked for. Snapshots under
    /// the locks and acts after dropping them: fail-fast for agents parked on
    /// a dialog past the grace, then for agents that have exited, then a poke
    /// to the queue worker for every agent whose re-nudge is due. The worker,
    /// not this pass, decides whether anything is typed.
    pub(crate) async fn sweep_owed(&self) {
        let now = tokio::time::Instant::now();
        let timing = *lock(&self.timing);
        let owed: Vec<(AgentId, Vec<MessageId>)> = lock(&self.owed)
            .iter()
            .map(|(agent, ids)| (agent.clone(), ids.iter().cloned().collect()))
            .collect();
        for (agent, ids) in owed {
            if self.fail_owed_if_blocked(&agent, &ids, now, timing).await {
                continue;
            }
            if self.is_exited(&agent) {
                for id in &ids {
                    self.fail_message(id, FailReason::exited(&agent)).await;
                }
                continue;
            }
            if self.poke_is_due(&agent, &ids, now, timing) {
                self.injector.reconsider(&agent);
            }
        }
    }

    /// Fails every owed ask once `agent` has sat on a permission dialog past
    /// the grace (spec §4.2), returning true when it did. The agent itself is
    /// left exactly as it is: the dialog stays up and nothing is typed.
    async fn fail_owed_if_blocked(
        &self,
        agent: &AgentId,
        ids: &[MessageId],
        now: tokio::time::Instant,
        timing: WatchdogTiming,
    ) -> bool {
        let Some(blocked) = self.blocked_since(agent) else {
            return false;
        };
        if now.duration_since(blocked.since) < timing.blocked_grace {
            return false;
        }
        tracing::warn!(agent = %agent.0, tool = blocked.tool.as_deref().unwrap_or("?"),
                       "blocked past the grace; failing its owed asks");
        for id in ids {
            let reason = FailReason::blocked(agent, blocked.tool.as_deref(), timing.blocked_grace);
            self.fail_message(id, reason).await;
        }
        true
    }

    /// The agent's debounced state, `None` when there is no state source (unit
    /// tests) or the agent is unknown to it.
    fn debounced_state(&self, agent: &AgentId) -> Option<AgentState> {
        self.state_source
            .get()
            .and_then(|source| source.subscribe_debounced(agent))
            .map(|rx| *rx.borrow())
    }

    /// Debounced `Exited`. `drive_message` stops watching an ask once it
    /// reaches `working`, so an exit after that point is only seen here — the
    /// ask would otherwise wait out its whole TTL (spec §4.1).
    fn is_exited(&self, agent: &AgentId) -> bool {
        self.debounced_state(agent) == Some(AgentState::Exited)
    }

    /// True when the queue worker should re-run the gate for this owed agent.
    /// Either it has been nudged and the backoff since has elapsed (spec §4.1),
    /// or it has never been nudged — the case a `HoldQuiet` leaves behind when
    /// the idle transition happened while the agent was blocked, so round 1 was
    /// never performed and no state exists to back off from (spec §4.2
    /// amendment, live 2026-08-18). Both branches demand a debounced-idle
    /// agent: a poke sent while it is working is buffered behind the worker's
    /// in-flight injection and lands the moment that injection is delivered,
    /// before the agent's `UserPromptSubmit` hook has moved it off idle — a
    /// nudge for the message it is in the act of answering. The never-nudged
    /// branch runs the same backoff clock off the ask's own age; with
    /// `ask_timeout_minutes = 1` (the minimum) that age can never reach
    /// `backoff_after(0)` before expiry fails the ask, so the branch is dead
    /// there by design — expiry wins. A blocked agent is never poked, in the
    /// grace or past it: typing into a dialog is never right, and the fail-fast
    /// owns that case. Nor is one waiting on its own downstream ask — the gate
    /// would hold quiet every tick.
    fn poke_is_due(
        &self,
        agent: &AgentId,
        ids: &[MessageId],
        now: tokio::time::Instant,
        timing: WatchdogTiming,
    ) -> bool {
        if self.blocked_since(agent).is_some() || self.pending_asks(agent) > 0 {
            return false;
        }
        if self.debounced_state(agent) != Some(AgentState::Idle) {
            return false;
        }
        match lock(&self.owed_nudges).get(agent).copied() {
            Some(state) => {
                now.duration_since(state.last_nudge_at) >= timing.backoff_after(state.nudges)
            }
            None => self.oldest_owed_age(ids, now) >= timing.backoff_after(0),
        }
    }

    /// How long the oldest of `ids` has been owed, read off the TTL deadline
    /// each ask was given at creation (`created + ask_timeout`). Zero when none
    /// of them is on the deadline map — an ask already expired is not poked for.
    fn oldest_owed_age(&self, ids: &[MessageId], now: tokio::time::Instant) -> Duration {
        let deadlines = lock(&self.deadlines);
        ids.iter()
            .filter_map(|id| deadlines.get(id))
            .map(|deadline| {
                self.workflow
                    .ask_timeout
                    .saturating_sub(deadline.saturating_duration_since(now))
            })
            .max()
            .unwrap_or(Duration::ZERO)
    }

    #[must_use]
    pub fn pending_asks(&self, agent: &AgentId) -> u64 {
        lock(&self.counts).get(agent).copied().unwrap_or(0)
    }

    /// Sum of pending outgoing asks across `agents` only. A flow's quiescence
    /// is scoped to its member set (multi-flow spec §4): another flow's
    /// traffic on the same run must delay nothing here.
    #[must_use]
    pub fn total_pending_asks_among(&self, agents: &[AgentId]) -> u64 {
        let counts = lock(&self.counts);
        agents.iter().filter_map(|a| counts.get(a)).sum()
    }

    /// Count of `agents` with an open obligation turn (member-scoped twin of
    /// the quiescence input; multi-flow spec §4).
    #[must_use]
    pub fn open_turns_among(&self, agents: &[AgentId]) -> u64 {
        let turns = lock(&self.turns);
        agents.iter().filter(|a| turns.contains_key(*a)).count() as u64
    }
}

impl ClearGate for Router {
    fn on_stable_idle(&self, agent: &AgentId) -> IdleDecision {
        // An unanswered outgoing ask means the turn (or plain ask) is in
        // progress: never nudge or clear past it.
        if self.pending_asks(agent) > 0 {
            return IdleDecision::HoldQuiet;
        }
        // Before the turn logic, and before the no-edges shortcut below: an
        // agent with no edges at all can still owe somebody a reply.
        if let Some(decision) = self.owed_reply_decision(agent) {
            return decision;
        }
        // Parked on a permission dialog (#63): a nudge or `/clear` would be
        // typed into it. Hold, and leave the turn's nudge unspent — the queue
        // worker re-runs the gate when the flag clears at idle.
        if self.blocked_since(agent).is_some() {
            return IdleDecision::HoldQuiet;
        }
        let Some(edges) = self.workflow.agents.get(agent).map(|c| c.edges.clone()) else {
            return IdleDecision::AllowClear;
        };
        let mut turns = lock(&self.turns);
        let Some(turn) = turns.get_mut(agent) else {
            return IdleDecision::AllowClear;
        };
        let unmet: Vec<Edge> = {
            let loops_done = lock(&self.loops_done);
            edges
                .into_iter()
                .filter(|e| {
                    let emitted = turn.met.contains(&(e.to.clone(), e.kind.message_kind()));
                    match e.kind {
                        EdgeKind::Ask | EdgeKind::Send => !emitted,
                        // A completed or capped loop imposes nothing more.
                        EdgeKind::Loop => {
                            !emitted
                                && !loops_done.contains(&(agent.clone(), e.to.clone()))
                                && !turn.capped.contains(&e.to)
                        }
                    }
                })
                .collect()
        };
        if unmet.is_empty() {
            // Capped loops get one nudge naming tempo done before the turn closes.
            if !turn.capped.is_empty() && !turn.nudged {
                turn.nudged = true;
                let capped: Vec<(AgentId, u32)> = turn
                    .capped
                    .iter()
                    .filter_map(|to| {
                        self.loop_edge(agent, to)
                            .map(|e| (to.clone(), e.effective_max_rounds()))
                    })
                    .collect();
                drop(turns);
                self.bus.publish(EventPayload::AgentNudged {
                    agent: agent.clone(),
                });
                return IdleDecision::Nudge(sinks::render_cap_nudge(&capped));
            }
            turns.remove(agent); // turn closes; normal drain-then-clear resumes
            return IdleDecision::AllowClear;
        }
        if turn.nudged {
            let publish = !turn.stalled;
            turn.stalled = true;
            drop(turns);
            if publish {
                self.bus.publish(EventPayload::AgentStalled {
                    agent: agent.clone(),
                });
            }
            return IdleDecision::HoldQuiet;
        }
        turn.nudged = true;
        drop(turns);
        self.bus.publish(EventPayload::AgentNudged {
            agent: agent.clone(),
        });
        IdleDecision::Nudge(sinks::render_nudge(&unmet))
    }
}

async fn drive_message(
    router: Arc<Router>,
    id: MessageId,
    to: AgentId,
    kind: MessageKind,
    rx: tokio::sync::oneshot::Receiver<Result<crate::pty::Injected, crate::pty::InjectError>>,
) {
    let injected = match rx.await {
        Ok(Ok(injected)) => injected,
        Ok(Err(e)) => {
            tracing::info!(message = %id.0, error = %e, "injection failed; failing message");
            router.fail_message(&id, FailReason::from_inject(&e)).await;
            return;
        }
        Err(_) => {
            tracing::warn!(message = %id.0, "injection receiver dropped; failing message");
            // The queue worker dropped the ack without answering: it goes with
            // the agent's PTY, so the target cannot still be running.
            router.fail_message(&id, FailReason::exited(&to)).await;
            return;
        }
    };
    let marked = router
        .transition(&id, &[MessageStatus::Queued], |rec| {
            rec.status = MessageStatus::Injected;
            rec.injected_at = Some(injected.at.clone());
        })
        .await;
    match marked {
        Ok(Some(_)) => {}
        Ok(None) => return,
        Err(e) => {
            tracing::error!(message = %id.0, error = %e, "injected transition failed");
            return;
        }
    }
    let Some(source) = router.state_source.get() else {
        tracing::error!(message = %id.0, "no StateSource wired; cannot drive status");
        return;
    };
    let Some(mut states) = source.subscribe_debounced(&to) else {
        return;
    };
    loop {
        let state = *states.borrow_and_update();
        if state == AgentState::Working {
            let marked = router
                .transition(&id, &[MessageStatus::Injected], |rec| {
                    rec.status = MessageStatus::Working;
                })
                .await;
            if let Err(e) = marked {
                tracing::error!(message = %id.0, error = %e, "working transition failed");
                return;
            }
            break;
        }
        if state == AgentState::Exited {
            router.fail_message(&id, FailReason::exited(&to)).await;
            return;
        }
        if states.changed().await.is_err() {
            return;
        }
    }
    if kind == MessageKind::Send {
        loop {
            if states.changed().await.is_err() {
                return;
            }
            let state = *states.borrow_and_update();
            if state == AgentState::Idle {
                let done = router
                    .transition(
                        &id,
                        &[MessageStatus::Injected, MessageStatus::Working],
                        |r| {
                            r.status = MessageStatus::Done;
                            r.completed_at = Some(Timestamp::now());
                        },
                    )
                    .await;
                match done {
                    Ok(Some(rec)) => router.settle(&rec),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!(message = %id.0, error = %e, "done transition failed");
                    }
                }
                return;
            }
            if state == AgentState::Exited {
                router.fail_message(&id, FailReason::exited(&to)).await;
                return;
            }
        }
    }
}
