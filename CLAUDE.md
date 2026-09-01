# CoreTempo

A desktop app (and headless daemon) that runs multi-agent Claude Code workflows.
CoreTempo spawns `claude` sessions in PTYs it owns, routes messages between them
by typing into their prompts, and shows the traffic in a terminal-centric UI.

Design spec: `docs/superpowers/specs/2026-08-01-coretempo-design.md`.
Frozen type/API contracts: `docs/superpowers/plans/2026-08-01-contracts.md` —
**read its "Reconciliation amendments" section; those amendments are the
authoritative type/API shapes wherever another doc disagrees.**
Later specs in `docs/superpowers/specs/` amend both — multi-flow (2026-08-12)
and agent-dialogs (2026-08-17) are the ones the gotchas below cite.

## Running it

```bash
./dev              # desktop app (vite :1420 + Rust core in one process)
./dev headless     # coretempod against ./tempo.toml
./dev check        # every gate: cargo test/clippy/fmt, svelte-check, oxlint, vitest, client tsc/oxlint/vitest
./dev live         # the real-claude legs (run + sessions): needs a logged-in claude, spends tokens
```

Copy `tempo.example.toml` to `tempo.toml` to get a workflow to run. The desktop
app embeds the backend — there is no separate server to start. In dev mode Tauri
points its native webview at vite; a release build bundles the assets instead.

## Layout

| Crate | What it owns |
|---|---|
| `core` | Everything. PTY manager, message router, SQLite store, axum `/v1` API, event bus, workflow load/freeze, session manager (`core/src/sessions/`). Zero UI dependencies. |
| `app` | Tauri 2 shell (`app/src-tauri`) + Svelte 5 webview (`app/src`). |
| `cli` | The `tempo` binary agents call from their Bash tool. |
| `daemon` | `coretempod`, the headless runner and the sessions daemon. Thin `main` over `core`. |
| `clients/js` | `@coretempo/client`, the typed npm client for webhook triggers (issue #17). Standalone pnpm package; zero runtime deps. |

`core` never depends on anything UI-related; that boundary is what makes the
headless daemon possible, and it is load-bearing — keep it.

Key modules: `core/src/run.rs` (the orchestrator that wires everything),
`core/src/pty/queue.rs` (the only writer of text into a PTY),
`core/src/pty/roster.rs` (what `PtyManager` knows per agent — it never reads
a workflow), `core/src/router/` (message lifecycle + reply sinks),
`core/src/api/`, `core/src/claude_config.rs` (the managed `CLAUDE_CONFIG_DIR`
for isolated agents).

## How messaging works

- `ask` expects a reply; `send` does not. Agents call `tempo ask|send|reply`.
- The server injects the message into the target's PTY as typed text. The queue
  is the **only** *injecting* writer, and per-agent injections are serialized
  through one queue — that serialization is the correctness story. The one way
  around it is `POST /v1/agents/{id}/pty` (raw bytes, run token), which the
  desktop terminal and `tempo session attach` type through; every workflow agent
  holds the run token, so an agent can reach a sibling's PTY that way (spec
  2026-08-27 §6 mandates the route; §11 defers scoping the run token).
- For an agent-origin `ask`, the reply is injected back into the asker's PTY. For
  a UI or HTTP origin it resolves that caller instead. Everything is logged to
  SQLite regardless.
- `send` completion is inferred from the target's observed state transition, not
  from any acknowledgement.
- Status lifecycle: `queued → injected → working → replied | done | failed`.
  `failed` carries `reason_code` (`timeout | blocked_on_permission |
  agent_exited | agent_restarted | orphaned`) and a `reason`.
- Edges (`[agents.<id>] edges = [{ to, kind }]`, kind `ask|send|loop`) are
  deterministic delegation steps: composed into the frozen prompt as numbered
  `tempo` commands and enforced by per-turn obligation tracking. An agent that
  idles with unmet steps gets a nudge instead of `/clear`; an owed *reply* is
  re-nudged on a 60/120/240 s backoff (`Router::sweep_owed` pokes the queue
  worker, which re-runs the gate — still the only decision point) and
  `agent.stalled` marks each idle-after-nudge. Messages from an agent the
  receiver has an edge to never arm its turn (downstream feedback is exempt),
  and replies never open a turn — except a loop target's reply, which re-arms
  the owner until `tempo done <target>` or the edge's `max_rounds` soft cap
  (default 10). Restart disarms and zeroes round counters. The decision point
  is `ClearGate::on_stable_idle`, evaluated inside the queue worker after the
  drain.
