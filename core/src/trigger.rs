//! Kickoff completion watching and the trigger hub (spec triggers §2).
//!
//! [`TriggerHub`] is the bookkeeping half: ids, a capped history of statuses,
//! and the id of the kickoff in flight. It is shared by the warm-run endpoint
//! and by serve mode, neither of which owns the other.
//!
//! A trigger fires one kickoff message and needs to know when the work it
//! started is over. An `ask` has an explicit end — its reply. A `send` does not,
//! so completion is inferred from quiescence: every agent debounced-idle, every
//! injection queue empty, no ask awaiting a reply, no obligation turn open.
//!
//! Quiescence is only meaningful once the kickoff has actually reached the
//! agent. At creation time the predicate is trivially true — the system was idle
//! a moment ago — so the watcher arms the quiescence phase only after observing
//! the kickoff at `working` (or `done`). Without that guard a swallowed kickoff
//! reports success.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Poll;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{broadcast, watch};
use tokio_stream::StreamExt;

use crate::api::PtySource;
use crate::bus::EventBus;
use crate::router::Router;
use crate::types::agent::AgentState;
use crate::types::event::{CompletionResult, Event, EventPayload, LifecyclePhase};
use crate::types::id::{AgentId, FlowName, MessageId};
use crate::types::message::{MessageKind, MessageRecord, MessageStatus};

/// Queue depth and the router's counters are polled, not watched, so the
/// quiescence loop re-checks on this interval even when no state moves.
const QUIESCENCE_POLL: Duration = Duration::from_millis(250);

/// Largest accepted trigger payload. A payload becomes typed keystrokes in a
/// PTY, so this is a ceiling on how much a caller can make an agent read.
pub const PAYLOAD_CAP_BYTES: usize = 64 * 1024;

/// Triggers waiting to be run before the server starts rejecting them.
pub const QUEUE_CAP: usize = 32;

/// Trigger records kept for status lookups; the oldest is dropped first.
pub const HISTORY_CAP: usize = 100;

/// Status updates buffered per long-poll subscriber. A waiter that falls this
/// far behind re-reads the record instead of missing its own result.
const UPDATE_BUFFER: usize = 64;

/// Margin the kickoff watcher's deadline sits under `ask_timeout`, so the
/// watcher's own clock — not the router's TTL sweeper — decides that a kickoff
/// timed out, and the result is labelled `timeout` rather than `failed`.
const DEADLINE_MARGIN: Duration = Duration::from_secs(2);

/// Deadline for a kickoff watcher, derived from the workflow's `ask_timeout`.
#[must_use]
pub fn watcher_deadline(ask_timeout: Duration) -> Duration {
    ask_timeout
        .saturating_sub(DEADLINE_MARGIN)
        .max(Duration::from_secs(1))
}

/// 8 lowercase hex chars, mirroring the `Origin::Trigger` ids minted elsewhere
/// (HTTP request ids, warm-trigger ids). An `on_start` kickoff has no HTTP
/// request of its own to borrow an id from, so `coretempod run` mints one
/// here; the desktop app instead registers its kickoff via
/// `TriggerHub::try_begin`.
#[must_use]
pub fn startup_id() -> String {
    use rand::RngExt;
    format!("{:08x}", rand::rng().random::<u32>())
}

/// Where one trigger is in its life. The wire form is flat and tagged by
/// `status`, so a caller reads one JSON object rather than a nested union.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum TriggerStatus {
    Queued {
        position: usize,
    },
    Running,
    Completed {
        result: CompletionResult,
        /// `Replied` only.
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<u8>,
        /// `Replied` only.
        #[serde(skip_serializing_if = "Option::is_none")]
        reply: Option<String>,
        /// The reply parsed against the webhook flow's `output` contract,
        /// alongside the raw `reply` it came from. Absent when the flow
        /// declares no contract.
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<Value>,
    },
    Failed {
        reason: String,
        /// Machine-readable failure kind (design 2026-08-06 §HTTP wire), so a
        /// caller can branch without matching on `reason` prose.
        reason_code: String,
    },
}

/// Wire form of an accepted trigger: the result arrives later, via
/// `GET /v1/trigger/{id}` or a `?wait` long-poll.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TriggerAccepted {
    pub trigger_id: String,
    /// 0 when the kickoff started immediately; queued triggers count the ones
    /// ahead of them, the running one included.
    pub position: usize,
}

/// Wire form of one trigger's status: the id plus the flat [`TriggerStatus`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriggerView {
    pub trigger_id: String,
    #[serde(flatten)]
    pub status: TriggerStatus,
}

/// True once a trigger's status will not change again.
#[must_use]
fn is_terminal(status: &TriggerStatus) -> bool {
    match status {
        TriggerStatus::Completed { .. } | TriggerStatus::Failed { .. } => true,
        TriggerStatus::Queued { .. } | TriggerStatus::Running => false,
    }
}

/// The status a finished kickoff is recorded under.
///
/// `completed` means the workflow ran to an end the caller can use; `failed`
/// carries the diagnosis for everything else, including a timeout. A caller
/// reading `status` alone therefore learns whether it got what it asked for.
#[must_use]
pub fn completion_status(completion: Completion) -> TriggerStatus {
    match completion {
        Completion::Replied {
            code,
            reply,
            output,
        } => TriggerStatus::Completed {
            result: CompletionResult::Replied,
            code: Some(code),
            reply: Some(reply),
            output,
        },
        Completion::Quiesced => TriggerStatus::Completed {
            result: CompletionResult::Quiesced,
            code: None,
            reply: None,
            output: None,
        },
        Completion::Failed {
            reason,
            reason_code,
        } => TriggerStatus::Failed {
            reason,
            reason_code: reason_code.to_string(),
        },
        Completion::Timeout => TriggerStatus::Failed {
            reason: "the kickoff did not complete before ask_timeout_minutes elapsed; \
                     check the target agent's pane for a stalled turn, then raise \
                     [workflow] ask_timeout_minutes or fire the trigger again"
                .to_string(),
            reason_code: "timeout".to_string(),
        },
    }
}

struct HubInner {
    /// Insertion-ordered so the cap evicts the oldest record; a trigger history
    /// is short and read by id, so a scan beats a map plus an eviction queue.
    records: VecDeque<(String, TriggerStatus)>,
    /// Per flow: the id of the trigger whose kickoff is running. Underpins
    /// serve's `begin` and warm mode's per-flow 409 (multi-flow spec §4–5).
    /// Serve's all-shared self-overlap records only the latest starter here;
    /// per-flow running *counts* live in the scheduler's `FlowLoad`.
    in_flight: BTreeMap<FlowName, String>,
}

