#![expect(clippy::unwrap_used, reason = "tests assert on known-good values")]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use coretempo_core::bus::EventBus;
use coretempo_core::pty::{Cursor, InjectError, Injected, InjectionQueue};
use coretempo_core::router::{MessageFilter, Router, RouterError, StateSource};
use coretempo_core::store::Store;
use coretempo_core::time::Timestamp;
use coretempo_core::types::agent::AgentState;
use coretempo_core::types::config::{AgentConfig, FrozenWorkflow};
use coretempo_core::types::event::EventPayload;
use coretempo_core::types::id::{AgentId, MessageId, RunId};
use coretempo_core::types::message::{MessageKind, MessageRecord, MessageStatus, Origin};
use tokio::sync::{oneshot, watch};

static DB_N: AtomicU64 = AtomicU64::new(0);

fn temp_db() -> PathBuf {
    let n = DB_N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("coretempo-router-{}-{n}.db", std::process::id()))
}

fn agent(id: &str) -> AgentId {
    AgentId(id.to_string())
}

#[derive(Clone, Copy, PartialEq)]
enum InjectMode {
    Auto,
    Fail,
    /// Never resolves the injection oneshot.
    Hold,
}

type HeldInjection = (AgentId, oneshot::Sender<Result<Injected, InjectError>>);

struct MockInjector {
    mode: Mutex<InjectMode>,
    calls: Mutex<Vec<(AgentId, String)>>,
    held: Mutex<Vec<HeldInjection>>,
}

