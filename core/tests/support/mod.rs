//! Shared integration-test harness: builds a real `Router` (stub injector, temp `SQLite`)
//! plus a `FakePty`, and serves the axum app on an ephemeral loopback port.
#![expect(
    dead_code,
    reason = "each integration-test crate uses a subset of this harness"
)]

pub mod run;
#[cfg(unix)]
pub mod sessions;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use coretempo_core::api::auth::{TokenHint, token_matches};
use coretempo_core::api::{
    ApiContext, ApiCore, ApiServerHandle, Caller, OperatorToken, PtyFuture, PtySource, Roster,
    TokenAuth, serve,
};
use coretempo_core::bus::EventBus;
use coretempo_core::pty::{Cursor, InjectError, Injected, InjectionQueue, PtyChunk, PtyError};
use coretempo_core::router::Router;
use coretempo_core::schema::OutputContract;
use coretempo_core::store::Store;
use coretempo_core::time::Timestamp;
use coretempo_core::trigger::TriggerHub;
use coretempo_core::types::config::{
    AgentConfig, FlowConfig, FrozenFlow, FrozenWorkflow, ServerSection, TriggerConfig, TriggerType,
    WorkflowFile, WorkflowSection,
};
use coretempo_core::types::{AgentExit, AgentId, AgentState, FlowName, RunId, Token};
use tokio::sync::{mpsc, oneshot, watch};

/// Injection stub: resolves every enqueue immediately with an advancing cursor.
pub struct StubInjector {
    cursor: AtomicU64,
    pub injected: Mutex<Vec<(AgentId, String)>>,
}

impl StubInjector {
    pub fn new() -> Arc<StubInjector> {
        Arc::new(StubInjector {
            cursor: AtomicU64::new(0),
            injected: Mutex::new(Vec::new()),
        })
    }
}

impl InjectionQueue for StubInjector {
    fn enqueue(
        &self,
        target: AgentId,
        text: String,
    ) -> oneshot::Receiver<Result<Injected, InjectError>> {
        let (tx, rx) = oneshot::channel();
        let at = self.cursor.fetch_add(text.len() as u64, Ordering::SeqCst);
        if let Ok(mut log) = self.injected.lock() {
            log.push((target, text));
        }
        let _ = tx.send(Ok(Injected {
            at: Timestamp::now(),
            cursor: Cursor(at),
        }));
        rx
    }
}

type FakeAgent = (AgentState, Option<AgentExit>, Vec<PtyChunk>);

/// In-memory `PtySource`. Tests preload states/chunks and can push live chunks afterwards.
/// One `report_refused` call: the agent, the tool, and the input summary.
pub type RefusedReport = (AgentId, Option<String>, Option<String>);

pub struct FakePty {
    pub agents: Mutex<BTreeMap<AgentId, FakeAgent>>,
    pub restarts: Mutex<Vec<AgentId>>,
    pub senders: Mutex<Vec<(AgentId, mpsc::Sender<PtyChunk>)>>,
    pub queue_depths: Mutex<BTreeMap<AgentId, u64>>,
    pub debounced: Mutex<BTreeMap<AgentId, watch::Sender<AgentState>>>,
    pub blocked: Mutex<BTreeSet<AgentId>>,
    /// Every `report_blocked` call, so tests can see the forwarded tool name.
    pub blocked_reports: Mutex<Vec<(AgentId, Option<String>)>>,
    /// Every `report_unblocked` call with the hook `agent_id` it carried.
    pub unblocked_reports: Mutex<Vec<(AgentId, Option<String>)>>,
    /// Every `report_refused` call with the tool and input summary the hook named.
    pub refused_reports: Mutex<Vec<RefusedReport>>,
    /// The hook `agent_id` of every `report_blocked` call, in the same order as
    /// `blocked_reports`.
    pub blocked_agent_ids: Mutex<Vec<Option<String>>>,
    /// Every raw `write` call: the agent and the exact bytes.
    pub writes: Mutex<Vec<(String, Vec<u8>)>>,
    /// Every `resize` call: the agent, cols, rows.
    pub resizes: Mutex<Vec<(String, u16, u16)>>,
    /// Every `pause` call: the agent and the flag it was set to.
    pub pauses: Mutex<Vec<(String, bool)>>,
}

