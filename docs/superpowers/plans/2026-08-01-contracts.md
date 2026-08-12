# CoreTempo — Frozen Contracts

Date: 2026-08-01 · Status: FROZEN — implementation planners code-name against this document.
Source: `docs/superpowers/specs/2026-08-01-coretempo-design.md`. On conflict, this document wins;
flag the conflict to the orchestrator rather than silently deviating.

---

## 1. Cargo workspace

Root `Cargo.toml` members: `["core", "app/src-tauri", "cli", "daemon"]`. Resolver `"3"`.

| dir | package name | kind | bin name |
|---|---|---|---|
| `core/` | `coretempo-core` | lib | — |
| `app/src-tauri/` | `coretempo-app` | Tauri 2 app | `coretempo` |
| `cli/` | `coretempo-cli` | bin | `tempo` |
| `daemon/` | `coretempo-daemon` | bin | `coretempod` |

**Feature gate (frozen):** `coretempo-core` has one feature, `server`, on by default, gating
everything that pulls tokio/axum/rusqlite/portable-pty/rand/subtle. With
`default-features = false` the crate compiles only `types` + `time` (deps: `serde`,
`serde_json`, `thiserror`). `coretempo-cli` depends on
`coretempo-core = { default-features = false }` — this realizes the spec's "cli only on shared
types" inside the 4-crate layout and keeps `tempo`'s tree tiny.

### 1.1 `core/src/` modules

Always compiled (no `server` feature):

| file | responsibility |
|---|---|
| `lib.rs` | crate root; `pub mod` tree; feature gates |
| `time.rs` | `Timestamp` newtype + `now()`; RFC 3339 UTC formatting with no external date dep |
| `types/mod.rs` | re-exports of all wire types |
| `types/id.rs` | `AgentId`, `MessageId`, `RunId`, `Token` newtypes; parse/validate; `::generate()` behind `server` |
| `types/message.rs` | `MessageRecord`, `MessageKind`, `MessageStatus`, `Origin` |
| `types/agent.rs` | `AgentState`, `AgentInfo`, `AgentDetail` |
| `types/event.rs` | `Event`, `EventPayload`, `LifecyclePhase` |
| `types/api.rs` | REST request/response bodies, `ApiErrorBody`, `Health`, `WorkflowResponse`, `Snapshot`, `RunInfo` |
| `types/config.rs` | `WorkflowFile`, `WorkflowSection`, `ServerSection`, `AgentConfig`, `ServerOverrides`, `ResolvedServer`, `FrozenWorkflow` |

`server` feature only:

| file | responsibility |
|---|---|
| `bus.rs` | `EventBus`: broadcast sender, seq assignment, 1024-event replay ring |
| `pty/mod.rs` | `PtyManager` facade; `InjectionQueue` + `ClearGate` traits; `PtyError`, `InjectError`, `Cursor`, `PtyChunk` |
| `pty/spawn.rs` | `claude` spawn recipe: portable-pty, args, env, PATH setup |
| `pty/detector.rs` | output-stream state detector (spinner/prompt) + 2 s debounce |
| `pty/queue.rs` | per-agent serialized injection queue; idle gating; drain-then-`/clear` ordering |
| `pty/ring.rs` | 256 KiB replay ring buffer with monotonic byte cursors; 8 ms / 32 KB read coalescing |
| `router/mod.rs` | `Router`: message lifecycle state machine, pending-asks accounting, idempotency |
| `router/sinks.rs` | per-origin reply sinks (PTY inject / bus event / long-poll wake) |
| `router/ttl.rs` | ask TTL sweeper → `failed` |
| `store/mod.rs` | SQLite (rusqlite bundled, WAL): dedicated writer thread, mpsc commands + oneshot replies |
| `store/schema.rs` | DDL: `messages`, `runs`, `agent_events` |
| `api/mod.rs` | axum router assembly + serve on resolved bind:port |
| `api/auth.rs` | bearer-token layer (constant-time), Host validation, JSON content-type guard, `api.json` writer |
| `api/messages.rs` | `/v1/messages*` handlers incl. `?wait` long-poll |
| `api/agents.rs` | `/v1/agents*` REST handlers + restart |
| `api/sse.rs` | `/v1/events` and `/v1/agents/{id}/pty` SSE streams, replay, `bus.reset` |
| `workflow.rs` | tempo.toml load/validate/freeze (`FrozenWorkflow` + sha256 hash); `resolve_server` precedence |
| `run.rs` | `Run`: wires bus/pty/router/store/api; run_id; start/stop |

### 1.2 `cli/src/` modules

| file | responsibility |
|---|---|
| `main.rs` | clap command tree, dispatch, exit codes |
| `connect.rs` | resolve `{port, token, agent_id}` from env, fallback `~/.coretempo/runs/current/api.json` |
| `client.rs` | ureq request helpers; render `ApiErrorBody.message` verbatim to stderr |
| `export.rs` | `tempo export`: emit tempo.toml + systemd user unit + Dockerfile |

### 1.3 `daemon/src/` modules

| file | responsibility |
|---|---|
| `main.rs` | clap → `ServerOverrides` → load/freeze workflow → `Run::start` → ctrl-c → `Run::stop` (~50 lines) |

### 1.4 `app/` layout

| path | responsibility |
|---|---|
| `src-tauri/src/main.rs` | tauri::Builder entry; register commands + `AppState` |
| `src-tauri/src/state.rs` | `AppState`: `Mutex<Option<Arc<Run>>>` + bridge task handles |
| `src-tauri/src/commands.rs` | all `#[tauri::command]` fns (§8) |
| `src-tauri/src/bridge.rs` | core bus → Tauri event `coretempo:event`; PTY subscriptions → Channels |
| `src/main.ts` | webview entry, mounts `App.svelte` |
| `src/App.svelte` | three-region shell; center swaps grid ⇄ editor |
| `src/lib/ipc.ts` | typed `invoke`/`listen`/Channel wrappers mirroring §8 signatures |
| `src/lib/wireEvents.ts` | single reducer: `Event` payloads → rune state; seq dedup |
| `src/lib/state/*.svelte.ts` | plain-runes modules: `run`, `agents`, `messages` |
| `src/lib/term/` | xterm.js lifecycle: WebGL→DOM fallback, channel→`term.write`, backpressure counter |
| `src/lib/views/` | roster, terminal grid, feed, chat, workflow editor |