impl MockInjector {
    fn new(mode: InjectMode) -> Arc<MockInjector> {
        Arc::new(MockInjector {
            mode: Mutex::new(mode),
            calls: Mutex::new(Vec::new()),
            held: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<(AgentId, String)> {
        self.calls.lock().unwrap().clone()
    }
}

impl InjectionQueue for MockInjector {
    fn enqueue(
        &self,
        target: AgentId,
        text: String,
    ) -> oneshot::Receiver<Result<Injected, InjectError>> {
        let (tx, rx) = oneshot::channel();
        self.calls.lock().unwrap().push((target.clone(), text));
        match *self.mode.lock().unwrap() {
            InjectMode::Auto => {
                let _ = tx.send(Ok(Injected {
                    at: Timestamp::now(),
                    cursor: Cursor(0),
                }));
            }
            InjectMode::Fail => {
                let _ = tx.send(Err(InjectError::AgentRestarted(target)));
            }
            InjectMode::Hold => self.held.lock().unwrap().push((target, tx)),
        }
        rx
    }
}

#[derive(Default)]
struct FakeStates {
    chans: Mutex<HashMap<AgentId, watch::Sender<AgentState>>>,
}

impl FakeStates {
    fn add(&self, id: &AgentId) {
        let (tx, _rx) = watch::channel(AgentState::Idle);
        self.chans.lock().unwrap().insert(id.clone(), tx);
    }

    fn set(&self, id: &AgentId, state: AgentState) {
        self.chans
            .lock()
            .unwrap()
            .get(id)
            .unwrap()
            .send_replace(state);
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
}

fn workflow(ids: &[&str], ttl: Duration) -> Arc<FrozenWorkflow> {
    let mut agents = BTreeMap::new();
    for id in ids {
        agents.insert(
            agent(id),
            AgentConfig::new(PathBuf::from("/tmp"), "test agent"),
        );
    }
    Arc::new(FrozenWorkflow {
        name: "test".to_string(),
        hash: "0".repeat(64),
        source_path: PathBuf::from("tempo.toml"),
        ask_timeout: ttl,
        idle_debounce: Duration::from_secs(2),
        scrollback: 5_000,
        agents,
        mcp_servers: BTreeMap::new(),
        flows: BTreeMap::new(),
    })
}

struct Harness {
    router: Arc<Router>,
    injector: Arc<MockInjector>,
    states: Arc<FakeStates>,
    bus: EventBus,
}

fn harness_on(
    path: &Path,
    run_id: RunId,
    agents: &[&str],
    ttl: Duration,
    mode: InjectMode,
) -> Harness {
    let store = Store::open(path, run_id).unwrap();
    let bus = EventBus::new();
    let injector = MockInjector::new(mode);
    let states = Arc::new(FakeStates::default());
    for id in agents {
        states.add(&agent(id));
    }
    let router = Router::new(store, bus.clone(), injector.clone(), workflow(agents, ttl));
    router.set_state_source(states.clone());
    Harness {
        router,
        injector,
        states,
        bus,
    }
}

fn harness_with(agents: &[&str], ttl: Duration, mode: InjectMode) -> Harness {
    harness_on(
        &temp_db(),
        RunId("r-11111111".to_string()),
        agents,
        ttl,
        mode,
    )
}

fn harness(agents: &[&str]) -> Harness {
    harness_with(agents, Duration::from_mins(30), InjectMode::Auto)
}

async fn wait_status(router: &Router, id: &MessageId, want: MessageStatus) -> MessageRecord {
    for _ in 0..300 {
        let rec = router.get_message(id).await.unwrap();
        if rec.status == want {
            return rec;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let rec = router.get_message(id).await.unwrap();
    assert_eq!(rec.status, want, "timed out waiting for status");
    rec
}

/// The router persists a terminal status before it releases the asker's pending
/// count (`fail_message`: `transition` then `settle`), so a poller can observe
/// the status a moment before the count drops. Wait for the count too.
async fn wait_pending_asks(router: &Router, agent: &AgentId, want: u64) {
    for _ in 0..300 {
        if router.pending_asks(agent) == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        router.pending_asks(agent),
        want,
        "timed out waiting for pending asks"
    );
}

#[tokio::test]
async fn create_to_unknown_agent_errors() {
    let h = harness(&["builder"]);
    let err = h
        .router
        .create_message(
            Origin::User,
            agent("buidler"),
            MessageKind::Send,
            "x".to_string(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RouterError::UnknownAgent(a) if a.0 == "buidler"));
}

#[tokio::test]
async fn ask_persists_queued_and_emits_created() {
    let h = harness(&["planner", "builder"]);
    let mut events = h.bus.subscribe();
    let rec = h
        .router
        .create_message(
            Origin::Agent(agent("planner")),
            agent("builder"),
            MessageKind::Ask,
            "Is the schema migration done?".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(rec.status, MessageStatus::Queued);
    assert_eq!(rec.kind, MessageKind::Ask);
    assert_eq!(rec.from, Origin::Agent(agent("planner")));
    assert_eq!(rec.to, agent("builder"));
    assert!(rec.id.0.starts_with("m-"));
    assert!(rec.code.is_none() && rec.reply.is_none());
    assert!(rec.injected_at.is_none() && rec.completed_at.is_none());
    let event = events.recv().await.unwrap();
    match event.payload {
        EventPayload::MessageCreated { message } => assert_eq!(message.id, rec.id),
        other => panic!("expected message.created, got {other:?}"),
    }
}

#[tokio::test]
async fn ask_injects_rendered_template_and_reaches_injected() {
    let h = harness(&["planner", "builder"]);
    let rec = h
        .router
        .create_message(
            Origin::Agent(agent("planner")),
            agent("builder"),
            MessageKind::Ask,
            "ping".to_string(),
        )
        .await
        .unwrap();
    let rec = wait_status(&h.router, &rec.id, MessageStatus::Injected).await;
    assert!(rec.injected_at.is_some());
    let calls = h.injector.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, agent("builder"));
    let expected = format!(
        "[CoreTempo {id} from planner — reply expected] ping\nReply first with: tempo \
         reply {id} --code 0 '<answer>' (--code 1 on failure), then continue.",
        id = rec.id.0,
    );
    assert_eq!(calls[0].1, expected);
}

#[tokio::test]
async fn send_completes_done_after_working_then_idle() {
    let h = harness(&["builder"]);
    let rec = h
        .router
        .create_message(
            Origin::User,
            agent("builder"),
            MessageKind::Send,
            "go".to_string(),
        )
        .await
        .unwrap();
    wait_status(&h.router, &rec.id, MessageStatus::Injected).await;
    let expected = format!("[CoreTempo {} from user] go", rec.id.0);
    assert_eq!(h.injector.calls()[0].1, expected);
    h.states.set(&agent("builder"), AgentState::Working);
    wait_status(&h.router, &rec.id, MessageStatus::Working).await;
    h.states.set(&agent("builder"), AgentState::Idle);
    let rec = wait_status(&h.router, &rec.id, MessageStatus::Done).await;
    assert!(rec.completed_at.is_some());
}

#[tokio::test]
async fn ask_stays_working_when_target_goes_idle() {
    let h = harness(&["planner", "builder"]);
    let rec = h
        .router
        .create_message(
            Origin::Agent(agent("planner")),
            agent("builder"),
            MessageKind::Ask,
            "ping".to_string(),
        )
        .await
        .unwrap();
    wait_status(&h.router, &rec.id, MessageStatus::Injected).await;
    h.states.set(&agent("builder"), AgentState::Working);
    wait_status(&h.router, &rec.id, MessageStatus::Working).await;
    h.states.set(&agent("builder"), AgentState::Idle);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let rec = h.router.get_message(&rec.id).await.unwrap();
    assert_eq!(rec.status, MessageStatus::Working);
}

#[tokio::test]
async fn pending_asks_counts_only_agent_origin_asks() {
    let h = harness(&["planner", "builder"]);
    assert_eq!(h.router.pending_asks(&agent("planner")), 0);
    h.router
        .create_message(
            Origin::Agent(agent("planner")),
            agent("builder"),
            MessageKind::Ask,
            "one".to_string(),
        )
        .await
        .unwrap();
    h.router
        .create_message(
            Origin::User,
            agent("builder"),
            MessageKind::Ask,
            "two".to_string(),
        )
        .await
        .unwrap();
    h.router
        .create_message(
            Origin::Agent(agent("planner")),
            agent("builder"),
            MessageKind::Send,
            "three".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(h.router.pending_asks(&agent("planner")), 1);
    assert_eq!(h.router.pending_asks(&agent("builder")), 0);
}

#[tokio::test]
async fn inject_error_fails_the_message_and_releases_pending() {
    let h = harness_with(
        &["planner", "builder"],
        Duration::from_mins(30),
        InjectMode::Fail,
    );
    let rec = h
        .router
        .create_message(
            Origin::Agent(agent("planner")),
            agent("builder"),
            MessageKind::Ask,
            "ping".to_string(),
        )
        .await
        .unwrap();
    let rec = wait_status(&h.router, &rec.id, MessageStatus::Failed).await;
    assert!(rec.completed_at.is_some());
    wait_pending_asks(&h.router, &agent("planner"), 0).await;
}

#[tokio::test]
async fn get_unknown_message_errors() {
    let h = harness(&["builder"]);
    let err = h
        .router
        .get_message(&MessageId("m-00000000".to_string()))
        .await
        .unwrap_err();
    assert!(matches!(err, RouterError::UnknownMessage(_)));
}

async fn asked(h: &Harness) -> MessageRecord {
    let rec = h
        .router
        .create_message(
            Origin::Agent(agent("planner")),
            agent("builder"),
            MessageKind::Ask,
            "ping".to_string(),
        )
        .await
        .unwrap();
    wait_status(&h.router, &rec.id, MessageStatus::Injected).await
}

#[tokio::test]
async fn reply_marks_replied_and_injects_into_asker() {
    let h = harness(&["planner", "builder"]);
    let rec = asked(&h).await;
    let updated = h
        .router
        .reply(
            Origin::Agent(agent("builder")),
            &rec.id,
            0,
            "pong".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(updated.status, MessageStatus::Replied);
    assert_eq!(updated.code, Some(0));
    assert_eq!(updated.reply.as_deref(), Some("pong"));
    assert!(updated.completed_at.is_some());
    assert_eq!(h.router.pending_asks(&agent("planner")), 0);
    for _ in 0..300 {
        if h.injector.calls().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let calls = h.injector.calls();
    assert_eq!(calls.len(), 2, "expected ask injection + reply injection");
    assert_eq!(calls[1].0, agent("planner"));
    let expected = format!(
        "[CoreTempo reply to {} from builder — code 0] pong",
        rec.id.0
    );
    assert_eq!(calls[1].1, expected);
}

#[tokio::test]
async fn identical_replay_is_noop_ok() {
    let h = harness(&["planner", "builder"]);
    let rec = asked(&h).await;
    let replier = Origin::Agent(agent("builder"));
    h.router
        .reply(replier.clone(), &rec.id, 0, "pong".to_string())
        .await
        .unwrap();
    let again = h
        .router
        .reply(replier, &rec.id, 0, "pong".to_string())
        .await
        .unwrap();
    assert_eq!(again.status, MessageStatus::Replied);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.injector.calls().len(), 2, "replay must not re-inject");
}

#[tokio::test]
async fn conflicting_replay_is_already_replied() {
    let h = harness(&["planner", "builder"]);
    let rec = asked(&h).await;
    let replier = Origin::Agent(agent("builder"));
    h.router
        .reply(replier.clone(), &rec.id, 0, "pong".to_string())
        .await
        .unwrap();
    let err = h
        .router
        .reply(replier, &rec.id, 1, "different".to_string())
        .await
        .unwrap_err();
    assert!(matches!(err, RouterError::AlreadyReplied(_)));
}

#[tokio::test]
async fn reply_to_send_is_not_an_ask() {
    let h = harness(&["planner", "builder"]);
    let rec = h
        .router
        .create_message(
            Origin::User,
            agent("builder"),
            MessageKind::Send,
            "x".to_string(),
        )
        .await
        .unwrap();
    let err = h
        .router
        .reply(
            Origin::Agent(agent("builder")),
            &rec.id,
            0,
            "pong".to_string(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RouterError::NotAnAsk(_)));
}

#[tokio::test]
async fn wrong_replier_is_rejected() {
    let h = harness(&["planner", "builder", "reviewer"]);
    let rec = asked(&h).await;
    for wrong in [
        Origin::Agent(agent("reviewer")),
        Origin::User,
        Origin::Http("1f2e3d4c".to_string()),
    ] {
        let err = h
            .router
            .reply(wrong, &rec.id, 0, "pong".to_string())
            .await
            .unwrap_err();
        assert!(matches!(err, RouterError::WrongReplier(_)));
    }
    let rec = h.router.get_message(&rec.id).await.unwrap();
    assert_eq!(
        rec.status,
        MessageStatus::Injected,
        "rejected replies must not mutate"
    );
}

#[tokio::test]
async fn invalid_code_is_rejected() {
    let h = harness(&["planner", "builder"]);
    let rec = asked(&h).await;
    let err = h
        .router
        .reply(
            Origin::Agent(agent("builder")),
            &rec.id,
            2,
            "pong".to_string(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RouterError::InvalidCode(2)));
}

#[tokio::test]
async fn reply_to_unknown_message_errors() {
    let h = harness(&["builder"]);
    let err = h
        .router
        .reply(
            Origin::Agent(agent("builder")),
            &MessageId("m-00000000".to_string()),
            0,
            "pong".to_string(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RouterError::UnknownMessage(_)));
}

/// The output-schema gate binds to a kickoff by its origin hex. Only a
/// `trigger:` origin is a kickoff: a plain HTTP ask that happens to carry the
/// same hex is never gated (#24).
#[tokio::test]
async fn a_bound_contract_gates_trigger_origin_asks_only() {
    let h = harness(&["builder"]);
    let schema = serde_json::json!({"type": "object", "required": ["name"]});
    let contract = Arc::new(
        coretempo_core::schema::OutputContract::compile(
            schema,
            coretempo_core::types::id::FlowName("hook".into()),
            agent("builder"),
            3,
        )
        .unwrap(),
    );
    h.router.bind_kickoff_contract("1f2e3d4c", contract.clone());
    let plain = h
        .router
        .create_message(
            Origin::Http("1f2e3d4c".to_string()),
            agent("builder"),
            MessageKind::Ask,
            "q".to_string(),
        )
        .await
        .unwrap();
    wait_status(&h.router, &plain.id, MessageStatus::Injected).await;
    let replied = h
        .router
        .reply(
            Origin::Agent(agent("builder")),
            &plain.id,
            0,
            "prose".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(
        replied.status,
        MessageStatus::Replied,
        "an http ask is not gated"
    );

    h.router.bind_kickoff_contract("1f2e3d4c", contract);
    let kickoff = h
        .router
        .create_message(
            Origin::Trigger("1f2e3d4c".to_string()),
            agent("builder"),
            MessageKind::Ask,
            "q".to_string(),
        )
        .await
        .unwrap();
    wait_status(&h.router, &kickoff.id, MessageStatus::Injected).await;
    let err = h
        .router
        .reply(
            Origin::Agent(agent("builder")),
            &kickoff.id,
            0,
            "prose".to_string(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RouterError::OutputSchema { .. }), "{err:?}");
}

#[tokio::test]
async fn user_http_and_trigger_origin_asks_get_no_injection_sink() {
    let h = harness(&["builder"]);
    for origin in [
        Origin::User,
        Origin::Http("1f2e3d4c".to_string()),
        Origin::Trigger("1f2e3d4c".to_string()),
    ] {
        let rec = h
            .router
            .create_message(origin, agent("builder"), MessageKind::Ask, "q".to_string())
            .await
            .unwrap();
        wait_status(&h.router, &rec.id, MessageStatus::Injected).await;
        let before = h.injector.calls().len();
        let mut events = h.bus.subscribe();
        h.router
            .reply(Origin::Agent(agent("builder")), &rec.id, 1, "a".to_string())
            .await
            .unwrap();
        let event = events.recv().await.unwrap();
        match event.payload {
            EventPayload::MessageStatusChanged { message } => {
                assert_eq!(message.id, rec.id);
                assert_eq!(message.status, MessageStatus::Replied);
                assert_eq!(message.code, Some(1));
            }
            other => panic!("expected message.status, got {other:?}"),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            h.injector.calls().len(),
            before,
            "no PTY sink for {}",
            rec.from
        );
    }
}

#[tokio::test]
async fn list_messages_passes_filter_through() {
    let h = harness(&["planner", "builder"]);
    h.router
        .create_message(
            Origin::Agent(agent("planner")),
            agent("builder"),
            MessageKind::Ask,
            "one".to_string(),
        )
        .await
        .unwrap();
    h.router
        .create_message(
            Origin::User,
            agent("builder"),
            MessageKind::Send,
            "two".to_string(),
        )
        .await
        .unwrap();
    let all = h
        .router
        .list_messages(MessageFilter::default())
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
    let asks = h
        .router
        .list_messages(MessageFilter {
            kind: Some(MessageKind::Ask),
            ..MessageFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].body, "one");
}

#[tokio::test]
async fn wait_terminal_returns_immediately_when_terminal() {
    let h = harness(&["planner", "builder"]);
    let rec = asked(&h).await;
    h.router
        .reply(
            Origin::Agent(agent("builder")),
            &rec.id,
            0,
            "pong".to_string(),
        )
        .await
        .unwrap();
    let start = std::time::Instant::now();
    let got = h
        .router
        .wait_terminal(&rec.id, Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(got.status, MessageStatus::Replied);
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn wait_terminal_wakes_on_reply() {
    let h = harness(&["planner", "builder"]);
    let rec = asked(&h).await;
    let router = h.router.clone();
    let id = rec.id.clone();
    let waiter =
        tokio::spawn(async move { router.wait_terminal(&id, Duration::from_secs(30)).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.router
        .reply(
            Origin::Agent(agent("builder")),
            &rec.id,
            1,
            "no".to_string(),
        )
        .await
        .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(got.status, MessageStatus::Replied);
    assert_eq!(got.code, Some(1));
}

#[tokio::test]
async fn wait_terminal_times_out_with_current_record() {
    let h = harness(&["planner", "builder"]);
    let rec = asked(&h).await;
    let start = std::time::Instant::now();
    let got = h
        .router
        .wait_terminal(&rec.id, Duration::from_millis(200))
        .await
        .unwrap();
    assert!(start.elapsed() >= Duration::from_millis(200));
    assert_eq!(
        got.status,
        MessageStatus::Injected,
        "returns current, non-terminal record"
    );
}

#[tokio::test]
async fn wait_terminal_unknown_message_errors() {
    let h = harness(&["builder"]);
    let err = h
        .router
        .wait_terminal(
            &MessageId("m-00000000".to_string()),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RouterError::UnknownMessage(_)));
}

#[tokio::test]
async fn restart_fails_pending_messages_to_agent() {
    let h = harness_with(
        &["planner", "builder"],
        Duration::from_mins(30),
        InjectMode::Hold,
    );
    let ask = h
        .router
        .create_message(
            Origin::Agent(agent("planner")),
            agent("builder"),
            MessageKind::Ask,
            "one".to_string(),
        )
        .await
        .unwrap();
    let send = h
        .router
        .create_message(
            Origin::User,
            agent("builder"),
            MessageKind::Send,
            "two".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(h.router.pending_asks(&agent("planner")), 1);
    h.router.on_agent_restarted(&agent("builder")).await;
    let ask = wait_status(&h.router, &ask.id, MessageStatus::Failed).await;
    let send = wait_status(&h.router, &send.id, MessageStatus::Failed).await;
    assert!(ask.completed_at.is_some() && send.completed_at.is_some());
    assert_eq!(h.router.pending_asks(&agent("planner")), 0);
}

#[tokio::test]
async fn restart_sweep_only_fails_its_own_runs_messages() {
    let path = temp_db();
    let run_a = harness_on(
        &path,
        RunId("r-aaaaaaaa".to_string()),
        &["builder"],
        Duration::from_mins(30),
        InjectMode::Hold,
    );
    let run_b = harness_on(
        &path,
        RunId("r-bbbbbbbb".to_string()),
        &["builder"],
        Duration::from_mins(30),
        InjectMode::Hold,
    );

    // One live ask to builder in each run. Hold mode keeps them non-terminal.
    let ask_a = run_a
        .router
        .create_message(
            Origin::User,
            agent("builder"),
            MessageKind::Ask,
            "a".to_string(),
        )
        .await
        .unwrap();
    let ask_b = run_b
        .router
        .create_message(
            Origin::User,
            agent("builder"),
            MessageKind::Ask,
            "b".to_string(),
        )
        .await
        .unwrap();

    // builder restarts in run B: B's ask is swept, A's must survive.
    run_b.router.on_agent_restarted(&agent("builder")).await;

    let swept = wait_status(&run_b.router, &ask_b.id, MessageStatus::Failed).await;
    assert_eq!(swept.status, MessageStatus::Failed);
    let survivor = run_a.router.get_message(&ask_a.id).await.unwrap();
    assert_ne!(
        survivor.status,
        MessageStatus::Failed,
        "run B's restart sweep crossed into run A"
    );
}

#[tokio::test]
async fn restart_does_not_touch_terminal_messages() {
    let h = harness(&["planner", "builder"]);
    let rec = asked(&h).await;
    h.router
        .reply(
            Origin::Agent(agent("builder")),
            &rec.id,
            0,
            "pong".to_string(),
        )
        .await
        .unwrap();
    h.router.on_agent_restarted(&agent("builder")).await;
    let rec = h.router.get_message(&rec.id).await.unwrap();
    assert_eq!(rec.status, MessageStatus::Replied);
    assert_eq!(rec.reply.as_deref(), Some("pong"));
}

#[tokio::test]
async fn reply_after_asker_restart_is_logged_not_injected() {
    let h = harness(&["planner", "builder"]);
    let rec = asked(&h).await;
    h.router.on_agent_restarted(&agent("planner")).await;
    let mut events = h.bus.subscribe();
    let updated = h
        .router
        .reply(
            Origin::Agent(agent("builder")),
            &rec.id,
            0,
            "pong".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(updated.status, MessageStatus::Replied);
    let event = events.recv().await.unwrap();
    match event.payload {
        EventPayload::MessageStatusChanged { message } => {
            assert_eq!(message.status, MessageStatus::Replied);
        }
        other => panic!("expected message.status, got {other:?}"),
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let calls = h.injector.calls();
    assert_eq!(
        calls.len(),
        1,
        "only the original ask injection; no reply injection"
    );
}

#[tokio::test]
async fn asks_created_after_restart_inject_normally() {
    let h = harness(&["planner", "builder"]);
    h.router.on_agent_restarted(&agent("planner")).await;
    let rec = asked(&h).await;
    h.router
        .reply(
            Origin::Agent(agent("builder")),
            &rec.id,
            0,
            "pong".to_string(),
        )
        .await
        .unwrap();
    for _ in 0..300 {
        if h.injector.calls().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        h.injector.calls().len(),
        2,
        "post-restart asks get reply injection"
    );
}

#[tokio::test]
async fn ask_expires_to_failed_after_ttl() {
    let h = harness_with(
        &["planner", "builder"],
        Duration::from_millis(200),
        InjectMode::Hold,
    );
    let mut events = h.bus.subscribe();
    let rec = h
        .router
        .create_message(
            Origin::Agent(agent("planner")),
            agent("builder"),
            MessageKind::Ask,
            "ping".to_string(),
        )
        .await
        .unwrap();
    let rec = wait_status(&h.router, &rec.id, MessageStatus::Failed).await;
    assert!(rec.completed_at.is_some());
    assert_eq!(h.router.pending_asks(&agent("planner")), 0);
    let created = events.recv().await.unwrap();
    assert!(matches!(
        created.payload,
        EventPayload::MessageCreated { .. }
    ));
    let status = events.recv().await.unwrap();
    match status.payload {
        EventPayload::MessageStatusChanged { message } => {
            assert_eq!(message.status, MessageStatus::Failed);
        }
        other => panic!("expected message.status failed, got {other:?}"),
    }
}

#[tokio::test]
async fn sends_are_not_subject_to_ttl() {
    let h = harness_with(&["builder"], Duration::from_millis(200), InjectMode::Hold);
    let rec = h
        .router
        .create_message(
            Origin::User,
            agent("builder"),
            MessageKind::Send,
            "go".to_string(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let rec = h.router.get_message(&rec.id).await.unwrap();
    assert_eq!(rec.status, MessageStatus::Queued);
}

#[tokio::test]
async fn replied_ask_is_not_expired() {
    let h = harness_with(
        &["planner", "builder"],
        Duration::from_millis(200),
        InjectMode::Auto,
    );
    let rec = asked(&h).await;
    h.router
        .reply(
            Origin::Agent(agent("builder")),
            &rec.id,
            0,
            "pong".to_string(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let rec = h.router.get_message(&rec.id).await.unwrap();
    assert_eq!(rec.status, MessageStatus::Replied);
    assert_eq!(rec.reply.as_deref(), Some("pong"));
}