impl FakePty {
    pub fn new() -> Arc<FakePty> {
        Arc::new(FakePty {
            agents: Mutex::new(BTreeMap::new()),
            restarts: Mutex::new(Vec::new()),
            senders: Mutex::new(Vec::new()),
            queue_depths: Mutex::new(BTreeMap::new()),
            debounced: Mutex::new(BTreeMap::new()),
            blocked: Mutex::new(BTreeSet::new()),
            blocked_reports: Mutex::new(Vec::new()),
            unblocked_reports: Mutex::new(Vec::new()),
            refused_reports: Mutex::new(Vec::new()),
            blocked_agent_ids: Mutex::new(Vec::new()),
            writes: Mutex::new(Vec::new()),
            resizes: Mutex::new(Vec::new()),
            pauses: Mutex::new(Vec::new()),
        })
    }

    pub fn set_agent(
        &self,
        id: &str,
        state: AgentState,
        exit: Option<AgentExit>,
        chunks: Vec<(u64, &[u8])>,
    ) {
        let chunks = chunks
            .into_iter()
            .map(|(start, bytes)| PtyChunk {
                start: Cursor(start),
                bytes: bytes.to_vec(),
            })
            .collect();
        if let Ok(mut agents) = self.agents.lock() {
            agents.insert(AgentId(id.to_string()), (state, exit, chunks));
        }
    }

    pub fn push_live(&self, id: &str, start: u64, bytes: &[u8]) {
        let chunk = PtyChunk {
            start: Cursor(start),
            bytes: bytes.to_vec(),
        };
        if let Ok(senders) = self.senders.lock() {
            for (agent, tx) in senders.iter() {
                if agent.0 == id {
                    let _ = tx.try_send(chunk.clone());
                }
            }
        }
    }

    pub fn set_blocked(&self, id: &str, on: bool) {
        if let Ok(mut blocked) = self.blocked.lock() {
            let agent = AgentId(id.to_string());
            if on {
                blocked.insert(agent);
            } else {
                blocked.remove(&agent);
            }
        }
    }

    pub fn blocked_reports(&self) -> Vec<(AgentId, Option<String>)> {
        self.blocked_reports
            .lock()
            .map(|reports| reports.clone())
            .unwrap_or_default()
    }

    /// Hook `agent_id`s forwarded with each `report_blocked`, in call order.
    pub fn blocked_agent_ids(&self) -> Vec<Option<String>> {
        self.blocked_agent_ids
            .lock()
            .map(|ids| ids.clone())
            .unwrap_or_default()
    }

    /// Every `report_refused` call with the tool and input summary it carried.
    pub fn refused_reports(&self) -> Vec<RefusedReport> {
        self.refused_reports
            .lock()
            .map(|reports| reports.clone())
            .unwrap_or_default()
    }

    /// Every `report_unblocked` call with the hook `agent_id` it carried.
    pub fn unblocked_reports(&self) -> Vec<(AgentId, Option<String>)> {
        self.unblocked_reports
            .lock()
            .map(|reports| reports.clone())
            .unwrap_or_default()
    }

    /// Every raw `write` call, in call order.
    pub fn writes(&self) -> Vec<(String, Vec<u8>)> {
        self.writes.lock().map(|w| w.clone()).unwrap_or_default()
    }

    /// Every `resize` call, in call order.
    pub fn resizes(&self) -> Vec<(String, u16, u16)> {
        self.resizes.lock().map(|r| r.clone()).unwrap_or_default()
    }

    /// Every `pause` call, in call order.
    pub fn pauses(&self) -> Vec<(String, bool)> {
        self.pauses.lock().map(|p| p.clone()).unwrap_or_default()
    }

    fn lookup(&self, agent: &AgentId) -> Result<FakeAgent, PtyError> {
        let agents = self.agents.lock().map_err(|_| poisoned())?;
        agents.get(agent).cloned().ok_or_else(|| unknown(agent))
    }

    /// The real manager refuses a write or resize with no live session; an
    /// `exited` fake agent is that state.
    fn live(&self, agent: &AgentId) -> Result<(), PtyError> {
        let (state, _, _) = self.lookup(agent)?;
        if state == AgentState::Exited {
            return Err(PtyError::AgentExited(agent.clone()));
        }
        Ok(())
    }
}

fn poisoned() -> PtyError {
    // Any PtyError value works here; the API maps every PtyError to 500 internal.
    PtyError::UnknownAgent(AgentId("poisoned-lock".to_string()))
}