---

## 2. Core wire types (`coretempo_core::types`)

Serde rules (frozen): all wire types derive `Debug, Clone, PartialEq, Serialize, Deserialize`.
JSON fields snake_case. Nullable fields are serialized explicitly as `null` (no
`skip_serializing_if`) — the record shape is constant. Config structs additionally carry
`#[serde(deny_unknown_fields)]`.

### 2.1 Ids and scalars

```rust
#[derive(..., Serialize, Deserialize)] #[serde(transparent)]
pub struct AgentId(pub String);    // toml key; must match ^[a-z0-9][a-z0-9_-]{0,31}$
#[serde(transparent)]
pub struct MessageId(pub String);  // "m-" + 8 lowercase hex, e.g. "m-a3f91c2e"
#[serde(transparent)]
pub struct RunId(pub String);      // "r-" + 8 lowercase hex
#[serde(transparent)]
pub struct Token(pub String);      // 64 lowercase hex chars (32 random bytes)
#[serde(transparent)]
pub struct Timestamp(pub String);  // RFC 3339 UTC, seconds precision, "Z": "2026-08-01T17:03:11Z"

impl Timestamp { pub fn now() -> Timestamp; }              // core::time
// behind `server`:
impl MessageId { pub fn generate() -> MessageId; }
impl RunId     { pub fn generate() -> RunId; }
impl Token     { pub fn generate() -> Token; }
```

### 2.2 Message model

```rust
#[serde(rename_all = "snake_case")]
pub enum MessageKind { Ask, Send }                          // "ask" | "send"

#[serde(rename_all = "snake_case")]
pub enum MessageStatus { Queued, Injected, Working, Replied, Done, Failed }
impl MessageStatus { pub fn is_terminal(&self) -> bool }    // replied | done | failed

/// Serializes as a plain string: "agent:planner" | "user" | "http:1f2e3d4c"
/// (custom Serialize/Deserialize + Display/FromStr; not a serde enum tag)
pub enum Origin {
    Agent(AgentId),   // "agent:<id>"
    User,             // "user" — in-process UI chat panel
    Http(String),     // "http:<req-id>", req-id = 8 lowercase hex generated per request
}

pub struct MessageRecord {
    pub id: MessageId,
    pub kind: MessageKind,
    pub from: Origin,
    pub to: AgentId,
    pub body: String,
    pub status: MessageStatus,
    pub code: Option<u8>,               // 0 | 1; null until replied
    pub reply: Option<String>,          // null until replied
    pub created_at: Timestamp,
    pub injected_at: Option<Timestamp>, // null until injected
    pub completed_at: Option<Timestamp>,// null until terminal
}
```

Canonical JSON (spec §3.2, field names exact):

```json
{ "id": "m-a3f91c2e", "kind": "ask", "from": "agent:planner", "to": "builder",
  "body": "Is the schema migration done?", "status": "replied", "code": 0,
  "reply": "Yes, migration 004 applied and tested.",
  "created_at": "2026-08-01T17:03:11Z", "injected_at": "2026-08-01T17:03:12Z",
  "completed_at": "2026-08-01T17:04:40Z" }
```

Status machine: `queued → injected → working → replied | done | failed`. Terminal: `replied`
(ask), `done` (send), `failed` (target exited/restarted before completion, or ask TTL expired).

### 2.3 Agent state

```rust
#[serde(rename_all = "snake_case")]
pub enum AgentState { Starting, Idle, Working, Exited, Restarting }
// wire strings: "starting" "idle" "working" "exited" "restarting" (UI labels exited "dead")

pub struct AgentInfo {                  // GET /v1/agents element
    pub id: AgentId,
    pub state: AgentState,              // raw (undebounced) state
    pub pending_asks: u64,              // asks SENT BY this agent, not yet terminal
    pub exit_code: Option<i32>,         // set only when state == exited
}

pub struct AgentDetail {                // GET /v1/agents/{id}
    #[serde(flatten)] pub info: AgentInfo,
    pub dir: String,                    // frozen, ~-expanded
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    pub auto_clear: bool,
    pub pty_cursor: u64,                // current end-of-stream byte cursor
}
```

### 2.4 Event enum

```rust
pub struct Event {
    pub seq: u64,                       // monotonic per run, starts at 1
    pub ts: Timestamp,
    #[serde(flatten)] pub payload: EventPayload,
}

#[serde(tag = "type")]
pub enum EventPayload {
    #[serde(rename = "run.started")]
    RunStarted { run_id: RunId, workflow_name: String, started_at: Timestamp },

    #[serde(rename = "agent.state")]                    // RAW transitions (UI shows truth);
    AgentStateChanged { agent: AgentId, state: AgentState },  // debounced signal is internal-only

    #[serde(rename = "agent.lifecycle")]
    AgentLifecycle { agent: AgentId, phase: LifecyclePhase, exit_code: Option<i32> },

    #[serde(rename = "message.created")]
    MessageCreated { message: MessageRecord },          // fat: full record snapshot

    #[serde(rename = "message.status")]
    MessageStatusChanged { message: MessageRecord },    // fat: full record snapshot

    #[serde(rename = "bus.reset")]                      // synthesized per-consumer on replay
    BusReset {},                                        // gap / broadcast::Lagged; seq = latest
}

#[serde(rename_all = "snake_case")]
pub enum LifecyclePhase { Spawned, Exited, Restarting }
```

Example wire forms:

```json
{"seq":1,"ts":"2026-08-01T17:00:00Z","type":"run.started","run_id":"r-1f2e3d4c","workflow_name":"core-tempo-dev","started_at":"2026-08-01T17:00:00Z"}
{"seq":6,"ts":"…","type":"agent.state","agent":"builder","state":"working"}
{"seq":7,"ts":"…","type":"agent.lifecycle","agent":"docs","phase":"exited","exit_code":1}
{"seq":9,"ts":"…","type":"message.created","message":{ …full MessageRecord… }}
{"seq":12,"ts":"…","type":"message.status","message":{ …full MessageRecord… }}
{"seq":41,"ts":"…","type":"bus.reset"}
```

### 2.5 `tempo.toml` structs

