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
| `store/schema.rs` | DDL: `messages`, `runs` (`agent_events` dropped — amendment 30) |
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
    pub exit: Option<AgentExit>,        // set only when state == exited (amendment 42)
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
    AgentLifecycle { agent: AgentId, phase: LifecyclePhase, exit: Option<AgentExit> },

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
{"seq":7,"ts":"…","type":"agent.lifecycle","agent":"docs","phase":"exited","exit":{"code":1}}
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
    pub credential_store: Option<PathBuf>,
    // per-agent files: RosterEntry (amendment 46)
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
`{sender}` renders `Origin` as: `Agent(id)` → the bare id; `User` → `user`; `Http(_)` and
`Trigger(_)` → `http` (amendment 38: the trigger variant is an observer-side discriminator; the
`flow` clause is what the agent reads).

```text
ask   → [CoreTempo {id} from {sender} — reply expected] {body}
        Reply first with: tempo reply {id} --code 0 '<answer>' (--code 1 on failure), then continue.
reply → [CoreTempo reply to {id} from {replier} — code {code}] {body}
send  → [CoreTempo {id} from {sender}] {body}
```

(ask template is one injection: two lines joined by `\n`. The full protocol primer lives in the
generated `--append-system-prompt`, not in injections.)

Amendment 31 adds one variant: a **flow kickoff** renders `{sender}` as `{sender}, flow {name}`,
so `[CoreTempo m-a3f91c2e from http, flow nightly — reply expected] {body}` (and the same clause in
the `send` template). Nothing else changes position — the id still follows `[CoreTempo `.

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
    such rows since the snapshot reseeds from the hub). *Closed by amendment
    38: kickoffs carry `Origin::Trigger`.*
25. **Store run-scoping (multi-flow phase 1, 2026-08-12 spec §3).** `MessageId`
    is now `m-` + 16 lowercase hex (was 8). `Store::open(path: &Path, run_id:
    RunId) -> Result<Store, StoreError>` takes the scoping run id; every
    message and agent event the handle writes is stamped with it.
    `messages` and `agent_events` gain a nullable `run_id TEXT` column, added
    in place by `ALTER TABLE` on open for any database that predates it
    (v1.0.0 databases lack the column); `PRAGMA user_version = 1` marks a
    database once migrated. `MessageRecord` (the wire shape) is unchanged —
    `run_id` lives only in the store layer. `Store::pending_to_agent`/
    `Store::pending_asks` filter on `run_id = ?`, excluding NULL legacy rows,
    so concurrent runs sharing one database file never sweep each other's
    traffic and pre-migration rows are inert rather than misattributed.
    Consequence: non-terminal `messages` rows left by an earlier run —
    including every pre-migration NULL-`run_id` row — are no longer swept by
    any later run's restart handling; they stay non-terminal in the shared
    file until a future reconciliation pass.
    **Superseded in part by amendment 27**: that pass landed in phase 3 as
    `Store::reconcile_orphans`, which fails such rows on every open when they
    carry no `run_id` or belong to a run that stopped cleanly. Rows left by a
    *crashed* run are still kept, so the "stays non-terminal" wording holds
    only for those.
26. **Multi-flow phase 2 (2026-08-12 spec §1–2):** the top-level `[trigger]` is
    removed. `WorkflowFile` gains `flows: BTreeMap<FlowName, FlowConfig>`
    (`FlowConfig { agents, trigger, output }`; `TriggerConfig` no longer carries
    `output`). `AgentConfig` gains `concurrency: exclusive|shared` (default
    exclusive); `ServerSection` gains `max_concurrent_runs` (default 2, 1..=16,
    file-only). `FrozenWorkflow` drops `output` and gains
    `flows: BTreeMap<FlowName, FrozenFlow>` (`members`, `trigger_type`, `edge`,
    `message`, compiled `output`), `for_flow(&FlowName) -> Option<FrozenWorkflow>`
    (derived member-subset; hash/source unchanged), and
    `webhook_output()` (**deleted in amendment 29** — the in-turn repair gate
    reads a per-kickoff contract instead). The freeze hash covers the
    tempo.toml bytes plus every flow's `schema_file` bytes in flow-name order.
    `trigger::startup_kickoff`/`single_webhook_flow` return
    `Result<Option<...>, String>` and refuse multi-on_start / multi-webhook
    files until the scheduler and per-flow routes land (**both deleted in
    amendment 29**, along with `single_on_start_flow` and
    `conflicting_webhook_output`: nothing auto-fires, so no caller remains).
    Wire: `WorkflowFile`
    JSON now carries `flows` and per-agent `concurrency`
    (`app/src/lib/types.ts` `FlowModel` mirrors it).