fn unknown(agent: &AgentId) -> PtyError {
    PtyError::UnknownAgent(agent.clone())
}

impl PtySource for FakePty {
    fn state(&self, agent: &AgentId) -> Result<AgentState, PtyError> {
        self.lookup(agent).map(|(state, _, _)| state)
    }
    fn report_state(&self, agent: &AgentId, state: AgentState) -> Result<(), PtyError> {
        let mut agents = self.agents.lock().map_err(|_| poisoned())?;
        let entry = agents.get_mut(agent).ok_or_else(|| unknown(agent))?;
        entry.0 = state;
        Ok(())
    }
    fn report_blocked(
        &self,
        agent: &AgentId,
        tool: Option<String>,
        agent_id: Option<String>,
    ) -> Result<(), PtyError> {
        self.lookup(agent)?;
        self.blocked_reports
            .lock()
            .map_err(|_| poisoned())?
            .push((agent.clone(), tool));
        self.blocked_agent_ids
            .lock()
            .map_err(|_| poisoned())?
            .push(agent_id);
        self.blocked
            .lock()
            .map_err(|_| poisoned())?
            .insert(agent.clone());
        Ok(())
    }
    fn report_refused(
        &self,
        agent: &AgentId,
        tool: Option<String>,
        input: Option<String>,
    ) -> Result<(), PtyError> {
        self.lookup(agent)?;
        self.refused_reports
            .lock()
            .map_err(|_| poisoned())?
            .push((agent.clone(), tool, input));
        Ok(())
    }
    fn report_unblocked(&self, agent: &AgentId, agent_id: Option<String>) -> Result<(), PtyError> {
        self.lookup(agent)?;
        self.unblocked_reports
            .lock()
            .map_err(|_| poisoned())?
            .push((agent.clone(), agent_id));
        self.blocked.lock().map_err(|_| poisoned())?.remove(agent);
        Ok(())
    }
    fn exit(&self, agent: &AgentId) -> Result<Option<AgentExit>, PtyError> {
        self.lookup(agent).map(|(_, exit, _)| exit)
    }
    fn end_cursor(&self, agent: &AgentId) -> Result<Cursor, PtyError> {
        let (_, _, chunks) = self.lookup(agent)?;
        let end = chunks
            .iter()
            .map(|c| c.start.0 + c.bytes.len() as u64)
            .max()
            .unwrap_or(0);
        Ok(Cursor(end))
    }
    fn subscribe_output(
        &self,
        agent: &AgentId,
        since: Option<Cursor>,
    ) -> Result<mpsc::Receiver<PtyChunk>, PtyError> {
        let (_, _, chunks) = self.lookup(agent)?;
        let (tx, rx) = mpsc::channel(64);
        let floor = since.map_or(0, |c| c.0);
        for chunk in chunks {
            if chunk.start.0 >= floor {
                let _ = tx.try_send(chunk);
            }
        }
        if let Ok(mut senders) = self.senders.lock() {
            senders.push((agent.clone(), tx));
        }
        Ok(rx)
    }
    fn begin_restart(&self, agent: AgentId) {
        if let Ok(mut restarts) = self.restarts.lock() {
            restarts.push(agent);
        }
    }
    fn queue_depth(&self, agent: &AgentId) -> Result<u64, PtyError> {
        self.lookup(agent)?;
        let depths = self.queue_depths.lock().map_err(|_| poisoned())?;
        Ok(depths.get(agent).copied().unwrap_or(0))
    }
    fn subscribe_debounced(
        &self,
        agent: &AgentId,
    ) -> Result<watch::Receiver<AgentState>, PtyError> {
        let (state, _, _) = self.lookup(agent)?;
        let mut debounced = self.debounced.lock().map_err(|_| poisoned())?;
        let sender = debounced
            .entry(agent.clone())
            .or_insert_with(|| watch::channel(state).0);
        Ok(sender.subscribe())
    }
    fn blocked(&self, agent: &AgentId) -> Result<bool, PtyError> {
        self.lookup(agent)?;
        let blocked = self.blocked.lock().map_err(|_| poisoned())?;
        Ok(blocked.contains(agent))
    }
    fn blocked_count(&self) -> usize {
        self.blocked.lock().map_or(0, |blocked| blocked.len())
    }
    fn write<'a>(&'a self, agent: &'a AgentId, bytes: Vec<u8>) -> PtyFuture<'a, ()> {
        Box::pin(async move {
            self.live(agent)?;
            self.writes
                .lock()
                .map_err(|_| poisoned())?
                .push((agent.0.clone(), bytes));
            Ok(())
        })
    }
    fn resize<'a>(&'a self, agent: &'a AgentId, cols: u16, rows: u16) -> PtyFuture<'a, ()> {
        Box::pin(async move {
            self.live(agent)?;
            self.resizes
                .lock()
                .map_err(|_| poisoned())?
                .push((agent.0.clone(), cols, rows));
            Ok(())
        })
    }
    fn pause(&self, agent: &AgentId, paused: bool) {
        if let Ok(mut pauses) = self.pauses.lock() {
            pauses.push((agent.0.clone(), paused));
        }
    }
}