```rust
#[serde(deny_unknown_fields)]
pub struct WorkflowFile {
    pub workflow: WorkflowSection,
    #[serde(default)] pub server: ServerSection,
    pub agents: BTreeMap<AgentId, AgentConfig>,   // ≥1 required; roster order = lexicographic
}

#[serde(deny_unknown_fields)]
pub struct WorkflowSection {
    pub name: String,
    #[serde(default = "d_db")]    pub db: PathBuf,               // "./tempo.db"
    #[serde(default = "d_port")]  pub port: u16,                 // 4820
    #[serde(default = "d_ttl")]   pub ask_timeout_minutes: u64,  // 30
    #[serde(default = "d_deb")]   pub idle_debounce_seconds: f64,// 2.0
}

#[derive(Default)] #[serde(deny_unknown_fields)]
pub struct ServerSection {
    pub bind: Option<IpAddr>,                     // default 127.0.0.1
    pub token_file: Option<PathBuf>,
    #[serde(default)] pub allowed_origins: Vec<String>,  // future remote UI; empty = no CORS
    pub log: Option<String>,                      // tracing EnvFilter string
}

#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub dir: PathBuf,                             // required; ~ expanded at freeze
    pub prompt: String,                           // required; --append-system-prompt (+ primer)
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    #[serde(default = "d_true")] pub auto_clear: bool,   // true
}
```

### 2.6 Config precedence (server-level settings only; agents come from file alone)

Precedence: **flags > `CORETEMPO_*` env > tempo.toml > defaults**.

```rust
#[derive(Default)]
pub struct ServerOverrides {          // one instance per layer (flags, env)
    pub bind: Option<IpAddr>,
    pub port: Option<u16>,
    pub db: Option<PathBuf>,
    pub token: Option<Token>,         // env CORETEMPO_TOKEN only
    pub token_file: Option<PathBuf>,
    pub log: Option<String>,
}
impl ServerOverrides { pub fn from_env() -> ServerOverrides; }  // reads CORETEMPO_* (§7.2)

pub struct ResolvedServer {
    pub bind: IpAddr,
    pub port: u16,
    pub db: PathBuf,
    pub token: Token,                 // provisioned or generated; generated forbidden off-loopback
    pub log: String,                  // default "info"
}

pub fn resolve_server(flags: ServerOverrides, env: ServerOverrides, file: &WorkflowFile)
    -> Result<ResolvedServer, ConfigError>;

pub struct FrozenWorkflow {           // immutable for the life of a run
    pub name: String,
    pub hash: String,                 // lowercase hex sha256 of the tempo.toml bytes
    pub source_path: PathBuf,
    pub ask_timeout: Duration,
    pub idle_debounce: Duration,
    pub agents: BTreeMap<AgentId, AgentConfig>,
}
pub fn load_workflow(path: &Path) -> Result<(WorkflowFile, FrozenWorkflow), ConfigError>;
pub fn validate_workflow(text: &str) -> Result<WorkflowFile, Vec<ValidationIssue>>;
pub struct ValidationIssue { pub path: String, pub message: String }   // path e.g. "agents.builder.dir"
```

---

## 3. PTY manager (`coretempo_core::pty`)

```rust
#[derive(Clone, Copy, ..., Serialize, Deserialize)] #[serde(transparent)]
pub struct Cursor(pub u64);           // monotonic byte offset in an agent's output stream

pub struct PtyChunk { pub start: Cursor, pub bytes: Vec<u8> }

pub struct PtyManager { /* opaque */ }

impl PtyManager {
    pub fn new(workflow: Arc<FrozenWorkflow>, bus: EventBus, env: AgentEnv) -> Arc<PtyManager>;
    /// Wiring break for the PtyManager⇄Router cycle; called once in Run::start before spawn.
    pub fn set_clear_gate(&self, gate: Arc<dyn ClearGate>);

    pub async fn spawn_all(&self) -> Result<(), PtyError>;
    pub async fn spawn(&self, agent: &AgentId) -> Result<(), PtyError>;
    /// Kill + respawn from the same frozen config. Fails queued/in-flight injections
    /// with InjectError::AgentRestarted. Emits agent.lifecycle restarting → spawned.
    pub async fn restart(&self, agent: &AgentId) -> Result<(), PtyError>;

    /// Raw user keystrokes — bypasses the injection queue entirely.
    pub async fn write(&self, agent: &AgentId, bytes: &[u8]) -> Result<(), PtyError>;
    pub async fn resize(&self, agent: &AgentId, cols: u16, rows: u16) -> Result<(), PtyError>;

    /// Live output. Guarantee: chunks are contiguous from max(since, ring_start); consumer
    /// detects aged-out data by first_chunk.start > since. since = None → full ring tail.
    pub fn subscribe_output(&self, agent: &AgentId, since: Option<Cursor>)
        -> Result<tokio::sync::mpsc::Receiver<PtyChunk>, PtyError>;
    /// One-shot ring read: (cursor after last byte, bytes from max(since, ring_start)).
    pub fn read_ring(&self, agent: &AgentId, since: Option<Cursor>)
        -> Result<(Cursor, Vec<u8>), PtyError>;
    /// Backpressure from the UI (>~1 MB unparsed): pause/resume reading this PTY.
    pub fn pause_output(&self, agent: &AgentId, paused: bool);

    pub fn state(&self, agent: &AgentId) -> Result<AgentState, PtyError>;
    pub fn subscribe_state_raw(&self, agent: &AgentId)
        -> Result<tokio::sync::watch::Receiver<AgentState>, PtyError>;      // feeds agent.state
    pub fn subscribe_state_debounced(&self, agent: &AgentId)
        -> Result<tokio::sync::watch::Receiver<AgentState>, PtyError>;      // 2 s stable idle
}

pub struct AgentEnv {                 // injected into every agent PTY
    pub port: u16,
    pub token: Token,
    pub tempo_bin_dir: PathBuf,       // prepended to PATH
}
```

### 3.1 Injection-queue boundary (router → pty)

