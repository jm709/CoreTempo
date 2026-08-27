#![expect(clippy::unwrap_used, reason = "tests assert on known-good values")]

//! Per-turn obligation tracking (spec §2): arming, met steps, nudge-once, stall.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use coretempo_core::bus::EventBus;
use coretempo_core::pty::{
    Blocked, ClearGate, Cursor, IdleDecision, InjectError, Injected, InjectionQueue,
};
use coretempo_core::router::{Router, StateSource, WatchdogTiming};
use coretempo_core::store::Store;
use coretempo_core::time::Timestamp;
use coretempo_core::types::agent::AgentState;
use coretempo_core::types::config::{AgentConfig, Edge, EdgeKind, FrozenWorkflow};
use coretempo_core::types::event::Event;
use coretempo_core::types::id::{AgentId, MessageId, RunId};
use coretempo_core::types::message::{MessageKind, MessageRecord, MessageStatus, Origin};
use tokio::sync::{broadcast, oneshot, watch};

static DB_N: AtomicU64 = AtomicU64::new(0);

fn temp_db() -> PathBuf {
    let n = DB_N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "coretempo-obligations-{}-{n}.db",
        std::process::id()
    ))
}

fn agent(id: &str) -> AgentId {
    AgentId(id.to_string())
}

/// Resolves every injection immediately; this suite is about the gate, not the PTY.
#[derive(Default)]
struct MockInjector {
    /// Set by the arming-order test only. While set, every `enqueue` records the
    /// gate's verdict for this agent as of the moment the injection is queued.
    probe: Mutex<Option<AgentId>>,
    router: OnceLock<Weak<Router>>,
    observed: Mutex<Vec<IdleDecision>>,
    /// Every `reconsider` the sweeper sent, in order.
    pokes: Mutex<Vec<AgentId>>,
}

impl MockInjector {
    fn probe(&self, agent: AgentId) {
        *self.probe.lock().unwrap() = Some(agent);
    }

    fn observed(&self) -> Vec<IdleDecision> {
        self.observed.lock().unwrap().clone()
    }

    fn pokes(&self) -> Vec<AgentId> {
        self.pokes.lock().unwrap().clone()
    }
}

impl InjectionQueue for MockInjector {
    fn enqueue(
        &self,
        _target: AgentId,
        _text: String,
    ) -> oneshot::Receiver<Result<Injected, InjectError>> {
        let probed = self.probe.lock().unwrap().clone();
        if let Some(id) = probed
            && let Some(router) = self.router.get().and_then(Weak::upgrade)
        {
            self.observed
                .lock()
                .unwrap()
                .push(router.on_stable_idle(&id));
        }
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(Ok(Injected {
            at: Timestamp::now(),
            cursor: Cursor(0),
        }));
        rx
    }

    fn reconsider(&self, target: &AgentId) {
        self.pokes.lock().unwrap().push(target.clone());
    }
}

#[derive(Default)]
struct FakeStates {
    chans: Mutex<BTreeMap<AgentId, watch::Sender<AgentState>>>,
    blocked: Mutex<BTreeMap<AgentId, Blocked>>,
}

impl FakeStates {
    /// Parks `id` on a permission dialog for `tool`, as the agent's
    /// `PermissionRequest` hook would.
    fn set_blocked(&self, id: &str, tool: Option<&str>) {
        self.blocked.lock().unwrap().insert(
            agent(id),
            Blocked {
                since: tokio::time::Instant::now(),
                tool: tool.map(str::to_string),
                agent_id: None,
            },
        );
    }

    fn clear_blocked(&self, id: &str) {
        self.blocked.lock().unwrap().remove(&agent(id));
    }

    /// Drives the debounced channel the router reads. `send_replace`, not
    /// `send`: a receiver may well have been dropped by now (`drive_message`
    /// stops watching an ask once it reaches `working`).
    fn set_state(&self, id: &str, state: AgentState) {
        if let Some(tx) = self.chans.lock().unwrap().get(&agent(id)) {
            tx.send_replace(state);
        }
    }

    /// What the sweeper and the gate see through `StateSource::blocked_since`.
    fn blocked_since_public(&self, id: &str) -> Option<Blocked> {
        self.blocked.lock().unwrap().get(&agent(id)).cloned()
    }
}

impl StateSource for FakeStates {
    fn subscribe_debounced(&self, agent: &AgentId) -> Option<watch::Receiver<AgentState>> {
        self.chans
            .lock()
            .unwrap()
            .get(agent)
            .map(watch::Sender::subscribe)
    }

    fn blocked_since(&self, agent: &AgentId) -> Option<Blocked> {
        self.blocked.lock().unwrap().get(agent).cloned()
    }
}

struct Harness {
    router: Arc<Router>,
    injector: Arc<MockInjector>,
    bus: EventBus,
    states: Arc<FakeStates>,
}

fn config(edges: Vec<Edge>) -> AgentConfig {
    AgentConfig {
        edges,
        ..AgentConfig::new(PathBuf::from("/tmp"), "test agent")
    }
}

/// `planner` delegates to `builder` (ask) then `notifier` (send); the others
/// have no edges and so can never be armed.
fn workflow() -> Arc<FrozenWorkflow> {
    workflow_with_timeout(Duration::from_mins(30))
}

