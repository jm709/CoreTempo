# CoreTempo — Design Spec

Date: 2026-08-01
Status: approved pending user review

## 1. Overview

CoreTempo is a lightweight Tauri 2 desktop app (with a headless export) that runs
multi-agent Claude Code workflows. It spawns `claude` sessions in PTYs it owns,
routes messages between agents by typing into their prompts, and shows everything
in a fast, terminal-centric UI.

V1 delivers:

- Agent-to-agent communication by direct PTY injection (no MCP anywhere).
- A `tempo` CLI + local HTTP API as the transport; replies routed
  deterministically server-side by message token to any origin (agent, human UI,
  external HTTP caller), always logged to SQLite.
- A desktop UI: terminal grid, message traffic feed, agent roster, human chat
  panel, workflow editor.
- Workflows defined in a frozen `tempo.toml`; a headless `coretempod` binary
  runs the identical core with no UI (export target for background/cloud).

Non-goals for v1: cloud hosting tests, TLS in the daemon, agent roster mutation
mid-run, message cancel/delete, pause/interrupt endpoints, free-form pane
dragging, scoped per-agent tokens.

## 2. Architecture

Cargo workspace, one process per run (desktop app or headless daemon):

```
coretempo/
├── core/     lib crate — PTY manager, message router, agent-state detector,
│             reply sinks, SQLite store, axum HTTP API, event bus. ZERO UI deps.
├── app/      Tauri 2 shell — adapter: core event bus → Tauri events/channels;
│             UI actions → core calls. Svelte 5 + xterm.js webview.
├── cli/      `tempo` binary — thin blocking HTTP client agents call via Bash.
└── daemon/   `coretempod` binary — core + ~50-line main. Headless export.
```

Dependency rule: `app`/`daemon`/`cli` depend on `core` (cli only on shared
types); `core` depends on nothing UI-related. The event bus that feeds Tauri is
the same bus the SSE endpoint serves; both serialize the identical serde `Event`
enum, so frontend code is portable to a future remote UI.

```
┌───────────────────── CoreTempo (one process) ─────────────────────────┐
│  Webview UI (Svelte 5 + xterm.js)                                     │
│    ▲ Tauri channels (PTY bytes) + events (lifecycle)  │ commands      │
│  ┌─────────┐   inject prompt   ┌────────┐  resolve    ┌───────┐       │
│  │   pty   │ ◄──────────────── │ router │ ───────────►│ sinks │       │
│  │ manager │                   └───▲────┘             └───────┘       │
│  └────┬────┘                       │        every msg → SQLite        │
│       ▼ spawns                 ┌───┴────────────────────┐             │
│   claude (PTY) ──tempo CLI──►  │ axum on 127.0.0.1:PORT │ ◄── curl /  │
│   claude (PTY)   over HTTP     │  /v1/* REST + SSE      │    scripts  │
└───────────────────────────────────────────────────────────────────────┘
```

## 3. Messaging model

### 3.1 Message kinds

- **`ask`** — reply expected. Fully async for agents: `tempo ask B "msg"`
  returns the message id immediately; the asker ends its turn. When B replies,
  the server injects the reply into the asker's PTY as a new prompt (the asker
  retains context because it is not auto-cleared while asks are pending).
- **`send`** — fire-and-forget. No reply token. The "response" is the observed
  state transition of the target (`injected → working → done`), streamed to the
  origin. The receiving agent does nothing special.

### 3.2 Message record (canonical shape, returned by all endpoints)

```json
{
  "id": "m-a3f9",
  "kind": "ask",
  "from": "agent:planner",
  "to": "builder",
  "body": "Is the schema migration done?",
  "status": "replied",
  "code": 0,
  "reply": "Yes, migration 004 applied and tested.",
  "created_at": "2026-08-01T17:03:11Z",
  "injected_at": "2026-08-01T17:03:12Z",
  "completed_at": "2026-08-01T17:04:40Z"
}
```

- `status`: `queued → injected → working → replied | done | failed`.
  Terminal states: `replied` (ask), `done` (send), `failed` (target exited or
  restarted before completion, or ask TTL expired).
- `code`: 0 | 1, set by the reply. `code`/`reply` are null until `replied`.
- `from` is derived server-side from auth context (`agent:<id>` via
  `X-CoreTempo-Agent`, `user` for in-process UI, `http:<req-id>` otherwise;
  flow kickoffs are minted internally as `trigger:<hex>`, contracts
  amendment 38) — never trusted from the request body.