```rust
/// Implemented by PtyManager; the ONLY write path the router uses.
pub trait InjectionQueue: Send + Sync + 'static {
    /// Enqueue on the target's serialized queue. Injection happens only when the target is
    /// debounced-idle. Receiver resolves when bytes hit the PTY (=> injected_at) or on failure.
    fn enqueue(&self, target: AgentId, text: String)
        -> tokio::sync::oneshot::Receiver<Result<Injected, InjectError>>;
}

pub struct Injected { pub at: Timestamp, pub cursor: Cursor }  // cursor = injection marker pos

#[derive(Debug, thiserror::Error)]
pub enum InjectError {
    #[error("agent '{0}' has exited")]        AgentExited(AgentId),
    #[error("agent '{0}' was restarted")]     AgentRestarted(AgentId),
    #[error("unknown agent '{0}'")]           UnknownAgent(AgentId),
}

/// Implemented by Router; consulted by the queue worker INSIDE the serialized queue at the
/// moment of a debounced working→idle transition. /clear is typed only if
/// pending_asks == 0 AND the queue is empty (strict drain-then-clear). auto_clear=false
/// agents are never cleared.
pub trait ClearGate: Send + Sync + 'static {
    fn pending_asks(&self, agent: &AgentId) -> u64;
}
```

### 3.2 Injection templates (exact format strings)

Injection = write the rendered text into the PTY, then `\r` to submit.
`{sender}` renders `Origin` as: `Agent(id)` → the bare id; `User` → `user`; `Http(_)` → `http`.

```text
ask   → [CoreTempo {id} from {sender} — reply expected] {body}
        Reply first with: tempo reply {id} --code 0 '<answer>' (--code 1 on failure), then continue.
reply → [CoreTempo reply to {id} from {replier} — code {code}] {body}
send  → [CoreTempo {id} from {sender}] {body}
```

(ask template is one injection: two lines joined by `\n`. The full protocol primer lives in the
generated `--append-system-prompt`, not in injections.)

---

## 4. Router (`coretempo_core::router`)

```rust
pub struct Router { /* opaque */ }

impl Router {
    pub fn new(store: Store, bus: EventBus, injector: Arc<dyn InjectionQueue>,
               workflow: Arc<FrozenWorkflow>) -> Arc<Router>;

    /// Validates target, assigns MessageId, persists (queued), emits message.created,
    /// enqueues injection, drives queued→injected→working via InjectionQueue + debounced
    /// state subscription. `from` comes from auth context only.
    pub async fn create_message(&self, from: Origin, to: AgentId, kind: MessageKind,
                                body: String) -> Result<MessageRecord, RouterError>;

    /// Idempotency: first reply fires sink once; identical replay (same code+body) is a
    /// no-op Ok; conflicting replay → AlreadyReplied; reply to send → NotAnAsk;
    /// replier != message.to → WrongReplier.
    pub async fn reply(&self, replier: Origin, id: &MessageId, code: u8, body: String)
        -> Result<MessageRecord, RouterError>;

    pub async fn get_message(&self, id: &MessageId) -> Result<MessageRecord, RouterError>;
    pub async fn list_messages(&self, filter: MessageFilter)
        -> Result<Vec<MessageRecord>, RouterError>;
    /// Long-poll: bus subscription filtered by id raced against timeout, then read from
    /// SQLite; always returns the current record.
    pub async fn wait_terminal(&self, id: &MessageId, timeout: Duration)
        -> Result<MessageRecord, RouterError>;

    /// Fail in-flight + queued messages TO the agent; suppress future reply injection for
    /// pending asks FROM it (log + event only).
    pub async fn on_agent_restarted(&self, agent: &AgentId);
}
impl ClearGate for Router { fn pending_asks(&self, agent: &AgentId) -> u64; }

pub struct MessageFilter {            // all optional; maps 1:1 to GET /v1/messages query
    pub to: Option<AgentId>,
    pub from: Option<Origin>,
    pub status: Option<MessageStatus>,
    pub kind: Option<MessageKind>,
    pub since: Option<Timestamp>,     // created_at >= since
    pub limit: u32,                   // default 100, max 1000
}

#[derive(Debug, thiserror::Error)]
pub enum RouterError { UnknownAgent(AgentId), UnknownMessage(MessageId),
    AlreadyReplied(MessageId), NotAnAsk(MessageId), WrongReplier(MessageId),
    InvalidCode(u8), Store(StoreError) }
```

Reply sinks by origin (all messages always hit SQLite first): `agent:*` → inject into asker's
PTY unless asker restarted/cleared since (then log+event only) · `user` → bus event only ·
`http:*` → wake that caller's `?wait` long-poll. Send completion = debounced
injected→working→idle observed on the target → `done`.

### 4.1 Run orchestration (`coretempo_core::run`)

```rust
pub struct Run { /* opaque */ }
impl Run {
    /// Wires store→bus→pty→router (set_clear_gate)→api; writes api.json (0600) +
    /// `current` symlink; spawns agents; emits run.started.
    pub async fn start(workflow: FrozenWorkflow, server: ResolvedServer)
        -> Result<Arc<Run>, RunError>;
    pub fn run_id(&self) -> &RunId;
    pub fn started_at(&self) -> &Timestamp;
    pub fn bus(&self) -> &EventBus;
    pub fn pty(&self) -> &PtyManager;
    pub fn router(&self) -> &Router;
    pub fn workflow(&self) -> &FrozenWorkflow;
    pub fn workflow_file(&self) -> &WorkflowFile;     // frozen file for GET /v1/workflow
    pub async fn stop(&self) -> Result<(), RunError>; // kill PTYs, flush store, stop server
}
```

`~/.coretempo/runs/<run_id>/api.json` (mode 0600; `~/.coretempo/runs/current` symlink):

```json
{ "port": 4820, "token": "<64 hex>", "run_id": "r-1f2e3d4c" }
```

---

## 5. REST surface

Base `http://127.0.0.1:<port>/v1`, all JSON. Auth: `Authorization: Bearer <token>` on every
route except `/v1/health` (401 `unauthorized` otherwise). Agent attribution:
`X-CoreTempo-Agent: <agent-id>` header → `Origin::Agent`; absent → `Origin::Http(req_id)`
(in-process UI never uses HTTP; it is `Origin::User` via direct core calls). Non-JSON
`Content-Type` on bodied requests → 415. Bad `Host` → 403 `invalid_host`.

| Method & path | Purpose | Success |
|---|---|---|
| `POST /v1/messages` | Create ask/send (`?wait=<secs>` sugar) | `201` / `200` |
| `GET /v1/messages/{id}` | Fetch; `?wait=<secs>` long-polls for terminal status | `200` |
| `POST /v1/messages/{id}/reply` | Deliver reply for an ask | `200` |
| `GET /v1/messages?to=&from=&status=&kind=&since=&limit=` | Traffic log | `200` |
| `GET /v1/agents` | Roster with live state + `pending_asks` | `200` |
| `GET /v1/agents/{id}` | Agent detail | `200` |
| `POST /v1/agents/{id}/restart` | Kill + respawn (async) | `202` |
| `GET /v1/agents/{id}/pty` | SSE stream of PTY output | `200` |
| `GET /v1/events` | SSE control-plane event bus | `200` |
| `GET /v1/workflow` | Frozen workflow, run id, started_at | `200` |
| `GET /v1/health` | Liveness only — unauthenticated | `200` |