fn workflow_with_timeout(ask_timeout: Duration) -> Arc<FrozenWorkflow> {
    let mut agents = BTreeMap::new();
    agents.insert(
        agent("planner"),
        config(vec![
            Edge {
                to: agent("builder"),
                kind: EdgeKind::Ask,
                max_rounds: None,
            },
            Edge {
                to: agent("notifier"),
                kind: EdgeKind::Send,
                max_rounds: None,
            },
        ]),
    );
    agents.insert(agent("builder"), config(Vec::new()));
    agents.insert(agent("notifier"), config(Vec::new()));
    // Upstream of planner: watcher → planner → {builder, notifier} is the chain.
    agents.insert(
        agent("watcher"),
        config(vec![Edge {
            to: agent("planner"),
            kind: EdgeKind::Send,
            max_rounds: None,
        }]),
    );
    Arc::new(FrozenWorkflow {
        name: "test".to_string(),
        hash: "0".repeat(64),
        source_path: PathBuf::from("tempo.toml"),
        ask_timeout,
        idle_debounce: Duration::from_secs(2),
        scrollback: 5_000,
        agents,
        mcp_servers: BTreeMap::new(),
        flows: BTreeMap::new(),
    })
}

fn harness() -> Harness {
    build_harness(workflow())
}

fn build_harness(workflow: Arc<FrozenWorkflow>) -> Harness {
    let store = Store::open(&temp_db(), RunId("r-11111111".to_string())).unwrap();
    let bus = EventBus::new();
    let states = Arc::new(FakeStates::default());
    for id in workflow.agents.keys() {
        let (tx, _rx) = watch::channel(AgentState::Idle);
        states.chans.lock().unwrap().insert(id.clone(), tx);
    }
    let injector = Arc::new(MockInjector::default());
    let router = Router::new(store, bus.clone(), injector.clone(), workflow);
    router.set_state_source(states.clone());
    let _ = injector.router.set(Arc::downgrade(&router));
    Harness {
        router,
        injector,
        bus,
        states,
    }
}

/// Millisecond-scale twin of the production backoff/grace, so the timing tests
/// finish in well under a second.
fn fast_timing() -> WatchdogTiming {
    WatchdogTiming {
        reply_nudge_backoff: [
            Duration::from_millis(40),
            Duration::from_millis(80),
            Duration::from_millis(160),
            Duration::from_millis(160),
        ],
        blocked_grace: Duration::from_millis(60),
    }
}

fn decision(router: &Router, id: &str) -> IdleDecision {
    router.on_stable_idle(&agent(id))
}

/// Arms `planner`: an incoming user message opens its obligation turn. A *send*
/// on purpose — an incoming ask also leaves planner owing a reply, which the
/// gate now answers before it looks at edge steps at all
/// (`owed_reply_check_precedes_turn_logic` covers that interaction).
async fn arm_planner(h: &Harness) {
    h.router
        .create_message(
            Origin::User,
            agent("planner"),
            MessageKind::Send,
            "go".to_string(),
        )
        .await
        .unwrap();
}

/// The webhook kickoff shape: an HTTP-origin ask increments nobody's outgoing
/// `pending_asks`, so only the owed-reply set can hold the gate off `/clear`.
async fn http_ask(h: &Harness, to: &str) -> MessageRecord {
    h.router
        .create_message(
            Origin::Http("1f2e3d4c".to_string()),
            agent(to),
            MessageKind::Ask,
            "produce the report".to_string(),
        )
        .await
        .unwrap()
}

/// Waits out the TTL sweeper (1s interval) for `id` to expire, returning the
/// last status seen so the caller reports what it got instead.
async fn await_failed(h: &Harness, id: &MessageId) -> MessageStatus {
    await_status(h, id, MessageStatus::Failed).await
}