- Ask TTL: per-workflow `ask_timeout_minutes` (default 30). Expiry moves the
  message to `failed`, emits an event, and decrements the asker's pending count.

### 3.3 Reply sinks

Routing on reply/completion is determined by the origin of the message:

| origin | sink behavior |
|---|---|
| agent | inject reply into the asker's PTY (unless asker restarted/cleared since — then log + event only, no injection) |
| UI chat panel | event to the panel (in-process bus) |
| external HTTP caller | resolves that caller's `?wait` long-poll |
| all | always written to SQLite regardless — the log is the floor, not a sink you pick |

### 3.4 Injection templates

The server is the only writer to any PTY. All injections for one agent flow
through one serialized queue.

- ask → target:
  `[CoreTempo m-a3f9 from planner — reply expected] <body>` followed by the
  instruction: reply first with `tempo reply m-a3f9 --code 0 '<answer>'`, then
  continue.
- reply → asker: `[CoreTempo reply to m-a3f9 from builder — code 0] <body>`
- send → target: `[CoreTempo m-b7c2 from planner] <body>`

The full protocol primer (what tokens mean, how to reply) lives in each agent's
generated `--append-system-prompt`, keeping injected messages short.

## 4. PTY & agent lifecycle

### 4.1 Spawn recipe

Per agent from the frozen workflow: launch `claude` in a PTY
(`portable-pty`) at `dir`, with `--append-system-prompt` (role prompt + tempo
protocol primer), optional `--model` / `--permission-mode`, and env
`CORETEMPO_AGENT_ID`, `CORETEMPO_PORT`, `CORETEMPO_TOKEN`, with `tempo` on
PATH.

### 4.2 State machine and detector

```
starting → idle ⇄ working → exited        (+ restarting, via manual restart)
```

(`exited` is the API state name; the UI labels it "dead".)

- `working` = Claude Code activity indicator present in the output stream
  (spinner / "esc to interrupt"); `idle` = prompt marker without activity.
- **Debounce**: raw transitions are emitted on the event bus immediately (UI
  shows truth), but all *actions* (auto-`/clear`, `send` completion, injection
  gating) use a debounced signal requiring 2 s of stable idle (default,
  tunable per workflow via `idle_debounce_seconds`). A false idle
  firing `/clear` mid-task is the worst failure in the system.
- No auto-restart on exit in v1. Restart is a manual action (UI button or
  `POST /v1/agents/{id}/restart`) that respawns from the same frozen config.

### 4.3 Injection queue rules

- **Inject only when the target is debounced-idle.** Messages wait in the
  per-agent queue otherwise. (We do not rely on Claude Code's mid-turn input
  queue: mid-`/clear` or mid-menu injection corrupts state, and gating makes
  `injected_at` meaningful.)
- **Auto-`/clear`**: on a debounced `working → idle` transition, if
  `pending_asks(agent) == 0` (asks *sent by* that agent not yet terminal) AND
  its injection queue is empty, the server types `/clear`. Ordering rule is
  strict drain-then-clear: the check happens inside the serialized queue at
  injection time, so a reply racing the idle transition cannot lose to
  `/clear`. This ordering gets a dedicated test.
- Agents with `auto_clear = false` in the workflow are never auto-cleared.
- **Restart semantics**: in-flight and queued messages *to* a restarted agent →
  `failed`. Pending asks *from* it: replies are logged + evented, not injected
  (§3.3).

### 4.4 Output buffering and replay

- Reads from each PTY are coalesced and flushed on whichever comes first:
  **8 ms elapsed or 32 KB accumulated** — this collapses TUI redraw storms.
- Core keeps a **per-agent replay ring buffer (~256 KB) with monotonic byte
  cursors**. Consumers (webview after reload, remote PTY endpoint) subscribe
  with a cursor and receive the tail before going live — no gap, no duplicate.
- Backpressure: the UI tracks unparsed bytes per terminal
  (`term.write` callback); past ~1 MB it signals core to pause reading that
  PTY.

## 5. Workflow definition (`tempo.toml`)