(axum 0.8 `{param}` route syntax.)

### 5.1 Bodies (serde struct names frozen)

```rust
pub struct CreateMessageRequest { pub to: AgentId, pub kind: MessageKind, pub body: String }
// → 201 MessageRecord (no wait) | 200 MessageRecord (with ?wait, after long-poll)

pub struct ReplyRequest { pub code: u8, pub body: String }        // code must be 0 | 1
// → 200 MessageRecord (updated)

pub struct MessageListResponse { pub messages: Vec<MessageRecord> }  // created_at DESC
pub struct AgentListResponse   { pub agents: Vec<AgentInfo> }        // lexicographic by id
// GET /v1/agents/{id} → 200 AgentDetail
pub struct RestartResponse { pub agent: AgentId, pub state: AgentState }  // 202, state=restarting

pub struct WorkflowResponse {
    pub run_id: RunId,
    pub started_at: Timestamp,
    pub workflow: WorkflowFile,       // frozen file serialized as JSON
}

pub struct Health { pub status: String, pub version: String,      // "ok", core crate version
                    pub run_id: RunId, pub uptime_secs: u64 }
```

`?wait` semantics: blocks up to N secs for terminal status, returns `200` + current record
either way; callers branch on `status`, never HTTP code. `wait` capped at **300 s**.

### 5.2 Error body (uniform)

```json
{ "error": { "code": "unknown_agent",
             "message": "no agent named 'buidler'; roster: planner, builder, reviewer" } }
```

```rust
pub struct ApiErrorBody { pub error: ApiErrorDetail }
pub struct ApiErrorDetail { pub code: String, pub message: String }
```

Frozen error codes → HTTP status:

| code | status | when |
|---|---|---|
| `unauthorized` | 401 | missing/wrong bearer token |
| `invalid_host` | 403 | Host header not loopback/configured |
| `wrong_replier` | 403 | reply from a non-addressee |
| `unknown_agent` | 404 | target/path agent not in roster |
| `unknown_message` | 404 | no such message id |
| `already_replied` | 409 | conflicting reply replay (identical replay → 200 no-op) |
| `not_an_ask` | 409 | reply to a `send` |
| `unsupported_media_type` | 415 | non-JSON Content-Type on bodied request |
| `invalid_request` | 400 | malformed JSON/params, bad `code`, bad `X-CoreTempo-Agent`, bad query |
| `internal` | 500 | anything else |

Messages are written for LLM readers: include the roster / valid values / the fix.

---

## 6. SSE wire formats

### 6.1 `GET /v1/events`

```
id: 42
event: message.status
data: {"seq":42,"ts":"2026-08-01T17:03:12Z","type":"message.status","message":{…}}

: keep-alive          ← comment every 15 s
```

- `id:` = `seq` · `event:` = the `type` string · `data:` = full `Event` JSON (single line).
- Header `X-Accel-Buffering: no`.
- Replay: `Last-Event-ID` header or `?since=<seq>` replays forward from the ~1024-event ring.
  If aged out (or `broadcast::Lagged` mid-stream): server sends one synthesized `bus.reset`
  event (seq = latest published seq) and continues live; client re-snapshots via REST.
- Filters: `?types=<comma-list>` with trailing-`*` glob (e.g. `types=message.*,agent.state`);
  `?agent=<id>` matches `agent.*` events' `agent` field and `message.*` events' `to`/`from`
  (agent origins). `run.started` and `bus.reset` always pass filters.

### 6.2 `GET /v1/agents/{id}/pty`

```
id: 183462
event: pty
data: {"seq":183462,"b64":"G1szMm0kIG0..."}
```

- `seq` = byte cursor (`Cursor`) of the chunk's first byte; monotonic per agent. `id:` mirrors
  it, so `Last-Event-ID` (or `?since=<cursor>`) resumes exactly.
- `b64` = standard base64 (RFC 4648, padded) of the raw chunk — split escape sequences /
  partial UTF-8 must survive transit.
- On connect: ring-buffer tail replayed from `max(since, ring_start)` before live. No reset
  event: a client detects aged-out data by `first seq > since`.
- Same 15 s keep-alive comment. PTY bytes NEVER appear on `/v1/events`.

---

## 7. `tempo` CLI

### 7.1 Commands

```
tempo ask   <agent> <message> [--wait | --no-wait]
tempo send  <agent> <message>
tempo reply <id> --code <0|1> <message>
tempo agents
tempo status <id> [--wait <secs>]
tempo export <dir>
```

- `<message>` is one positional argument (callers quote it).
- `ask`: with `CORETEMPO_AGENT_ID` set → POST, print message id to stdout, exit 0. Without it →
  blocks by default (`GET …?wait=30` loop until terminal; TTL bounds it), prints reply body to
  stdout. `--no-wait` forces the async path; `--wait` forces blocking even for agents.
- `send`: POST, print message id, exit 0.
- `reply`: POST reply, exit 0 (idempotent replay is also 0).
- `agents`: prints one line per agent: `<id>\t<state>\t<pending_asks>`.
- `status`: prints the full `MessageRecord` JSON; `--wait <secs>` long-polls once (cap 300).
- `export`: writes `tempo.toml` + systemd user unit template + Dockerfile into `<dir>` from the
  running server's `GET /v1/workflow`.
- API errors: print `error.message` verbatim to stderr.

Exit codes (frozen): `0` success / reply code 0 · `1` blocking ask replied with code 1 ·
`2` message reached `failed` · `3` usage, connection, or HTTP error.

### 7.2 Environment variables (frozen names)

| var | consumer | meaning |
|---|---|---|
| `CORETEMPO_AGENT_ID` | cli | set in agent PTYs; selects async ask + `X-CoreTempo-Agent` |
| `CORETEMPO_PORT` | cli, daemon | server port |
| `CORETEMPO_TOKEN` | cli, daemon | bearer token (daemon: provisioned token) |
| `CORETEMPO_BIND` | daemon | bind address |
| `CORETEMPO_DB` | daemon | SQLite path |
| `CORETEMPO_TOKEN_FILE` | daemon | file containing token |
| `CORETEMPO_LOG` | daemon, app | tracing EnvFilter |