/// A [`TokenAuth`] with an operator token and one hook token per agent — the
/// sessions daemon's shape, for tests that exercise the hook-token scope.
pub struct HookTokens {
    pub operator: Token,
    pub hooks: Vec<(AgentId, Token)>,
}

impl TokenAuth for HookTokens {
    fn classify(&self, bearer: &str) -> Caller {
        if token_matches(&self.operator, bearer) {
            return Caller::Operator;
        }
        for (id, token) in &self.hooks {
            if token_matches(token, bearer) {
                return Caller::Hook(id.clone());
            }
        }
        Caller::Unknown
    }
    fn hint(&self) -> TokenHint {
        TokenHint::Sessions
    }
}

pub fn temp_path(name: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "coretempo-api-test-{}-{n}-{name}",
        std::process::id()
    ))
}

/// Standard base64 (RFC 4648) decoder for PTY-stream assertions: test-local,
/// so an assertion verifies bytes rather than trusting our own encoder. A
/// character outside the alphabet (the `=` padding included) ends the decode.
pub fn b64_decode(s: &str) -> Vec<u8> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        let Some(v) = TABLE.iter().position(|t| *t == c) else {
            break;
        };
        acc = (acc << 6) | u32::try_from(v).unwrap_or_default();
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    out
}

fn agent_config() -> AgentConfig {
    AgentConfig::new(PathBuf::from("/tmp"), "test agent")
}

pub fn test_workflow() -> (Arc<FrozenWorkflow>, Arc<WorkflowFile>) {
    workflow_with(None)
}

fn workflow_with(flow: Option<(FlowName, FlowConfig)>) -> (Arc<FrozenWorkflow>, Arc<WorkflowFile>) {
    workflow_from(agent_config(), flow.into_iter().collect(), None)
}

/// The frozen shape of one declared flow; `output` is the compiled contract the
/// output tests install, `None` everywhere else.
fn frozen_flow(flow: &FlowConfig, output: Option<Arc<OutputContract>>) -> FrozenFlow {
    FrozenFlow {
        members: flow.agents.iter().cloned().collect(),
        trigger_type: flow.trigger.trigger_type,
        edge: flow.trigger.edge.clone(),
        message: flow.trigger.message.clone(),
        output,
    }
}

/// Same two-agent roster, with `planner`'s config supplied by the caller —
/// the loop-done tests give planner a loop edge to builder. `output` is frozen
/// onto every declared flow.
fn workflow_from(
    planner: AgentConfig,
    declared: Vec<(FlowName, FlowConfig)>,
    output: Option<&Arc<OutputContract>>,
) -> (Arc<FrozenWorkflow>, Arc<WorkflowFile>) {
    let declared = declared
        .into_iter()
        .map(|flow| (flow, output.cloned()))
        .collect();
    workflow_from_contracts(planner, declared)
}

/// One declared flow and the contract to freeze onto it, if any.
type DeclaredFlow = ((FlowName, FlowConfig), Option<Arc<OutputContract>>);