/// Shared trigger bookkeeping: id generation, capped history, the in-flight id
/// that stops a second kickoff from landing on a busy workflow, and update
/// notifications for `?wait` long-polls.
pub struct TriggerHub {
    inner: Mutex<HubInner>,
    updates: broadcast::Sender<(String, TriggerStatus)>,
}

fn lock(mutex: &Mutex<HubInner>) -> MutexGuard<'_, HubInner> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `t-` + 8 lowercase hex, mirroring the HTTP request ids in `api::auth`.
fn trigger_id() -> String {
    use rand::RngExt;
    format!("t-{:08x}", rand::rng().random::<u32>())
}

/// Appends a record, evicting the oldest once the history is full.
fn push_record(inner: &mut HubInner, id: String, status: TriggerStatus) {
    inner.records.push_back((id, status));
    while inner.records.len() > HISTORY_CAP {
        inner.records.pop_front();
    }
}

impl TriggerHub {
    #[must_use]
    pub fn new() -> Arc<TriggerHub> {
        Arc::new(TriggerHub {
            inner: Mutex::new(HubInner {
                records: VecDeque::new(),
                in_flight: BTreeMap::new(),
            }),
            updates: broadcast::channel(UPDATE_BUFFER).0,
        })
    }

    /// Registers a new trigger at `status` and returns its id.
    pub fn register(&self, status: TriggerStatus) -> String {
        let id = trigger_id();
        push_record(&mut lock(&self.inner), id.clone(), status);
        id
    }

    /// Registers a trigger and claims `flow` for it in one step, so two
    /// concurrent callers cannot both find the flow free.
    ///
    /// # Errors
    /// The active trigger id when a kickoff is already in flight *in this
    /// flow* — other flows' kickoffs do not conflict.
    pub fn try_begin(&self, flow: &FlowName) -> Result<String, String> {
        let mut inner = lock(&self.inner);
        if let Some(active) = inner.in_flight.get(flow) {
            return Err(active.clone());
        }
        let id = trigger_id();
        push_record(&mut inner, id.clone(), TriggerStatus::Running);
        inner.in_flight.insert(flow.clone(), id.clone());
        Ok(id)
    }

    /// Moves an already-registered trigger to `running` and claims `flow`.
    /// Serve's flow workers call this unconditionally after locks + permit:
    /// the queue and locks, not this flag, are what serialize them.
    pub fn begin(&self, flow: &FlowName, id: &str) {
        lock(&self.inner)
            .in_flight
            .insert(flow.clone(), id.to_string());
        self.set_status(id, TriggerStatus::Running);
    }

    /// Records a trigger's terminal status and releases whichever flow it
    /// holds.
    pub fn finish(&self, id: &str, status: TriggerStatus) {
        lock(&self.inner).in_flight.retain(|_, active| active != id);
        self.set_status(id, status);
    }

    /// Updates `id`'s status and notifies waiters. An id the hub never issued
    /// (or one already evicted) is ignored: resurrecting it would report a
    /// trigger nobody can act on.
    pub fn set_status(&self, id: &str, status: TriggerStatus) {
        {
            let mut inner = lock(&self.inner);
            let Some(record) = inner.records.iter_mut().find(|(known, _)| known == id) else {
                tracing::debug!(trigger = id, "status update for an unknown trigger id");
                return;
            };
            record.1 = status.clone();
        }
        let _ = self.updates.send((id.to_string(), status));
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<TriggerStatus> {
        lock(&self.inner)
            .records
            .iter()
            .find(|(known, _)| known == id)
            .map(|(_, status)| status.clone())
    }

    /// The id running in `flow`, if any.
    #[must_use]
    pub fn in_flight(&self, flow: &FlowName) -> Option<String> {
        lock(&self.inner).in_flight.get(flow).cloned()
    }

    /// Snapshot of every flow's in-flight id (health, `GET /v1/flows`).
    #[must_use]
    pub fn in_flight_by_flow(&self) -> BTreeMap<FlowName, String> {
        lock(&self.inner).in_flight.clone()
    }

    /// Every status change, as `(trigger id, new status)`.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<(String, TriggerStatus)> {
        self.updates.subscribe()
    }

    /// Every record, oldest first — the snapshot's session history.
    #[must_use]
    pub fn views(&self) -> Vec<TriggerView> {
        lock(&self.inner)
            .records
            .iter()
            .map(|(id, status)| TriggerView {
                trigger_id: id.clone(),
                status: status.clone(),
            })
            .collect()
    }
}

/// Where a [`SettleOnDrop`] guard reports its trigger's terminal status. The
/// hub is the whole story for a warm run; serve mode wraps its per-flow
/// counters around the same call, which is why this is a trait rather than an
/// `Arc<TriggerHub>`.
pub trait SettleSink: Send + Sync + 'static {
    /// Records `id`'s terminal status and releases whatever it holds.
    fn settle(&self, id: &str, status: TriggerStatus);
}

impl SettleSink for TriggerHub {
    fn settle(&self, id: &str, status: TriggerStatus) {
        self.finish(id, status);
    }
}

/// Settles its trigger on drop unless the task already did — a panic, a
/// cancelled task and an early return included (multi-flow spec §4–5: every
/// exit path settles). A trigger task holds its flow's in-flight slot from the
/// moment it is accepted, so one that ends without settling wedges that flow:
/// every later trigger to it 409s until the process restarts.
///
/// Build it before `tokio::spawn` and let the task capture it: constructed
/// inside the future instead, it never exists for a task the runtime drops
/// before its first poll.
pub struct SettleOnDrop {
    sink: Arc<dyn SettleSink>,
    id: String,
    settled: bool,
}

impl SettleOnDrop {
    #[must_use]
    pub fn new(sink: Arc<dyn SettleSink>, id: String) -> SettleOnDrop {
        SettleOnDrop {
            sink,
            id,
            settled: false,
        }
    }

    /// Settles with the status the task actually reached; consuming, so the
    /// drop below cannot also fire.
    pub fn settle(mut self, status: TriggerStatus) {
        self.sink.settle(&self.id, status);
        self.settled = true;
    }
}

impl Drop for SettleOnDrop {
    fn drop(&mut self) {
        if !self.settled {
            self.sink.settle(
                &self.id,
                TriggerStatus::Failed {
                    reason: "internal: the trigger task ended without settling this \
                             trigger; report this"
                        .to_string(),
                    reason_code: "internal".to_string(),
                },
            );
        }
    }
}

