#![expect(clippy::unwrap_used, reason = "tests assert on known-good values")]

//! Kickoff completion watching (spec triggers §2): ask-terminal, quiescence with
//! its arming guard, agent-exit fast path, and the creation-based deadline.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use coretempo_core::bus::EventBus;
use coretempo_core::pty::{Cursor, InjectError, Injected, InjectionQueue, PtyChunk, PtyError};
use coretempo_core::router::{Router, StateSource};
use coretempo_core::schema::OutputContract;
use coretempo_core::store::Store;
use coretempo_core::time::Timestamp;
use coretempo_core::trigger::{
    Completion, WatchInputs, completion_status, normalize_payload, watch_completion,
};
use coretempo_core::types::agent::{AgentExit, AgentState};
use coretempo_core::types::config::{
    AgentConfig, Edge, EdgeKind, FrozenFlow, FrozenWorkflow, TriggerType,
};
use coretempo_core::types::event::{Event, EventPayload, LifecyclePhase};
use coretempo_core::types::id::{AgentId, FlowName, MessageId, RunId};
use coretempo_core::types::message::{MessageKind, MessageRecord, MessageStatus, Origin};
use coretempo_core::{api::PtySource, types::event::CompletionResult};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

/// One dwell, kept short because these tests wait on it for real.
///
/// Paused time is unusable here: the store runs its own OS thread, so while the
/// watcher awaits a record read the tokio runtime looks idle and auto-advances
/// the clock straight past the very deadlines under test.
const DWELL: Duration = Duration::from_millis(200);

static DB_N: AtomicU64 = AtomicU64::new(0);

fn temp_db() -> PathBuf {
    let n = DB_N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "coretempo-completion-{}-{n}.db",
        std::process::id()
    ))
}

fn agent(id: &str) -> AgentId {
    AgentId(id.to_string())
}

#[derive(Clone, Copy, PartialEq)]
enum InjectMode {
    Auto,
    /// Never resolves the injection oneshot: the message is stuck at `queued`.
    Hold,
}

struct MockInjector {
    mode: InjectMode,
    /// Held senders must outlive the test: dropping one resolves the receiver
    /// with an error, which fails the message instead of holding it.
    held: Mutex<Vec<oneshot::Sender<Result<Injected, InjectError>>>>,
}

impl InjectionQueue for MockInjector {
    fn enqueue(
        &self,
        _target: AgentId,
        _text: String,
    ) -> oneshot::Receiver<Result<Injected, InjectError>> {
        let (tx, rx) = oneshot::channel();
        match self.mode {
            InjectMode::Auto => {
                let _ = tx.send(Ok(Injected {
                    at: Timestamp::now(),
                    cursor: Cursor(0),
                }));
            }
            InjectMode::Hold => self.held.lock().unwrap().push(tx),
        }
        rx
    }
}

/// Debounced states and queue depths the test drives directly. Doubles as the
/// router's `StateSource` so message status and quiescence read one truth.
struct FakePty {
    chans: Mutex<BTreeMap<AgentId, watch::Sender<AgentState>>>,
    depths: Mutex<BTreeMap<AgentId, Arc<AtomicU64>>>,
}

impl FakePty {
    fn new(ids: &[&str]) -> Arc<FakePty> {
        let mut chans = BTreeMap::new();
        let mut depths = BTreeMap::new();
        for id in ids {
            chans.insert(agent(id), watch::channel(AgentState::Idle).0);
            depths.insert(agent(id), Arc::new(AtomicU64::new(0)));
        }
        Arc::new(FakePty {
            chans: Mutex::new(chans),
            depths: Mutex::new(depths),
        })
    }

    fn set(&self, id: &str, state: AgentState) {
        self.chans
            .lock()
            .unwrap()
            .get(&agent(id))
            .unwrap()
            .send_replace(state);
    }

    fn depth(&self, id: &str) -> Arc<AtomicU64> {
        self.depths.lock().unwrap().get(&agent(id)).unwrap().clone()
    }
}