/// [`workflow_from`] with a contract per flow: its single-contract argument
/// cannot express two flows carrying different schemas.
fn workflow_from_contracts(
    planner: AgentConfig,
    declared: Vec<DeclaredFlow>,
) -> (Arc<FrozenWorkflow>, Arc<WorkflowFile>) {
    let mut agents = BTreeMap::new();
    agents.insert(AgentId("builder".to_string()), agent_config());
    agents.insert(AgentId("planner".to_string()), planner);
    let frozen_flows = declared
        .iter()
        .map(|((name, flow), contract)| (name.clone(), frozen_flow(flow, contract.clone())))
        .collect();
    let flows: BTreeMap<FlowName, FlowConfig> =
        declared.into_iter().map(|(flow, _)| flow).collect();
    let frozen = FrozenWorkflow {
        name: "test".to_string(),
        hash: "0".repeat(64),
        source_path: PathBuf::from("tempo.toml"),
        ask_timeout: Duration::from_mins(30),
        idle_debounce: Duration::from_secs(2),
        scrollback: 5_000,
        agents: agents.clone(),
        mcp_servers: BTreeMap::new(),
        flows: frozen_flows,
    };
    let file = WorkflowFile {
        workflow: WorkflowSection {
            name: "test".to_string(),
            db: PathBuf::from("./tempo.db"),
            port: 4820,
            ask_timeout_minutes: 30,
            idle_debounce_seconds: 2.0,
            scrollback: 5_000,
        },
        server: ServerSection::default(),
        agents,
        flows,
    };
    (Arc::new(frozen), Arc::new(file))
}

pub struct TestHandles {
    pub bus: EventBus,
    pub router: Arc<Router>,
    pub fake_pty: Arc<FakePty>,
    pub injector: Arc<StubInjector>,
    /// The operator token the context was built with. `ApiCore` keeps only an
    /// `Arc<dyn TokenAuth>`, which no test can read a token back out of.
    pub token: Token,
    /// The context's stop signal, held here so a test can trip it the way
    /// `Run::stop` does — and so it outlives a test that drops the runtime.
    pub stopping: watch::Sender<bool>,
    /// `Router::new` spawns background drivers, so the runtime is created first and
    /// shared with `start`/`TestServer`.
    pub rt: Arc<tokio::runtime::Runtime>,
}

pub fn test_ctx() -> anyhow::Result<(ApiContext, TestHandles)> {
    ctx_with(None)
}

/// A context where `planner` loops `builder`, for the loop-done endpoint.
pub fn test_ctx_with_planner_loop() -> anyhow::Result<(ApiContext, TestHandles)> {
    let planner = AgentConfig {
        edges: vec![coretempo_core::types::config::Edge {
            to: AgentId("builder".to_string()),
            kind: coretempo_core::types::config::EdgeKind::Loop,
            max_rounds: None,
        }],
        ..agent_config()
    };
    ctx_from(workflow_from(planner, Vec::new(), None))
}

/// The contract the output-schema tests install: `builder` must answer with
/// `{"name": <string>}` and nothing else.
fn output_contract(flow: &FlowName, max_repairs: u32) -> anyhow::Result<OutputContract> {
    let schema = serde_json::json!({
        "type": "object",
        "properties": { "name": { "type": "string" } },
        "required": ["name"],
        "additionalProperties": false
    });
    OutputContract::compile(
        schema,
        flow.clone(),
        AgentId("builder".to_string()),
        max_repairs,
    )
    .map_err(|e| anyhow::anyhow!("test schema must compile: {e}"))
}

/// The webhook flow the output tests hang their contract on: both agents,
/// kicked off at `builder`.
fn output_flow() -> (FlowName, FlowConfig) {
    (
        FlowName("hook".to_string()),
        FlowConfig {
            agents: vec![
                AgentId("builder".to_string()),
                AgentId("planner".to_string()),
            ],
            trigger: TriggerConfig {
                trigger_type: TriggerType::Webhook,
                edge: coretempo_core::types::config::Edge {
                    to: AgentId("builder".to_string()),
                    kind: coretempo_core::types::config::EdgeKind::Ask,
                    max_rounds: None,
                },
                message: None,
            },
            output: None,
        },
    )
}

/// A context whose frozen workflow carries an output contract targeting
/// `builder`, for the router's reply validation.
pub fn test_ctx_with_output(max_repairs: u32) -> anyhow::Result<(ApiContext, TestHandles)> {
    let flow = output_flow();
    let contract = Arc::new(output_contract(&flow.0, max_repairs)?);
    ctx_from(workflow_from(agent_config(), vec![flow], Some(&contract)))
}

