//! Shared integration-test harness: builds a real `Router` (stub injector, temp `SQLite`)
//! plus a `FakePty`, and serves the axum app on an ephemeral loopback port.
#![expect(
    dead_code,
    reason = "each integration-test crate uses a subset of this harness"
)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use coretempo_core::api::{ApiContext, ApiServerHandle, PtySource, serve};
use coretempo_core::bus::EventBus;
use coretempo_core::pty::{Cursor, InjectError, Injected, InjectionQueue, PtyChunk, PtyError};
use coretempo_core::router::Router;
use coretempo_core::schema::OutputContract;
use coretempo_core::store::Store;
use coretempo_core::time::Timestamp;
use coretempo_core::trigger::TriggerHub;
use coretempo_core::types::config::{
    AgentConfig, FrozenWorkflow, ServerSection, TriggerConfig, WorkflowFile, WorkflowSection,
};
use coretempo_core::types::{AgentId, AgentState, RunId, Token};
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

type FakeAgent = (AgentState, Option<i32>, Vec<PtyChunk>);

/// In-memory `PtySource`. Tests preload states/chunks and can push live chunks afterwards.
pub struct FakePty {
    pub agents: Mutex<BTreeMap<AgentId, FakeAgent>>,
    pub restarts: Mutex<Vec<AgentId>>,
    pub senders: Mutex<Vec<(AgentId, mpsc::Sender<PtyChunk>)>>,
    pub queue_depths: Mutex<BTreeMap<AgentId, u64>>,
    pub debounced: Mutex<BTreeMap<AgentId, watch::Sender<AgentState>>>,
}

impl FakePty {
    pub fn new() -> Arc<FakePty> {
        Arc::new(FakePty {
            agents: Mutex::new(BTreeMap::new()),
            restarts: Mutex::new(Vec::new()),
            senders: Mutex::new(Vec::new()),
            queue_depths: Mutex::new(BTreeMap::new()),
            debounced: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn set_agent(
        &self,
        id: &str,
        state: AgentState,
        exit_code: Option<i32>,
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
            agents.insert(AgentId(id.to_string()), (state, exit_code, chunks));
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

    fn lookup(&self, agent: &AgentId) -> Result<FakeAgent, PtyError> {
        let agents = self.agents.lock().map_err(|_| poisoned())?;
        agents.get(agent).cloned().ok_or_else(|| unknown(agent))
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
    fn exit_code(&self, agent: &AgentId) -> Result<Option<i32>, PtyError> {
        self.lookup(agent).map(|(_, code, _)| code)
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
}

pub fn temp_path(name: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "coretempo-api-test-{}-{n}-{name}",
        std::process::id()
    ))
}

fn agent_config() -> AgentConfig {
    AgentConfig {
        dir: PathBuf::from("/tmp"),
        prompt: "test agent".to_string(),
        model: None,
        permission_mode: None,
        auto_clear: true,
        edges: Vec::new(),
        tools: Vec::new(),
    }
}

pub fn test_workflow() -> (Arc<FrozenWorkflow>, Arc<WorkflowFile>) {
    workflow_with(None)
}

fn workflow_with(trigger: Option<TriggerConfig>) -> (Arc<FrozenWorkflow>, Arc<WorkflowFile>) {
    workflow_from(agent_config(), trigger)
}

/// Same two-agent roster, with `planner`'s config supplied by the caller —
/// the loop-done tests give planner a loop edge to builder.
fn workflow_from(
    planner: AgentConfig,
    trigger: Option<TriggerConfig>,
) -> (Arc<FrozenWorkflow>, Arc<WorkflowFile>) {
    let mut agents = BTreeMap::new();
    agents.insert(AgentId("builder".to_string()), agent_config());
    agents.insert(AgentId("planner".to_string()), planner);
    let frozen = FrozenWorkflow {
        name: "test".to_string(),
        hash: "0".repeat(64),
        source_path: PathBuf::from("tempo.toml"),
        ask_timeout: Duration::from_mins(30),
        idle_debounce: Duration::from_secs(2),
        scrollback: 5_000,
        agents: agents.clone(),
        output: None,
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
        trigger,
    };
    (Arc::new(frozen), Arc::new(file))
}

pub struct TestHandles {
    pub bus: EventBus,
    pub router: Arc<Router>,
    pub fake_pty: Arc<FakePty>,
    pub injector: Arc<StubInjector>,
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
    ctx_from(workflow_from(planner, None))
}

/// The contract the output-schema tests install: `builder` must answer with
/// `{"name": <string>}` and nothing else.
fn output_contract(max_repairs: u32) -> anyhow::Result<OutputContract> {
    let schema = serde_json::json!({
        "type": "object",
        "properties": { "name": { "type": "string" } },
        "required": ["name"],
        "additionalProperties": false
    });
    OutputContract::compile(schema, AgentId("builder".to_string()), max_repairs)
        .map_err(|e| anyhow::anyhow!("test schema must compile: {e}"))
}

/// A context whose frozen workflow carries an output contract targeting
/// `builder`, for the router's reply validation.
pub fn test_ctx_with_output(max_repairs: u32) -> anyhow::Result<(ApiContext, TestHandles)> {
    let (frozen, file) = workflow_with(None);
    let frozen = FrozenWorkflow {
        output: Some(Arc::new(output_contract(max_repairs)?)),
        ..(*frozen).clone()
    };
    ctx_from((Arc::new(frozen), file))
}

/// A context whose workflow declares `trigger`, for the `/v1/trigger` endpoints.
pub fn test_ctx_with_trigger(trigger: TriggerConfig) -> anyhow::Result<(ApiContext, TestHandles)> {
    ctx_with(Some(trigger))
}

/// A context that declares `trigger` and carries an output contract targeting
/// `builder`, for the trigger boundary's schema validation.
pub fn test_ctx_with_trigger_and_output(
    trigger: TriggerConfig,
    max_repairs: u32,
) -> anyhow::Result<(ApiContext, TestHandles)> {
    let (frozen, file) = workflow_with(Some(trigger));
    let frozen = FrozenWorkflow {
        output: Some(Arc::new(output_contract(max_repairs)?)),
        ..(*frozen).clone()
    };
    ctx_from((Arc::new(frozen), file))
}

fn ctx_with(trigger: Option<TriggerConfig>) -> anyhow::Result<(ApiContext, TestHandles)> {
    ctx_from(workflow_with(trigger))
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
    let store = Store::open(&temp_path("db.sqlite"))?;
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
    let ctx = ApiContext {
        router: router.clone(),
        pty: fake_pty.clone() as Arc<dyn PtySource>,
        bus: bus.clone(),
        workflow,
        workflow_file,
        run_id: RunId::generate(),
        started_at: Timestamp::now(),
        started: Instant::now(),
        token: Token::generate(),
        token_provisioned: true,
        bind: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        port: 0,
        triggers: TriggerHub::new(),
    };
    Ok((
        ctx,
        TestHandles {
            bus,
            router,
            fake_pty,
            injector,
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
    let token = ctx.token.0.clone();
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

    /// GET without auth header.
    pub fn get_raw(&self, path: &str) -> anyhow::Result<(u16, serde_json::Value)> {
        let mut res = TestServer::agent().get(self.url(path)).call()?;
        Ok((res.status().as_u16(), res.body_mut().read_json()?))
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
        Ok((res.status().as_u16(), res.body_mut().read_json()?))
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
        Ok((res.status().as_u16(), res.body_mut().read_json()?))
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