```toml
[workflow]
name = "core-tempo-dev"
db = "./tempo.db"
port = 4820
ask_timeout_minutes = 30

[agents.planner]
dir = "~/projects/CoreTempo"
prompt = "You are the planning agent…"     # --append-system-prompt (+ primer)
model = "opus"                              # optional
auto_clear = true                           # default true

[agents.builder]
dir = "~/projects/CoreTempo"
prompt = "You implement tasks sent to you…"
permission_mode = "acceptEdits"             # optional passthrough
```

- Parsed and **frozen at run start**; the roster is immutable for the life of a
  run. Change = stop → edit → rerun. The same file boots identically under
  `coretempod run tempo.toml`.
- Server-level settings (bind, port, db path, token file, log level) obey
  precedence `flags > CORETEMPO_* env > tempo.toml`. Agent definitions come
  from the file alone — env-overridable agents would break run reproducibility.

## 6. HTTP API

Base `http://127.0.0.1:<port>/v1`, all JSON.

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
| `GET /v1/health` | Liveness only: `{status, version, run_id, uptime_secs}` — unauthenticated | `200` |

### 6.1 Long-poll (`?wait`)

`GET /v1/messages/{id}?wait=30` blocks up to N seconds for a terminal status
and returns `200` with the current record either way — callers distinguish by
`status`, not HTTP code, and re-poll in a plain loop. `wait` capped at 300 s
(proxy-friendly). Implementation: subscribe to the broadcast bus filtered by
id, race a timeout, then read the record from SQLite.

### 6.2 Reply idempotency

- First reply: `200`, sink fires exactly once.
- Identical replay (same `code` + `body`): `200`, no re-injection — Bash-retry
  safe.
- Conflicting replay: `409 already_replied`. Reply to a send: `409 not_an_ask`.
  Reply from a non-addressee: `403 wrong_replier`.
- Message creation has no idempotency key in v1 (duplicate asks are visible,
  low-harm log noise; `Idempotency-Key` is a v2 candidate).

### 6.3 Errors

```json
{ "error": { "code": "unknown_agent",
             "message": "no agent named 'buidler'; roster: planner, builder, reviewer" } }
```

Messages are written for an LLM audience (include the roster, valid statuses,
the fix) — the CLI prints them verbatim to the calling agent.

### 6.4 Event stream (SSE)

- `GET /v1/events`: fat events (full record snapshots), monotonic per-run
  `seq` mirrored in the SSE `id:` field. Types: `run.started`, `agent.state`,
  `agent.lifecycle`, `message.created`, `message.status`, `bus.reset`.
  Filters via `?types=message.*&agent=builder`.
- Replay: ~1024-event in-memory ring; `Last-Event-ID` (or `?since=`) replays
  forward; aged-out or `broadcast::Lagged` ⇒ emit `bus.reset` and the client
  re-snapshots via REST. Keep-alive comments every ~15 s;
  `X-Accel-Buffering: no`.
- **PTY output is never on this bus.** `GET /v1/agents/{id}/pty` streams
  base64-encoded raw chunks (`{"seq":N,"b64":"…"}`) with ring-buffer replay on
  connect. Base64 because split escape sequences / partial UTF-8 must survive
  transit. `seq` is the chunk's first byte; the SSE `id:` is the cursor to
  resume at (`start + len`), so `Last-Event-ID` and `?since=` both resume
  byte-exactly. The in-process Tauri channel path is cursor-exact too.

### 6.5 Auth & security

- **Per-run bearer token, always required** (except `/v1/health`): 32 random
  bytes hex, generated at startup, constant-time compared (`subtle`).
  Rationale: any webpage can POST to localhost — an unauthenticated API here is
  a prompt-injection cannon; other local users are also locked out.
- Distribution: env into agent PTYs; `{port, token, run_id}` written 0600 to
  `~/.coretempo/runs/<run_id>/api.json` + `current` symlink (also solves
  multi-run port discovery — `tempo` never scans ports).
- Reject non-JSON `Content-Type` (`415`) and validate `Host` — kills browser
  preflight-less requests and DNS rebinding. No CORS support at all: the API
  never emits CORS headers and has no config to make it. A browser caller
  reaches it same-origin, or through a reverse proxy that serves the API under
  the page's own origin and rewrites `Host` to one the API accepts.
- Default bind `127.0.0.1`. Non-loopback bind requires an explicitly
  provisioned token (env or `token_file`) or the daemon refuses to start. No
  TLS in the daemon — remote exposure goes behind Caddy/nginx/Tailscale.

## 7. `tempo` CLI