/// A context whose workflow declares one flow, for the `/v1/trigger` endpoints.
pub fn test_ctx_with_flow(
    flow: (FlowName, FlowConfig),
) -> anyhow::Result<(ApiContext, TestHandles)> {
    ctx_with(Some(flow))
}

/// A context that declares one flow and carries an output contract targeting
/// `builder`, for the trigger boundary's schema validation.
pub fn test_ctx_with_flow_and_output(
    flow: (FlowName, FlowConfig),
    max_repairs: u32,
) -> anyhow::Result<(ApiContext, TestHandles)> {
    let contract = Arc::new(output_contract(&flow.0, max_repairs)?);
    ctx_from(workflow_from(agent_config(), vec![flow], Some(&contract)))
}

/// A context whose workflow declares `flows`, for the multi-flow trigger paths.
pub fn test_ctx_with_flows(
    flows: Vec<(FlowName, FlowConfig)>,
) -> anyhow::Result<(ApiContext, TestHandles)> {
    ctx_from(workflow_from(agent_config(), flows, None))
}

/// One webhook flow over `builder` alone, named `name` — the shape the warm
/// cross-flow tests declare twice.
pub fn builder_webhook_flow(name: &str) -> (FlowName, FlowConfig) {
    (
        FlowName(name.to_string()),
        FlowConfig {
            agents: vec![AgentId("builder".to_string())],
            trigger: TriggerConfig {
                trigger_type: TriggerType::Webhook,
                edge: coretempo_core::types::config::Edge {
                    to: AgentId("builder".to_string()),
                    kind: coretempo_core::types::config::EdgeKind::Ask,
                    max_rounds: None,
                },
                message: None,
            },
            output: None,
        },
    )
}

/// One declared flow and the contract frozen onto it.
pub type FlowWithContract = ((FlowName, FlowConfig), Arc<OutputContract>);

/// A webhook flow over `builder` named `name`, plus the compiled contract to
/// freeze onto it.
///
/// # Errors
/// The schema failing to compile under draft 2020-12.
pub fn webhook_flow_with_schema(
    name: &str,
    schema: serde_json::Value,
    max_repairs: u32,
) -> anyhow::Result<FlowWithContract> {
    let contract = OutputContract::compile(
        schema,
        FlowName(name.to_string()),
        AgentId("builder".to_string()),
        max_repairs,
    )
    .map_err(|e| anyhow::anyhow!("test schema must compile: {e}"))?;
    Ok((builder_webhook_flow(name), Arc::new(contract)))
}

/// Like [`test_ctx_with_flows`], but each flow carries its own frozen contract.
pub fn test_ctx_with_flow_contracts(
    flows: Vec<FlowWithContract>,
) -> anyhow::Result<(ApiContext, TestHandles)> {
    let flows = flows
        .into_iter()
        .map(|(flow, contract)| (flow, Some(contract)))
        .collect();
    ctx_from(workflow_from_contracts(agent_config(), flows))
}

/// Two webhook flows ("a" and "b") both spanning `builder` (exclusive by
/// default), for the warm cross-flow serialization tests.
pub fn test_ctx_with_two_webhook_flows() -> anyhow::Result<(ApiContext, TestHandles)> {
    test_ctx_with_flows(vec![builder_webhook_flow("a"), builder_webhook_flow("b")])
}

fn ctx_with(flow: Option<(FlowName, FlowConfig)>) -> anyhow::Result<(ApiContext, TestHandles)> {
    ctx_from(workflow_with(flow))
}

fn ctx_from(
    (workflow, workflow_file): (Arc<FrozenWorkflow>, Arc<WorkflowFile>),
) -> anyhow::Result<(ApiContext, TestHandles)> {
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?,
    );
    let bus = EventBus::new();
    let store = Store::open(&temp_path("db.sqlite"), RunId("r-11111111".to_string()))?;
    let injector = StubInjector::new();
    let guard = rt.enter();
    let router = Router::new(
        store,
        bus.clone(),
        injector.clone() as Arc<dyn InjectionQueue>,
        workflow.clone(),
    );
    drop(guard);
    let fake_pty = FakePty::new();
    fake_pty.set_agent("planner", AgentState::Idle, None, Vec::new());
    fake_pty.set_agent("builder", AgentState::Idle, None, Vec::new());
    let agent_locks = Arc::new(coretempo_core::locks::AgentLocks::new(&workflow.agents));
    let (stopping, stopping_rx) = watch::channel(false);
    let token = Token::generate();
    let ctx = ApiContext {
        core: ApiCore {
            pty: fake_pty.clone() as Arc<dyn PtySource>,
            bus: bus.clone(),
            roster: workflow.clone() as Arc<dyn Roster>,
            auth: Arc::new(OperatorToken(token.clone())),
            token_provisioned: true,
            bind: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port: 0,
            started_at: Timestamp::now(),
            started: Instant::now(),
        },
        router: router.clone(),
        workflow,
        workflow_file,
        run_id: RunId::generate(),
        triggers: TriggerHub::new(),
        agent_locks,
        stopping: stopping_rx,
    };
    Ok((
        ctx,
        TestHandles {
            bus,
            router,
            fake_pty,
            injector,
            token,
            stopping,
            rt,
        },
    ))
}