impl PtySource for FakePty {
    fn state(&self, id: &AgentId) -> Result<AgentState, PtyError> {
        let chans = self.chans.lock().unwrap();
        let tx = chans
            .get(id)
            .ok_or_else(|| PtyError::UnknownAgent(id.clone()))?;
        Ok(*tx.borrow())
    }
    fn report_state(&self, id: &AgentId, state: AgentState) -> Result<(), PtyError> {
        let chans = self.chans.lock().unwrap();
        let tx = chans
            .get(id)
            .ok_or_else(|| PtyError::UnknownAgent(id.clone()))?;
        tx.send_replace(state);
        Ok(())
    }
    fn report_blocked(
        &self,
        _id: &AgentId,
        _tool: Option<String>,
        _agent_id: Option<String>,
    ) -> Result<(), PtyError> {
        Ok(())
    }
    fn report_refused(
        &self,
        _id: &AgentId,
        _tool: Option<String>,
        _input: Option<String>,
    ) -> Result<(), PtyError> {
        Ok(())
    }
    fn report_unblocked(&self, _id: &AgentId, _agent_id: Option<String>) -> Result<(), PtyError> {
        Ok(())
    }
    fn exit(&self, _id: &AgentId) -> Result<Option<AgentExit>, PtyError> {
        Ok(None)
    }
    fn end_cursor(&self, _id: &AgentId) -> Result<Cursor, PtyError> {
        Ok(Cursor(0))
    }
    fn subscribe_output(
        &self,
        _id: &AgentId,
        _since: Option<Cursor>,
    ) -> Result<mpsc::Receiver<PtyChunk>, PtyError> {
        // The watcher never reads PTY bytes; an immediately-closed stream is enough.
        let (_tx, rx) = mpsc::channel(1);
        Ok(rx)
    }
    fn begin_restart(&self, _id: AgentId) {}
    fn queue_depth(&self, id: &AgentId) -> Result<u64, PtyError> {
        let depths = self.depths.lock().unwrap();
        let depth = depths
            .get(id)
            .ok_or_else(|| PtyError::UnknownAgent(id.clone()))?;
        Ok(depth.load(Ordering::SeqCst))
    }
    fn subscribe_debounced(&self, id: &AgentId) -> Result<watch::Receiver<AgentState>, PtyError> {
        let chans = self.chans.lock().unwrap();
        let tx = chans
            .get(id)
            .ok_or_else(|| PtyError::UnknownAgent(id.clone()))?;
        Ok(tx.subscribe())
    }
    fn blocked(&self, _id: &AgentId) -> Result<bool, PtyError> {
        Ok(false)
    }
    fn blocked_count(&self) -> usize {
        0
    }
}

impl StateSource for FakePty {
    fn subscribe_debounced(&self, id: &AgentId) -> Option<watch::Receiver<AgentState>> {
        self.chans
            .lock()
            .unwrap()
            .get(id)
            .map(watch::Sender::subscribe)
    }
}

struct Harness {
    router: Arc<Router>,
    pty: Arc<FakePty>,
    bus: EventBus,
    roster: Vec<AgentId>,
    output: Option<Arc<OutputContract>>,
}

/// `builder` must answer with `{"name": <string>}` and nothing else. The router
/// and the trigger watcher share one contract, so `max_repairs` decides which of
/// them an off-schema reply is refused by: while the budget lasts the router
/// rejects it, once it is spent the boundary fails the trigger.
fn contract(max_repairs: u32) -> Arc<OutputContract> {
    let schema = serde_json::json!({
        "type": "object",
        "properties": { "name": { "type": "string" } },
        "required": ["name"],
        "additionalProperties": false
    });
    // The flow the watcher's workflow declares, so the rejection names the same
    // one the kickoff's header does.
    Arc::new(
        OutputContract::compile(
            schema,
            FlowName("hook".to_string()),
            agent("builder"),
            max_repairs,
        )
        .unwrap(),
    )
}