Commands: `tempo ask <agent> <msg>` · `tempo send <agent> <msg>` ·
`tempo reply <id> --code 0|1 <msg>` · `tempo agents` · `tempo status <id>`.

- With `CORETEMPO_AGENT_ID` set: `ask` returns the id immediately (reply comes
  later by injection). Without it (humans/scripts): `ask` blocks via `?wait`
  loop by default; `--no-wait` opts out. `--wait` forces blocking for agents'
  scripts if ever needed.
- Reads `CORETEMPO_PORT`/`CORETEMPO_TOKEN` from env, falling back to
  `~/.coretempo/runs/current/api.json`.
- Built on `ureq` (`default-features = false`, `json`) — blocking, tiny tree,
  **no TLS stack at all** (loopback-only), because cold-start dominates: agents
  exec this binary constantly.

## 8. Tauri shell (`app` crate, Rust side)

- Subscribes to the core bus **in-process** (`broadcast::Receiver<Event>`) and
  forwards to the webview: low-rate events (message lifecycle, agent state, run
  state) via Tauri events; **PTY bytes via Tauri Channels
  (`tauri::ipc::Channel`, raw `Vec<u8>`)** — the event system JSON-encodes and
  may reorder, which corrupts terminal streams.
- Commands: `snapshot()` (roster + states + recent messages + per-agent PTY
  cursors), `subscribe_pty(agent, channel, since_cursor)` (ring-buffer replay
  then live), `write_pty(agent, bytes)` (user typing), `run_start/run_stop`,
  `restart_agent`, workflow file open/save/validate.
- The desktop app does not dogfood HTTP loopback; the core boundary is proven
  structurally (no Tauri dep in core) and behaviorally by `coretempod` +
  integration tests against the HTTP/SSE surface.

## 9. UI (webview)

### 9.1 Stack

Svelte 5 (runes) + Vite 8; xterm.js 6 lazy-loaded when a run starts (app shell
paints < ~100 ms). Terminal renderer: try WebGL addon, auto-fall back to DOM on
context loss or failed init; one-time frame-time probe flips the persisted
default if WebGL is secretly software-rasterized (webkitgtk masks this).
Document `WEBKIT_DISABLE_DMABUF_RENDERER=1` for WSLg glitches. All 2–6
terminals stay mounted and live (hidden ones render nothing but keep absorbing
writes); `scrollback` from the workflow file (default 5 000).

### 9.2 Layout

Three-region shell; right dock is tabbed Feed/Chat; center swaps between
terminal grid (running) and workflow editor (stopped) — "roster frozen during a
run" is structural, not a validation error.

```
┌────────────────────────────────────────────────────────────────────────────┐
│ ◉ CoreTempo   my-workflow.toml ▾        ▶ Run          agents 3/4 · 12:41 │
├──────────┬──────────────────────────────────────────┬──────────────────────┤
│ AGENTS   │ ┌────────────────────┐┌─────────────────┐│ FEED │ CHAT          │
│ ● lead   │ │ ▸ lead      working││ ▸ api      idle ││──────┴───────────────│
│ ● api    │ │ $ reviewing api…   ││ $ ▂             ││ 12:40:52             │
│ ◐ ui     │ └────────────────────┘└─────────────────┘│ lead → api    ask    │
│ ✕ docs   │ ┌────────────────────┐┌─────────────────┐│ "spec for /runs?"    │
│  restart │ │ ▸ ui       working ││ ▸ docs     dead ││ ⟳ working            │
│          │ │ $ building grid…   ││ [exited 1]      ││ …                    │
│          │ └────────────────────┘└─────────────────┘│                      │
├──────────┴──────────────────────────────────────────┴──────────────────────┤
│ ⌘1–9 focus terminal · ⌘` release · ⌘E edit workflow          ⏺ run 14m 02s │
└────────────────────────────────────────────────────────────────────────────┘
```

- Grid auto-layout: 1 full · 2 side-by-side · 3–4 2×2 · 5–6 3×2; double-click
  header / `⌘Enter` maximizes (others `display:none`, buffers stay hot). No
  free-form dragging in v1.
- Chat is the feed filtered to `human ↔ agent` with an input box; external
  (HTTP-origin) messages appear in the feed with an `external` chip — same
  rendering path.
- Feed → terminal linkage: click a feed item → focus recipient's terminal +
  200 ms border flash (scroll-to-injection-marker is a fast-follow). Hover
  highlights sender/recipient in roster.
- Focus model: click into pane = keys captured (accent border); `⌘`` releases
  (never `Esc` — Claude Code uses it); `⌘1–9`/`⌘F`/`⌘T`/`⌘E`/`⌘R` app-scope.
  Footer permanently shows the three keybindings that matter.