/// Waits up to `wait` for `id` to reach a terminal status, returning it if it
/// arrives in time. Backs `?wait` long-polls in both warm and serve mode.
pub async fn await_terminal(hub: &TriggerHub, id: &str, wait: Duration) -> Option<TriggerStatus> {
    // Subscribe before reading: a status set in this window is not missed.
    let mut updates = hub.subscribe();
    if let Some(status) = hub.get(id).filter(is_terminal) {
        return Some(status);
    }
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match tokio::time::timeout_at(deadline, updates.recv()).await {
            Err(_elapsed) => return None,
            Ok(Ok((updated, status))) => {
                if updated == id && is_terminal(&status) {
                    return Some(status);
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(skipped))) => {
                tracing::warn!(skipped, trigger = id, "trigger waiter lagged; re-reading");
                if let Some(status) = hub.get(id).filter(is_terminal) {
                    return Some(status);
                }
            }
            // Unreachable while the caller holds the hub, but a waiter must not
            // spin if the sender ever goes away.
            Ok(Err(broadcast::error::RecvError::Closed)) => return hub.get(id).filter(is_terminal),
        }
    }
}

/// Everything the watcher needs; grouped to stay inside the 5-param rule.
pub struct WatchInputs {
    pub bus: EventBus,
    pub router: Arc<Router>,
    pub pty: Arc<dyn PtySource>,
    /// The agents whose quiescence settles this kickoff: the kickoff flow's
    /// member set (`Run::watch_inputs_for_flow`), or the whole frozen roster for a
    /// flowless run.
    pub roster: Vec<AgentId>,
    /// How long the system must hold still before it counts as quiesced.
    pub idle_debounce: Duration,
    /// Measured from watcher start, never from injection.
    pub deadline: Duration,
    /// The webhook flow's `output` contract, when it declares one: the final
    /// reply is re-validated against it before the caller sees it.
    pub output: Option<Arc<crate::schema::OutputContract>>,
    /// The hub id this watcher settles, when the kickoff is registered.
    pub trigger_id: Option<String>,
}

/// Outcome of one kickoff. `code`/`reply`/`output` are carried only by `Replied`.
#[derive(Debug, Clone, PartialEq)]
pub enum Completion {
    Replied {
        code: u8,
        reply: String,
        /// Parsed, schema-conforming reply — `Some` only when a contract is
        /// declared, the code is 0, and validation passed.
        output: Option<Value>,
    },
    Quiesced,
    Failed {
        reason: String,
        /// Machine-readable failure kind (design 2026-08-06 §HTTP wire).
        reason_code: &'static str,
    },
    Timeout,
}

impl Completion {
    fn result(&self) -> CompletionResult {
        match self {
            Completion::Replied { .. } => CompletionResult::Replied,
            Completion::Quiesced => CompletionResult::Quiesced,
            Completion::Failed { .. } => CompletionResult::Failed,
            Completion::Timeout => CompletionResult::Timeout,
        }
    }
}

/// The bus payload for one finished kickoff. `reason`/`reason_code` are set only
/// for `Failed`; a timeout is already distinguished by `result = "timeout"`.
#[must_use]
pub fn completion_event(
    completion: &Completion,
    trigger_id: Option<String>,
    message: MessageId,
) -> EventPayload {
    let (code, reply, output) = match completion {
        Completion::Replied {
            code,
            reply,
            output,
        } => (Some(*code), Some(reply.clone()), output.clone()),
        Completion::Quiesced | Completion::Failed { .. } | Completion::Timeout => {
            (None, None, None)
        }
    };
    let (reason, reason_code) = match completion {
        Completion::Failed {
            reason,
            reason_code,
        } => (Some(reason.clone()), Some((*reason_code).to_string())),
        Completion::Replied { .. } | Completion::Quiesced | Completion::Timeout => (None, None),
    };
    EventPayload::WorkflowCompleted {
        result: completion.result(),
        code,
        reply,
        trigger_id,
        message,
        output,
        reason,
        reason_code,
    }
}

/// Watches `kickoff` (already created via `Router::create_message`) to completion
/// and publishes `workflow.completed`.
///
/// The deadline runs from NOW, so callers invoke this immediately after creating
/// the message: a kickoff that is never injected must still terminate.
pub async fn watch_completion(inputs: WatchInputs, kickoff: MessageRecord) -> Completion {
    let deadline = tokio::time::Instant::now() + inputs.deadline;
    // Subscribe before reading any status: `drive` re-reads the record from the
    // store first, so a transition that lands in this window is not missed.
    let mut events = inputs.bus.subscribe();
    let completion = tokio::select! {
        c = drive(&inputs, &mut events, &kickoff) => c,
        () = tokio::time::sleep_until(deadline) => Completion::Timeout,
    };
    tracing::info!(
        message = %kickoff.id.0,
        result = ?completion,
        "kickoff completed"
    );
    inputs.bus.publish(completion_event(
        &completion,
        inputs.trigger_id.clone(),
        kickoff.id.clone(),
    ));
    completion
}

/// CRLF and lone `\r` become `\n`. A raw CR is Enter to the injection queue, so
/// a multi-line payload carrying one would submit the prompt early.
#[must_use]
pub fn normalize_payload(raw: &str) -> String {
    raw.replace("\r\n", "\n").replace('\r', "\n")
}

/// Why a trigger request body could not become a kickoff message.
#[derive(Debug, thiserror::Error)]
pub enum PayloadError {
    #[error(
        "trigger payload exceeds the {PAYLOAD_CAP_BYTES}-byte cap; the payload is typed \
         into an agent's prompt, so send a reference (a path, an id, a URL) instead of \
         the document itself"
    )]
    TooLarge,
    #[error(
        "trigger payload is not valid UTF-8 text ({0}); the body is typed into an \
         agent's prompt verbatim — send text, in any content type"
    )]
    NotUtf8(std::str::Utf8Error),
    #[error("could not read the trigger payload: {0}; resend the request")]
    Unreadable(String),
}

/// Reads a trigger request body into a kickoff message: capped at
/// [`PAYLOAD_CAP_BYTES`], required to be UTF-8, and normalized.
///
/// The body is accumulated chunk by chunk against the cap rather than buffered
/// and then measured, so an oversized request never occupies more than the cap.
///
/// # Errors
/// [`PayloadError::TooLarge`] above the cap, [`PayloadError::NotUtf8`] for
/// binary bodies, [`PayloadError::Unreadable`] when the request stream breaks.
pub async fn read_payload(body: axum::body::Body) -> Result<String, PayloadError> {
    let mut stream = body.into_data_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| PayloadError::Unreadable(error.to_string()))?;
        if buf.len() + chunk.len() > PAYLOAD_CAP_BYTES {
            return Err(PayloadError::TooLarge);
        }
        buf.extend_from_slice(&chunk);
    }
    let text = std::str::from_utf8(&buf).map_err(PayloadError::NotUtf8)?;
    Ok(normalize_payload(text))
}