fn workflow(
    ids: &[&str],
    output: Option<Arc<OutputContract>>,
    edges: &[(&str, &str, EdgeKind)],
) -> Arc<FrozenWorkflow> {
    let mut agents = BTreeMap::new();
    for id in ids {
        let agent_edges = edges
            .iter()
            .filter(|(from, _, _)| from == id)
            .map(|(_, to, kind)| Edge {
                to: agent(to),
                kind: *kind,
                max_rounds: None,
            })
            .collect();
        agents.insert(
            agent(id),
            AgentConfig {
                edges: agent_edges,
                ..AgentConfig::new(PathBuf::from("/tmp"), "test agent")
            },
        );
    }
    Arc::new(FrozenWorkflow {
        name: "test".to_string(),
        hash: "0".repeat(64),
        source_path: PathBuf::from("tempo.toml"),
        ask_timeout: Duration::from_mins(30),
        idle_debounce: DWELL,
        scrollback: 5_000,
        agents,
        mcp_servers: BTreeMap::new(),
        flows: webhook_flows(ids, output),
    })
}

/// The router reads its reply contract off the webhook flow, so an output
/// contract is frozen onto one: a single flow spanning the whole roster,
/// kicked off at the contract's target.
fn webhook_flows(
    ids: &[&str],
    output: Option<Arc<OutputContract>>,
) -> BTreeMap<FlowName, FrozenFlow> {
    let Some(contract) = output else {
        return BTreeMap::new();
    };
    BTreeMap::from([(
        FlowName("hook".to_string()),
        FrozenFlow {
            members: ids.iter().map(|id| agent(id)).collect(),
            trigger_type: TriggerType::Webhook,
            edge: Edge {
                to: contract.target.clone(),
                kind: EdgeKind::Ask,
                max_rounds: None,
            },
            message: None,
            output: Some(contract),
        },
    )])
}

fn harness(ids: &[&str], mode: InjectMode) -> Harness {
    harness_with(ids, mode, None)
}

fn harness_with(ids: &[&str], mode: InjectMode, output: Option<Arc<OutputContract>>) -> Harness {
    harness_from(ids, mode, output, &[])
}

/// Twin of `harness_with` that wires `edges` (`(from, to, kind)`) into each
/// agent's `AgentConfig`, so tests can arm obligation turns.
fn harness_with_edges(ids: &[&str], edges: &[(&str, &str, EdgeKind)]) -> Harness {
    harness_from(ids, InjectMode::Auto, None, edges)
}

fn harness_from(
    ids: &[&str],
    mode: InjectMode,
    output: Option<Arc<OutputContract>>,
    edges: &[(&str, &str, EdgeKind)],
) -> Harness {
    let store = Store::open(&temp_db(), RunId("r-11111111".to_string())).unwrap();
    let bus = EventBus::new();
    let pty = FakePty::new(ids);
    let injector = Arc::new(MockInjector {
        mode,
        held: Mutex::new(Vec::new()),
    });
    let router = Router::new(
        store,
        bus.clone(),
        injector,
        workflow(ids, output.clone(), edges),
    );
    router.set_state_source(pty.clone());
    Harness {
        router,
        pty,
        bus,
        roster: ids.iter().map(|id| agent(id)).collect(),
        output,
    }
}

fn inputs(h: &Harness, deadline: Duration) -> WatchInputs {
    WatchInputs {
        bus: h.bus.clone(),
        router: h.router.clone(),
        pty: h.pty.clone(),
        roster: h.roster.clone(),
        idle_debounce: DWELL,
        deadline,
        output: h.output.clone(),
        trigger_id: None,
    }
}

/// A deadline far past anything a test waits for: only quiescence or a reply
/// may resolve the watcher.
const NO_DEADLINE: Duration = Duration::from_hours(24);

async fn kickoff(h: &Harness, to: &str, kind: MessageKind) -> MessageRecord {
    h.router
        .create_message(
            Origin::Trigger("cli".to_string()),
            agent(to),
            kind,
            "go".to_string(),
        )
        .await
        .unwrap()
}