CLI connection resolution: `CORETEMPO_PORT`/`CORETEMPO_TOKEN` env, else
`~/.coretempo/runs/current/api.json`. `tempo` never scans ports.

### 7.3 `coretempod`

```
coretempod run <CONFIG> [--bind <ip>] [--port <n>] [--db <path>] [--token-file <path>]
```

Flags → `ServerOverrides` (highest precedence). Non-loopback `--bind` without a provisioned
token (env or file) → refuse to start, non-zero exit. No daemonization.

---

## 8. Tauri surface (`coretempo-app`)

### 8.1 Commands (exact signatures; `CmdError` serializes as `{"code":"…","message":"…"}`)

```rust
#[tauri::command] async fn snapshot(state) -> Result<Snapshot, CmdError>;
#[tauri::command] async fn run_start(state, config_path: String) -> Result<RunInfo, CmdError>;
#[tauri::command] async fn run_stop(state) -> Result<(), CmdError>;
#[tauri::command] async fn restart_agent(state, agent: String) -> Result<(), CmdError>;

#[tauri::command] async fn subscribe_pty(state, agent: String, since_cursor: Option<u64>,
    channel: tauri::ipc::Channel<tauri::ipc::InvokeResponseBody>) -> Result<(), CmdError>;
#[tauri::command] async fn write_pty(state, agent: String, data: Vec<u8>) -> Result<(), CmdError>;
#[tauri::command] async fn resize_pty(state, agent: String, cols: u16, rows: u16) -> Result<(), CmdError>;
#[tauri::command] async fn pause_pty(state, agent: String, paused: bool) -> Result<(), CmdError>;

#[tauri::command] async fn workflow_open(path: String) -> Result<String, CmdError>;   // file text
#[tauri::command] async fn workflow_save(path: String, text: String) -> Result<(), CmdError>;
#[tauri::command] async fn workflow_validate(text: String) -> Result<ValidationReport, CmdError>;
#[tauri::command] async fn send_chat(state, to: String, kind: MessageKind, body: String)
    -> Result<MessageRecord, CmdError>;   // chat panel; Origin::User — UI never dogfoods HTTP

pub struct Snapshot {
    pub run: Option<RunInfo>,
    pub agents: Vec<AgentDetail>,
    pub messages: Vec<MessageRecord>,          // most recent 200, created_at DESC
    pub pty_cursors: BTreeMap<AgentId, u64>,   // subscribe_pty(since_cursor) values
    pub last_seq: u64,                          // event dedup floor
}
pub struct RunInfo { pub run_id: RunId, pub workflow_name: String, pub workflow_path: String,
                     pub started_at: Timestamp, pub port: u16 }
pub struct ValidationReport { pub ok: bool, pub errors: Vec<ValidationIssue> }
```

### 8.2 Events and channels

- Control plane: every core `Event` forwarded as Tauri event **`coretempo:event`**, payload =
  the identical `Event` JSON as SSE `data:`. Frontend dedups by `seq` (`> last_seq`).
- PTY bytes: NEVER on the event system. `subscribe_pty` replays the ring tail from
  `since_cursor` then goes live on the given Channel, sending
  `InvokeResponseBody::Raw(chunk_bytes)` — JS `onmessage` receives `ArrayBuffer`; wrap in
  `Uint8Array` and pass straight to `term.write` (no decode).
- Reload mid-run: `snapshot()` → resubscribe events (dedup by `last_seq`) → `subscribe_pty` per
  agent with the snapshot's cursors. No gap, no duplicate.
- Bridge lag: if the in-process `broadcast::Receiver` reports `Lagged`, the bridge emits a
  synthesized `bus.reset` event on `coretempo:event`; the frontend re-snapshots.

---

## 9. Event bus (`coretempo_core::bus`)

```rust
pub struct EventBus { /* Arc-cloneable */ }
impl EventBus {
    pub const CAPACITY: usize = 1024;       // tokio::sync::broadcast channel capacity
    pub const REPLAY_RING: usize = 1024;    // SSE replay ring length (events)
    pub fn new() -> EventBus;
    /// SOLE seq authority: AtomicU64 starting at 1, assigned at publish; also appends to the
    /// replay ring. Everyone else (router, pty, run) publishes payloads, never seqs.
    pub fn publish(&self, payload: EventPayload) -> u64;
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Event>;
    /// Events with seq > since. None => aged out — caller must emit bus.reset.
    pub fn replay_since(&self, since: u64) -> Option<Vec<Event>>;
    pub fn last_seq(&self) -> u64;
}
```

`bus.reset` is synthesized per-consumer (SSE connection / Tauri bridge) — never `publish`ed.

---

## 10. Naming conventions & frozen constants

Conventions:

- JSON: snake_case fields; enum values snake_case strings; event types dot.namespaced
  (`message.created`); nulls explicit; timestamps RFC 3339 UTC seconds precision with `Z`.
- Ids: messages `m-` + 8 lowercase hex · runs `r-` + 8 lowercase hex · HTTP request ids
  8 lowercase hex. Agent ids = tempo.toml keys, `^[a-z0-9][a-z0-9_-]{0,31}$`.
- Origin strings: `agent:<id>` · `user` · `http:<req-id>`.
- Env prefix `CORETEMPO_`; header `X-CoreTempo-Agent`; auth `Authorization: Bearer <token>`.
- Rust: packages `coretempo-*`; bins `coretempo`, `tempo`, `coretempod`; `thiserror` in core,
  `anyhow` in binaries; `tracing` only.
- Files: `~/.coretempo/runs/<run_id>/api.json` (0600) + `~/.coretempo/runs/current` symlink.

Constants:

| constant | value |
|---|---|
| default port | `4820` |
| token | 32 random bytes → 64 hex; constant-time compare (`subtle`) |
| ask TTL default | 30 min (`ask_timeout_minutes`) |
| idle debounce default | 2.0 s (`idle_debounce_seconds`) |
| PTY read flush | 8 ms elapsed OR 32 KB accumulated |
| PTY replay ring | 256 KiB per agent |
| UI backpressure threshold | ~1 MB unparsed per terminal |
| broadcast capacity / event replay ring | 1024 / 1024 events |
| SSE keep-alive | 15 s comment |
| `?wait` cap | 300 s |
| `GET /v1/messages` limit | default 100, max 1000 |
| snapshot messages | most recent 200 |
| xterm scrollback | [workflow] scrollback, default 5 000 |
| reply `code` domain | `0 \| 1` |