- Edge states: no-workflow card (open/new/recents); dead agent keeps last
  screen dimmed 60% + exit code + restart overlay; stopping dims panes until
  confirmed, then switches to editor.

### 9.3 Aesthetic: "instrument panel"

Studio-equipment restraint; chrome recedes, ANSI colors are the loudest thing.

- Type: JetBrains Mono (terminals — NL no-ligature variant — and ALL data UI:
  names, timestamps, states, dirs, keybindings, TOML editor); IBM Plex Sans for
  prose only. 13 px terminals, 12 px mono data, 13 px prose, 11 px uppercase
  mono labels at 0.08 em tracking.
- Palette (dark-first):
  `--bg #101214` · `--panel #16191C` · `--panel-edge #262B30` (1 px borders,
  no shadows) · `--terminal-bg #0C0E10` (inset-glass) · `--text #D8DEE4` ·
  `--text-dim #7C8691` · `--accent #E2A85F` (warm amber — the identity move;
  focus, active tab, Run, captured border) · `--ok #7FB069` ·
  `--busy = accent` · `--info #6699CC` · `--err #C4585A`. ANSI theme tuned so
  blue/green/red sit in the same hue families.
- Status glyphs everywhere: `●` working (1.2 s opacity pulse — the only looping
  animation) · `◌` idle · `◐` starting · `✕` dead; codes as mono chips `∅0`/
  `∅1`; lifecycle `○ → ⟳ ∅0|✓`.
- Motion only on state change (150 ms feed fade, 200 ms border flash, 120 ms
  grid⇄maximize). Zero animation on launch, tab switches, insertion, hover.

### 9.4 State management

Plain runes in `.svelte.ts` modules (no store library); one `wireEvents()`
reduces bus payloads into state. Terminals bypass reactivity: channel →
`term.write(Uint8Array)` (no JS-side decode). Reload mid-run = snapshot
(`invoke("snapshot")`) → subscribe events (dedup by seq) + PTY channels with
cursors (core replays tails). Feed virtualization: `virtua` (reverse/
stick-to-bottom mode).

## 10. Headless export (`daemon` crate)

- `coretempod`: clap-parse `--config --bind --port --db --token-file`, init
  `tracing_subscriber` (`CORETEMPO_LOG` env filter), load + freeze workflow,
  run core, clear non-zero exit on startup failure. No daemonization — process
  supervision belongs to systemd/Docker.
- Build: static musl (`x86_64`/`aarch64-unknown-linux-musl`) via
  cargo-zigbuild/cross — painless (no TLS deps anywhere; rusqlite `bundled`;
  portable-pty fine on musl). ~5–10 MB artifact. Same for `tempo`.