/// Polls the store until the message reaches `want`.
async fn wait_status(h: &Harness, id: &MessageId, want: MessageStatus) {
    for _ in 0..2000 {
        if h.router.get_message(id).await.unwrap().status == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let last = h.router.get_message(id).await.unwrap().status;
    assert_eq!(last, want, "timed out waiting for status");
}

/// The `result` field of the single `workflow.completed` event on the bus.
fn completed_result(events: &mut broadcast::Receiver<Event>) -> CompletionResult {
    let mut found = None;
    while let Ok(event) = events.try_recv() {
        if let EventPayload::WorkflowCompleted { result, .. } = event.payload {
            assert!(found.is_none(), "workflow.completed published twice");
            found = Some(result);
        }
    }
    assert!(found.is_some(), "no workflow.completed event on the bus");
    found.unwrap()
}

/// Drives the kickoff send to `working` so the quiescence phase is armed, then
/// parks the target back at idle.
async fn arm_send(h: &Harness, target: &str) -> MessageRecord {
    let record = kickoff(h, target, MessageKind::Send).await;
    h.pty.set(target, AgentState::Working);
    wait_status(h, &record.id, MessageStatus::Working).await;
    h.pty.set(target, AgentState::Idle);
    record
}

#[test]
fn normalize_payload_strips_carriage_returns() {
    // A raw CR is Enter to the injection queue: a multi-line payload must never
    // submit the prompt early.
    assert_eq!(normalize_payload("a\r\nb\rc\n"), "a\nb\nc\n");
    assert_eq!(normalize_payload("plain"), "plain");
    assert_eq!(normalize_payload("\r\n\r\n"), "\n\n");
}

#[tokio::test]
async fn ask_kickoff_completes_on_reply_with_body() {
    let h = harness(&["builder"], InjectMode::Auto);
    let mut events = h.bus.subscribe();
    let ask = kickoff(&h, "builder", MessageKind::Ask).await;
    let watcher = tokio::spawn(watch_completion(inputs(&h, NO_DEADLINE), ask.clone()));
    tokio::task::yield_now().await;
    h.router
        .reply(
            Origin::Agent(agent("builder")),
            &ask.id,
            0,
            "done".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(
        watcher.await.unwrap(),
        Completion::Replied {
            code: 0,
            reply: "done".to_string(),
            output: None,
        }
    );
    assert_eq!(completed_result(&mut events), CompletionResult::Replied);
}

#[tokio::test]
async fn send_kickoff_does_not_quiesce_before_working() {
    // The arming guard (spec triggers §2). Everything is idle and the queues are
    // empty from the instant the message is created, so a watcher that starts
    // its quiescence phase before the kickoff is observed at `working` reports
    // success for a message the agent never saw.
    let h = harness(&["builder"], InjectMode::Hold);
    let send = kickoff(&h, "builder", MessageKind::Send).await;
    let watcher = watch_completion(inputs(&h, NO_DEADLINE), send);
    let outcome = tokio::time::timeout(DWELL * 5, watcher).await;
    assert!(
        outcome.is_err(),
        "watcher resolved {outcome:?} while the kickoff was still queued"
    );
}

#[tokio::test]
async fn send_kickoff_quiesces_after_working_then_all_idle_dwell() {
    let h = harness(&["builder"], InjectMode::Auto);
    let mut events = h.bus.subscribe();
    let send = arm_send(&h, "builder").await;
    let watcher = watch_completion(inputs(&h, NO_DEADLINE), send);
    assert_eq!(
        tokio::time::timeout(DWELL * 4, watcher).await.unwrap(),
        Completion::Quiesced
    );
    assert_eq!(completed_result(&mut events), CompletionResult::Quiesced);
}

#[tokio::test]
async fn a_state_round_trip_inside_the_dwell_resets_quiescence() {
    // The watcher is spawned, so it genuinely reaches the dwell before anything
    // moves. The flip goes working and straight back to idle, so the state
    // VALUE ends where it started: only the version counter records that the
    // agent moved, which is what forces the re-verify to use `has_changed`
    // rather than comparing values.
    let h = harness(&["builder"], InjectMode::Auto);
    let send = arm_send(&h, "builder").await;
    let watcher = tokio::spawn(watch_completion(inputs(&h, NO_DEADLINE), send));
    tokio::time::sleep(DWELL / 2).await;
    h.pty.set("builder", AgentState::Working);
    h.pty.set("builder", AgentState::Idle);
    // One dwell after the flip. The dwell the watcher was already in has now
    // expired, so a watcher that failed to discard it would have resolved.
    tokio::time::sleep(DWELL).await;
    assert!(
        !watcher.is_finished(),
        "the dwell that the agent moved inside of must be discarded, not honoured"
    );
    assert_eq!(
        tokio::time::timeout(DWELL * 8, watcher)
            .await
            .unwrap()
            .unwrap(),
        Completion::Quiesced,
        "a full clean dwell after the flip settles it"
    );
}

#[tokio::test]
async fn an_injection_queued_inside_the_dwell_resets_quiescence() {
    // Queue depth is polled, not watched, so this movement leaves every state
    // channel untouched: only re-running the whole predicate after the dwell
    // catches it.
    let h = harness(&["builder", "docs"], InjectMode::Auto);
    let send = arm_send(&h, "builder").await;
    let depth = h.pty.depth("docs");
    let watcher = tokio::spawn(watch_completion(inputs(&h, NO_DEADLINE), send));
    tokio::time::sleep(DWELL / 2).await;
    depth.store(1, Ordering::SeqCst);
    tokio::time::sleep(DWELL).await;
    assert!(
        !watcher.is_finished(),
        "an injection queued mid-dwell must not be settled over"
    );
    depth.store(0, Ordering::SeqCst);
    assert_eq!(
        tokio::time::timeout(DWELL * 8, watcher)
            .await
            .unwrap()
            .unwrap(),
        Completion::Quiesced
    );
}

#[tokio::test]
async fn a_working_agent_during_the_dwell_resets_quiescence() {
    // The value-visible case: the agent is still working when the dwell ends.
    let h = harness(&["builder"], InjectMode::Auto);
    let send = arm_send(&h, "builder").await;
    let watcher = tokio::spawn(watch_completion(inputs(&h, NO_DEADLINE), send));
    tokio::time::sleep(DWELL / 2).await;
    h.pty.set("builder", AgentState::Working);
    tokio::time::sleep(DWELL).await;
    assert!(!watcher.is_finished(), "a working agent is not quiescent");
    h.pty.set("builder", AgentState::Idle);
    assert_eq!(
        tokio::time::timeout(DWELL * 8, watcher)
            .await
            .unwrap()
            .unwrap(),
        Completion::Quiesced
    );
}

#[tokio::test]
async fn queued_injection_blocks_quiescence() {
    // Idle with a pending injection is mid-hand-off, not settled.
    let h = harness(&["builder", "docs"], InjectMode::Auto);
    let send = arm_send(&h, "builder").await;
    let depth = h.pty.depth("docs");
    depth.store(1, Ordering::SeqCst);
    let mut watcher = Box::pin(watch_completion(inputs(&h, NO_DEADLINE), send));
    assert!(
        tokio::time::timeout(DWELL * 4, &mut watcher).await.is_err(),
        "a non-empty injection queue is not quiescent"
    );
    depth.store(0, Ordering::SeqCst);
    assert_eq!(
        tokio::time::timeout(DWELL * 4, &mut watcher).await.unwrap(),
        Completion::Quiesced
    );
}

#[tokio::test]
async fn downstream_idle_while_upstream_works_is_not_quiescent() {
    let h = harness(&["builder", "docs"], InjectMode::Auto);
    let send = arm_send(&h, "builder").await;
    h.pty.set("builder", AgentState::Working);
    let mut watcher = Box::pin(watch_completion(inputs(&h, NO_DEADLINE), send));
    assert!(
        tokio::time::timeout(DWELL * 4, &mut watcher).await.is_err(),
        "docs being idle says nothing while builder still works"
    );
    h.pty.set("builder", AgentState::Idle);
    assert_eq!(
        tokio::time::timeout(DWELL * 4, &mut watcher).await.unwrap(),
        Completion::Quiesced
    );
}

#[tokio::test]
async fn agent_exit_fails_the_kickoff_fast() {
    let h = harness(&["builder", "docs"], InjectMode::Auto);
    let mut events = h.bus.subscribe();
    let send = arm_send(&h, "builder").await;
    let watcher = tokio::spawn(watch_completion(inputs(&h, NO_DEADLINE), send));
    tokio::task::yield_now().await;
    h.pty.set("docs", AgentState::Exited);
    h.bus.publish(EventPayload::AgentLifecycle {
        agent: agent("docs"),
        phase: LifecyclePhase::Exited,
        exit: Some(AgentExit::Code(1)),
    });
    let outcome = tokio::time::timeout(DWELL * 4, watcher)
        .await
        .unwrap()
        .unwrap();
    match outcome {
        Completion::Failed {
            reason,
            reason_code,
        } => {
            assert!(
                reason.contains("docs"),
                "the failure must name the agent that exited, got: {reason}"
            );
            assert_eq!(reason_code, "agent_exited");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(completed_result(&mut events), CompletionResult::Failed);
}

#[tokio::test]
async fn deadline_yields_timeout_even_when_never_injected() {
    // The deadline runs from watcher start, not from injection: a kickoff that
    // never lands still terminates.
    let h = harness(&["builder"], InjectMode::Hold);
    let mut events = h.bus.subscribe();
    let send = kickoff(&h, "builder", MessageKind::Send).await;
    let outcome = watch_completion(inputs(&h, Duration::from_millis(200)), send).await;
    assert_eq!(outcome, Completion::Timeout);
    assert_eq!(completed_result(&mut events), CompletionResult::Timeout);
}

#[tokio::test]
async fn ask_kickoff_that_fails_is_not_a_reply() {
    let h = harness(&["builder"], InjectMode::Auto);
    let mut events = h.bus.subscribe();
    let ask = kickoff(&h, "builder", MessageKind::Ask).await;
    let watcher = tokio::spawn(watch_completion(inputs(&h, NO_DEADLINE), ask.clone()));
    tokio::task::yield_now().await;
    h.router.on_agent_restarted(&agent("builder")).await;
    let outcome = tokio::time::timeout(DWELL * 4, watcher)
        .await
        .unwrap()
        .unwrap();
    match outcome {
        Completion::Failed {
            reason,
            reason_code,
        } => {
            assert!(
                reason.contains("restarted"),
                "the reason must name why the record failed, got: {reason}"
            );
            assert_eq!(reason_code, "agent_restarted");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(completed_result(&mut events), CompletionResult::Failed);
}

/// A code-0 reply that matches the schema reaches the caller as a parsed value,
/// not just the raw text the agent typed.
///
/// The reply is fenced: the router accepts it (repair strips the fence before
/// validating) and stores it verbatim, so `output` can only be right if it came
/// from the boundary's own repair-and-parse pass rather than from `reply`.
#[tokio::test]
async fn replied_valid_carries_parsed_output() {
    const FENCED: &str = "```json\n{\"name\":\"x\"}\n```";
    let h = harness_with(&["builder"], InjectMode::Auto, Some(contract(2)));
    let ask = kickoff(&h, "builder", MessageKind::Ask).await;
    let watcher = tokio::spawn(watch_completion(inputs(&h, NO_DEADLINE), ask.clone()));
    tokio::task::yield_now().await;
    h.router
        .reply(
            Origin::Agent(agent("builder")),
            &ask.id,
            0,
            FENCED.to_string(),
        )
        .await
        .unwrap();
    match tokio::time::timeout(DWELL * 4, watcher)
        .await
        .unwrap()
        .unwrap()
    {
        Completion::Replied {
            code,
            reply,
            output,
        } => {
            assert_eq!(code, 0);
            assert_eq!(reply, FENCED, "the raw reply is kept verbatim");
            let output = output.expect("a conforming reply carries its parsed output");
            assert_eq!(output["name"], "x");
        }
        other => panic!("expected Replied, got {other:?}"),
    }
}

/// `max_repairs: 0` spends the router's budget on the first rejection, so the
/// off-schema reply is accepted there and the trigger boundary is what fails it.
#[tokio::test]
async fn replied_invalid_fails_with_schema_reason_code() {
    let h = harness_with(&["builder"], InjectMode::Auto, Some(contract(0)));
    let ask = kickoff(&h, "builder", MessageKind::Ask).await;
    let watcher = tokio::spawn(watch_completion(inputs(&h, NO_DEADLINE), ask.clone()));
    tokio::task::yield_now().await;
    h.router
        .reply(
            Origin::Agent(agent("builder")),
            &ask.id,
            0,
            "{}".to_string(),
        )
        .await
        .unwrap();
    match tokio::time::timeout(DWELL * 4, watcher)
        .await
        .unwrap()
        .unwrap()
    {
        Completion::Failed {
            reason,
            reason_code,
        } => {
            assert_eq!(reason_code, "schema_validation_failed");
            assert!(reason.contains("at (root)"), "reason: {reason}");
            assert!(reason.contains("name"), "reason: {reason}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// A non-zero code is the agent's escape hatch: it is a completion, and the
/// schema never applies to it.
#[tokio::test]
async fn code_1_reply_is_completed_without_output() {
    let h = harness_with(&["builder"], InjectMode::Auto, Some(contract(2)));
    let ask = kickoff(&h, "builder", MessageKind::Ask).await;
    let watcher = tokio::spawn(watch_completion(inputs(&h, NO_DEADLINE), ask.clone()));
    tokio::task::yield_now().await;
    h.router
        .reply(
            Origin::Agent(agent("builder")),
            &ask.id,
            1,
            "no".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(DWELL * 4, watcher)
            .await
            .unwrap()
            .unwrap(),
        Completion::Replied {
            code: 1,
            reply: "no".to_string(),
            output: None,
        }
    );
}

#[tokio::test]
async fn timeout_reason_code() {
    let h = harness(&["builder"], InjectMode::Hold);
    let send = kickoff(&h, "builder", MessageKind::Send).await;
    let outcome = watch_completion(inputs(&h, Duration::from_millis(200)), send).await;
    assert_eq!(outcome, Completion::Timeout);
    let wire = serde_json::to_value(completion_status(outcome)).unwrap();
    assert_eq!(wire["status"], "failed");
    assert_eq!(wire["reason_code"], "timeout");
}

#[tokio::test]
async fn quiescence_ignores_a_non_member_agents_pending_ask() {
    let h = harness(&["member", "outsider"], InjectMode::Auto);
    // The outsider stays busy for the whole test; it is outside the roster,
    // so only an unscoped counter or state check can see it.
    h.pty.set("outsider", AgentState::Working);
    // An ask FROM the outsider TO the outsider that never resolves: asker and
    // target are both outside the roster, so the only thing that can block
    // quiescence is the pending-ask counter being unscoped.
    h.router
        .create_message(
            Origin::Agent(agent("outsider")),
            agent("outsider"),
            MessageKind::Ask,
            "outsider blocks the pool, not the flow".to_string(),
        )
        .await
        .unwrap();
    let kick = arm_send(&h, "member").await;
    h.pty.set("member", AgentState::Idle);
    let mut watch = inputs(&h, Duration::from_secs(10));
    watch.roster = vec![agent("member")];
    let completion = watch_completion(watch, kick).await;
    assert_eq!(
        completion,
        Completion::Quiesced,
        "a member-scoped watcher must not wait on the outsider's ask"
    );
}

#[tokio::test]
async fn quiescence_ignores_a_non_member_agents_open_turn() {
    // outsider has an edge, so an HTTP-origin message arms its turn; the turn
    // stays open (its edge step is never met) while the member quiesces.
    let h = harness_with_edges(
        &["member", "outsider"],
        &[("outsider", "member", EdgeKind::Send)],
    );
    let arming = h
        .router
        .create_message(
            Origin::Http("cafe0123".into()),
            agent("outsider"),
            MessageKind::Send,
            "arm the outsider's turn".to_string(),
        )
        .await
        .unwrap();
    h.pty.set("outsider", AgentState::Working);
    wait_status(&h, &arming.id, MessageStatus::Working).await;
    let kick = arm_send(&h, "member").await;
    h.pty.set("member", AgentState::Idle);
    let mut watch = inputs(&h, Duration::from_secs(10));
    watch.roster = vec![agent("member")];
    assert_eq!(watch_completion(watch, kick).await, Completion::Quiesced);
}