---

## Reconciliation amendments (2026-08-01, post-planning)

Additive contract changes the reconciled plans now depend on — treat these as
frozen alongside the sections above:

1. `StateSource` trait + `Router::set_state_source(Arc<dyn StateSource>)`
   (messaging plan) — gives the router the debounced agent-state signal that
   drives `injected → working` and `send` completion. Wired in
   `Run::start` (workflow-run Task 7) via an adapter over
   `PtyManager::subscribe_state_debounced`, immediately after
   `set_clear_gate`.
2. `ResolvedServer.token_provisioned: bool` — forwarded into `ApiContext`;
   required for the refuse-non-loopback-without-provisioned-token rule.
3. `FrozenWorkflow::system_prompt(&self, &AgentId) -> Option<String>` — role
   prompt + tempo protocol primer; `PtyManager` spawn passes this (not raw
   `cfg.prompt`) as `--append-system-prompt`.
4. `coretempo_core::export` module: `systemd_unit(...)` and `dockerfile(...)`
   template fns (workflow-run) consumed by `tempo export` (api-surface
   Task 12, which owns `cli/src/export.rs`).
5. `PtyManager::shutdown` (kill PTYs, stop tasks, fail queued injections,
   idempotent), `PtyManager::exit_code(&AgentId) -> Result<Option<i32>, PtyError>`,
   and `PtyManager::new_with_program` (test seam).
6. `ApiFile`, `RunInfo`, and `Snapshot` live in `core/src/types/api.rs`;
   `RunInfo`/`Snapshot` are appended by tauri-shell Tasks 7/8 with the exact
   section-8.1 shapes.
7. `ServerOverrides::from_env() -> Result<ServerOverrides, ConfigError>`
   (deviation from the infallible signature frozen above) — malformed
   `CORETEMPO_PORT` must fail fast.
8. Dependency pins added: `sha2 0.10.9` (core, `server` feature only,
   workflow hashing) · `tauri 2.11.5` · `tauri-build 2.6.3`.
9. PTY SSE resume caveat: `Last-Event-ID` (chunk-start cursor) re-delivers the
   final chunk on resume; byte-exact resume uses `?since=<start+len>`. The
   Tauri channel path is cursor-exact and unaffected.
10. **Exit codes have one source of truth: `PtyManager::exit_code`.** The
    tauri-shell plan's `AppState.exit_codes` cache and the bridge's
    `track_exit_code` existed only because the original contract lacked an
    accessor; amendment 5 added one (`core/src/pty/mod.rs`). Tasks 3, 5, and 8
    must therefore DROP `exit_codes` from `AppState` and `track_exit_code`
    from the event bridge, and read `run.pty().exit_code(&agent)` directly
    when building `AgentInfo`/`Snapshot`. No shadow copy of core state in the
    app layer.
11. `AgentConfig.edges: Vec<Edge>` where `Edge { to: AgentId, kind: MessageKind }`
    (serde default empty; ordered; validated: known target, no self-edges, no
    duplicate (to, kind)). `MessageKind` gains `Hash`. Additive — old tomls valid.
12. `FrozenWorkflow::system_prompt` (amendment 3) additionally composes a
    "Required workflow steps" block from `edges`: numbered `tempo ask`/`tempo
    send` commands in edge order; ask steps say end-your-turn, never wait.
    Edge-free agents' prompts are byte-identical to before.
13. `EventPayload` gains `agent.nudged { agent }` and `agent.stalled { agent }`
    (spec 2026-08-03 §2). Nudges are queue injections (like `/clear`), not
    `MessageRecord`s — the events are their observable trace.
14. `ClearGate` is now `fn on_stable_idle(&AgentId) -> IdleDecision`
    (`AllowClear | Nudge(String) | HoldQuiet`); the queue worker evaluates it
    after the drain on every debounced working→idle transition and honors
    `auto_clear` only for `AllowClear`. `Router::pending_asks` remains as an
    inherent method. App command `workflow_validate` is replaced by
    `workflow_parse(text: String) -> ParseReport` (§8.1 `ParseReport { ok, errors,
    model: WorkflowFile | null }`). New app command `workflow_merge(text: String,
    model: WorkflowFile) -> String` re-serializes an edited model back into `text`.
    Dependency pins added: `toml_edit 0.25.13` · `tauri-plugin-dialog 2.7.2` ·
    `@xyflow/svelte 1.6.2` · `@tauri-apps/plugin-dialog 2.7.2`.
15. `WorkflowFile.trigger: Option<TriggerConfig>` where `TriggerConfig { type:
    "on_start"|"webhook", edge: Edge, message: Option<String>, output:
    Option<OutputConfig> }` (validated: known target; on_start requires
    non-empty message; webhook rejects one; `output` — added by the 2026-08-06
    structured-output design, shape correction 2026-08-11 — requires webhook +
    ask, exactly one of schema/schema_file, a non-empty schema_file, and
    max_repairs 0..=5).
    `EventPayload` gains `workflow.completed { result: replied|quiesced|
    failed|timeout, code?, reply? }`.
16. Endpoints `POST /v1/trigger` (any content type, UTF-8, 64 KB cap — exempt
    from the JSON guard) and `GET /v1/trigger/{id}` on the run API (webhook
    workflows; 409 `trigger_in_flight` while one runs) and on `coretempod
    serve` (FIFO cap 32 → 429; history 100; serve health has its own shape).
    `TriggerHub`'s in-flight claim is an id-carrying `Option<String>`, not a
    bool: `try_begin`/`begin`/`finish` claim and release it atomically, so a
    second trigger during a live kickoff is refused rather than racing it.
    `TriggerStatus` folds both `timeout` and `failed` completions into
    `failed { reason }` for `GET /v1/trigger/{id}` — the full `replied|
    quiesced|failed|timeout` result set from amendment 15 surfaces only on the
    `workflow.completed` bus event, so a machine caller distinguishing a
    stall from a hard failure must read that event rather than the polled
    status (the `reason` prose says "ask_timeout_minutes" for a timeout, but
    is not a stable machine signal). `PtySource` gains `queue_depth` and
    `subscribe_debounced`.