27. **Multi-flow phase 3 (2026-08-12 spec §4–5):** serve schedules flows
    concurrently. `TriggerHub.in_flight` is keyed by `FlowName`:
    `try_begin(&FlowName)`, `begin(&FlowName, &str)`, `in_flight(&FlowName)`,
    `in_flight_by_flow()`; `finish` unchanged. New `core::locks::AgentLocks`
    (`new(&pool)`, `acquire(&BTreeSet<AgentId>) -> MemberGuards`, sorted
    acquisition, read=shared/write=exclusive); `ApiContext` gains
    `agent_locks: Arc<AgentLocks>` (warm lock table, spec §5) and `Run` keeps
    the same `Arc`: `Run::lock_flow(&FlowName) -> Option<MemberGuards>` is how
    a warm `on_start` kickoff (bare run and desktop alike) takes that table,
    holding the guards across the kickoff *and* its watcher, so both warm
    entry points serialize on an `exclusive` member. Router gains
    `total_pending_asks_among`/`open_turns_among` (member-scoped quiescence;
    the unscoped pair is removed). `Run::watch_inputs_for_flow` scopes a
    watcher to a flow's members + contract. `trigger::single_on_start_flow`
    mirrors `single_webhook_flow`; `startup_kickoff`'s mixed-file refusal
    (amendment 26's e70a3f4 follow-up) is narrowed, not removed: the
    flow-scoped batch watcher resolves disjoint agents and webhook flows
    with no output contract, but the router's in-turn repair contract stays
    workflow-wide, so `startup_kickoff` still refuses a webhook output
    contract whose target agent is also an on_start member (a7b4dcb) until
    4b's per-kickoff plumbing lifts it. Warm `fire_flow`
    pre-validates cheap invariants before lock acquisition (sync 4xx:
    unknown flow 404 `unknown_flow`, on_start 400, payload, per-flow 409);
    post-lock failures settle asynchronously as `kickoff_rejected`. Serve:
    `POST /v1/flows/{name}/trigger` added (404 `unknown_flow`); bare
    `POST /v1/trigger` shims to the single webhook flow (removed in 4b);
    health reports `queue_depth` (total) + `running`, `current_run_id`
    removed; `queue_full` is per flow. Store: `Store::open` runs under
    `spawn_blocking`; shutdown checkpoint busy → debug skip;
    `idx_messages_run_status` created post-migration; startup
    reconciliation fails non-terminal rows of NULL-`run_id` or stopped runs
    (#30; crashed-run rows deliberately kept). `list_agent_events` stays
    unscoped by decision (cross-run history is the point of the shared
    file).
28. **Multi-flow phase 4a (2026-08-12 spec §6–§8):** bare `coretempod run` /
    desktop ▶ Run is a warm whole-pool run; nothing auto-fires.
    `coretempod run <config> --flow <name>` spawns the flow's member subset via
    `FrozenWorkflow::for_flow` (on_start: fires the configured message holding
    the flow's locks, exits 0/1; webhook: warm with the flow armed); a subset
    run's API `WorkflowFile` view is narrowed to the frozen roster. Desktop
    commands `run_flows() -> Vec<FlowInfo { name, type, target }>` and
    `fire_flow(name) -> String` (the hub trigger id; errors: `unknown_flow`,
    `invalid_request` — webhook flows, and the shared-contract-target refusal
    relocated here from `startup_kickoff`, now caller-less until 4b deletes
    it — and `trigger_in_flight`). Canvas node ids are keyed:
    `§trigger:<flow>` / `§output:<flow>`; `addFlow` creates `flow-N` spanning
    the roster. `@coretempo/client` 2.0.0 requires `flow` and targets
    `POST /v1/flows/{name}/trigger` (the warm-run route lands in phase 4b;
    serve already answers it). `core::export::ExportTarget { Serve, Batch {
    flow }, WarmRun }` + `export_target(file, flow)` replace
    `template_trigger`; `tempo export --flow <on_start>` emits a `run --flow`
    batch unit.
29. **Multi-flow phase 4b (2026-08-12 spec §5):** warm and serve APIs expose
    `POST /v1/flows/{name}/trigger` (unknown name → 404 `unknown_flow` naming
    declared flows; a declared on_start flow → 400 pointing at `run --flow`;
    per-flow 409 `trigger_in_flight` warm, per-flow 429 `queue_full` serve) and
    `GET /v1/flows` → `[FlowView { name, type, target, queue_depth, running }]`.
    `Health` gains `queued: {flow: depth}` and `running`; `ServeHealth`'s
    `queue_depth` total is replaced by the same `queued` map. Bare
    `POST /v1/trigger` is removed; its 404 names the flows and the new route.
    `GET /v1/trigger/{id}` is unchanged (ids are global). The in-turn 422
    repair binds its contract per kickoff rather than per workflow, because a
    contract keyed by target agent cannot tell two flows' kickoffs apart once
    both target the same agent — the schema of whichever flow was declared
    first would gate the other's reply. So the router reads a per-kickoff
    contract (`Router::bind_kickoff_contract`, keyed by the kickoff's
    `Origin::Http` id, bound before `create_message`, dropped at settle); the
    addressee-match clause is gone and `FrozenWorkflow::webhook_output()` is
    deleted, so `Run::watch_inputs` returns `output: None` and flow scoping
    comes only from `watch_inputs_for_flow`. The shared-contract-target
    refusal (a7b4dcb,
    relocated to the desktop `fire_flow` in 4a) is retired: an on_start kickoff
    binds no contract, so a webhook flow's schema cannot reach it.
    `ActiveRun.kickoff` becomes `kickoffs: BTreeMap<FlowName, JoinHandle>` so
    `run_stop` aborts every fired flow's watcher. Deleted as dead:
    `trigger::startup_kickoff`, `trigger::conflicting_webhook_output`,
    `trigger::single_on_start_flow`, `trigger::single_webhook_flow`,
    `FrozenWorkflow::webhook_output`. Serve's listener outlives its queues, so
    a trigger arriving while the daemon drains gets 503 `shutting_down` rather
    than 429 `queue_full`; accept and close-and-drain share a per-flow
    interlock, so an accepted trigger is always either drained (failed
    `daemon_shutdown`) or refused.
30. **Capstone cleanups (issue #41).** The `agent_events` table is deleted, not
    deprecated: nothing outside its own tests ever read a row back, so
    `Store::insert_agent_event`, `Store::list_agent_events` and
    `store::AgentEventRecord` are gone with it, and amendment 27's
    "`list_agent_events` stays unscoped by decision" is moot. Migrations are
    append-only, so the drop is schema version 2: `store::migrate` (renamed from
    `migrate_run_id`) brings any database up to `SCHEMA_VERSION`, adding
    `messages.run_id` when `user_version < 1` and running
    `DROP TABLE IF EXISTS agent_events` when it is below 2. Amendment 25's
    "`messages` and `agent_events` gain a nullable `run_id`" therefore holds for
    `messages` only. Freeze hash: each contributing flow's name and `schema_file`
    bytes are appended behind their own length rather than concatenated raw, so
    two adjacent schema files can no longer build the input a different split of
    the same bytes would — the coverage amendment 26 describes is unchanged, but
    the hash of any workflow that declares a `schema_file` moves.
    `enable_wal` returns the
    journal mode the file ended up in (the pragma reports a refusal in its
    result row instead of failing) and `Store::open` warns on anything but WAL.
31. **A flow kickoff names its flow in the injected header (#42).** §3.2's
    `{sender}` gains an optional flow clause: a kickoff renders
    `[CoreTempo {id} from http, flow {name} — reply expected] {body}` (and
    `[CoreTempo {id} from http, flow {name}] {body}` for a `send` kickoff).
    Ordinary agent-to-agent and user-origin messages, and every reply, are
    unchanged — the label means "this is a flow kickoff", so an unlabelled
    header is never one. Both trigger types are labelled: `on_start` kickoffs
    bind no contract, but leaving them bare would keep exactly one ambiguous
    case, and one rule is cheaper for the agent than two. The name rides as a
    render-time argument, not on the record: `Router::create_kickoff(FlowKickoff
    { flow, from, to, kind, body })` sits beside the unchanged
    `create_message(from, to, kind, body)`, and `MessageRecord` keeps its §2.2
    shape (no store column, no API field, no `@coretempo/client` change).
    `Origin::Http` is *not* extended — its string form (`http:<id>`) is the
    store's `from` column, the API's `from` field, and the key
    `bind_kickoff_contract` uses, so a flow inside it would touch all three for
    a prompt-format change. `OutputContract` gains `flow: FlowName` (from
    `compile`), which is what lets the 422 name the schema that rejected:
    `render_rejection(errors, attempts_left, flow)` now reads "does not match
    the output schema of flow '<name>' (the flow named in that ask's
    [CoreTempo …] line)". The primer explains the labelled header, and each
    output-contract prompt block (amendment: commit 8206357) now points at its
    own flow's header rather than telling the agent to guess and repair off the
    rejection. Nothing parses the header back out: obligation turns, met-step
    recording and the auto-clear gate all key on `MessageRecord`
    (`create_message`'s own bookkeeping), and the only reader of injected text
    anywhere is the test fake agent's `m-[0-9a-f]+` match, whose position is
    unchanged.
32. **Parked-dialog signal + `allow` (#26, spec 2026-08-17 §3).**
    `AgentConfig.allow: Vec<String>` (verbatim permission rules appended
    after the `Bash(...)` entries); generated settings carry six hooks;
    `ReportedState::{Blocked, Unblocked}` and
    `ReportStateRequest.tool: Option<String>`;
    `PtySource::report_blocked/report_unblocked/blocked/blocked_count`;
    `EventPayload::AgentBlocked { agent, blocked, tool }` (`agent.blocked`);
    `AgentInfo.blocked: bool`, `Health.blocked: usize` — all serde-default,
    additive.
33. **MCP opt-in (#2, spec 2026-08-17 §2).** `AgentConfig.mcp: Vec<String>`
    (server names, serde-default `[]`); `pub type McpServers =
    BTreeMap<String, serde_json::Value>`; `FrozenWorkflow.mcp_servers:
    BTreeMap<AgentId, McpServers>` (only opted-in agents; joins `hash` in
    canonical JSON after the flow schema files); `AgentEnv.mcp_paths:
    BTreeMap<AgentId, PathBuf>`; `ConfigError::Mcp { agent, source: McpError }`;
    spawn args end `--strict-mcp-config [--mcp-config <path>]`. Additive.
34. **Trust preflight (#1, spec 2026-08-17 §1).** `ServerSection.trust_agent_dirs:
    bool` (serde-default false); `user_config::UserConfig { trust_agent_dirs }`
    from `~/.coretempo/config.toml` (`CORETEMPO_CONFIG` override), loaded by
    the binaries only; `trust::{trust_root, TrustStore, TrustError, TrustPolicy,
    preflight, TrustGate}`; `RunOptions.trust: TrustPolicy` (default no grant);
    `RunError::Trust(TrustError)`; `pty::SpawnGate` + `PtyManager::set_spawn_gate`;
    desktop command `run_untrusted_dirs(config_path) -> Vec<String>` and
    `run_start(config_path, trust_confirmed: bool)` — the confirmation becomes
    that run's `TrustPolicy`. Additive except the `run_start` parameter (its
    only caller is `app/src/lib/ipc.ts`).
35. **Serve token + body-less POSTs (#57).** `api::auth::TokenHint {Run,
    Serve}`, and `require_bearer(token, headers, hint)` takes it (its only
    outside caller is `daemon/src/serve.rs`, which passes `Serve`). The 401
    `unauthorized` body is unchanged in run mode; in serve mode it names
    `CORETEMPO_TOKEN`, `--token-file`/`CORETEMPO_TOKEN_FILE` and `[server]
    token_file` instead of `api.json`, which serve never writes.
    `coretempod serve` now fails at startup when
    `ResolvedServer::token_provisioned` is false, before it binds anything.
    The JSON content-type guard accepts a POST that declares no body (no
    `Transfer-Encoding`, absent or zero `Content-Length`) and no
    `Content-Type`; a declared non-JSON type, or a body without
    `Content-Type: application/json`, is still 415.
36. **The owed-ask watchdog (#55, #56, spec 2026-08-17 §4).** `MessageRecord`
    gains `reason: Option<String>`, `reason_code: Option<String>` (serde
    default, `null` unless `status = failed`); SQLite schema version 3 adds
    the two nullable `messages` columns additively, existing rows read back
    as `None`. `Router::fail_message` takes a `FailReason { code: &'static
    str, reason: String }`; codes are exactly `timeout | blocked_on_permission
    | agent_exited | agent_restarted`, plus `orphaned` from the startup
    orphan sweep (amendment 30's `reconcile_orphans`, unchanged otherwise).
    `pty::Blocked { since: tokio::time::Instant, tool: Option<String> }`
    replaces the handle's `blocked: bool`; `PtyManager::blocked_since` reads
    it and `StateSource` gains `fn blocked_since(&self, &AgentId) ->
    Option<Blocked>` (default `None`) so the router can read it without a PTY
    dependency. `report_blocked` now accepts a report at raw `working` **or
    `idle`** (still dropped at `starting`/`restarting`/`exited`); a repeat
    while already set is a no-op and does not move `since`; `unblocked`, a
    raw-state *change* to `working`/`idle`, restart, exit and shutdown all
    still clear it — an `idle` report no longer clears the flag when the
    agent is already idle, so a subagent's dialog stays visible after the
    parent's own `Stop` has already fired. `InjectionQueue` gains `fn
    reconsider(&self, &AgentId)` (default no-op) / `QueueCmd::Reconsider`: the
    worker re-runs `ClearGate::on_stable_idle` at debounced idle exactly as a
    state-transition would, minus the drain, and acts only on a `Nudge` — a
    poke never types `/clear` and a non-idle target is ignored. `Router`
    gains `WatchdogTiming { reply_nudge_backoff: [Duration; 4], blocked_grace:
    Duration }` with `DEFAULT_REPLY_NUDGE_BACKOFF = [60, 120, 240, 240]s` and
    `DEFAULT_BLOCKED_GRACE = 90s`, plus `Router::set_watchdog_timing` (test
    knob only — no config surface). `agent.stalled` keeps firing once per
    nudge round: the first idle observed after a nudge, while the reply is
    still owed; `agent.nudged` fires once per nudge sent. The 1 s TTL
    sweeper runs `sweep_expired` (unchanged) then a second pass over `owed`:
    an agent blocked past `blocked_grace` fails every owed ask
    `blocked_on_permission` naming the tool and does not touch the agent; an
    agent whose debounced state is `Exited` fails every owed ask `agent_exited`
    (previously these waited on TTL, since `drive_message` stops watching
    after `working`); otherwise, once an owed agent's backoff has elapsed,
    the sweeper calls `injector.reconsider` rather than nudging directly —
    the queue worker is still the only place a nudge or `/clear` is decided.
    `workflow.completed.reason_code` (trigger watcher) now prefers the
    record's `reason_code` when it is one of the four watchdog codes
    (`blocked_on_permission | timeout | agent_restarted | agent_exited`);
    otherwise it falls back to the watcher's own synthesised `agent_failed`
    as before. Additive throughout except `fail_message`'s new parameter
    (`InjectError` maps to `agent_exited`/`agent_restarted` in its place, no
    `inject_failed`).
    *Amendment 2026-08-18:* `ReportStateRequest` gains `agent_id:
    Option<String>` (serde default) and `pty::Blocked` gains `agent_id:
    Option<String>` — the hook payload's `agent_id`, `None` for the main
    session — carried on both the `blocked` and `unblocked` reports, and
    `PtyManager::report_unblocked(&AgentId, Option<String>)` clears the flag
    only when the two match, so a sibling Claude Code helper agent's
    `PostToolBatch` cannot cancel another agent's dialog. `sweep_owed`'s poke
    walk additionally pokes an owed agent with no `ReplyNudgeState` at all whose
    debounced state is `Idle` and whose oldest owed ask is older than
    `reply_nudge_backoff[0]` — the case a `HoldQuiet` at a blocked idle
    transition leaves behind — and never pokes a blocked agent.
37. **Isolated agent config (#67, spec 2026-08-24).** `AgentConfig` gains
    `isolated_config: bool` (serde default false) and `skills: Vec<PathBuf>`
    (serde default empty; paths relative to the `tempo.toml` directory,
    `~`-expanded at freeze like `dir`). `validate_workflow` rejects `skills`
    without `isolated_config`, blank or nameless entries, and duplicate
    basenames; `load_workflow` rejects a non-directory or a directory without
    `SKILL.md` (`ConfigError::Invalid`, path `agents.<id>.skills[<n>]`) and an
    unreadable or non-regular entry inside a skill dir (`ConfigError::SkillIo`
    / `Invalid`), with the texts in spec §1.
    `FrozenAgent`/`AgentConfig` in `FrozenWorkflow` carry the resolved skill
    paths. Freeze hash: after the MCP frames, per isolated agent with skills
    in agent-id order, `push_framed(agent_id)` then per skill in declaration
    order `push_framed(name)` + `push_framed(file_count as u64 BE)` + for
    each regular file in sorted relative-path order `push_framed(rel_path)`
    + `push_framed(bytes)` (name and path as raw OS bytes); any symlink or
    non-regular entry inside a skill dir fails the load, and any other IO
    failure (unreadable parent, entry or file) is `ConfigError::SkillIo`
    naming that entry. New module
    `core::claude_config`: `write_agent_config_dirs(runs_dir, run_id,
    &FrozenWorkflow) -> Result<BTreeMap<AgentId, PathBuf>, ClaudeConfigError>`
    creates `<runs_dir>/<run_id>/claude-config-<agent_id>/` (0700) holding
    `.claude.json` = `{"hasCompletedOnboarding":true}`, `settings.json` =
    `{"autoMemoryEnabled":false,"skipDangerousModePermissionPrompt":true}`,
    and `skills/<name>` symlinks — never a credentials file; returns the
    map for isolated agents only. `operator_credential_store() ->
    Option<PathBuf>` = `$CLAUDE_SECURESTORAGE_CONFIG_DIR`, else
    `operator_config_dir()`. `AgentEnv` gains `config_dirs:
    BTreeMap<AgentId, PathBuf>` and `credential_store: Option<PathBuf>`;
    `spawn_spec` adds `("CLAUDE_CONFIG_DIR", dir)` and, when
    `credential_store` is `Some`, `("CLAUDE_SECURESTORAGE_CONFIG_DIR",
    store)` to `env` for agents in the map and touches nothing otherwise.
    (Amended after PR #71: the original `credentials_source` symlink was
    replaced by Claude Code's temp+rename credential write, verified live on
    2.1.241.) `TrustGate::new(store, policy)` becomes
    `TrustGate::new(store, policy, mirrors: BTreeMap<AgentId, TrustStore>)`;
    `before_spawn` runs the operator-store preflight unchanged, then
    `grant`s the root into the agent's mirror store when one exists.
    `AgentDetail` gains `isolated_config: bool` and `skills: Vec<String>`
    (the frozen absolute paths). `RunError::SourceChanged`'s text names
    declared skills alongside schema files and MCP selections. Tauri
    `AgentModel`/`merge_agent` round-trip both keys (optionals only when
    non-default). Additive throughout.
38. **Trigger kickoffs carry their own origin (#24).** `Origin` gains
    `Trigger(String)`, wire form `trigger:<hex>` where the hex is the trigger
    hub id minus `t-`; `FromStr`/`Display`/serde round-trip it beside the
    existing forms and `OriginParseError` names it. Every flow kickoff —
    warm `POST /v1/flows/{name}/trigger` (`core/src/api/trigger.rs`), serve
    cold starts (`daemon/src/serve.rs`), `coretempod run --flow`
    (`daemon/src/main.rs`) and the desktop fire (`app/src-tauri/src/
    commands.rs`, which now goes through `Router::create_kickoff` so its
    header names the flow like the others) — is created with it.
    `Origin::Http(<request-id>)` remains what a plain authenticated
    `POST /v1/messages` gets and is never a kickoff: the output-schema gate
    (`bind_kickoff_contract` / `reject_off_schema` / `settle`) keys on
    `Trigger` only, so an HTTP ask that happens to share a hex is not gated.
    The injected `{sender}` for `Trigger` is still `http` (§3.2), so agents
    see byte-identical headers. UI: the Run tab and graph output box
    correlate on `trigger:` only; the feed chip renders `trigger:` as
    `trigger` and `http:` as `external`; `isExternal` accepts both. The
    amendment-24 looseness is closed. Additive on the wire; a client that
    parses `from` must accept the new prefix.
39. **`Run` gains `port()` (#8).** `Run::port() -> u16` returns the port the
    API listener actually bound — under `RunOptions::ephemeral_port` the one
    the kernel picked, not the configured `[workflow] port`. It is the same
    value written to `api.json` and handed to agents as `CORETEMPO_PORT`, so
    an in-process caller no longer reads api.json back to learn the address.
    Additive; nothing on the wire changes.
40. **The PTY SSE `id:` is the resume cursor, not the chunk start (#7).**
    `core/src/api/sse.rs` emits `id: <start + len>` — where the next chunk
    begins — while `data.seq` stays the chunk's first byte, so §6.2's "`id:`
    mirrors it" no longer holds and amendment 9's caveat is closed:
    `Last-Event-ID` and `?since=<cursor>` take the same value and both resume
    byte-exactly, with no re-delivered chunk. Amendment 20's parenthetical
    (resubscribe by `?since=<start + len>` after a lagging subscriber is
    dropped) is likewise superseded — the header alone is exact now. A client
    that stored the old `id:` as its resume point re-reads the last chunk once,
    which is what it already did. `/v1/events` is unchanged: bus events are
    single units, so `id: <seq>` and `replay_since(seq)` (strictly greater
    than) were already exact.
41. **`allowed_origins` removed (#6).** `ServerSection` drops
    `allowed_origins: Vec<String>`. It parsed since day one and was never
    read: no CORS layer exists anywhere, so the key emitted no header and
    only looked like a setting. `deny_unknown_fields` now makes a file that
    still sets it fail to load, and `validate_workflow` appends a note to
    serde's unknown-field error saying it was removed, that CoreTempo emits
    no CORS headers, and that a cross-origin browser goes through a reverse
    proxy serving the API same-origin (rewriting `Host`, which the API
    validates). Tauri `merge_server_section` no longer writes the key and
    the TS `WorkflowModel["server"]` mirror drops it. Breaking for any
    tempo.toml that set it; nothing behavioural changes, because nothing
    ever depended on the value.

42. **Signal deaths are reported as the signal, not as code 1 (#90).**
    `AgentInfo.exit_code: Option<i32>` and
    `AgentLifecycle { exit_code: Option<i32> }` become
    `exit: Option<AgentExit>`, with
    `enum AgentExit { Code(i32), Signal(String) }` serialised externally
    tagged: `{"code": 3}` or `{"signal": "Terminated"}`. `Signal` carries
    the name `strsignal(3)` gives (portable-pty converts the number before
    handing it back); `Code(-1)` is the pre-existing value for a `wait` that
    itself failed. `PtyManager::exit_code` → `PtyManager::exit(&AgentId) ->
    Result<Option<AgentExit>, PtyError>`, and the `api::PtySource` trait
    method renames with it. The `agent_events` table this doc's §5 once
    listed was already deleted (amendment 30), so nothing persists the
    value. Breaking on the wire for anything reading `exit_code` from
    `GET /v1/agents` or the `agent.lifecycle` event; `@coretempo/client`
    never exposed it. Desktop: `AgentExit = { code } | { signal }` in
    `types.ts`, and the dead-pane overlay reads `[exited 3]` /
    `[killed: Terminated]` (`exitLabel`).
43. **Stop and restart wait for the agent process to exit (#94).**
    `PtyManager::shutdown` and `PtyManager::restart` keep their signatures
    but now return only once the signalled `claude` has been reaped:
    SIGHUP, then SIGKILL after `pty::EXIT_GRACE` (5 s), then `wait`. The
    reaper thread completes a per-session `oneshot` after recording the
    exit, so `PtyManager::exit` is populated by the time `shutdown`
    returns. `Run::stop` is unchanged in shape and can now remove the run
    dir (`cleanup_run_dir`) without racing Claude Code's session-end
    write, and a restart no longer spawns the replacement into the managed
    `CLAUDE_CONFIG_DIR` while the old process is still writing to it.
    `Run::stop` may therefore take up to `EXIT_GRACE` per wedged agent
    (agents are reaped concurrently). `core` gains a direct `libc`
    dependency for the SIGKILL; portable-pty only sends SIGHUP.
44. **The `PermissionRequest` hook answers the dialog by default.**
    `AgentConfig` gains `on_permission_prompt: PermissionPrompt` (`enum
    PermissionPrompt { Deny, Wait }`, serde lowercase, default `deny`).
    Under `deny` the generated settings' `PermissionRequest` hook runs
    `tempo state refused` instead of `tempo state blocked`: it prints
    `{"hookSpecificOutput": {"hookEventName": "PermissionRequest",
    "decision": {"behavior": "deny", "message": …}}}` on stdout (exit 0
    always — the decision must not depend on the API) and reports
    `ReportedState::Refused` (`{"state":"refused","tool":…,"agent_id":…}`)
    best-effort. `PtySource`/`PtyManager` gain
    `report_refused(&AgentId, Option<String>)`, which logs at warn and
    publishes the new `EventPayload::AgentPermissionRefused { agent, tool }`
    (wire `agent.permission_refused`, `?agent=` filterable, never
    always-pass). No blocked flag is set and `agent.blocked` is not
    published for a refusal. `wait` keeps the pre-amendment behaviour
    exactly. Desktop `types.ts` carries the event; the UI does nothing
    with it yet. Files without the key freeze to the same hash (the hash
    covers file bytes).
45. **Refusals carry an input summary, and the desktop shows them.**
    `ReportStateRequest` gains `input: Option<String>` (with `refused`
    only): `tempo state refused` derives it from the hook payload's
    `tool_input` — the Bash `command`, else a `file_path`, else the whole
    input as compact JSON — capped at 200 bytes with a trailing `…`; the
    server caps at the same length. `PtySource::report_refused` and
    `PtyManager::report_refused` take `(agent, tool, input)`;
    `EventPayload::AgentPermissionRefused` gains `input: Option<String>`
    (wire key `input`, `#[serde(default)]`), and the warn line carries it.
    Desktop: `types.ts` `Refusal { tool, input, ts }`,
    `agentsState.refused: Record<agent, Refusal>` set by the event and
    cleared on resync, rendered as a ⛔ badge (roster and graph node) whose
    title is the refused tool and input plus the allow-rule hint.
46. **`PtyManager` is built from a `PtyRoster`, not a `FrozenWorkflow`**
    (spec 2026-08-27 §4). `core/src/pty/roster.rs`: `RosterEntry { cfg:
    AgentConfig, system_prompt: Option<String>, mcp: McpPolicy,
    settings_path: Option<PathBuf>, config_dir: Option<PathBuf>, token:
    Option<Token>, resume: Option<String> }`, `enum McpPolicy {
    Strict(Option<PathBuf>), Inherit }`, `PtyRoster { agents:
    BTreeMap<AgentId, RosterEntry>, idle_debounce: Duration }`
    (`PtyRoster::empty(debounce)`, `RosterEntry::new(cfg)`).
    `PtyManager::new(roster, bus, env)` / `new_with_program(roster, bus,
    env, program)`; `AgentEnv` keeps `port`, `token`, `tempo_bin_dir`,
    `credential_store` — the per-agent maps moved onto the entries. The
    spawn recipe reads the entry: `--append-system-prompt` only with a
    `Some` prompt, `--resume <id>` when set (consumed by the next spawn
    that succeeds — a spawn refused by the gate or failed in `open_pty`
    leaves it armed), `--settings` when set, `Strict(file)` = today's
    `--strict-mcp-config` plus optional `--mcp-config`, `Inherit` =
    neither flag, `CORETEMPO_TOKEN` = the entry's token else the env
    token, `CLAUDE_CONFIG_DIR` (+ credential store) only with a
    `config_dir`. `Run` builds one entry per frozen agent so a workflow
    run's argv and env are unchanged. New: `add_agent(id, entry) ->
    Result<(), PtyError>` (creates channels/workers, no spawn; must run
    inside a tokio runtime; `PtyError::AgentExists`),
    `set_resume(&id, Option<String>)`, `async stop(&id)` (one agent's
    `shutdown`: SIGHUP → SIGKILL after `EXIT_GRACE`, blocked flag cleared
    with `blocked: false`, exit recorded before return, handle/ring/
    subscribers kept; `PtyError::AgentExited` when nothing is live),
    `async remove_agent(&id)` (stop if live, then closes output
    subscribers explicitly — `hub.subscribers.clear()` — before dropping
    the handle: queue worker, write pump and state subscribers all end).
    `PtyError::UnknownAgent`'s text is now "unknown agent '<id>'; not in
    the roster". Workflow runs call none of the new methods.
47. **Session manager core, daemon and API** (spec 2026-08-27 §2, §3,
    §5, §6, §9). Types (`core/src/types/session.rs`, types-only build):
    `ProjectId` (`p-` + 8 hex), `SessionState { starting, idle, working,
    stopped, exited }`, `WorktreeStatus { present, missing, none }`,
    `WorktreeInfo { path, branch, base }`, `BlockedView { tool, since }`,
    `ProjectView`, `SessionView` (every §2 field plus `state`, `blocked`,
    `exit`, `pty_cursor`, `branch`, `changed_files`, `ahead`,
    `worktree_status`), `CreateProjectRequest { path, name? }`,
    `CreateSessionRequest { project, worktree, cwd?, title?, prompt?,
    model?, permission_mode?, isolated_config }`, `ResumeResponse {
    session, resumed }`, `DeleteSessionResponse { branch_kept }`,
    `SessionCounts { live, total }`, `SessionsHealth { ok, sessions }`,
    `SessionsApiFile { port, token, pid }`. `ReportStateRequest` gains
    optional `claude_session_id` (forwarded by `tempo state` from every
    hook payload's `session_id`; runs ignore it, the daemon stores the
    latest). Events: `session.created|stopped|deleted { agent }`,
    `session.resumed { agent, resumed }` (pass `?agent=`),
    `project.registered|forgotten { project }` (always pass).
    API split: `ApiCore { pty, bus, roster: Arc<dyn Roster>, auth:
    Arc<dyn TokenAuth>, token_provisioned, bind, port, started_at,
    started }`, `ApiContext { core, router, workflow, workflow_file,
    run_id, triggers, agent_locks, stopping }` (`FromRef`).
    `trait Roster { contains, ids, on_claude_session_id }` (impl for
    `FrozenWorkflow` and `SessionManager`); `trait TokenAuth { classify
    -> Caller::{Operator, Hook(AgentId), Unknown}, hint }`
    (`OperatorToken` for runs). Guard: `Hook(id)` may only `POST
    /v1/agents/{id}/state` (403 `forbidden_scope` elsewhere); a hook
    token's identity beats `X-CoreTempo-Agent` (403 `wrong_agent` on
    mismatch); `TokenHint::Sessions`; raw-body `POST …/pty` skips the JSON
    content-type check. `core::api::auth::write_private_file` is now
    `pub` — the daemon writes its own `api.json` with it. `PtySource`
    gains `write`, `resize`, `pause`.
    Shared routes (`shared_routes()`): `POST /v1/agents/{id}/state`,
    `GET /v1/events`; `pty_routes(prefix)`: `GET|POST {prefix}/{id}/pty`
    (POST = raw bytes, 204), `POST …/pty/resize { cols, rows }` (204),
    `POST …/pty/pause { paused }` (204), 409 `agent_exited` with no live
    session — mounted at `/v1/agents` on runs and `/v1/sessions` on the
    daemon. `serve_app(listener, app, bind, token_provisioned)`.
    `PtyManager::agent_ids()`, `is_live(&id)`; a mid-spawn `UnknownAgent`
    reaps the orphan. `TrustStore::project_keys` / `grant_with_keys`
    / `derive_project` (grant + keys in one read-modify-rename;
    `-> bool`, false when the entry already said that and nothing
    was written).
    `core/src/pid.rs`: `pid_alive(pid) -> bool`, an unconditional module
    — `libc` is no longer an optional dependency of `core` — shared by
    `coretempod sessions stop` and `tempo session` discovery (pid 0 is
    never alive).
    `core/src/sessions/`: `SessionsRoot { dir, worktrees }`
    (`from_home`, `at`), `SessionStore` (own `sessions.db`, 0600, WAL,
    `user_version` 1, schema of spec §9 plus `exit_code`/`exit_signal`),
    `worktree::{create, status, ahead, is_listed, remove, prune,
    delete_branch_if_unmoved, toplevel, slug}`, `SessionTrustGate`
    (`SessionTrust { project_root, derived_worktree, mirror }`; derived
    grants copy `MCP_APPROVAL_KEYS`), `files::{write_session_files,
    remove_session_files}`, `SessionManager::boot(SessionManagerInputs)`
    with `register_project`, `list_projects`, `forget_project`, `create`,
    `get`, `list`, `stop`, `resume`, `delete(id, remove_worktree, force)`,
    `record_claude_session_id`, `counts`, `begin_shutdown` (arms the
    `stopping` flag alone; the daemon calls it before the API winds
    down), `shutdown`; `SessionError`
    (`unknown_session` 404, `unknown_project` 404, `project_exists` 409,
    `project_in_use` 409, `not_a_git_repo` 422, `cwd_outside_project` 422,
    `cwd_missing` 422, `wrong_state` 409, `worktree_missing` 409,
    `dirty_worktree` 422, `untrusted` 409 (create *and* resume re-check the
    project root), `git_failed` 422, `spawn_failed` 500, `shutting_down` 503
    for a create/resume that reaches the manager after shutdown began).
    `stop` on a session whose child left first records `exited` rather
    than failing; `delete_branch_if_unmoved` on a branch that no longer
    exists is `Ok(false)` (`branch_kept: false`), not `git_failed`;
    `SessionStore::open` refuses a `user_version` above 1
    (`SessionStoreError::Schema`). Known gap: the derived `projects[<worktree>]` entries a
    worktree session writes into the operator's `.claude.json` are not
    removed on delete or on a rolled-back create. `POST
    /v1/agents/{id}/state` with the operator token still requires
    `X-CoreTempo-Agent: <id>` (the run rule, unchanged); a hook token
    needs no header and may repeat its own id. `core/src/api/sessions.rs`:
    `SessionsApi { core,
    sessions }`, `build_sessions_router`; routes as spec §6 with `POST
    /v1/sessions` → 201, `DELETE /v1/projects/{id}` → 204.
    `coretempod sessions [--root DIR] [--port N] [stop]`: loopback,
    generated token (so `serve_app` is called with `token_provisioned:
    false`), `api.json` 0600 with pid (removed on clean exit),
    `sessions.lock` `flock`ed (second start exits 1 naming pid/port),
    `daemon.log`; `--root` overrides `~/.coretempo/sessions` (worktrees
    then under `<root>/worktrees`). Both `sessions stop` and that
    second-start refusal treat the lock — probed with a non-blocking
    `flock` — as the authority for "a daemon holds this root": `api.json`
    alone never triggers a signal, because a daemon that crashed leaves
    it behind naming a pid the kernel recycles. `coretempod` loads a
    workflow only under `run`/`serve`; `tempo` resolves a run connection
    only for run-scoped commands.
48. **`tempo session`** (spec 2026-08-27 §7). `tempo session [--root DIR]
    new <project-path> [--worktree] [--cwd DIR] [--title T] [--prompt P]
    [--model M] [--permission-mode PM] [--isolated-config]` (prints the id,
    then the branch for a worktree), `list` (tab-separated `id project
    branch state changed ahead title`, `-` for absent, `blocked` shown
    while the flag is set), `show <id>` (JSON), `stop <id>`, `resume <id>`
    (`resumed conversation <id>` / `started fresh`), `rm <id>
    [--remove-worktree] [--force]` (`branch kept` when it was), `attach
    <id>` (raw passthrough, `Ctrl-]` detaches; exit 0 on detach, 1 when the
    session exits or is not live), `projects [rm <id>]`. Discovery reads
    `<root>/api.json` only (default `~/.coretempo/sessions/api.json`) and
    treats a dead pid as no daemon; only a `NotFound` on that file reads as
    "no session daemon running", and any other io error is reported as
    itself (`cannot read <path>: …`), or the operator would go start a
    daemon that is already running. Exit statuses: 0, 1 (attach as above),
    3 (usage, transport, or an API refusal — the server's message printed
    verbatim).
49. **Desktop Sessions mode** (Spec B, 2026-09-01). New Tauri commands
    `sessions_status`, `session_list|create|stop|resume|delete`,
    `project_list|register|forget`, `session_subscribe_pty(session, resume)`,
    `session_unsubscribe_pty`, `session_write_pty|resize_pty|pause_pty` — thin
    proxies over the sessions daemon's `/v1` routes; request/response bodies
    are the amendment-47 wire types verbatim. `sessions_status` returns
    `{ state, health }`, `state` one of `idle|starting|connected|unreachable`
    (`health` populated only once `state` is `connected` and the daemon
    answered); opening Sessions mode is what calls it, and that call is what
    starts the daemon hunt. Two Tauri events: `coretempo:session-event`
    (daemon `/v1/events` payloads, forwarded verbatim, plus one
    shell-synthesized member the daemon never sends —
    `{"type":"pty.stream_error","agent":"<session id>","message":…}`,
    emitted when a session's PTY stream fails to open or dies mid-flight; a
    stream the daemon closes cleanly stays silent, since that is already a
    `session.stopped` event) and `coretempo:sessions-status`
    (`{ state: "starting"|"connected"|"unreachable" }`, shell-originated —
    three states, `idle` is not broadcast). PTY cursors are shell-owned (SSE
    `id:` = start+len); `resume: false` resets them. `uiState.mode ∈
    workflows|sessions`. The term manager is a per-transport factory
    (`createTerminalManager`); workflow behaviour unchanged. Release bundles
    ship `coretempod` as `externalBin` (`app/src-tauri/scripts/prepare-sidecar.sh`
    copies the built binary to `binaries/coretempod-<target-triple>` before
    `tauri build`, which resolves the sidecar path at compile time — even
    `--no-bundle` fails without it present); the shell spawns it via
    `std::process::Command` in its own process group, never
    `tauri-plugin-shell`. `Discovery::production` finds the binary at
    `$CORETEMPOD_BIN` when set, else beside the running app binary — `./dev`
    builds `coretempod` and exports `CORETEMPOD_BIN` before `pnpm tauri dev`.