/// Polls `id` for up to 4 s (the sweeper ticks every second), returning the
/// last status seen so a failing assertion names what it got instead.
async fn await_status(h: &Harness, id: &MessageId, want: MessageStatus) -> MessageStatus {
    let mut status = MessageStatus::Queued;
    for _ in 0..40 {
        status = h.router.get_message(id).await.unwrap().status;
        if status == want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    status
}

async fn emit_ask(h: &Harness, from: &str, to: &str) -> MessageRecord {
    h.router
        .create_message(
            Origin::Agent(agent(from)),
            agent(to),
            MessageKind::Ask,
            "do it".to_string(),
        )
        .await
        .unwrap()
}

async fn emit_send(h: &Harness, from: &str, to: &str) -> MessageRecord {
    h.router
        .create_message(
            Origin::Agent(agent(from)),
            agent(to),
            MessageKind::Send,
            "fyi".to_string(),
        )
        .await
        .unwrap()
}

/// Drains everything currently broadcast, looking for one wire `type`.
async fn saw_event(events: &mut broadcast::Receiver<Event>, wire: &str) -> bool {
    tokio::task::yield_now().await;
    let mut found = false;
    while let Ok(event) = events.try_recv() {
        let value = serde_json::to_value(&event.payload).unwrap();
        if value.get("type").and_then(serde_json::Value::as_str) == Some(wire) {
            found = true;
        }
    }
    found
}

#[tokio::test]
async fn unarmed_agent_with_edges_allows_clear() {
    // No turn open: edges alone impose nothing (spec §2 arming rule).
    let h = harness();
    assert_eq!(decision(&h.router, "planner"), IdleDecision::AllowClear);
}

#[tokio::test]
async fn turn_is_not_armed_until_the_injection_is_queued() {
    // The gate runs on the queue worker's task and can be consulted while
    // create_message is awaiting the SQLite insert. Arming before the message
    // is queued leaves a window where an open turn coexists with an empty
    // queue: the worker drains nothing, sees unmet steps and nudges for a
    // message the agent was never given — burning the one-per-turn budget so
    // the real follow-up only gets stalled. Spec §2: a turn opens when the
    // agent RECEIVES an injected ask or send.
    let h = harness();
    h.injector.probe(agent("planner"));
    arm_planner(&h).await;
    assert_eq!(
        h.injector.observed(),
        vec![IdleDecision::AllowClear],
        "planner must not look armed until its injection is queued"
    );
    // Queued now, so the turn is open and the gate demands the steps.
    assert!(matches!(
        decision(&h.router, "planner"),
        IdleDecision::Nudge(_)
    ));
}

#[tokio::test]
async fn armed_turn_with_nothing_emitted_nudges_once_then_stalls() {
    let h = harness();
    let mut events = h.bus.subscribe();
    arm_planner(&h).await;
    // The incoming send armed the turn and planner has emitted nothing, so its
    // outgoing ask count is 0 and the gate reaches the edge-step logic.
    match decision(&h.router, "planner") {
        IdleDecision::Nudge(text) => {
            assert!(text.contains("tempo ask builder"));
            assert!(text.contains("tempo send notifier"));
        }
        other => panic!("expected nudge, got {other:?}"),
    }
    assert!(saw_event(&mut events, "agent.nudged").await);
    // Second idle, still nothing emitted: hold quiet + stalled, exactly once.
    assert_eq!(decision(&h.router, "planner"), IdleDecision::HoldQuiet);
    assert!(saw_event(&mut events, "agent.stalled").await);
    assert_eq!(decision(&h.router, "planner"), IdleDecision::HoldQuiet);
    assert!(
        !saw_event(&mut events, "agent.stalled").await,
        "stalled fires once per turn"
    );
}

#[tokio::test]
async fn in_flight_obligation_ask_holds_quiet_without_nudge() {
    let h = harness();
    arm_planner(&h).await;
    emit_ask(&h, "planner", "builder").await;
    // Ask emitted but unanswered: pending_asks(planner) == 1 → in progress.
    assert_eq!(decision(&h.router, "planner"), IdleDecision::HoldQuiet);
}

#[tokio::test]
async fn after_reply_the_remaining_step_is_nudged_by_name() {
    let h = harness();
    arm_planner(&h).await;
    let ask = emit_ask(&h, "planner", "builder").await;
    h.router
        .reply(
            Origin::Agent(agent("builder")),
            &ask.id,
            0,
            "done".to_string(),
        )
        .await
        .unwrap();
    match decision(&h.router, "planner") {
        IdleDecision::Nudge(text) => {
            assert!(text.contains("tempo send notifier"));
            assert!(
                !text.contains("tempo ask builder"),
                "met steps are not renudged"
            );
        }
        other => panic!("expected nudge, got {other:?}"),
    }
}

#[tokio::test]
async fn all_steps_met_closes_the_turn_and_allows_clear() {
    let h = harness();
    arm_planner(&h).await;
    let ask = emit_ask(&h, "planner", "builder").await;
    h.router
        .reply(
            Origin::Agent(agent("builder")),
            &ask.id,
            0,
            "ok".to_string(),
        )
        .await
        .unwrap();
    emit_send(&h, "planner", "notifier").await;
    assert_eq!(decision(&h.router, "planner"), IdleDecision::AllowClear);
    // Turn closed: the decision is stable, and a late look sees no turn at all.
    assert_eq!(decision(&h.router, "planner"), IdleDecision::AllowClear);
}

#[tokio::test]
async fn reply_after_turn_close_does_not_rearm() {
    // Loop-prevention (spec §2): replies never open a turn. The reply must land
    // when NO turn is open, or the property is untested — a reply arriving into
    // an already-open turn is indistinguishable from one that arms nothing.
    let h = harness();
    // Open planner's turn and satisfy both steps.
    arm_planner(&h).await;
    emit_send(&h, "planner", "notifier").await;
    let first = emit_ask(&h, "planner", "builder").await;
    h.router
        .reply(
            Origin::Agent(agent("builder")),
            &first.id,
            0,
            "ok".to_string(),
        )
        .await
        .unwrap();
    // This consult closes the turn and drops it.
    assert_eq!(decision(&h.router, "planner"), IdleDecision::AllowClear);
    // With no turn open, a fresh ask/reply round trip must leave it that way:
    // the reply injected into planner is not a new arming message.
    let second = emit_ask(&h, "planner", "builder").await;
    h.router
        .reply(
            Origin::Agent(agent("builder")),
            &second.id,
            0,
            "ok again".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(
        decision(&h.router, "planner"),
        IdleDecision::AllowClear,
        "the injected reply armed nothing"
    );
    assert_eq!(decision(&h.router, "planner"), IdleDecision::AllowClear);
}

#[tokio::test]
async fn merge_arming_resets_the_nudge_budget_and_keeps_met_steps() {
    let h = harness();
    arm_planner(&h).await;
    emit_send(&h, "planner", "notifier").await; // one step met
    assert!(matches!(
        decision(&h.router, "planner"),
        IdleDecision::Nudge(_)
    ));
    assert_eq!(
        decision(&h.router, "planner"),
        IdleDecision::HoldQuiet,
        "budget spent"
    );
    arm_planner(&h).await; // second incoming message merges into the open turn
    match decision(&h.router, "planner") {
        IdleDecision::Nudge(text) => {
            assert!(text.contains("tempo ask builder"));
            assert!(!text.contains("notifier"), "met set carried over the merge");
        }
        other => panic!("expected nudge after merge-arm, got {other:?}"),
    }
}

#[tokio::test]
async fn downstream_feedback_does_not_arm() {
    // Edge-semantics spec (2026-08-05): builder and notifier are planner's
    // delegates; their messages back are feedback on delegated work and must
    // not re-obligate the delegation.
    let h = harness();
    emit_send(&h, "builder", "planner").await;
    assert_eq!(decision(&h.router, "planner"), IdleDecision::AllowClear);
    // An ask from a delegate is owed a reply like any other, so answer it: what
    // must not happen is planner's own edge steps coming back into force.
    let ask = emit_ask(&h, "notifier", "planner").await;
    h.router
        .reply(
            Origin::Agent(agent("planner")),
            &ask.id,
            0,
            "noted".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(decision(&h.router, "planner"), IdleDecision::AllowClear);
}

#[tokio::test]
async fn upstream_send_still_arms_the_chain() {
    // watcher → planner → {builder, notifier}: the chain must keep propagating —
    // planner's obligations arm when its upstream's work arrives.
    let h = harness();
    emit_send(&h, "watcher", "planner").await;
    assert!(matches!(
        decision(&h.router, "planner"),
        IdleDecision::Nudge(_)
    ));
}

#[tokio::test]
async fn restart_disarms() {
    let h = harness();
    arm_planner(&h).await;
    h.router.on_agent_restarted(&agent("planner")).await;
    assert_eq!(decision(&h.router, "planner"), IdleDecision::AllowClear);
}

/// `owner` loops `worker` with a 2-round cap; `kicker` is upstream of owner.
fn loop_harness() -> Harness {
    let mut agents = BTreeMap::new();
    agents.insert(
        agent("owner"),
        config(vec![Edge {
            to: agent("worker"),
            kind: EdgeKind::Loop,
            max_rounds: Some(2),
        }]),
    );
    agents.insert(agent("worker"), config(Vec::new()));
    agents.insert(
        agent("kicker"),
        config(vec![Edge {
            to: agent("owner"),
            kind: EdgeKind::Send,
            max_rounds: None,
        }]),
    );
    build_harness(Arc::new(FrozenWorkflow {
        name: "loop-test".to_string(),
        hash: "0".repeat(64),
        source_path: PathBuf::from("tempo.toml"),
        ask_timeout: Duration::from_mins(30),
        idle_debounce: Duration::from_secs(2),
        scrollback: 5_000,
        agents,
        mcp_servers: BTreeMap::new(),
        flows: BTreeMap::new(),
    }))
}

/// One loop round: owner asks worker, worker replies code 0.
async fn round(h: &Harness) {
    let ask = emit_ask(h, "owner", "worker").await;
    h.router
        .reply(
            Origin::Agent(agent("worker")),
            &ask.id,
            0,
            "round done".to_string(),
        )
        .await
        .unwrap();
}

async fn arm_owner(h: &Harness) {
    h.router
        .create_message(
            Origin::User,
            agent("owner"),
            MessageKind::Send,
            "start looping".to_string(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn loop_reply_rearms_until_done() {
    let h = loop_harness();
    arm_owner(&h).await;
    // Armed, no round yet: the nudge names both continue and done.
    match decision(&h.router, "owner") {
        IdleDecision::Nudge(text) => {
            assert!(text.contains("tempo ask worker"));
            assert!(text.contains("tempo done worker"));
        }
        other => panic!("expected loop nudge, got {other:?}"),
    }
    let ask = emit_ask(&h, "owner", "worker").await;
    // Round in flight: pending ask holds quiet.
    assert_eq!(decision(&h.router, "owner"), IdleDecision::HoldQuiet);
    h.router
        .reply(
            Origin::Agent(agent("worker")),
            &ask.id,
            0,
            "round 1 done".to_string(),
        )
        .await
        .unwrap();
    // The reply re-armed the loop: idling without action nudges again.
    assert!(matches!(
        decision(&h.router, "owner"),
        IdleDecision::Nudge(_)
    ));
    h.router
        .mark_loop_done(&agent("owner"), &agent("worker"))
        .unwrap();
    assert_eq!(decision(&h.router, "owner"), IdleDecision::AllowClear);
    // After done, another full round's reply must NOT re-arm.
    round(&h).await;
    assert_eq!(decision(&h.router, "owner"), IdleDecision::AllowClear);
}

#[tokio::test]
async fn new_arming_turn_restarts_a_done_loop() {
    let h = loop_harness();
    arm_owner(&h).await;
    round(&h).await;
    h.router
        .mark_loop_done(&agent("owner"), &agent("worker"))
        .unwrap();
    assert_eq!(decision(&h.router, "owner"), IdleDecision::AllowClear);
    // A fresh upstream kickoff clears loops_done and the round counter.
    emit_send(&h, "kicker", "owner").await;
    assert!(matches!(
        decision(&h.router, "owner"),
        IdleDecision::Nudge(_)
    ));
    // Rounds count from zero again: two more rounds run before the cap.
    round(&h).await;
    assert!(matches!(
        decision(&h.router, "owner"),
        IdleDecision::Nudge(_)
    ));
}

#[tokio::test]
async fn round_cap_stops_rearming_with_one_cap_nudge() {
    let h = loop_harness();
    arm_owner(&h).await;
    round(&h).await; // round 1: reply re-arms
    round(&h).await; // round 2: cap reached — reply must not re-arm a round
    // One cap nudge naming tempo done, then quiesce.
    match decision(&h.router, "owner") {
        IdleDecision::Nudge(text) => {
            assert!(text.contains("cap"), "cap nudge names the cap: {text}");
            assert!(text.contains('2'), "cap nudge names the limit: {text}");
            assert!(text.contains("tempo done worker"));
        }
        other => panic!("expected cap nudge, got {other:?}"),
    }
    assert_eq!(decision(&h.router, "owner"), IdleDecision::AllowClear);
    // Late replies after the cap change nothing.
    assert_eq!(decision(&h.router, "owner"), IdleDecision::AllowClear);
}

#[tokio::test]
async fn done_racing_a_reply_does_not_rearm() {
    let h = loop_harness();
    arm_owner(&h).await;
    let ask = emit_ask(&h, "owner", "worker").await;
    // tempo done lands while the round's reply is still in flight.
    h.router
        .mark_loop_done(&agent("owner"), &agent("worker"))
        .unwrap();
    h.router
        .reply(
            Origin::Agent(agent("worker")),
            &ask.id,
            0,
            "late".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(decision(&h.router, "owner"), IdleDecision::AllowClear);
}

#[tokio::test]
async fn mark_loop_done_requires_a_loop_edge() {
    let h = loop_harness();
    let err = h
        .router
        .mark_loop_done(&agent("worker"), &agent("owner"))
        .unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("worker") && text.contains("loop"),
        "error names the caller and the missing loop edge: {text}"
    );
}

#[tokio::test]
async fn idle_with_owed_reply_nudges_then_stalls_inside_the_backoff() {
    // An agent that went idle without answering an ask addressed to it is not
    // clearable: `/clear` would throw away the context the asker is blocked on.
    // `builder` has no edges, so nothing but the owed reply can hold the gate.
    // The default 60 s backoff covers every idle below: one nudge, then one
    // stall, then silence until the backoff elapses.
    let h = harness();
    let mut events = h.bus.subscribe();
    let ask = http_ask(&h, "builder").await;
    match decision(&h.router, "builder") {
        IdleDecision::Nudge(text) => {
            assert!(text.contains(&ask.id.0), "nudge names the ask: {text}");
            assert!(
                text.contains("tempo reply"),
                "nudge names the command: {text}"
            );
        }
        other => panic!("expected owed-reply nudge, got {other:?}"),
    }
    assert!(saw_event(&mut events, "agent.nudged").await);
    // Budget spent: hold quiet rather than clear, and stall exactly once.
    assert_eq!(decision(&h.router, "builder"), IdleDecision::HoldQuiet);
    assert!(saw_event(&mut events, "agent.stalled").await);
    assert_eq!(decision(&h.router, "builder"), IdleDecision::HoldQuiet);
    assert!(
        !saw_event(&mut events, "agent.stalled").await,
        "stalled fires once per nudge round"
    );
}

#[tokio::test]
async fn owed_reply_is_renudged_on_a_backoff_and_stalls_each_round() {
    let h = harness();
    h.router.set_watchdog_timing(fast_timing());
    let mut events = h.bus.subscribe();
    let ask = http_ask(&h, "builder").await;
    // Round 1: nudge now.
    assert!(matches!(
        decision(&h.router, "builder"),
        IdleDecision::Nudge(_)
    ));
    assert!(saw_event(&mut events, "agent.nudged").await);
    // Idle again inside the backoff: stalled once, then quiet.
    assert_eq!(decision(&h.router, "builder"), IdleDecision::HoldQuiet);
    assert!(saw_event(&mut events, "agent.stalled").await);
    assert_eq!(decision(&h.router, "builder"), IdleDecision::HoldQuiet);
    assert!(!saw_event(&mut events, "agent.stalled").await);
    // Backoff elapsed: round 2, with the subagent hint, and stalled re-arms.
    tokio::time::sleep(Duration::from_millis(50)).await;
    match decision(&h.router, "builder") {
        IdleDecision::Nudge(text) => {
            assert!(text.contains(&ask.id.0));
            assert!(
                text.contains("background subagents"),
                "round 2 hint: {text}"
            );
        }
        other => panic!("expected round-2 nudge, got {other:?}"),
    }
    assert!(saw_event(&mut events, "agent.nudged").await);
    assert_eq!(decision(&h.router, "builder"), IdleDecision::HoldQuiet);
    assert!(
        saw_event(&mut events, "agent.stalled").await,
        "stalled fires per round"
    );
    // Round 3 needs the longer backoff: 50 ms is not enough, 90 ms is.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(decision(&h.router, "builder"), IdleDecision::HoldQuiet);
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(matches!(
        decision(&h.router, "builder"),
        IdleDecision::Nudge(_)
    ));
}

#[tokio::test]
async fn owed_reply_holds_quiet_while_the_agent_is_blocked() {
    let h = harness();
    h.router.set_watchdog_timing(fast_timing());
    let mut events = h.bus.subscribe();
    let _ask = http_ask(&h, "builder").await;
    h.states.set_blocked("builder", Some("Bash"));
    assert_eq!(decision(&h.router, "builder"), IdleDecision::HoldQuiet);
    assert!(
        !saw_event(&mut events, "agent.nudged").await,
        "never type into a dialog"
    );
    h.states.clear_blocked("builder");
    assert!(matches!(
        decision(&h.router, "builder"),
        IdleDecision::Nudge(_)
    ));
}

#[tokio::test]
async fn owed_reply_is_not_reported_until_the_injection_is_queued() {
    // The same ordering rule the obligation turn obeys, for the same reason:
    // the gate runs on the queue worker and must not report an obligation for
    // a message the agent has not been handed. Recording the owed reply before
    // the enqueue spends the one-shot nudge budget on an ask that was never
    // delivered, so the real idle that follows has nothing left but a stall.
    // `builder` has no edges, so the owed reply is the only thing the gate
    // could report here.
    let h = harness();
    h.injector.probe(agent("builder"));
    let ask = http_ask(&h, "builder").await;
    assert_eq!(
        h.injector.observed(),
        vec![IdleDecision::AllowClear],
        "builder must not owe a reply until its injection is queued"
    );
    // Queued now, so the obligation is live and the budget is intact.
    match decision(&h.router, "builder") {
        IdleDecision::Nudge(text) => assert!(text.contains(&ask.id.0)),
        other => panic!("expected owed-reply nudge, got {other:?}"),
    }
}

#[tokio::test]
async fn reply_clears_the_owed_state() {
    let h = harness();
    let ask = http_ask(&h, "builder").await;
    assert!(matches!(
        decision(&h.router, "builder"),
        IdleDecision::Nudge(_)
    ));
    h.router
        .reply(
            Origin::Agent(agent("builder")),
            &ask.id,
            0,
            "here it is".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(decision(&h.router, "builder"), IdleDecision::AllowClear);
}

#[tokio::test]
async fn ttl_failure_clears_the_owed_state() {
    // A dead ask owes nothing: the TTL sweeper's `failed` transition must free
    // the agent, or an expired ask would wedge auto-clear for the whole run.
    let h = build_harness(workflow_with_timeout(Duration::from_millis(1)));
    let ask = http_ask(&h, "builder").await;
    assert_eq!(
        await_failed(&h, &ask.id).await,
        MessageStatus::Failed,
        "the TTL sweeper must expire the ask"
    );
    assert_eq!(decision(&h.router, "builder"), IdleDecision::AllowClear);
}

/// The record says why (spec 2026-08-17 §4.3): a TTL expiry writes the
/// `timeout` code and a reason naming the ask and the knob that governs it.
#[tokio::test]
async fn ttl_failure_records_a_timeout_reason() {
    let h = build_harness(workflow_with_timeout(Duration::from_millis(1)));
    let ask = http_ask(&h, "builder").await;
    assert_eq!(await_failed(&h, &ask.id).await, MessageStatus::Failed);
    let rec = h.router.get_message(&ask.id).await.unwrap();
    assert_eq!(rec.reason_code.as_deref(), Some("timeout"));
    let reason = rec.reason.unwrap_or_default();
    assert!(reason.contains(&ask.id.0), "reason names the ask: {reason}");
    assert!(
        reason.contains("ask_timeout"),
        "reason names the knob: {reason}"
    );
}

#[tokio::test]
async fn restart_clears_owed_nudge_state() {
    let h = harness();
    let first = http_ask(&h, "builder").await;
    assert!(matches!(
        decision(&h.router, "builder"),
        IdleDecision::Nudge(_)
    ));
    // The restart sweep fails messages TO builder, so nothing is owed after it.
    h.router.on_agent_restarted(&agent("builder")).await;
    assert_eq!(
        h.router.get_message(&first.id).await.unwrap().status,
        MessageStatus::Failed
    );
    assert_eq!(decision(&h.router, "builder"), IdleDecision::AllowClear);
    // And the spent nudge budget went with it: a fresh ask gets a fresh nudge,
    // not an immediate stall inherited from the dead session.
    let second = http_ask(&h, "builder").await;
    match decision(&h.router, "builder") {
        IdleDecision::Nudge(text) => assert!(text.contains(&second.id.0)),
        other => panic!("restart must reset the owed-nudge budget, got {other:?}"),
    }
}

#[tokio::test]
async fn owed_reply_check_precedes_turn_logic() {
    // Both obligations are open at once. The owed reply wins: someone is
    // blocked on this agent, and the edge steps are still there afterwards.
    let h = harness();
    let ask = h
        .router
        .create_message(
            Origin::User,
            agent("planner"),
            MessageKind::Ask,
            "go".to_string(),
        )
        .await
        .unwrap();
    match decision(&h.router, "planner") {
        IdleDecision::Nudge(text) => {
            assert!(
                text.contains("tempo reply"),
                "owed reply comes first: {text}"
            );
            assert!(
                !text.contains("tempo ask builder"),
                "edge steps wait their turn: {text}"
            );
        }
        other => panic!("expected owed-reply nudge, got {other:?}"),
    }
    // The ask armed the turn all the same: once answered, the steps are nudged.
    h.router
        .reply(
            Origin::Agent(agent("planner")),
            &ask.id,
            0,
            "starting".to_string(),
        )
        .await
        .unwrap();
    match decision(&h.router, "planner") {
        IdleDecision::Nudge(text) => {
            assert!(text.contains("tempo ask builder"));
            assert!(text.contains("tempo send notifier"));
        }
        other => panic!("expected edge-steps nudge, got {other:?}"),
    }
}

#[tokio::test]
async fn open_turns_counts_armed_agents_and_totals_sum_asks() {
    let h = harness();
    let roster = [agent("builder"), agent("planner")];
    assert_eq!(h.router.open_turns_among(&roster), 0);
    assert_eq!(h.router.total_pending_asks_among(&roster), 0);
    arm_planner(&h).await;
    assert_eq!(h.router.open_turns_among(&roster), 1);
    let _ask = emit_ask(&h, "planner", "builder").await;
    assert_eq!(h.router.total_pending_asks_among(&roster), 1);
}

/// Spec §4.2: an agent parked on a permission dialog past the grace has every
/// owed ask failed, with the tool and the fix in the reason — and is itself
/// left exactly as it was.
#[tokio::test]
async fn blocked_past_the_grace_fails_every_owed_ask_naming_the_tool() {
    let h = harness();
    // A grace no single tick can outrun first, so the "still working"
    // assertion below is about the grace and not about the sweeper's cadence.
    let patient = WatchdogTiming {
        blocked_grace: Duration::from_secs(30),
        ..fast_timing()
    };
    h.router.set_watchdog_timing(patient);
    let first = http_ask(&h, "builder").await;
    let second = http_ask(&h, "builder").await;
    // The shape this fires in: the agent took the asks, went working, and
    // parked on a dialog it will never leave on its own.
    h.states.set_state("builder", AgentState::Working);
    assert_eq!(
        await_status(&h, &first.id, MessageStatus::Working).await,
        MessageStatus::Working
    );
    h.states.set_blocked("builder", Some("Bash(python3 …)"));
    tokio::time::sleep(Duration::from_millis(1100)).await; // a tick, inside the grace
    assert_eq!(
        h.router.get_message(&first.id).await.unwrap().status,
        MessageStatus::Working,
        "inside the grace the ask is left alone"
    );
    h.router.set_watchdog_timing(fast_timing()); // grace 60 ms: long spent
    assert_eq!(await_failed(&h, &first.id).await, MessageStatus::Failed);
    for id in [&first.id, &second.id] {
        let rec = h.router.get_message(id).await.unwrap();
        assert_eq!(rec.reason_code.as_deref(), Some("blocked_on_permission"));
        let reason = rec.reason.unwrap_or_default();
        assert!(
            reason.contains("Bash(python3 …)"),
            "reason names the tool: {reason}"
        );
        assert!(reason.contains("allow"), "reason names the fix: {reason}");
    }
    // The agent itself is untouched: still blocked, nothing typed, no poke —
    // and the gate keeps holding while the dialog is up (#63): `/clear` would
    // be typed into it.
    assert!(h.states.blocked_since_public("builder").is_some());
    assert!(h.injector.pokes().is_empty());
    assert_eq!(decision(&h.router, "builder"), IdleDecision::HoldQuiet);
    h.states.clear_blocked("builder");
    assert_eq!(decision(&h.router, "builder"), IdleDecision::AllowClear);
}

#[tokio::test]
async fn blocked_agent_with_nothing_owed_is_left_alone() {
    let h = harness();
    h.router.set_watchdog_timing(fast_timing());
    h.states.set_blocked("builder", Some("Read"));
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(h.injector.pokes().is_empty());
    assert!(
        h.states.blocked_since_public("builder").is_some(),
        "flag untouched"
    );
}

/// `drive_message` stops watching an ask once it reaches `working`, so an exit
/// after that point is the sweeper's to notice (spec §4.1) — otherwise the ask
/// stays owed until its TTL.
#[tokio::test]
async fn exited_agent_has_its_owed_asks_failed() {
    let h = harness();
    let ask = http_ask(&h, "builder").await;
    h.states.set_state("builder", AgentState::Working);
    assert_eq!(
        await_status(&h, &ask.id, MessageStatus::Working).await,
        MessageStatus::Working
    );
    h.states.set_state("builder", AgentState::Exited);
    assert_eq!(await_failed(&h, &ask.id).await, MessageStatus::Failed);
    let rec = h.router.get_message(&ask.id).await.unwrap();
    assert_eq!(rec.reason_code.as_deref(), Some("agent_exited"));
}

#[tokio::test]
async fn sweeper_pokes_the_worker_once_the_backoff_elapses() {
    let h = harness();
    h.router.set_watchdog_timing(fast_timing());
    let _ask = http_ask(&h, "builder").await;
    assert!(matches!(
        decision(&h.router, "builder"),
        IdleDecision::Nudge(_)
    ));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(h.injector.pokes().is_empty(), "inside the backoff: no poke");
    // Sweeper ticks every second; wait for one past the 40 ms backoff.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(h.injector.pokes(), vec![agent("builder")]);
}

/// A poke sent while the agent is working is buffered behind whatever the queue
/// worker is doing and lands the instant the injection is delivered — before
/// the agent's `UserPromptSubmit` hook has moved it off idle. The nudged branch
/// checks the debounced state for the same reason the never-nudged one does.
#[tokio::test]
async fn sweeper_does_not_poke_a_nudged_agent_that_is_not_idle() {
    let h = harness();
    h.router.set_watchdog_timing(fast_timing());
    let _ask = http_ask(&h, "builder").await;
    assert!(matches!(
        decision(&h.router, "builder"),
        IdleDecision::Nudge(_)
    ));
    h.states.set_state("builder", AgentState::Working);

    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        h.injector.pokes().is_empty(),
        "a working agent is never poked, backoff spent or not"
    );

    h.states.set_state("builder", AgentState::Idle);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let pokes = h.injector.pokes();
    assert!(!pokes.is_empty(), "back at idle the re-nudge poke resumes");
    assert!(pokes.iter().all(|a| a == &agent("builder")));
}

/// An agent that owes a reply *and* is waiting on its own downstream ask would
/// get `HoldQuiet` from the gate every tick, so poking it once a second is pure
/// noise for the whole downstream wait.
#[tokio::test]
async fn sweeper_does_not_poke_while_the_agent_awaits_its_own_ask() {
    let h = harness();
    h.router.set_watchdog_timing(fast_timing());
    let _owed = http_ask(&h, "builder").await;
    assert!(matches!(
        decision(&h.router, "builder"),
        IdleDecision::Nudge(_)
    ));
    let downstream = emit_ask(&h, "builder", "notifier").await;
    // notifier now owes builder a reply and is poked on its own account; only
    // builder's pokes are the subject here.
    let builder_pokes = || {
        h.injector
            .pokes()
            .iter()
            .filter(|a| *a == &agent("builder"))
            .count()
    };

    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        builder_pokes(),
        0,
        "builder waits on its own ask; the gate would hold quiet anyway"
    );

    h.router
        .reply(
            Origin::Agent(agent("notifier")),
            &downstream.id,
            0,
            "ok".to_string(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        builder_pokes() > 0,
        "the settled downstream ask resumes the poke"
    );
}

/// Live 2026-08-18: an agent that idles *while blocked* gets `HoldQuiet`, so no
/// nudge state is ever created. Once the dialog clears it sits idle with the ask
/// still owed and nothing wakes it — the sweeper only poked agents that already
/// had nudge state. It must poke an owed idle agent that was never nudged too;
/// the worker then runs the gate, which performs round 1.
#[tokio::test]
async fn sweeper_pokes_an_owed_idle_agent_that_was_never_nudged() {
    let h = harness();
    h.router.set_watchdog_timing(fast_timing());
    let mut events = h.bus.subscribe();
    let _ask = http_ask(&h, "builder").await;
    h.states.set_blocked("builder", Some("Bash"));
    assert_eq!(decision(&h.router, "builder"), IdleDecision::HoldQuiet);
    assert!(
        !saw_event(&mut events, "agent.nudged").await,
        "HoldQuiet leaves no nudge state behind"
    );
    h.states.clear_blocked("builder");

    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(h.injector.pokes(), vec![agent("builder")]);
}

/// The other half: an agent still parked on the dialog is never poked, inside
/// the grace or past it. Typing into a dialog is never right, and the fail-fast
/// owns that case.
#[tokio::test]
async fn sweeper_does_not_poke_an_agent_still_on_the_dialog() {
    let h = harness();
    // A grace 1.1 s cannot outrun, so the tick sees a blocked agent, not a
    // failed ask.
    h.router.set_watchdog_timing(WatchdogTiming {
        blocked_grace: Duration::from_secs(30),
        ..fast_timing()
    });
    let ask = http_ask(&h, "builder").await;
    h.states.set_blocked("builder", Some("Bash"));
    assert_eq!(decision(&h.router, "builder"), IdleDecision::HoldQuiet);

    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        h.injector.pokes().is_empty(),
        "inside the grace, a blocked agent is left alone"
    );
    assert_eq!(
        h.router.get_message(&ask.id).await.unwrap().status,
        MessageStatus::Injected,
        "and the ask is still owed, not failed by the grace"
    );
}

/// The never-nudged branch runs the same backoff clock as the re-nudge, off the
/// ask's own age. An agent handed an ask a moment ago still reads debounced-idle
/// until its `UserPromptSubmit` hook fires, and the sweeper ticks every second —
/// poking there would nudge it for the message it is in the act of answering.
#[tokio::test]
async fn a_freshly_owed_idle_agent_is_not_poked_before_the_first_backoff() {
    let h = harness();
    h.router.set_watchdog_timing(WatchdogTiming {
        reply_nudge_backoff: [Duration::from_secs(30); 4],
        ..fast_timing()
    });
    // Idle (the harness default), owed, never nudged — branch (b)'s shape, but
    // the ask is seconds old, not minutes.
    let _ask = http_ask(&h, "builder").await;

    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        h.injector.pokes().is_empty(),
        "the ask is younger than one backoff"
    );
}

/// Expiry runs first in the same tick, so a dead ask leaves `owed` before the
/// poke walk can ask the worker to re-nudge for it (spec §4.1).
#[tokio::test]
async fn expiry_precedes_the_poke() {
    let h = build_harness(workflow_with_timeout(Duration::from_millis(1)));
    h.router.set_watchdog_timing(fast_timing());
    let ask = http_ask(&h, "builder").await;
    // Arms the re-nudge state, so a poke is due on the very tick that expires
    // the ask: only the ordering keeps it from being sent.
    assert!(matches!(
        decision(&h.router, "builder"),
        IdleDecision::Nudge(_)
    ));
    assert_eq!(await_failed(&h, &ask.id).await, MessageStatus::Failed);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        h.injector.pokes().is_empty(),
        "a dead ask is never poked for"
    );
}

/// #63: an armed turn on an agent parked on a permission dialog holds quiet —
/// the nudge would be typed into the dialog — and does not spend the turn's
/// one nudge, so it goes out once the dialog is gone.
#[tokio::test]
async fn armed_turn_holds_quiet_while_blocked_without_spending_the_nudge() {
    let h = harness();
    let mut events = h.bus.subscribe();
    h.router
        .create_message(
            Origin::User,
            agent("planner"),
            MessageKind::Send,
            "go".to_string(),
        )
        .await
        .unwrap();
    h.states.set_blocked("planner", Some("Bash"));
    assert_eq!(decision(&h.router, "planner"), IdleDecision::HoldQuiet);
    assert!(
        !saw_event(&mut events, "agent.nudged").await,
        "never type into a dialog"
    );
    h.states.clear_blocked("planner");
    match decision(&h.router, "planner") {
        IdleDecision::Nudge(text) => assert!(text.contains("tempo ask builder")),
        other => panic!("expected the deferred nudge, got {other:?}"),
    }
}