17. `Run::start_with(workflow, server, RunOptions { ephemeral_port,
    repoint_current, cleanup_run_dir })`; the API listener binds before
    `AgentEnv` is constructed, so `AgentEnv.port` (and therefore
    `CORETEMPO_PORT` for every spawned agent) is always the port actually
    bound rather than the configured one. `ApiServerHandle::shutdown` aborts
    still-open SSE streams after a 500 ms grace (changes desktop `Run::stop`
    too — fixes the shutdown hang).
18. `coretempod serve <toml>` (webhook only; startup-hash baseline, edits fail
    triggers until restart, not adopted mid-queue); `run` with an on_start
    trigger exits 0 on a `replied` completion with `code == 0` or on
    `quiesced`, 1 on any other completion (a nonzero reply code, `failed`, or
    `timeout`), 130 on ctrl-c. Completion: ask kickoff → its terminal status;
    send kickoff → global quiescence, armed only once the kickoff reaches
    `working` — never weaken that guard. Watcher deadline =
    `ask_timeout_minutes` minus a 2 s margin, measured from the kickoff
    message's creation (not from injection), so the watcher's own clock —
    not the router's TTL sweeper — is what labels a stall `timeout`.
19. **xterm scrollback is workflow-configurable.** `[workflow] scrollback`
    (default 5 000, was frozen at 10 000) flows through `FrozenWorkflow` and
    `RunInfo.scrollback` to the desktop terminals. Motivated by the 2026-08-04
    memory review: scrollback was the app's dominant heap consumer
    (~1 KiB x scrollback x agents).
20. **A lagging PTY subscriber is dropped, not waited on.** The fan-out in
    `PtyManager`'s pipeline uses `try_send`; a subscriber that fills its
    256-slot channel is pruned and its channel closed, so one stalled consumer
    can no longer stall the pipeline or the other subscribers. Contiguity is
    preserved by disconnection rather than by backpressure — a dropped
    consumer never sees a hole, it sees the stream end and must resubscribe by
    cursor to have the ring replay the gap. SSE clients get this for free:
    `EventSource` reconnects with `Last-Event-ID`, subject to the amendment-9
    boundary caveat (the id is the chunk-start cursor, so the correct
    byte-exact resume is `?since=<start + len>`). The Tauri channel path has
    no close signal, so a dropped desktop terminal stays dark until the run
    restarts; unreachable in practice because the frontend's 1 MiB pause gauge
    throttles the reader long before 256 chunks go outstanding.
21. **Edge kinds are their own enum, and `loop` joins them.** `Edge.kind` is
    `EdgeKind` (`ask | send | loop`), no longer a `MessageKind`; wire values
    for `ask`/`send` are unchanged and a loop round travels as an `ask`
    message. Loop edges take an optional `max_rounds` (default 10, ≥ 1,
    loop-only) and the loop-edge subgraph must be acyclic — both enforced at
    freeze, as is the rule that a trigger edge cannot be `loop`.
22. **Obligations arm on non-downstream turns only, and loop replies re-arm.**
    A message from an agent the receiver has an edge TO (its delegate) never
    opens or merges the receiver's obligation turn — feedback does not
    re-obligate delegation; chains, triggers, user and HTTP arming are
    unchanged. One scoped exception to "replies never open a turn": a reply
    from a loop target re-arms the owner's loop step until `tempo done
    <target>` (`POST /v1/agents/{target}/loop-done`, caller identity via
    `X-CoreTempo-Agent`, `no_loop_edge` on a caller without one) or the
    `max_rounds` soft cap. At the cap the loop stops re-arming and the owner's
    next stable idle gets one nudge naming `tempo done`; round counters are
    in-memory and reset on restart, and a fresh arming turn restarts a done or
    capped loop. The primer now also forbids acknowledgement traffic and
    directs agents to externalize durable state to files.
23. **Per-agent CLI tool allowlisting: `AgentConfig.tools: Vec<String>`.**
    Serde-default empty; validated at freeze as bare binary names (ascii
    letters, digits, `.`, `_`, `-` only — no paths, spaces, or shell
    metacharacters), since each becomes a `Bash(<tool>:*)` allowlist entry.
    `settings_json(tempo_bin: &Path, tools: &[String]) -> String`
    (`core/src/pty/hooks.rs`) now takes `tools` and emits `Bash(tempo:*)`
    unconditionally plus one deduped `Bash(<tool>:*)` per entry under
    `permissions.allow`. `write_agent_settings_files` writes one file per
    agent — `<runs_dir>/<run_id>/agent-settings-<agent_id>.json` (mode 0600)
    — so each agent's generated settings allow only its own declared tools.
    `AgentEnv` gains `settings_paths: BTreeMap<AgentId, PathBuf>`, and spawn
    passes the matching path as `--settings` per agent.
24. **Trigger lifecycle reaches observers: enriched `workflow.completed` and
    snapshot trigger history (design 2026-08-07).** `workflow.completed` gains
    additive fields: `trigger_id: Option<String>` (the hub id, `t-<hex>`),
    `message: MessageId` (the kickoff), `output: Option<Value>` (present only
    when a `[trigger.output]` schema validated), and `reason: Option<String>` /
    `reason_code: Option<String>` (set only for `result = "failed"`; a timeout
    is already distinguished by `result = "timeout"`). `TriggerStatus.reason_code`
    becomes `String` (wire form unchanged) so `TriggerStatus`/`TriggerView`
    derive `Deserialize`, and `Snapshot` gains `triggers: Vec<TriggerView>` —
    the hub's insertion-ordered, 100-capped records. `Run` retains the
    `Arc<TriggerHub>` it constructs, and the desktop `on_start` kickoff
    registers in the hub (its `Origin::Http` hex is the hub id minus `t-`,
    replacing the unregistered `startup_id()` mint on that path only). The
    `http:<hex>` origin is not exclusive to trigger kickoffs: any authenticated
    `POST /v1/messages` without an `X-CoreTempo-Agent` header also gets
    `Origin::Http(<request-id>)` (`core/src/api/auth.rs`), so the UI's kickoff
    correlation can open a lifecycle row for a non-trigger HTTP message until a
    dedicated origin discriminator lands (tracked follow-up; a reload clears
    such rows since the snapshot reseeds from the hub).
