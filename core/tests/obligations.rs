#![expect(clippy::unwrap_used, reason = "tests assert on known-good values")]

//! Per-turn obligation tracking (spec §2): arming, met steps, nudge-once, stall.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use coretempo_core::bus::EventBus;
use coretempo_core::pty::{ClearGate, Cursor, IdleDecision, InjectError, Injected, InjectionQueue};
use coretempo_core::router::{Router, StateSource};
use coretempo_core::store::Store;
use coretempo_core::time::Timestamp;
use coretempo_core::types::agent::AgentState;
use coretempo_core::types::config::{AgentConfig, Edge, EdgeKind, FrozenWorkflow};
use coretempo_core::types::event::Event;
use coretempo_core::types::id::{AgentId, MessageId};
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
}

impl MockInjector {
    fn probe(&self, agent: AgentId) {
        *self.probe.lock().unwrap() = Some(agent);
    }

    fn observed(&self) -> Vec<IdleDecision> {
        self.observed.lock().unwrap().clone()
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
}

#[derive(Default)]
struct FakeStates {
    chans: Mutex<BTreeMap<AgentId, watch::Sender<AgentState>>>,
}

impl StateSource for FakeStates {
    fn subscribe_debounced(&self, agent: &AgentId) -> Option<watch::Receiver<AgentState>> {
        self.chans
            .lock()
            .unwrap()
            .get(agent)
            .map(watch::Sender::subscribe)
    }
}

struct Harness {
    router: Arc<Router>,
    injector: Arc<MockInjector>,
    bus: EventBus,
}

fn config(edges: Vec<Edge>) -> AgentConfig {
    AgentConfig {
        dir: PathBuf::from("/tmp"),
        prompt: "test agent".to_string(),
        model: None,
        permission_mode: None,
        auto_clear: true,
        edges,
        tools: Vec::new(),
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
        output: None,
    })
}

fn harness() -> Harness {
    build_harness(workflow())
}

fn build_harness(workflow: Arc<FrozenWorkflow>) -> Harness {
    let store = Store::open(&temp_db()).unwrap();
    let bus = EventBus::new();
    let states = Arc::new(FakeStates::default());
    for id in workflow.agents.keys() {
        let (tx, _rx) = watch::channel(AgentState::Idle);
        states.chans.lock().unwrap().insert(id.clone(), tx);
    }
    let injector = Arc::new(MockInjector::default());
    let router = Router::new(store, bus.clone(), injector.clone(), workflow);
    router.set_state_source(states);
    let _ = injector.router.set(Arc::downgrade(&router));
    Harness {
        router,
        injector,
        bus,
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
    let mut status = MessageStatus::Queued;
    for _ in 0..40 {
        status = h.router.get_message(id).await.unwrap().status;
        if status == MessageStatus::Failed {
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
        output: None,
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
async fn idle_with_owed_reply_nudges_once() {
    // An agent that went idle without answering an ask addressed to it is not
    // clearable: `/clear` would throw away the context the asker is blocked on.
    // `builder` has no edges, so nothing but the owed reply can hold the gate.
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
        "stalled fires once per owed-reply idle"
    );
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
    assert_eq!(h.router.open_turns(), 0);
    assert_eq!(h.router.total_pending_asks(), 0);
    arm_planner(&h).await;
    assert_eq!(h.router.open_turns(), 1);
    let _ask = emit_ask(&h, "planner", "builder").await;
    assert_eq!(h.router.total_pending_asks(), 1);
}