- `[flows.<name>]` sections make the workflow self-starting: each declares
  an agent subset (`agents = [...]`, non-empty and edge-closed), a
  `trigger = { type = "on_start" | "webhook", edge = { to, kind } }`
  (on_start also carries `message`), and optionally
  `[flows.<name>.output]`. An `on_start` flow injects its configured message
  when fired (`coretempod run --flow <name>` or the desktop Run tab's
  per-flow fire control — see the run modes below); a `webhook` flow takes
  its kickoff over HTTP, and `coretempod serve` cold-starts a run per API
  call. Completion is inferred: ask kickoff → its reply; send kickoff → the
  member subset's quiescence (armed only after the kickoff reaches
  `working` — never weaken that guard). Per-agent
  `concurrency = "exclusive" | "shared"` (default exclusive) and
  `[server] max_concurrent_runs` (default 2, 1..=16, file-only) govern
  scheduling: `coretempod serve` schedules flows concurrently: one FIFO
  queue per webhook flow, one `RwLock` per pool agent (read for `shared`,
  write for `exclusive`) acquired in sorted agent-id order, then a
  `max_concurrent_runs` permit — locks before permit, and never the
  reverse. Each triggered run spawns only the flow's member subset and
  completes on that subset's reply/quiescence. `webhook` flows are each
  their own endpoint — `POST /v1/flows/{name}/trigger` on both the serve
  listener and a warm run's own API (`?wait=<secs>` long-polls; 202 +
  trigger id otherwise). `GET /v1/flows` lists every flow with queue depth
  and running count; `/v1/health` carries per-flow depths. Bare
  `POST /v1/trigger` is gone — its 404 names the flows and the new route.
  An on_start flow 400s over HTTP, pointing at `run --flow`. Warm runs
  serialize flows sharing an exclusive agent through the same lock table
  (one live session per agent) and take one in-flight trigger per flow (409
  otherwise). `on_start` flows fire via `coretempod run --flow <name>`
  (spawns only the flow's members, injects its message at launch holding
  the flow's locks, exits 0/1 on completion) or the desktop Run tab's
  per-flow fire control; bare `coretempod run` / desktop ▶ Run is a warm
  whole-pool run — every pool agent spawns and nothing auto-fires.
  `run --flow <webhook-flow>` is warm with that flow armed; a `run --flow`
  subset excludes every other flow's contract. The reply-schema gate binds
  a contract per kickoff, so only a webhook kickoff fired against a flow
  that declares `[flows.<name>.output]` is validated; on_start kickoffs
  never are. `tempo export` emits a serve unit when any webhook flow
  exists; `tempo export --flow <on_start-flow>` emits a batch unit running
  `coretempod run --flow`; an on_start-only file without `--flow` fails
  naming the flows.
- `[flows.<name>.output]` declares a JSON Schema (inline `schema` or
  `schema_file`, exactly one) for that flow's webhook reply. `tempo reply`
  rejects non-conforming bodies with the validation errors (422) so the
  agent repairs in-turn, up to `max_repairs`; the watcher re-validates what
  it returns, so callers get a parsed `output` object or a `reason_code`d
  failure. `--code 1` always bypasses validation. Every flow's schema-file
  bytes join the freeze hash in flow-name order.
- Every flow kickoff names its flow in the header it is injected with —
  `[CoreTempo <id> from http, flow <name> — reply expected] <body>` — so an
  agent holding two flows' contracts knows which schema applies before it
  replies. Agent-to-agent asks, sends and replies are unlabelled; the label
  means "flow kickoff". `Router::create_kickoff` renders it,
  `Router::create_message` does not, and it is not persisted on the
  `MessageRecord`.

## Sessions (spec 2026-08-27)

`coretempod sessions` runs the session manager: independent Claude Code
sessions across registered projects, each optionally in its own worktree
under `~/.coretempo/worktrees/<project-id>/<slug>` on `session/<slug>`.
Sessions are not workflow agents — no prompt, no router, no auto-`/clear`,
`on_permission_prompt = wait`, `McpPolicy::Inherit`. `tempo session
new|list|show|stop|resume|rm|attach|projects` drives it and finds the daemon
through `~/.coretempo/sessions/api.json` only (never `CORETEMPO_*`).
`core/src/sessions/` owns it; every lifecycle call runs under that
session's `tokio::Mutex`. Each session has its own hook token that
authorises exactly `POST /v1/agents/{id}/state` (`TokenAuth`, amendment 47).
Trust for a worktree is derived from the project root on every spawn, and the
root's MCP approvals (`enabledMcpjsonServers` …) are copied with it — one read
of the operator's `~/.claude.json` and a write only when the entry differs,
since Claude Code rewrites that file on its own cadence and every write of ours
is a window in which one of its flushes is lost. Those derived
`projects[<worktree>]` entries are never removed: `~/.claude.json` accumulates
one per deleted worktree session. Harmless, and prunable by hand.

`McpPolicy::Inherit` is the one startup dialog sessions do not prevent: with no
`--strict-mcp-config`, a session sees every server in scope — `~/.mcp.json`
included — so one the operator has never approved for the project root raises
"New MCP server found", and the session sits in `starting` with no
`SessionStart` hook until a human answers it through `tempo session attach`.
`permission_mode = "bypassPermissions"` skips that dialog (verified live on
2.1.247). Copying the root's approvals is what keeps a *derived worktree* from
re-asking about servers already approved for the repository.

The desktop app has a Sessions mode alongside its workflow view (amendment
49). The webview never talks to the daemon directly — no CORS, and the
daemon's token never reaches the webview — every call goes through Tauri
commands that proxy the daemon's `/v1` routes, and the daemon's own events
arrive as the `coretempo:session-event` Tauri event; `coretempo:sessions-status`
is a separate, shell-originated event for the connection itself. The shell
spawns `coretempod sessions` detached on first entry to the mode if none is
already running; `CORETEMPOD_BIN` overrides the binary it spawns (`./dev`
sets it to the freshly built debug binary).

## Agent state comes from hooks, not the screen

CoreTempo writes one `agent-settings-<agent_id>.json` per agent and passes
`--settings` for the matching file when spawning each agent. Those hooks call
`tempo state`:

| Hook | Reported state |
|---|---|
| `SessionStart` | idle |
| `UserPromptSubmit` | working |
| `Stop`, `StopFailure` | idle |
| `PermissionRequest` | blocked (side flag; permission dialog is up) |
| `PostToolBatch` | unblocked |

This replaced screen-scraping the TUI, which broke: Claude Code 2.1.220 emits
neither `esc to interrupt` nor `? for shortcuts`, and its spinner verbs are
randomised. **Do not reintroduce marker matching.** Reported state feeds the same
raw-state channel the old detector drove, so the debouncer, injection gating and
auto-`/clear` are unchanged downstream.

Auto-`/clear`: on a debounced working→idle transition with zero pending asks, an
empty queue, and no open obligation turn, the server types `/clear`. Ordering is
strict drain-then-clear.

## Gotchas that cost real debugging time

- **Enter must be a separate write.** Injecting `text + "\r"` in one write leaves
  the prompt typed but unsubmitted whenever Claude Code is rebuilding its input
  box — right after spawn, and after the session restart `/clear` triggers. The
  queue sends the text, waits `SUBMIT_DELAY`, then sends `\r`. Even a separate
  Enter is dropped on a cold spawn still drawing its welcome box (#54: 3 of 13
  spawns on 2.1.233, every cold spawn on 2.1.234), and no hook says "prompt
  ready" — so the queue verifies
  the submit instead: if the debounced state has not left idle
  (`UserPromptSubmit`) within `SUBMIT_VERIFY` it resends `\r`, at most
  `MAX_ENTER_RESENDS` times, then warns. Any state change, restart, or a
  permission dialog (#63) ends the wait — Enter into a dialog answers it.
- **The state detector's stripper passes printable ASCII only.** Any marker or
  parsing you add against PTY output cannot rely on `❯`, box drawing, or emoji.
- **Spawned agents must not inherit `CLAUDE_CODE_*`.** A daemon launched from
  inside a Claude Code session leaks its own session markers into every agent and
  silently changes their behaviour. `spawn.rs` strips them.
- **A late hook must not revive an exited agent**, or the queue injects into a
  dead PTY and the write vanishes. `report_state` guards this.
- **Claude Code startup dialogs fire no hook**, so an agent parked on one sits
  in `starting` forever with no `SessionStart`. Four exist; CoreTempo
  prevents three of them and *reports* the fourth — the in-turn permission
  dialog — rather than preventing it (spec 2026-08-17 §3):
  - **Trust dialog** — any git repo (or bare dir) Claude Code has not seen;
    `--dangerously-skip-permissions` does not skip it. Preflighted before
    spawning against `~/.claude.json` (`$CLAUDE_CONFIG_DIR/.claude.json` when
    the operator sets that variable; the same rule applies to MCP resolution)
    `projects[trust_root(dir)].hasTrustDialogAccepted` (`trust_root` = git
    toplevel, physical path): `Run::start_with` over its roster,
    `coretempod serve` over the whole frozen pool at boot, including agents only
    an `on_start` flow would spawn. With `trust_agent_dirs = true` — in
    `~/.coretempo/config.toml` (`CORETEMPO_CONFIG` overrides the path) or under
    `[server]` in `tempo.toml` — it writes the key (0600, atomic rename,
    other content kept); otherwise the run refuses to start naming every root
    and both fixes, and the desktop shows a confirm dialog first (the answer
    becomes that run's policy). A live Claude session can revert the key, so
    `PtyManager` re-checks through a `SpawnGate` (`TrustGate`) before every
    spawn and restart; a refused restart leaves the agent `Exited` with the
    reason in the log and nothing auto-recovers it. Trust is never granted
    silently.
  - **"New MCP server found"** — every *workflow* agent spawns with
    `--strict-mcp-config` (sessions do not — see Sessions), so it sees only
    the servers its `mcp = [...]` names. Names resolve at load
    against `~/.claude.json` `mcpServers`, then its `projects["<dir>"]`, then
    `~/.mcp.json`, then `<dir>/.mcp.json` (first match wins — CoreTempo's
    precedence, not Claude Code's), are written to `agent-mcp-<agent_id>.json`
    and passed as `--mcp-config`; an unknown name fails the load naming every
    declared server, so `load_workflow` (and every serve trigger) can now fail
    on machine-local MCP state. **`mcp` is not permission to call the tools**:
    pair it with `allow = ["mcp__<server>__<tool>"]` per tool the agent calls
    (listing needs no rule; calling does — verified live on 2.1.233). The
    canonical JSON of each selection joins the freeze hash (`hash mismatch`
    after an MCP edit is expected). Plugin-provided servers are dropped unless
    redeclared in one of those four sources; workflows that silently inherited
    ambient servers lose them until they declare `mcp`.
  - **Onboarding / theme picker** — an empty `CLAUDE_CONFIG_DIR` opens on
    "Let's get started" before trust. Only `isolated_config = true` agents
    run against a fresh dir, and `core/src/claude_config.rs` seeds it
    (`hasCompletedOnboarding`, `autoMemoryEnabled: false`, `skills/` links).
    Login is **not** seeded: the spawn exports
    `CLAUDE_SECURESTORAGE_CONFIG_DIR` at the operator's config dir so the
    agent shares the operator's `.credentials.json` and refresh lock. Never
    copy or symlink that file into the managed dir — Claude Code writes it
    by temp+rename, which replaces a symlink and strands every other holder
    (rotated refresh token → they log out); verified live on 2.1.241.
    `skipDangerousModePermissionPrompt: true` in that managed `settings.json`
    also suppresses the **Bypass Permissions acknowledgment** a fresh dir
    raises for `permission_mode = "bypassPermissions"` agents — the
    `.claude.json` key `bypassPermissionsModeAccepted` does not (verified
    live on 2.1.241). Trust for that dir is a **mirror**: the gate
    re-checks the operator's `~/.claude.json` and then writes the key into the
    managed `.claude.json` before every spawn — never a second consent.
  - **In-turn permission dialog** — a tool call with no matching allow rule.
    By default (`on_permission_prompt = "deny"`) the agent's `PermissionRequest`
    hook runs `tempo state refused`, which answers the dialog itself with a deny
    decision and a message naming the fix (verified live on 2.1.246: the call
    fails inside the turn, the agent carries on, no dialog is ever shown);
    CoreTempo logs the refused tool and a ≤200-byte summary of its input (the
    Bash command / file path) at warn, publishes `agent.permission_refused`,
    and the desktop shows ⛔ on the agent with both in the tooltip — that is
    the allow rule you are missing.
    Read-only commands (`ls | wc -l`) raise no dialog at all on 2.1.246. With
    `on_permission_prompt = "wait"` the dialog stays up for a human, and the
    rest of this bullet applies:
    no turn hook fires, so the agent reads **`working` forever** and is never
    nudged or stalled. Its `PermissionRequest` hook reports `tempo state
    blocked`; CoreTempo publishes `agent.blocked` with the tool name and the UI
    shows ⏸; `PostToolBatch`, turn end and restart clear it. Owed asks on it
    fail after 90 s with `blocked_on_permission: <tool>`; the agent itself is
    never touched — a new ask/send aimed at it parks in the queue (nothing is
    typed at a dialog: a digit picks an option, Enter takes the default) and
    fails on the same 90 s clock whatever state it reads, and nudges/`/clear`
    hold until the flag clears. A subagent's dialog fires the parent's hook too and is
    accepted while the parent is idle; an `unblocked` report clears the flag
    only when its `agent_id` matches the dialog's — Claude Code helper agents
    fire `PostToolBatch` for tools they did not run. Do not add a PTY-silence
    heuristic for this: Claude Code
    2.1.233 keeps blinking a glyph while the dialog waits, so silence never
    comes.
- **Generated per-agent settings always allow `Bash(tempo:*)`.** Each agent's
  `agent-settings-<agent_id>.json` (`write_agent_settings_files`) allows it
  unconditionally, plus `Bash(<bin>:*)` for every entry in that agent's
  `tools = [...]`, plus every `allow = [...]` rule verbatim (Claude Code
  permission syntax: `"WebSearch"`, `"Read(//data/**)"`, `"mcp__…"`). Editing
  the agent dir's `.claude` settings by hand is only needed for tools not
  declared this way.
- **WSL:** the webkitgtk window never maps under Wayland, and MESA has no device
  without `/dev/dri`. `./dev` sets `GDK_BACKEND=x11` and `LIBGL_ALWAYS_SOFTWARE=1`
  for you. Without a GPU, expect xterm's DOM renderer rather than WebGL.
- **`coretempod serve` bound off-loopback still validates `Host`.** A public
  `0.0.0.0` bind 403s any caller whose `Host` header isn't `localhost`,
  loopback, or the bind IP literal — the same rule the run API enforces. Put a
  public deployment behind a reverse proxy that rewrites `Host`, or bind
  loopback and tunnel in. Serve also refuses to start without a provisioned
  token (`CORETEMPO_TOKEN`, `--token-file`/`CORETEMPO_TOKEN_FILE`, or
  `[server] token_file`): it writes no `api.json`, so a generated one would
  reach no caller — `coretempod run` and the desktop still generate theirs.
- **Concurrent runs share one SQLite file.** `Store::open` runs under
  `spawn_blocking`; the shutdown WAL checkpoint is skipped (debug log) when a
  peer holds the file; and every open sweeps orphaned non-terminal rows of
  NULL-`run_id` or cleanly-stopped runs (crashed runs' rows are deliberately
  left — see #30).

## Conventions

- TDD: failing test first, run it, confirm the expected failure, then implement.
  Where implementation and tests land together, prove the test bites by mutating
  the code and watching it fail.
- Zero warnings. `cargo clippy --workspace --all-targets --all-features --
  -D warnings` must be clean, pedantic included. No `unwrap`/`panic` in `src`,
  `tracing` not `println`, 100-char lines, max 5 positional params (group extras
  into a struct — see `SpawnInputs`).
- Integration test files need
  `#![expect(clippy::panic_in_result_fn, reason = "assertions are the vocabulary of tests")]`.
- Remove an `#[expect(dead_code, …)]` as soon as your change makes the item live;
  an unfulfilled expectation is itself a warning.
- Frontend: exact pinned versions, no `^`. TypeScript stays on 5.9.x — TS 7
  breaks `svelte-check`.
- Errors are read by LLMs. Include the roster, the valid values, the fix.
- Every type/API change appends a numbered entry to the contracts doc's
  "Reconciliation amendments". Numbers are taken in merge order: when several
  PRs are in flight, assign each its number up front, and the ones that merge
  later renumber on rebase (the doc's tail is a guaranteed conflict).
- Parallel worktrees must not share one `CARGO_TARGET_DIR`: the crates have the
  same names, so a sibling worktree's build makes cargo serve a stale test
  binary for yours — false greens *and* false reds. Give each worktree its own
  target dir (and build one at a time on WSL, where concurrent cold builds have
  crashed the session), or gate on PR CI, which builds each branch alone.

## Testing against real agents

Unit and integration tests use a scripted fake agent, which cannot catch PTY
timing or TUI behaviour — the gotchas above all escaped it. When touching the
spawn recipe, injection, or state reporting, run `./dev live`: it builds the
workspace and runs two `#[ignore]`d files against the real `claude` on PATH.
`daemon/tests/live_claude.rs` is the run leg — hooks report idle, an ask
round-trips, auto-`/clear` is typed, a second ask survives it; it trusts
`~/.coretempo/live-test/agent` once and reuses it.
`daemon/tests/live_sessions.rs` is the sessions leg — a worktree session
answers, is stopped and resumed, and still remembers (so `--resume` reopened
the conversation the `SessionStart` hook identified), while a
`claude_session_id` Claude Code does not know makes the resumed process exit
instead of starting fresh; it also reads back the MCP approvals the derived
worktree grant copied. It trusts `~/.coretempo/live-test/repo` once, and its
root, db and worktrees are per-run scratch, so the operator's own sessions
daemon is untouched. Four Haiku turns between them. For anything they do not
cover, write a workflow, `coretempod run` it, and check a round trip with
`tempo ask <agent> "..."`. Real agents cost tokens; keep prompts trivial.