async fn drive(
    inputs: &WatchInputs,
    events: &mut broadcast::Receiver<Event>,
    kickoff: &MessageRecord,
) -> Completion {
    if kickoff.kind == MessageKind::Ask {
        return await_reply(inputs, events, kickoff).await;
    }
    if let Err(completion) = await_arming(inputs, events, kickoff).await {
        return completion;
    }
    // The quiescence loop sees an exit through the debounced state; the
    // lifecycle event is the faster of the two and needs no roster scan.
    tokio::select! {
        c = await_quiescence(inputs) => c,
        c = await_exit(inputs, events) => c,
    }
}

fn exited(agent: &AgentId) -> Completion {
    Completion::Failed {
        reason: format!(
            "agent '{}' exited before the kickoff completed; check its pane, \
             then restart it and fire the trigger again",
            agent.0
        ),
        reason_code: "agent_exited",
    }
}

fn bus_closed() -> Completion {
    Completion::Failed {
        reason: "the event bus closed before the kickoff completed; the run is \
                 shutting down"
            .to_string(),
        reason_code: "internal",
    }
}

fn channel_closed(agent: &AgentId) -> Completion {
    Completion::Failed {
        reason: format!(
            "the state channel for agent '{}' closed before the kickoff \
             completed; the run is shutting down",
            agent.0
        ),
        reason_code: "internal",
    }
}

fn store_failed(error: &crate::router::RouterError) -> Completion {
    Completion::Failed {
        reason: format!("could not read the kickoff message: {error}"),
        reason_code: "internal",
    }
}

/// `Some` once the record is terminal: `replied` is the only success.
///
/// A code-0 reply is re-validated against `contract` here rather than trusted
/// from the router: the router accepts the last reply once the repair budget is
/// spent, so this is the boundary that tells the caller the shape it was
/// promised never arrived. The `output` it returns is the parsed value, not the
/// stored reply text, which can still carry markdown fences around valid JSON.
fn terminal_completion(
    rec: &MessageRecord,
    contract: Option<&crate::schema::OutputContract>,
) -> Option<Completion> {
    match rec.status {
        MessageStatus::Replied => {
            let code = rec.code.unwrap_or(1);
            let reply = rec.reply.clone().unwrap_or_default();
            // A non-zero code is the agent's declared escape hatch: prose, by
            // design, so the schema does not apply to it.
            match contract.filter(|_| code == 0) {
                Some(contract) => match contract.check(&reply) {
                    Ok(value) => Some(Completion::Replied {
                        code,
                        reply,
                        output: Some(value),
                    }),
                    // The router issued its whole budget before accepting this
                    // one, so `max_repairs` is the number of rejections used.
                    Err(errors) => Some(Completion::Failed {
                        reason: crate::schema::render_trigger_failure(
                            &errors,
                            contract.max_repairs,
                        ),
                        reason_code: "schema_validation_failed",
                    }),
                },
                None => Some(Completion::Replied {
                    code,
                    reply,
                    output: None,
                }),
            }
        }
        MessageStatus::Done | MessageStatus::Failed => Some(failed_completion(
            rec,
            format!(
                "kickoff ask '{}' ended at status '{}' without a reply",
                rec.id.0,
                rec.status.as_str()
            ),
        )),
        MessageStatus::Queued | MessageStatus::Injected | MessageStatus::Working => None,
    }
}

/// The record's own diagnosis, translated into the trigger wire's vocabulary.
/// A code the wire does not publish (`orphaned`, or anything a later router
/// adds) degrades to `agent_failed` rather than leaking; a record with no
/// reason of its own gets `fallback`. Shared by every kickoff shape so an ask
/// and a send that fail the same way report the same thing.
fn failed_completion(rec: &MessageRecord, fallback: String) -> Completion {
    let reason_code = match rec.reason_code.as_deref() {
        Some("blocked_on_permission") => "blocked_on_permission",
        Some("timeout") => "timeout",
        Some("agent_restarted") => "agent_restarted",
        Some("agent_exited") => "agent_exited",
        Some(_) | None => "agent_failed",
    };
    Completion::Failed {
        reason: rec.reason.clone().unwrap_or(fallback),
        reason_code,
    }
}

/// Re-reads the record from the store. Used on entry (the reply may already have
/// landed) and after a `Lagged`, where the missed events cannot be replayed.
async fn refetch(
    inputs: &WatchInputs,
    kickoff: &MessageRecord,
) -> Result<MessageRecord, Completion> {
    inputs
        .router
        .get_message(&kickoff.id)
        .await
        .map_err(|e| store_failed(&e))
}

/// `Some` when a fresh read of the record already settles the ask.
async fn recheck(inputs: &WatchInputs, kickoff: &MessageRecord) -> Option<Completion> {
    match refetch(inputs, kickoff).await {
        Err(completion) => Some(completion),
        Ok(rec) => terminal_completion(&rec, inputs.output.as_deref()),
    }
}

async fn await_reply(
    inputs: &WatchInputs,
    events: &mut broadcast::Receiver<Event>,
    kickoff: &MessageRecord,
) -> Completion {
    if let Some(completion) = recheck(inputs, kickoff).await {
        return completion;
    }
    loop {
        match events.recv().await {
            Ok(event) => {
                if let Some(completion) = ask_event(inputs, kickoff, &event.payload) {
                    return completion;
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, "kickoff watcher lagged; re-reading the record");
                if let Some(completion) = recheck(inputs, kickoff).await {
                    return completion;
                }
            }
            Err(broadcast::error::RecvError::Closed) => return bus_closed(),
        }
    }
}

fn ask_event(
    inputs: &WatchInputs,
    kickoff: &MessageRecord,
    payload: &EventPayload,
) -> Option<Completion> {
    match payload {
        EventPayload::MessageStatusChanged { message } if message.id == kickoff.id => {
            terminal_completion(message, inputs.output.as_deref())
        }
        EventPayload::AgentLifecycle {
            agent,
            phase: LifecyclePhase::Exited,
            ..
        } if inputs.roster.contains(agent) => Some(exited(agent)),
        _ => None,
    }
}

/// `Some(Ok)` once the kickoff has been observed by the agent, `Some(Err)` when
/// it can never be.
fn arming(rec: &MessageRecord) -> Option<Result<(), Completion>> {
    match rec.status {
        // `replied` cannot happen for a send (the router rejects it), but it is
        // still proof the agent saw the message.
        MessageStatus::Working | MessageStatus::Done | MessageStatus::Replied => Some(Ok(())),
        MessageStatus::Failed => Some(Err(failed_completion(
            rec,
            format!("kickoff send '{}' failed before the agent saw it", rec.id.0),
        ))),
        MessageStatus::Queued | MessageStatus::Injected => None,
    }
}