- `tempo export` (or app action) emits a directory: `tempo.toml` + systemd
  *user* unit template (agents need the user's credentials/home) + Dockerfile
  whose real job is the agent runtime (Node 22 + `@anthropic-ai/claude-code` +
  git; `ANTHROPIC_API_KEY` at run time; `--bind 0.0.0.0` inside the container
  is correct, paired with a required token).

## 11. Persistence (SQLite)

- `rusqlite` (`bundled`), WAL mode, one dedicated writer thread — never on
  tokio workers.
- Tables (v1): `messages` (the canonical record, §3.2), `runs`
  (run_id, workflow name+hash, started/stopped), `agent_events` (lifecycle
  transitions for the roster/feed history). Message *output* history is not
  stored — PTY scrollback lives in terminals; the log is messages.
- The traffic feed reloads history from SQLite on app start;
  `GET /v1/messages` serves the same data.

## 12. Error handling

- `thiserror` in `core`; `anyhow` in the three binaries. `tracing` throughout
  (no println).
- API errors: uniform `{error: {code, message}}`, messages written for LLM
  consumers (§6.3).
- Agent exit: pane shows exit code + restart affordance; messages to it fail
  fast (`failed` status + event) rather than queueing forever.
- Ask TTL (§3.2) guarantees no permanently-stuck pending state; `bus.reset`
  guarantees no silently-desynced consumer.

## 13. Testing

- **Core unit**: router state machine (status transitions incl. `failed`),
  sink routing per origin, reply idempotency (identical vs conflicting replay),
  pending-asks accounting, TTL expiry, tempo.toml validation.
- **Dedicated race tests**: drain-then-clear ordering (reply arriving at the
  idle transition must beat `/clear`); injection gating on debounced idle;
  reply-after-restart suppression.
- **State detector**: golden PTY transcripts of Claude Code output (spinner /
  prompt states) → assert raw + debounced transitions. Fixtures recorded from
  real sessions.
- **API integration**: spawn core with a fake agent (scripted PTY echo process,
  not real `claude`), drive the full HTTP+SSE surface incl. `Last-Event-ID`
  replay, `bus.reset` on lag, auth rejection paths, Host validation.
- **Frontend**: vitest for state reduction (`wireEvents` against recorded event
  streams), snapshot dedup logic. Terminal rendering is exercised manually in
  v1.
- Mutation-test the router with `cargo-mutants` once it stabilizes.

## 14. Implementation split — the six concepts

| # | Concept | Crate | Depends on |
|---|---|---|---|
| 1 | PTY & agent lifecycle: spawn, injection queue, state detector + debounce, auto-`/clear`, ring buffers | core | — |
| 2 | Messaging: router, message model, sinks, pending-asks, TTL, SQLite store | core | 1 (injection interface) |
| 3 | API surface: axum REST + SSE + auth + `tempo` CLI | core, cli | 2 |
| 4 | Workflow definition & run orchestration: tempo.toml, freeze semantics, run lifecycle, `coretempod` | core, daemon | types from 1–3 |
| 5 | Tauri shell & event bridge: channels/events, commands, snapshot/replay | app | 1–4 |
| 6 | Frontend UI: all views + design system | app | 5 (contracts) |

Parallelization rule: every concept's plan opens by freezing its **contracts**
(injection trait, `Event` enum + message record schema, REST/SSE shapes,
tempo.toml schema, Tauri command/channel signatures) so downstream concepts
build against contracts, not code.

## 15. Pinned dependencies (verified stable, 2026-08-01)

Rust: axum 0.8.9 · tokio 1.53.1 · tokio-stream 0.1.19 · tower-http 0.6.11
(pin 0.6 line — dedupes with axum's internal dep) · portable-pty 0.9.0 ·
rusqlite 0.40.1 (`bundled`) · serde 1.0.229 · serde_json 1.0.151 · toml 1.1.4 ·
thiserror 2.0.19 · anyhow 1.0.104 · tracing 0.1.44 · tracing-subscriber 0.3.23
· clap 4.6.5 · ureq 3.3.0 (cli; `default-features = false`) · rand 0.10.2 ·
subtle 2.6.1 · sha2 0.10.9 (core `server` feature; workflow hashing) ·
tauri 2.11.5 · tauri-build 2.6.3. Toolchain: latest stable (1.97); workspace lints per user
standards (clippy pedantic, deny unwrap/panic/todo).

Frontend (exact pins): svelte 5.56.8 · @tauri-apps/api 2.11.1 · @xterm/xterm
6.0.0 · @xterm/addon-webgl 0.19.0 · @xterm/addon-fit 0.11.0 ·
@xterm/addon-search 0.16.0 · @xterm/addon-web-links 0.12.0 ·
@xterm/addon-unicode11 0.9.0 · virtua 0.50.0. Dev: vite 8.2.0 ·
@sveltejs/vite-plugin-svelte 7.2.0 · @tauri-apps/cli 2.11.4 ·
**typescript 5.9.3 (NOT 7.x — svelte-check peers `^5||^6`)** · svelte-check
4.7.4 · vitest 4.1.10 · oxlint 1.76.0 · oxfmt 0.61.0.

Known traps: xterm 6 removed the canvas addon (`@xterm/addon-canvas` is 5.x-era
— must not appear); axum 0.8 uses `{param}` route syntax; rand 0.10 renamed
`thread_rng()`→`rng()`; ureq 3 API differs sharply from 2.x examples; verify
whether oxfmt formats `.svelte` files during setup.

## 16. Open questions deferred (explicitly v2+)

Scoped per-agent tokens · `Idempotency-Key` on message creation ·
pause/interrupt endpoint · scroll-to-injection-marker in feed linkage ·
compact terminal pane mode · PTY output persistence · remote UI.
