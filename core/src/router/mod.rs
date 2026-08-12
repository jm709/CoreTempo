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
use crate::pty::{ClearGate, IdleDecision, InjectionQueue};
use crate::store::{Store, StoreError};
use crate::time::Timestamp;
use crate::types::agent::AgentState;
use crate::types::config::{AgentConfig, Edge, EdgeKind, FrozenWorkflow};
use crate::types::event::EventPayload;
use crate::types::id::{AgentId, MessageId};
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

/// Debounced agent-state signal used to drive `injected → working` and send completion
/// (`working → idle` ⇒ `done`). Contract ADDITION: implemented in `Run::start` by an
/// adapter over `PtyManager::subscribe_state_debounced`, wired via
/// [`Router::set_state_source`] next to `PtyManager::set_clear_gate`.
pub trait StateSource: Send + Sync + 'static {
    /// `None` for unknown agents.
    fn subscribe_debounced(&self, agent: &AgentId) -> Option<watch::Receiver<AgentState>>;
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

/// One nudge-then-stall budget for an agent idling with an unanswered incoming
/// ask. Lives beside `owed` and is dropped with it.
#[derive(Default)]
struct ReplyNudgeState {
    nudged: bool,
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
    /// One nudge-then-stall budget per owed-reply idle; cleared with `owed`.
    owed_nudges: Mutex<HashMap<AgentId, ReplyNudgeState>>,
    /// Loops ended by `tempo done`, keyed (owner, target); cleared when a new
    /// arming turn opens for the owner (edge-semantics spec).
    loops_done: Mutex<HashSet<(AgentId, AgentId)>>,
    /// Rounds asked per loop, keyed (owner, target); in-memory by design —
    /// restart resets it, matching restart's disarm semantics.
    loop_rounds: Mutex<HashMap<(AgentId, AgentId), u32>>,
    /// Output-schema rejections per kickoff ask (design 2026-08-06); dropped in
    /// `settle`. In-memory by design: restart clears it with everything else.
    repairs: Mutex<HashMap<MessageId, u32>>,
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
            loops_done: Mutex::new(HashSet::new()),
            loop_rounds: Mutex::new(HashMap::new()),
            repairs: Mutex::new(HashMap::new()),
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
            MessageKind::Ask => sinks::render_ask(&record.id, &from, &body),
            MessageKind::Send => sinks::render_send(&record.id, &from, &body),
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

    /// Output-schema gate (design 2026-08-06). A code-0 reply to the kickoff ask
    /// — the HTTP-origin ask addressed to the contract's target — must match the
    /// workflow's output schema. Returns the rejection to send back while the
    /// repair budget lasts; `None` when the contract does not apply, the body
    /// conforms, or the budget is spent (the trigger fails it at the boundary
    /// instead of leaving the caller waiting on an agent that cannot comply).
    ///
    /// Runs under the `transition` guard: synchronous throughout, no `.await`.
    fn reject_off_schema(&self, rec: &MessageRecord, code: u8, body: &str) -> Option<RouterError> {
        let contract = self.workflow.output.as_ref()?;
        if code != 0
            || rec.kind != MessageKind::Ask
            || !matches!(rec.from, Origin::Http(_))
            || rec.to != contract.target
        {
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
        let rendered = crate::schema::render_rejection(&errors, contract.max_repairs - rejections);
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

    /// Terminal bookkeeping: drop TTL deadline, repair count and suppression entry, and
    /// decrement the asker's pending count for agent-origin asks. A `replied` record keeps
    /// its suppression entry
    /// until [`Router::fire_reply_sink`] consumes it — settling runs first (the auto-clear
    /// gate must see the decremented count when `message.status` publishes).
    fn settle(&self, rec: &MessageRecord) {
        lock(&self.deadlines).remove(&rec.id);
        lock(&self.repairs).remove(&rec.id);
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
                    self.fail_message(&rec.id).await;
                }
            }
            Err(e) => {
                tracing::error!(agent = %agent.0, error = %e,
                                "restart sweep: pending_to_agent query failed");
            }
        }
    }

    /// Idempotent: no-op if the message is already terminal (or missing).
    pub(crate) async fn fail_message(&self, id: &MessageId) {
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

    /// Owed-reply gate (design 2026-08-06 companion fix). An agent that idles
    /// without answering an ask addressed to it gets one nudge, then holds
    /// quiet — `/clear` would destroy the very context the asker is blocked on,
    /// and after a schema rejection it would destroy the work being repaired.
    /// `None` means nothing is owed and the caller carries on to turn logic.
    fn owed_reply_decision(&self, agent: &AgentId) -> Option<IdleDecision> {
        let mut owed: Vec<MessageId> = lock(&self.owed)
            .get(agent)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default();
        if owed.is_empty() {
            return None;
        }
        owed.sort_by(|a, b| a.0.cmp(&b.0)); // the set's order is not stable
        let mut nudges = lock(&self.owed_nudges);
        let state = nudges.entry(agent.clone()).or_default();
        if state.nudged {
            let publish = !state.stalled;
            state.stalled = true;
            drop(nudges);
            if publish {
                self.bus.publish(EventPayload::AgentStalled {
                    agent: agent.clone(),
                });
            }
            return Some(IdleDecision::HoldQuiet);
        }
        state.nudged = true;
        drop(nudges);
        self.bus.publish(EventPayload::AgentNudged {
            agent: agent.clone(),
        });
        Some(IdleDecision::Nudge(sinks::render_reply_nudge(&owed)))
    }

    #[must_use]
    pub fn pending_asks(&self, agent: &AgentId) -> u64 {
        lock(&self.counts).get(agent).copied().unwrap_or(0)
    }

    /// Count of agents with an open obligation turn (spec triggers §2 quiescence input).
    #[must_use]
    pub fn open_turns(&self) -> u64 {
        lock(&self.turns).len() as u64
    }

    /// Sum of pending outgoing asks across all agents.
    #[must_use]
    pub fn total_pending_asks(&self) -> u64 {
        lock(&self.counts).values().sum()
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
            router.fail_message(&id).await;
            return;
        }
        Err(_) => {
            tracing::warn!(message = %id.0, "injection receiver dropped; failing message");
            router.fail_message(&id).await;
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
            router.fail_message(&id).await;
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
                router.fail_message(&id).await;
                return;
            }
        }
    }
}