/// Phase 1 of a send: block until the kickoff reaches the agent. This is the
/// arming guard — quiescence measured before this point is meaningless.
async fn await_arming(
    inputs: &WatchInputs,
    events: &mut broadcast::Receiver<Event>,
    kickoff: &MessageRecord,
) -> Result<(), Completion> {
    if let Some(verdict) = arming(&refetch(inputs, kickoff).await?) {
        return verdict;
    }
    loop {
        match events.recv().await {
            Ok(event) => match &event.payload {
                EventPayload::MessageStatusChanged { message } if message.id == kickoff.id => {
                    if let Some(verdict) = arming(message) {
                        return verdict;
                    }
                }
                EventPayload::AgentLifecycle {
                    agent,
                    phase: LifecyclePhase::Exited,
                    ..
                } if inputs.roster.contains(agent) => return Err(exited(agent)),
                _ => {}
            },
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, "kickoff watcher lagged; re-reading the record");
                if let Some(verdict) = arming(&refetch(inputs, kickoff).await?) {
                    return verdict;
                }
            }
            Err(broadcast::error::RecvError::Closed) => return Err(bus_closed()),
        }
    }
}

async fn await_exit(inputs: &WatchInputs, events: &mut broadcast::Receiver<Event>) -> Completion {
    loop {
        match events.recv().await {
            Ok(event) => {
                if let EventPayload::AgentLifecycle {
                    agent,
                    phase: LifecyclePhase::Exited,
                    ..
                } = &event.payload
                    && inputs.roster.contains(agent)
                {
                    return exited(agent);
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, "kickoff watcher lagged watching for agent exits");
            }
            Err(broadcast::error::RecvError::Closed) => return bus_closed(),
        }
    }
}

type States = Vec<(AgentId, watch::Receiver<AgentState>)>;

enum Verdict {
    Quiescent,
    Blocked(String),
    Exited(AgentId),
}

fn subscribe_all(inputs: &WatchInputs) -> Result<States, Completion> {
    let mut states = States::new();
    for agent in &inputs.roster {
        match inputs.pty.subscribe_debounced(agent) {
            Ok(rx) => states.push((agent.clone(), rx)),
            Err(error) => {
                return Err(Completion::Failed {
                    reason: format!("cannot watch agent '{}': {error}", agent.0),
                    reason_code: "internal",
                });
            }
        }
    }
    Ok(states)
}

fn check(inputs: &WatchInputs, states: &mut States) -> Verdict {
    // Mark every receiver seen before judging any of them: an unread receiver
    // makes the next `changed()` fire instantly and spins the wait loop.
    let observed: Vec<(&AgentId, AgentState)> = states
        .iter_mut()
        .map(|(agent, rx)| (&*agent, *rx.borrow_and_update()))
        .collect();
    if let Some((agent, _)) = observed.iter().find(|(_, s)| *s == AgentState::Exited) {
        return Verdict::Exited((*agent).clone());
    }
    if let Some((agent, state)) = observed.iter().find(|(_, s)| *s != AgentState::Idle) {
        return Verdict::Blocked(format!("agent '{}' is {state:?}", agent.0));
    }
    for (agent, _) in &observed {
        match inputs.pty.queue_depth(agent) {
            Ok(0) => {}
            Ok(depth) => {
                return Verdict::Blocked(format!("agent '{}' has {depth} queued", agent.0));
            }
            Err(error) => {
                return Verdict::Blocked(format!("agent '{}' queue depth: {error}", agent.0));
            }
        }
    }
    let asks = inputs.router.total_pending_asks_among(&inputs.roster);
    if asks > 0 {
        return Verdict::Blocked(format!("{asks} ask(s) awaiting a reply"));
    }
    let turns = inputs.router.open_turns_among(&inputs.roster);
    if turns > 0 {
        return Verdict::Blocked(format!("{turns} agent(s) mid-turn"));
    }
    Verdict::Quiescent
}