/// Arguments for [`TestServer::post_raw`], grouped to stay inside the param rule.
#[derive(Clone, Copy)]
pub struct RawPost<'a> {
    pub path: &'a str,
    pub content_type: Option<&'a str>,
    pub body: &'a [u8],
    /// `None` uses the server's real token.
    pub token: Option<&'a str>,
}

pub struct TestServer {
    pub addr: SocketAddr,
    pub token: String,
    pub handles: TestHandles,
    rt: Arc<tokio::runtime::Runtime>,
    _server: ApiServerHandle, // keeps the accept loop alive for the test's lifetime
}

pub fn start(ctx: ApiContext, handles: TestHandles) -> anyhow::Result<TestServer> {
    let token = handles.token.0.clone();
    let rt = handles.rt.clone();
    let server = rt.block_on(serve(ctx))?;
    let addr = server.local_addr();
    Ok(TestServer {
        addr,
        token,
        handles,
        rt,
        _server: server,
    })
}

pub fn start_default() -> anyhow::Result<TestServer> {
    let (ctx, handles) = test_ctx()?;
    start(ctx, handles)
}

pub fn start_with_output(max_repairs: u32) -> anyhow::Result<TestServer> {
    let (ctx, handles) = test_ctx_with_output(max_repairs)?;
    start(ctx, handles)
}

impl TestServer {
    pub fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        self.rt.block_on(fut)
    }

    fn agent() -> ureq::Agent {
        let cfg = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)))
            .build();
        ureq::Agent::new_with_config(cfg)
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// The status and parsed body of one response. A 204 carries no body at
    /// all, so an empty one reads as `null` rather than a parse error.
    fn finish(
        res: &mut ureq::http::Response<ureq::Body>,
    ) -> anyhow::Result<(u16, serde_json::Value)> {
        let status = res.status().as_u16();
        let raw = res.body_mut().read_to_string()?;
        if raw.trim().is_empty() {
            return Ok((status, serde_json::Value::Null));
        }
        Ok((status, serde_json::from_str(&raw)?))
    }

    /// GET without auth header.
    pub fn get_raw(&self, path: &str) -> anyhow::Result<(u16, serde_json::Value)> {
        let mut res = TestServer::agent().get(self.url(path)).call()?;
        TestServer::finish(&mut res)
    }

    /// GET with bearer token and optional X-CoreTempo-Agent header.
    pub fn get(
        &self,
        path: &str,
        as_agent: Option<&str>,
    ) -> anyhow::Result<(u16, serde_json::Value)> {
        let mut req = TestServer::agent()
            .get(self.url(path))
            .header("Authorization", format!("Bearer {}", self.token));
        if let Some(a) = as_agent {
            req = req.header("X-CoreTempo-Agent", a);
        }
        let mut res = req.call()?;
        TestServer::finish(&mut res)
    }

    /// GET as an arbitrary bearer token (`None` uses the operator token).
    pub fn get_as(
        &self,
        path: &str,
        token: Option<&str>,
    ) -> anyhow::Result<(u16, serde_json::Value)> {
        let token = token.unwrap_or(&self.token);
        let mut res = TestServer::agent()
            .get(self.url(path))
            .header("Authorization", format!("Bearer {token}"))
            .call()?;
        TestServer::finish(&mut res)
    }

    /// POST an arbitrary body: the trigger endpoint takes any content type, and a
    /// rejected request does not necessarily answer with JSON.
    pub fn post_raw(&self, req: RawPost<'_>) -> anyhow::Result<(u16, String)> {
        let token = req.token.unwrap_or(&self.token);
        let mut out = TestServer::agent()
            .post(self.url(req.path))
            .header("Authorization", format!("Bearer {token}"));
        if let Some(ct) = req.content_type {
            out = out.header("Content-Type", ct);
        }
        let mut res = out.send(req.body)?;
        Ok((res.status().as_u16(), res.body_mut().read_to_string()?))
    }

    /// POST JSON with bearer token and optional X-CoreTempo-Agent header.
    pub fn post(
        &self,
        path: &str,
        as_agent: Option<&str>,
        body: &serde_json::Value,
    ) -> anyhow::Result<(u16, serde_json::Value)> {
        let mut req = TestServer::agent()
            .post(self.url(path))
            .header("Authorization", format!("Bearer {}", self.token));
        if let Some(a) = as_agent {
            req = req.header("X-CoreTempo-Agent", a);
        }
        let mut res = req.send_json(body)?;
        TestServer::finish(&mut res)
    }

    /// POST JSON as an arbitrary bearer token, with no identity header.
    pub fn post_json_as(
        &self,
        path: &str,
        body: &serde_json::Value,
        token: Option<&str>,
    ) -> anyhow::Result<(u16, serde_json::Value)> {
        let token = token.unwrap_or(&self.token);
        let mut res = TestServer::agent()
            .post(self.url(path))
            .header("Authorization", format!("Bearer {token}"))
            .send_json(body)?;
        TestServer::finish(&mut res)
    }

    /// POST JSON as an arbitrary bearer token, claiming `agent` in
    /// X-CoreTempo-Agent.
    pub fn post_json_as_agent(
        &self,
        path: &str,
        body: &serde_json::Value,
        token: Option<&str>,
        agent: &str,
    ) -> anyhow::Result<(u16, serde_json::Value)> {
        let token = token.unwrap_or(&self.token);
        let mut res = TestServer::agent()
            .post(self.url(path))
            .header("Authorization", format!("Bearer {token}"))
            .header("X-CoreTempo-Agent", agent)
            .send_json(body)?;
        TestServer::finish(&mut res)
    }
}