/// Resolves when any agent's debounced state changes; `Err` names an agent whose
/// channel closed.
///
/// Hand-rolled `select_all`: `tokio::select!` is fixed-arity, the workspace has
/// no futures-combinator dependency, and spawning a task per receiver would have
/// to hand the receivers back before the next `borrow_and_update` — the
/// seen-version marks live in the receivers this loop owns.
async fn wait_any_change(states: &mut States) -> Result<(), AgentId> {
    type Changed<'a> =
        Pin<Box<dyn Future<Output = Result<(), watch::error::RecvError>> + Send + 'a>>;
    let mut waits: Vec<(AgentId, Changed<'_>)> = states
        .iter_mut()
        .map(|(agent, rx)| (agent.clone(), Box::pin(rx.changed()) as Changed<'_>))
        .collect();
    std::future::poll_fn(move |cx| {
        for (agent, wait) in &mut waits {
            if let Poll::Ready(result) = wait.as_mut().poll(cx) {
                return Poll::Ready(result.map_err(|_| agent.clone()));
            }
        }
        Poll::Pending
    })
    .await
}

/// `Err` names an agent whose channel closed; `Ok(true)` means something moved.
fn moved_during_dwell(states: &States) -> Result<bool, AgentId> {
    let mut moved = false;
    for (agent, rx) in states {
        match rx.has_changed() {
            Ok(changed) => moved |= changed,
            Err(_) => return Err(agent.clone()),
        }
    }
    Ok(moved)
}

/// One pass of the quiescence loop. `None` means the candidate was disqualified
/// and the caller should try again.
async fn settle_once(inputs: &WatchInputs, states: &mut States) -> Option<Completion> {
    match check(inputs, states) {
        Verdict::Exited(agent) => return Some(exited(&agent)),
        Verdict::Blocked(reason) => {
            tracing::debug!(%reason, "kickoff not quiescent yet");
            tokio::select! {
                result = wait_any_change(states) => {
                    if let Err(agent) = result {
                        return Some(channel_closed(&agent));
                    }
                }
                () = tokio::time::sleep(QUIESCENCE_POLL) => {}
            }
            return None;
        }
        Verdict::Quiescent => {}
    }
    // Candidate quiescence: hold for one dwell, then re-verify. Any movement at
    // all disqualifies, including a working→idle round trip inside the dwell
    // that a value comparison alone would miss.
    tokio::time::sleep(inputs.idle_debounce).await;
    match moved_during_dwell(states) {
        Err(agent) => return Some(channel_closed(&agent)),
        Ok(true) => return None,
        Ok(false) => {}
    }
    match check(inputs, states) {
        Verdict::Exited(agent) => Some(exited(&agent)),
        Verdict::Blocked(_) => None,
        Verdict::Quiescent => Some(Completion::Quiesced),
    }
}

/// Phase 2 of a send: the whole workflow must hold still for one dwell.
async fn await_quiescence(inputs: &WatchInputs) -> Completion {
    let mut states = match subscribe_all(inputs) {
        Ok(states) => states,
        Err(completion) => return completion,
    };
    loop {
        if let Some(completion) = settle_once(inputs, &mut states).await {
            return completion;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use crate::time::Timestamp;
    use crate::trigger::{
        Completion, HISTORY_CAP, PAYLOAD_CAP_BYTES, PayloadError, SettleOnDrop, SettleSink,
        TriggerHub, TriggerStatus, TriggerView, arming, await_terminal, completion_event,
        completion_status, normalize_payload, read_payload, startup_id, terminal_completion,
        watcher_deadline,
    };
    use crate::types::event::{CompletionResult, EventPayload};
    use crate::types::id::{AgentId, FlowName, MessageId};
    use crate::types::message::{MessageKind, MessageRecord, MessageStatus, Origin};

    #[test]
    fn normalize_leaves_plain_newlines_alone() {
        assert_eq!(normalize_payload("a\nb"), "a\nb");
        assert_eq!(normalize_payload(""), "");
    }

    fn failed_ask() -> MessageRecord {
        MessageRecord {
            id: MessageId("m-0000000000000001".into()),
            kind: MessageKind::Ask,
            from: Origin::Http("1f2e3d4c".into()),
            to: AgentId("r".into()),
            body: "go".into(),
            status: MessageStatus::Failed,
            code: None,
            reply: None,
            created_at: Timestamp("2026-08-18T00:00:00Z".into()),
            injected_at: None,
            completed_at: Some(Timestamp("2026-08-18T00:01:30Z".into())),
            reason: None,
            reason_code: None,
        }
    }

    #[test]
    fn failed_record_reason_wins_over_the_generic_agent_failed() {
        let mut rec = failed_ask();
        rec.reason = Some("agent 'r' has been waiting on … Bash(python3 …)".into());
        rec.reason_code = Some("blocked_on_permission".into());
        match terminal_completion(&rec, None) {
            Some(Completion::Failed {
                reason,
                reason_code,
            }) => {
                assert_eq!(reason_code, "blocked_on_permission");
                assert!(reason.contains("Bash(python3"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        rec.reason_code = Some("orphaned".into());
        match terminal_completion(&rec, None) {
            Some(Completion::Failed { reason_code, .. }) => assert_eq!(reason_code, "agent_failed"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// A send kickoff that fails before arming reports the record's own code,
    /// exactly as the identical ask kickoff does through `terminal_completion`.
    #[test]
    fn failed_send_kickoff_arming_reports_the_records_code() {
        let mut rec = failed_ask();
        rec.kind = MessageKind::Send;
        rec.reason = Some("agent 'r' restarted before it saw the message".into());
        rec.reason_code = Some("agent_restarted".into());
        match arming(&rec) {
            Some(Err(Completion::Failed {
                reason,
                reason_code,
            })) => {
                assert_eq!(reason_code, "agent_restarted");
                assert!(reason.contains("restarted"), "got {reason}");
            }
            other => panic!("expected Err(Failed), got {other:?}"),
        }
        // An unmapped code still degrades to agent_failed, with generic prose
        // when the record carries no reason of its own.
        rec.reason = None;
        rec.reason_code = Some("orphaned".into());
        match arming(&rec) {
            Some(Err(Completion::Failed {
                reason,
                reason_code,
            })) => {
                assert_eq!(reason_code, "agent_failed");
                assert!(reason.contains("before the agent saw it"), "got {reason}");
            }
            other => panic!("expected Err(Failed), got {other:?}"),
        }
    }

    #[test]
    fn completion_event_carries_output_and_failure_detail() {
        let replied = Completion::Replied {
            code: 0,
            reply: "{\"ok\":true}".to_string(),
            output: Some(serde_json::json!({"ok": true})),
        };
        let ev = completion_event(&replied, Some("t-a3f91c2e".into()), MessageId("m-1".into()));
        let EventPayload::WorkflowCompleted {
            result,
            code,
            output,
            reason_code,
            ..
        } = ev
        else {
            panic!("wrong payload variant");
        };
        assert_eq!(result, CompletionResult::Replied);
        assert_eq!(code, Some(0));
        assert_eq!(output, Some(serde_json::json!({"ok": true})));
        assert_eq!(reason_code, None);

        let failed = Completion::Failed {
            reason: "agent exited".to_string(),
            reason_code: "agent_exited",
        };
        let ev = completion_event(&failed, None, MessageId("m-2".into()));
        let EventPayload::WorkflowCompleted {
            result,
            reason,
            reason_code,
            output,
            ..
        } = ev
        else {
            panic!("wrong payload variant");
        };
        assert_eq!(result, CompletionResult::Failed);
        assert_eq!(reason, Some("agent exited".to_string()));
        assert_eq!(reason_code, Some("agent_exited".to_string()));
        assert_eq!(output, None);

        let ev = completion_event(&Completion::Timeout, None, MessageId("m-3".into()));
        let EventPayload::WorkflowCompleted {
            result,
            reason_code,
            ..
        } = ev
        else {
            panic!("wrong payload variant");
        };
        assert_eq!(result, CompletionResult::Timeout);
        assert_eq!(
            reason_code, None,
            "timeout is already distinguished by result"
        );
    }

    #[test]
    fn register_mints_distinct_prefixed_ids() {
        let hub = TriggerHub::new();
        let mut ids = Vec::new();
        for _ in 0..16 {
            let id = hub.register(TriggerStatus::Running);
            assert!(id.starts_with("t-"), "id {id}");
            assert_eq!(id.len(), 10, "id {id}");
            assert!(
                id[2..]
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "id {id}"
            );
            ids.push(id);
        }
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 16, "register minted a duplicate id");
    }

    #[test]
    fn set_status_round_trips_and_unknown_ids_are_none() {
        let hub = TriggerHub::new();
        let id = hub.register(TriggerStatus::Queued { position: 3 });
        assert_eq!(hub.get(&id), Some(TriggerStatus::Queued { position: 3 }));
        hub.set_status(&id, TriggerStatus::Running);
        assert_eq!(hub.get(&id), Some(TriggerStatus::Running));
        assert_eq!(hub.get("t-deadbeef"), None);
        // A status set on an id the hub never issued is dropped, not inserted.
        hub.set_status("t-deadbeef", TriggerStatus::Running);
        assert_eq!(hub.get("t-deadbeef"), None);
    }

    #[test]
    fn history_caps_and_evicts_oldest_first() {
        let hub = TriggerHub::new();
        let ids: Vec<String> = (0..=HISTORY_CAP)
            .map(|position| hub.register(TriggerStatus::Queued { position }))
            .collect();
        assert_eq!(hub.get(&ids[0]), None, "the oldest record survived the cap");
        assert_eq!(
            hub.get(&ids[1]),
            Some(TriggerStatus::Queued { position: 1 }),
            "the cap evicted more than the oldest record"
        );
        assert_eq!(
            hub.get(&ids[HISTORY_CAP]),
            Some(TriggerStatus::Queued {
                position: HISTORY_CAP
            })
        );
    }

    fn flow_name(name: &str) -> FlowName {
        FlowName(name.to_string())
    }

    #[test]
    fn try_begin_claims_one_flow_until_the_trigger_finishes() {
        let hub = TriggerHub::new();
        let post = flow_name("post");
        assert_eq!(hub.in_flight(&post), None);
        let first = hub.try_begin(&post).expect("the flow starts free");
        assert_eq!(hub.get(&first), Some(TriggerStatus::Running));
        assert_eq!(hub.in_flight(&post), Some(first.clone()));
        // The second caller is told which trigger holds the flow, and no record
        // is minted for it.
        assert_eq!(hub.try_begin(&post), Err(first.clone()), "same flow: 409");
        hub.finish(
            &first,
            TriggerStatus::Completed {
                result: CompletionResult::Quiesced,
                code: None,
                reply: None,
                output: None,
            },
        );
        assert_eq!(hub.in_flight(&post), None);
        hub.try_begin(&post).expect("finishing releases the flow");
    }

    #[test]
    fn a_dropped_settle_guard_releases_the_flow_as_an_internal_failure() {
        // The whole point of the guard: a task that never reaches its own
        // `settle` call still gives the flow back, labelled so the failure
        // reads as a bug rather than a workflow outcome.
        let hub = TriggerHub::new();
        let hook = flow_name("hook");
        let id = hub.try_begin(&hook).expect("free");
        drop(SettleOnDrop::new(
            Arc::clone(&hub) as Arc<dyn SettleSink>,
            id.clone(),
        ));
        assert_eq!(hub.in_flight(&hook), None);
        let TriggerStatus::Failed {
            reason,
            reason_code,
        } = hub.get(&id).expect("the record survives")
        else {
            panic!("a dropped guard must settle the trigger as failed");
        };
        assert_eq!(reason_code, "internal");
        assert!(reason.contains("without settling"), "reason: {reason}");
    }

    #[test]
    fn an_explicitly_settled_guard_does_not_overwrite_on_drop() {
        let hub = TriggerHub::new();
        let hook = flow_name("hook");
        let id = hub.try_begin(&hook).expect("free");
        let guard = SettleOnDrop::new(Arc::clone(&hub) as Arc<dyn SettleSink>, id.clone());
        guard.settle(TriggerStatus::Completed {
            result: CompletionResult::Quiesced,
            code: None,
            reply: None,
            output: None,
        });
        assert_eq!(
            hub.get(&id),
            Some(TriggerStatus::Completed {
                result: CompletionResult::Quiesced,
                code: None,
                reply: None,
                output: None,
            }),
            "the task's own outcome must survive the guard's drop"
        );
        assert_eq!(hub.in_flight(&hook), None);
    }

    #[test]
    fn flows_claim_independently_and_finish_releases_only_its_own() {
        let hub = TriggerHub::new();
        let a = hub.try_begin(&flow_name("a")).expect("free");
        let b = hub
            .try_begin(&flow_name("b"))
            .expect("a busy flow must not block a different one");
        assert_eq!(
            hub.in_flight_by_flow(),
            std::collections::BTreeMap::from([
                (flow_name("a"), a.clone()),
                (flow_name("b"), b.clone()),
            ])
        );
        hub.finish(
            &a,
            TriggerStatus::Failed {
                reason: "x".to_string(),
                reason_code: "internal".to_string(),
            },
        );
        assert_eq!(hub.in_flight(&flow_name("a")), None);
        assert_eq!(hub.in_flight(&flow_name("b")), Some(b));
    }

    #[test]
    fn finishing_a_stale_id_leaves_the_active_claim_alone() {
        // Serve mode's worker and a late warm watcher can both call finish; only
        // the trigger that holds the claim may release it.
        let hub = TriggerHub::new();
        let hook = flow_name("hook");
        let stale = hub.register(TriggerStatus::Running);
        let active = hub.try_begin(&hook).expect("free");
        hub.finish(
            &stale,
            TriggerStatus::Failed {
                reason: "late".to_string(),
                reason_code: "internal".to_string(),
            },
        );
        assert_eq!(hub.in_flight(&hook), Some(active));
    }

    #[test]
    fn begin_marks_a_queued_trigger_running() {
        let hub = TriggerHub::new();
        let hook = flow_name("hook");
        let id = hub.register(TriggerStatus::Queued { position: 1 });
        hub.begin(&hook, &id);
        assert_eq!(hub.get(&id), Some(TriggerStatus::Running));
        assert_eq!(hub.in_flight(&hook), Some(id));
    }

    #[test]
    fn completion_maps_replies_and_quiescence_to_completed() {
        assert_eq!(
            completion_status(Completion::Replied {
                code: 1,
                reply: "nope".to_string(),
                output: None,
            }),
            TriggerStatus::Completed {
                result: CompletionResult::Replied,
                code: Some(1),
                reply: Some("nope".to_string()),
                output: None,
            }
        );
        // A validated reply carries the parsed value alongside the raw text.
        assert_eq!(
            completion_status(Completion::Replied {
                code: 0,
                reply: "```json\n{\"n\":1}\n```".to_string(),
                output: Some(serde_json::json!({"n": 1})),
            }),
            TriggerStatus::Completed {
                result: CompletionResult::Replied,
                code: Some(0),
                reply: Some("```json\n{\"n\":1}\n```".to_string()),
                output: Some(serde_json::json!({"n": 1})),
            }
        );
        assert_eq!(
            completion_status(Completion::Quiesced),
            TriggerStatus::Completed {
                result: CompletionResult::Quiesced,
                code: None,
                reply: None,
                output: None,
            }
        );
        // A timeout keeps its diagnosis, which `completed { result }` has
        // nowhere to carry.
        match completion_status(Completion::Timeout) {
            TriggerStatus::Failed {
                reason,
                reason_code,
            } => {
                assert!(reason.contains("ask_timeout_minutes"), "reason: {reason}");
                assert_eq!(reason_code, "timeout");
            }
            other => panic!("expected failed, got {other:?}"),
        }
        assert_eq!(
            completion_status(Completion::Failed {
                reason: "boom".to_string(),
                reason_code: "agent_failed",
            }),
            TriggerStatus::Failed {
                reason: "boom".to_string(),
                reason_code: "agent_failed".to_string(),
            }
        );
    }

    #[test]
    fn watcher_deadline_stays_under_the_ask_timeout() {
        // The watcher must resolve before the router's TTL sweeper marks the
        // kickoff failed, so that a timeout is labelled as one.
        assert_eq!(
            watcher_deadline(Duration::from_secs(30)),
            Duration::from_secs(28)
        );
        // A tiny ask_timeout still leaves a positive deadline.
        assert_eq!(
            watcher_deadline(Duration::from_secs(1)),
            Duration::from_secs(1)
        );
        assert_eq!(watcher_deadline(Duration::ZERO), Duration::from_secs(1));
    }

    #[tokio::test]
    async fn await_terminal_returns_a_result_that_lands_while_waiting() {
        let hub = TriggerHub::new();
        let id = hub.try_begin(&flow_name("hook")).expect("free");
        let waiter = {
            let hub = Arc::clone(&hub);
            let id = id.clone();
            tokio::spawn(async move { await_terminal(&hub, &id, Duration::from_secs(5)).await })
        };
        tokio::task::yield_now().await;
        hub.finish(
            &id,
            TriggerStatus::Failed {
                reason: "boom".to_string(),
                reason_code: "internal".to_string(),
            },
        );
        assert_eq!(
            waiter.await.expect("waiter panicked"),
            Some(TriggerStatus::Failed {
                reason: "boom".to_string(),
                reason_code: "internal".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn await_terminal_gives_up_on_a_running_trigger() {
        let hub = TriggerHub::new();
        let id = hub.try_begin(&flow_name("hook")).expect("free");
        assert_eq!(
            await_terminal(&hub, &id, Duration::from_millis(50)).await,
            None
        );
        // An already-terminal trigger resolves without any update at all.
        hub.finish(
            &id,
            TriggerStatus::Failed {
                reason: "done".to_string(),
                reason_code: "internal".to_string(),
            },
        );
        assert!(
            await_terminal(&hub, &id, Duration::from_millis(50))
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn read_payload_caps_normalizes_and_rejects_binary() {
        use axum::body::Body;
        assert_eq!(
            read_payload(Body::from("a\r\nb")).await.expect("utf-8"),
            "a\nb"
        );
        let at_cap = read_payload(Body::from(vec![b'x'; PAYLOAD_CAP_BYTES])).await;
        assert_eq!(at_cap.map(|s| s.len()).ok(), Some(PAYLOAD_CAP_BYTES));
        let over = read_payload(Body::from(vec![b'x'; PAYLOAD_CAP_BYTES + 1])).await;
        assert!(matches!(over, Err(PayloadError::TooLarge)), "{over:?}");
        let binary = read_payload(Body::from(vec![0xff, 0xfe])).await;
        assert!(
            matches!(binary, Err(PayloadError::NotUtf8(_))),
            "{binary:?}"
        );
    }

    #[test]
    fn status_wire_forms() {
        let json = |s: &TriggerStatus| serde_json::to_string(s).expect("serialize");
        assert_eq!(
            json(&TriggerStatus::Queued { position: 2 }),
            r#"{"status":"queued","position":2}"#
        );
        assert_eq!(json(&TriggerStatus::Running), r#"{"status":"running"}"#);
        assert_eq!(
            json(&TriggerStatus::Completed {
                result: CompletionResult::Replied,
                code: Some(0),
                reply: Some("done".to_string()),
                output: None,
            }),
            r#"{"status":"completed","result":"replied","code":0,"reply":"done"}"#
        );
        // Dual emission: a validated reply ships parsed and raw side by side.
        assert_eq!(
            json(&TriggerStatus::Completed {
                result: CompletionResult::Replied,
                code: Some(0),
                reply: Some(r#"{"name":"x"}"#.to_string()),
                output: Some(serde_json::json!({"name": "x"})),
            }),
            concat!(
                r#"{"status":"completed","result":"replied","code":0,"#,
                r#""reply":"{\"name\":\"x\"}","output":{"name":"x"}}"#
            )
        );
        // Quiesced carries no code, reply or output, and all three are omitted.
        assert_eq!(
            json(&TriggerStatus::Completed {
                result: CompletionResult::Quiesced,
                code: None,
                reply: None,
                output: None,
            }),
            r#"{"status":"completed","result":"quiesced"}"#
        );
        assert_eq!(
            json(&TriggerStatus::Failed {
                reason: "nope".to_string(),
                reason_code: "agent_failed".to_string(),
            }),
            r#"{"status":"failed","reason":"nope","reason_code":"agent_failed"}"#
        );
    }

    #[test]
    fn views_lists_records_in_insertion_order() {
        let hub = TriggerHub::new();
        let first = hub.register(TriggerStatus::Running);
        let second = hub.register(TriggerStatus::Queued { position: 1 });
        let views = hub.views();
        assert_eq!(
            views
                .iter()
                .map(|v| v.trigger_id.as_str())
                .collect::<Vec<_>>(),
            vec![first.as_str(), second.as_str()],
        );
        assert_eq!(views[0].status, TriggerStatus::Running);
    }

    #[test]
    fn trigger_view_round_trips_through_json() {
        let failed = TriggerView {
            trigger_id: "t-a3f91c2e".to_string(),
            status: TriggerStatus::Failed {
                reason: "the agent exited".to_string(),
                reason_code: "agent_exited".to_string(),
            },
        };
        let json = serde_json::to_value(&failed).unwrap();
        assert_eq!(json["status"], "failed");
        assert_eq!(json["reason_code"], "agent_exited");
        let back: TriggerView = serde_json::from_value(json).unwrap();
        assert_eq!(back, failed);

        let completed = TriggerView {
            trigger_id: "t-b7c21d0e".to_string(),
            status: TriggerStatus::Completed {
                result: CompletionResult::Replied,
                code: Some(0),
                reply: Some("{\"ok\":true}".to_string()),
                output: Some(serde_json::json!({"ok": true})),
            },
        };
        let back: TriggerView =
            serde_json::from_value(serde_json::to_value(&completed).unwrap()).unwrap();
        assert_eq!(back, completed);
    }

    #[test]
    fn startup_id_mints_distinct_lowercase_hex() {
        let mut ids = Vec::new();
        for _ in 0..16 {
            let id = startup_id();
            assert_eq!(id.len(), 8, "id {id}");
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "id {id}"
            );
            ids.push(id);
        }
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 16, "startup_id minted a duplicate id");
    }
}