/// One parsed SSE record (comments skipped).
pub struct SseRecord {
    pub id: Option<String>,
    pub event: Option<String>,
    pub data: String,
}

pub struct SseReader {
    lines: std::io::Lines<BufReader<Box<dyn Read + Send + Sync>>>,
}

impl SseReader {
    pub fn next_event(&mut self) -> anyhow::Result<SseRecord> {
        let mut rec = SseRecord {
            id: None,
            event: None,
            data: String::new(),
        };
        for line in self.lines.by_ref() {
            let line = line?;
            if line.is_empty() {
                if !rec.data.is_empty() || rec.id.is_some() || rec.event.is_some() {
                    return Ok(rec);
                }
                continue;
            }
            if let Some(v) = line.strip_prefix("id: ") {
                rec.id = Some(v.to_string());
            } else if let Some(v) = line.strip_prefix("event: ") {
                rec.event = Some(v.to_string());
            } else if let Some(v) = line.strip_prefix("data: ") {
                rec.data.push_str(v);
            }
            // lines starting with ':' are keep-alive comments — ignored
        }
        anyhow::bail!("SSE stream ended before a full event arrived")
    }
}

impl TestServer {
    /// Open an SSE stream; asserts 200 and `X-Accel-Buffering: no`.
    pub fn open_sse(&self, path: &str, last_event_id: Option<&str>) -> anyhow::Result<SseReader> {
        let cfg = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(None)
            .build();
        let agent = ureq::Agent::new_with_config(cfg);
        let mut req = agent
            .get(self.url(path))
            .header("Authorization", format!("Bearer {}", self.token));
        if let Some(id) = last_event_id {
            req = req.header("Last-Event-ID", id);
        }
        let res = req.call()?;
        anyhow::ensure!(res.status().as_u16() == 200, "SSE status {}", res.status());
        let buffering = res
            .headers()
            .get("x-accel-buffering")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        anyhow::ensure!(buffering == "no", "missing X-Accel-Buffering: no");
        let reader: Box<dyn Read + Send + Sync> = Box::new(res.into_body().into_reader());
        Ok(SseReader {
            lines: BufReader::new(reader).lines(),
        })
    }
}
