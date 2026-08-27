# Agents parked on Claude Code dialogs (#1, #2, #26)

Date: 2026-08-17
Status: approved design, pre-implementation
Closes: #1 (trust dialog), #2 (MCP approval), #26 (in-turn permission dialog)

A spawned `claude` can park on an interactive dialog CoreTempo cannot see.
Three sightings share that shape and split into two families:

- **Startup dialogs** — workspace trust (#1) and MCP server approval (#2).
  The agent never reaches its prompt, never reports `idle`, and every message
  sits `queued`. Both are preventable at spawn time.
- **In-turn dialogs** — a permission prompt inside a turn (#26): out-of-scope
  `Read`, un-allowlisted binary, non-Bash tool such as `WebSearch`. The turn
  pauses without ending, no hook fires, raw state stays pinned at `working`
  forever, and the only exit is `ask_timeout` (which a webhook caller reads as
  a terminal `timeout`). Partly preventable; the general case needs detection.

All fixes live in `core`. The desktop app gains one confirmation dialog.

## Goals

- No agent ever parks on the trust or MCP-approval dialog at spawn.
- A workflow can declare non-Bash permissions so the in-turn dialog does not
  fire for tools the author knows the agent needs.
- An agent that parks anyway is surfaced to the operator (`agent.blocked`)
  within a fixed threshold, not absorbed into `ask_timeout`.
- CoreTempo never grants trust silently: it is opt-in per workflow, or a
  deliberate global setting, or a desktop confirmation.

## Non-goals

- Auto-recovering a blocked agent (answering the dialog, sending Escape,
  restarting). ~~Failing asks with a distinct code~~ — superseded by
  Section 4 (2026-08-18): owed asks on a blocked agent now fail with
  `blocked_on_permission`; the agent itself is still never touched.
- Screen-scraping the dialogs. Agent state — including "parked on a
  dialog" — stays hook-driven; nothing reads PTY content or timing.
- A general-purpose global config. `~/.coretempo/config.toml` is introduced
  with exactly one key.
- Heuristics on PTY output (recency, byte rate, markers). Tried and
  disproven: Claude Code 2.1.233 blinks a glyph every ~600 ms while a
  permission dialog waits, so "silent" never happens (see Amendment below).
- Auto-answering the dialog through the `PermissionRequest` `decision`
  object. That is a permission-policy change (deny degrades webhook results
  silently; allow defeats `permission_mode`); a later issue if wanted.
- Validating Claude Code permission-rule syntax in `allow`.

## Section 1 — Trust (#1)

Accepting the trust dialog sets `hasTrustDialogAccepted = true` on a
`projects["<path>"]` entry in `~/.claude.json`. `--dangerously-skip-permissions`
does not skip it; `-p`/non-TTY does, but CoreTempo needs the PTY.

**Trust is keyed on the enclosing git repository root, not the cwd** (observed
live 2026-08-07: a workdir inside a trusted repo needs no dialog; trusting the
workdir alone is not honoured). Everything below operates on
`trust_root(dir)` = the git toplevel containing `dir` (walk up for `.git`,
which may be a file for worktrees — resolve through it), else `dir` itself.

### Config surface

- **Global user config**, new file `~/.coretempo/config.toml` (path override
  `CORETEMPO_CONFIG` for tests). Loaded once at process start by whichever
  binary embeds `core`; missing file = defaults. One key:

  ```toml
  trust_agent_dirs = false      # true = grant trust for every agent dir
  ```

- **Per-workflow opt-in**, `tempo.toml`:

  ```toml
  [server]
  trust_agent_dirs = true       # default false
  ```

Either being `true` means "policy allows granting". They combine into a
`TrustPolicy { grant: bool }` that the embedding binary passes into
`Run::start_with` via `RunOptions`; `core` never reads the global file on
its own and never knows whether a UI exists.

### `core` API

```rust
pub fn trust_root(dir: &Path) -> PathBuf;
pub fn untrusted_agent_dirs(dirs: &BTreeSet<PathBuf>) -> io::Result<Vec<PathBuf>>; // returns roots
pub fn grant_trust(roots: &[PathBuf]) -> io::Result<()>;
```

Both file functions operate on `~/.claude.json` (honouring `HOME`) with
read-modify-write that preserves unrelated content, writes `0600`, and
renames atomically. A missing file is created with just the project entries.

### Behaviour

**Preflight** runs over the distinct trust roots of every agent about to
spawn — the whole pool, or the flow's subset — before any spawn:

| root already trusted | policy | result |
|---|---|---|
| yes | any | nothing |
| no | `grant` | grant trust, log `info` naming the root |
| no | no `grant` | `Run::start_with` fails: error lists every untrusted root and both fixes (open `claude` there once, or set `trust_agent_dirs`) |

Who calls it:

- **`coretempod run`**: `Run::start_with` performs the preflight; failure is
  exit 1 with the message.
- **`coretempod serve`**: preflight the *whole pool* at `serve` boot (the
  file is frozen, so the roster cannot change) and refuse to start on
  failure. This avoids discovering the problem after a trigger has been
  202'd, queued, and holds locks — where the only outlet is
  `TriggerStatus::Failed { reason_code: "internal" }`.
- **Desktop**: before `Run::start`, the shell calls `untrusted_agent_dirs`;
  if non-empty and policy does not grant, it shows a confirm dialog listing
  the roots ("Trust these folders? CoreTempo will mark them trusted in
  `~/.claude.json`"); confirm → `grant_trust` → `Run::start`; decline → no
  run. `Run::start_with` still runs the preflight (idempotent) so the
  invariant holds regardless of caller.

**Re-apply before every spawn.** A live Claude session flushes its in-memory
`~/.claude.json` on its own cadence (observed on #1), so a granted key can be
reverted minutes later. Every spawn — initial and `restart()` — re-reads the
root's key immediately beforehand: with `grant`, re-apply; without it, fail
that spawn with the same error rather than let the agent park.

Implemented: `trust::preflight` (called by `Run::start_with` and serve boot) and
`TrustGate` via `pty::SpawnGate` for the per-spawn re-check; the desktop asks
through `run_untrusted_dirs` → confirm → `run_start(.., trust_confirmed)`.
A spawn the gate refuses on `restart()` leaves that agent `Exited` with the
reason logged; nothing auto-recovers it.

### Tests

- `trust_root`: plain dir, dir inside repo, nested worktree (`.git` file),
  dir with no repo above.
- Preflight matrix over a temp `HOME`: root trusted / untrusted × `grant` /
  no-grant; subdir of a trusted repo → nothing to do.
- Re-apply-before-spawn: key removed between preflight and spawn → re-granted
  (grant) or spawn error (no grant); same on `restart()`.
- Error message names every untrusted root.
- Read-modify-write preserves sibling keys; missing file created.
- `serve` boot with an untrusted root refuses to start.
- Real agent: never-seen dir + workflow opt-in → agent reaches `idle`.

### Real-agent verification (2026-08-17)

Verified against Claude Code 2.1.234 with two single-agent workflows in
never-seen scratch dirs outside any repository (so `trust_root` is the agent dir
itself). With `[server] trust_agent_dirs = true`, `coretempod run` logged
`granted Claude Code trust for an agent dir root=<dir>` 18 ms into boot, before
the run started, and agent `a` read `idle` at the first poll (12 s after spawn)
— no trust dialog. `~/.claude.json` gained exactly one `projects` entry
(35 → 36) with `hasTrustDialogAccepted: true`, still 0600;
`tempo ask a "Reply with the single word ok."` answered `ok` in 3.6 s.

Deleting that project entry mid-run and restarting the agent over
`POST /v1/agents/a/restart` reproduced the live-flush case the `SpawnGate`
exists for: the gate re-granted before the respawn
(`trust key was missing right before spawn (reverted by a live Claude session?);
re-granted agent=a`), the key was back, and the respawned agent reached `idle`
and answered `ok` again in 3.7 s.

Without any opt-in the same run refused in 11 ms, exit 1, naming the root and
both fixes; no key was written for it and no SQLite file was created, since the
preflight precedes the store open. Setting `trust_agent_dirs = true` in
`~/.coretempo/config.toml` alone (workflow with no `[server]` section) granted
that same dir and reached `idle`, so the global surface carries a run on its
own.

The desktop confirm dialog was **not** exercised live: driving the webview needs
a synthetic click and this WSL box has neither `xdotool` nor `wmctrl` (X11 is
otherwise healthy). Its branch logic is covered by `app/src/lib/session.test.ts`.
Two incidentals: this machine's `~/.claude.json` was already 0600, so the
0644-tightening path only has unit coverage; and the restart endpoint needs
`Content-Type: application/json` (415 without it) even though it takes no body.

## Section 2 — MCP (#2)

Claude Code prompts on any newly discovered MCP server — the agent dir's
`.mcp.json` **and** `~/.mcp.json` (observed on #2). Spawned agents are
workers; they should not inherit whatever the machine happens to declare.

### Spawn recipe

Every agent is spawned with `--strict-mcp-config`. With no `--mcp-config`
that is zero MCP servers, so no discovery prompt can fire.

### Opt-in per agent

```toml
[agents.resolver]
mcp = ["context7"]     # names only; default []
```

At workflow load, each name is resolved against the user's own declarations,
first match wins: `~/.claude.json` top-level `mcpServers`, then
`~/.claude.json` `projects["<agent dir>"].mcpServers` (project-local scope,
`claude mcp add -s local`), then `~/.mcp.json`, then `<agent dir>/.mcp.json`. An unknown name is a load error listing the names
that are available. The resolved subset is written to
`~/.coretempo/runs/<run_id>/agent-mcp-<agent_id>.json` (0600, beside the
settings file, shape `{"mcpServers": {...}}`) and passed as
`--mcp-config <path>`.

Servers supplied via `--mcp-config` are expected not to trigger the
"new server found" approval; this is verified against a real agent before
merge. If it does trigger, the fallback is to drop `--strict-mcp-config`
and instead write `disabledMcpjsonServers` for every discovered server the
agent did not opt into (the workaround that held on #2) — decided then, not
pre-built.

### Freeze hash

The resolved servers join the freeze hash per agent (in agent-id order), as
flow schema files already do: the tool surface is part of what makes a run
reproducible. Hash a **canonical** form — only the selected servers, keys
sorted, compact JSON — never the raw source bytes: `~/.claude.json` is
reformatted by every live session's flush, and `Run::start_with` and every
serve trigger re-run the load and compare hashes, so raw bytes would refuse
runs with a misleading "tempo.toml changed". The freeze-mismatch error text
names MCP sources as a possible cause. Implemented in `load_workflow` via
`resolve_agent_mcp` + `crate::mcp::canonical_bytes`, framed after the schema
files.

### Tests

- `spawn.rs` arg test: `--strict-mcp-config` always present; `--mcp-config`
  only for agents with `mcp` declared, pointing at the per-agent file.
- Resolver over a temp `HOME`: found in each source in precedence order;
  unknown name → error naming the available names; empty `mcp` → no file.
- Freeze-hash test: changing an MCP server definition changes the hash; the
  same servers with different source formatting/key order do not.
- Real agent: an agent cwd'd in this repo (MCP configured) reaches `idle` with
  no prompt; one with `mcp = ["context7"]` can call a context7 tool.

### Real-agent verification (2026-08-17)

Verified against Claude Code 2.1.233 with a two-agent workflow (`plain`, no
`mcp`; `ctx`, `mcp = ["context7"]`), both cwd'd in `~/projects/CoreTempo`, on a
machine that declares `mailbox` in `~/.claude.json` and `context7` in
`~/.mcp.json`. Both agents reached `idle` within ~2 s of spawn (first poll) with
no "new MCP server found" approval — twice, on two runs. `--mcp-config` does not
re-trigger discovery approval, so the `disabledMcpjsonServers` fallback is not
needed and was not built. `ctx` listed exactly
`mcp__context7__query-docs, mcp__context7__resolve-library-id`; `plain` replied
`none`, so `mailbox` reached neither agent. Only `agent-mcp-ctx.json` was
written, 0600, holding
`{"mcpServers":{"context7":{"args":[…],"command":"npx"}}}`; `plain` got
`--strict-mcp-config` with no `--mcp-config`.

An `allow` rule **is** required to actually call the tool. On the first run,
`ctx` parked on the in-turn permission dialog for 4.5 minutes — §3's signal
fired correctly (`agent is waiting on a permission dialog agent=ctx
tool="mcp__context7__resolve-library-id"`), and the agent read `working`, never
idle, so nothing recovered it. Adding
`allow = ["mcp__context7__resolve-library-id"]` to `[agents.ctx]` fixed it: the
same ask replied `/websites/rs_tokio_tokio` in 11 s with `blocked: false`
throughout. MCP tool rules are per tool (`mcp__<server>__<tool>`), so opting an
agent into a server is not the same as letting it call that server's tools —
`mcp` and `allow` are both needed. That pairing is the documented pattern in
`tempo.example.toml`'s `mcp` comment and CLAUDE.md's MCP bullet:

```toml
[agents.ctx]
mcp   = ["context7"]
allow = ["mcp__context7__resolve-library-id"]
```

## Section 3 — In-turn dialogs (#26)

### Prevention: `allow`

```toml
[agents.resolver]
tools = ["gh", "jq"]                                    # unchanged: Bash(<bin>:*)
allow = ["WebSearch", "WebFetch", "Read(//data/**)"]    # verbatim rules, default []
```

`allow` entries are appended verbatim to the generated settings file's
`permissions.allow` after the `Bash(...)` entries. No heuristics: `tools`
keeps meaning "Bash binaries" and `allow` means "Claude Code permission
rules as written". Load rejects empty / whitespace-only entries; rule syntax
is Claude Code's to validate (a bad rule is ignored, the dialog still fires,
and the `PermissionRequest` hook below makes that visible).

### Detection: `PermissionRequest` hook

Claude Code fires the `PermissionRequest` hook the moment it is about to
show a permission dialog; a hook that exits 0 without a `decision` leaves
the dialog to proceed normally (observe-only). Its stdin JSON carries
`tool_name`, `tool_input` and `permission_suggestions`. `PostToolBatch`
fires once after every tool call in a batch has resolved (approve or
deny), so it is the clear signal — `PostToolUse` fires per tool and
concurrently, and would clear on a sibling tool while the dialog is still
up; `PermissionDenied` fires only in auto mode, never on a manual "No".

- The generated per-agent settings gain two hooks beside the four
  turn-boundary ones: `PermissionRequest → tempo state blocked` and
  `PostToolBatch → tempo state unblocked`.
- `tempo state blocked` reads the hook's stdin JSON (when stdin is not a
  TTY) and forwards `tool_name`; the API accepts
  `{"state": "blocked", "tool": "<name>"}` alongside `working`/`idle`/
  `unblocked`.
- `PtyManager` keeps `blocked` as a per-agent side flag next to raw state
  (blocked is orthogonal to `working` — the turn is still open). `blocked`
  sets the flag only when raw state is `working` (else debug-log and
  ignore) and publishes `agent.blocked { agent, blocked: true, tool }`
  once; `unblocked`, `working`, `idle`, `restart`, exit and shutdown clear
  it and publish `{ blocked: false, tool: null }` once if it was set. A
  `working`/`idle` report clears the flag even when the raw state is
  unchanged (that path currently early-returns).
- `/v1/agents` carries `blocked: bool`; `/v1/health` a blocked-agent count.
- Frontend: `wireEvents` case, `agentsState.blocked[id]` holds the tool
  name (or `null`), roster/canvas badge titled with it; cleared on
  `blocked: false` and on snapshot reload (`setAgents` reseeds from
  `AgentInfo.blocked`, tool unknown).
- Startup dialogs (trust, MCP approval) fire no hook — Claude Code holds
  every settings-file hook back until trust is accepted — so they still
  present as `starting` with no `SessionStart`. Sections 1–2 prevent them
  by preflight; no separate detector.

Detection only. No injection, no kill, no ask failure. *(Amended by Section 4:
owed asks now fail after a grace; the agent is still not touched.)*

**Amendment (2026-08-17, real-agent run r-dd6b0de9).** The first design
was a PTY-recency watchdog (flag `working`/`starting` agents silent >
90 s). Verified false on Claude Code 2.1.233: an agent parked on
`Read(/etc/hostname) — Do you want to proceed?` kept emitting a blinking
`●` (~70 B every ~600 ms), so it was never flagged in 150 s. The trust
dialog *is* static, but that family is prevented by §1/§2. The watchdog,
its output-freshness plumbing and tests were removed rather than kept as
a second mechanism ("replace, don't deprecate").

### Docs

- Correct the CLAUDE.md gotcha: an un-allowlisted call parks the agent at
  perpetual `working` (in-turn dialog), not idle → nudge → stall.
- Document `allow`, `mcp`, `trust_agent_dirs` (both surfaces), and
  `~/.coretempo/config.toml` in CLAUDE.md and `tempo.example.toml`.

### Tests

Settings JSON renders the six hooks; `allow` → settings JSON and a
combined `tools`+`allow` test; wire form of `agent.blocked` with and
without `tool`; `blocked` sets the flag only at raw `working` and publishes
once; a repeated `blocked` does not republish; `unblocked`, `working`,
`idle`, restart and exit clear it and publish `blocked: false` exactly
once; `tempo state blocked` forwards `tool_name` from stdin JSON and
tolerates missing/invalid stdin; API rejects unknown state words naming
all four; frontend badge set/clear/reseed. Real agent: force an
out-of-scope `Read` and see `agent.blocked {tool: "Read"}` at once and the
badge; answering the dialog (yes or no) clears it; a parallel Read+Bash
batch does not flap; the fund-data resolver case with
`allow = ["WebSearch"]` completes.

## Delivery

Three PRs, each closing its issue, in this order:

1. **#26** — `allow` + `PermissionRequest`/`PostToolBatch` hook signal + event + badge + CLAUDE.md correction.
   Self-contained, highest operational pain.
2. **#2** — `--strict-mcp-config`, `mcp = [...]`, resolver, freeze hash.
3. **#1** — global config file, `[server] trust_agent_dirs`, preflight,
   `untrusted_agent_dirs`/`grant_trust`, daemon error path, desktop dialog.

PR 1 also introduces a shared `AgentConfig` test builder, since each PR adds
a field and the fixtures are otherwise built literally in several test
modules. Each PR carries its real-agent verification per the repo
convention.

## Section 4 — Recovery: the owed-ask watchdog (#55, #56)

*Addendum 2026-08-18. Supersedes the "detection only" line in §3 and the
"failing asks with a distinct code" non-goal.*

### Live findings that shape this section (Claude Code 2.1.234, 2026-08-18)

Reproduced with a one-agent workflow whose agent is asked to fan out to a
Claude Code `Agent`-tool subagent that runs an un-allowlisted binary:

- **#55 shape**: the `Agent` tool returns at once; the parent writes
  "waiting for its result", ends its turn (`Stop` → `idle`), gets its one
  nudge, answers "still waiting", and CoreTempo goes quiet for the rest of
  `ask_timeout`. In fund-data (issue text) the completion notification never
  woke the parent; in the scratch run it did once — either way the single
  nudge lands one turn too early.
- **#56 case 2 is coverable.** The subagent's permission dialog renders in
  the parent's pane *and fires the parent's `PermissionRequest` hook*: the
  daemon logs `ignoring blocked report outside a turn agent=parent
  state=Idle`. The signal exists; today's `report_blocked` guard
  (`raw state != working` → drop) is what hides it. A first check at
  `log = "info"` wrongly concluded "no hook fires" — the drop is a debug
  line, and the level comes from `[server] log`, not `RUST_LOG`.
- `PostToolBatch` (→ `unblocked`) also arrives while the parent is idle.

### Decisions

| Question | Decision |
|---|---|
| Re-nudge policy | Backoff 60 s → 120 s → 240 s → 240 s… (constants). `agent.stalled` keeps its meaning: it fires when the agent idles again after a nudge; nudging continues underneath. |
| Where fail-fast applies | Every run mode (serve, `run`, desktop). Grace **90 s** from `blocked_since`, constant. |
| Reason wire | `MessageRecord` gains `reason: Option<String>` and `reason_code: Option<String>`, persisted and on every message surface. |
| Agent after the fail | Left parked. ⏸ stays. Nothing restarts, types, or kills. |
| Subagent dialogs | Covered: blocked reports are accepted outside a turn (see below). |

### 4.1 One decision point, poked by a timer (#55)

The queue worker's `ClearGate::on_stable_idle` stays the **only** place a
nudge or `/clear` is decided (CLAUDE.md invariant). The sweeper never
enqueues text. Instead:

- `InjectionQueue` gains `fn reconsider(&self, agent: &AgentId)`: a
  `QueueCmd::Reconsider` poke. The worker handles it exactly like a
  debounced working→idle transition minus the drain — if the agent is
  debounced-idle it calls `on_stable_idle` and acts on the decision
  (`Nudge` → type it; `AllowClear` → honoured as today; `HoldQuiet` →
  nothing). Not idle → ignored. This keeps the `pending_asks > 0 →
  HoldQuiet` guard, the drain ordering, and the "drained injection
  short-circuits the consult" rule intact; a poke can never produce a
  stale nudge because the gate re-reads `owed` at the moment it runs.
- `ReplyNudgeState { nudged, stalled }` becomes
  `{ nudges: u32, last_nudge_at: tokio::time::Instant, stalled: bool }`.
  `owed_reply_decision`: nothing owed → `None`. Owed and blocked (4.2) →
  `HoldQuiet` (never type into a dialog). Owed, `nudges == 0` → nudge now.
  Owed, `nudges > 0` and `now - last < backoff(nudges)` → publish
  `agent.stalled` once per nudge (see below) and `HoldQuiet`. Owed and the
  backoff has elapsed → nudge again, `nudges += 1`, `stalled = false`. Both
  entry paths (transition and poke) go through this one function, so
  there is no double-nudge race: the check-and-bump is under the
  `owed_nudges` guard.
- The 1 s TTL sweeper (`ttl.rs`) grows one step, *after* expiry: for every
  agent in `owed` whose `nudges > 0` and whose backoff has elapsed, call
  `injector.reconsider(agent)`. No state subscription; the worker decides.
  (An agent with `nudges == 0` and an owed ask is one whose first
  idle-transition nudge has not happened yet — e.g. still `working`, or the
  #54 swallowed-Enter case where it never went working; the poke covers
  it too: if it is idle the gate nudges, if not the poke is dropped.)
- `agent.stalled`: publishes on the first idle after each nudge (so the ⚠
  reappears per round); `agent.nudged` publishes per nudge. The desktop
  clears ⚠ on `working` as today. Rustdoc on both events, the roster
  tooltip and CLAUDE.md say "idled again after a nudge; nudging continues
  on a backoff".
- Nudge text for `nudges >= 2` appends: "If you are waiting on background
  subagents, poll for their result inside this turn (`/tasks`, or check
  their output files) rather than ending it — CoreTempo cannot see them."
- No skip-past-TTL rule: expiry runs before the owed walk in the same tick,
  so an ask due to expire is failed, dropped from `owed`, and never poked.
- Same tick, same pass: an owed agent whose raw state is `Exited` has its
  owed asks failed `agent_exited` (today they wait for TTL because
  `drive_message` stops watching after `working` and exit never calls the
  router). Restart already clears `owed`/`owed_nudges`.

### 4.2 Blocked fail-fast (#56)

- `PtyManager` handle: `blocked: bool` → `blocked: Option<Blocked { since:
  tokio::time::Instant, tool: Option<String> }>`. `report_blocked` accepts
  the report at raw `working` **or `idle`** (drops only `starting`,
  `restarting`, `exited`, still debug-logged); a repeat while set publishes
  nothing and does not move `since`. Clearing rules unchanged
  (`unblocked`, `working`, `idle` reports, restart, exit, shutdown), except
  that an `idle` report no longer clears it when the agent is *already*
  idle — a subagent dialog goes up while the parent is idle and its `Stop`
  already fired. Concretely: `working`/`idle` clear the flag only on a raw
  state *change*; `unblocked` always clears. Known gap: a parent woken
  (`working`) while its subagent's dialog is still up clears the flag early;
  that ask then rides the nudge path and TTL like today.
- `StateSource` gains `fn blocked_since(&self, agent) -> Option<Blocked>`,
  where `Blocked { since: tokio::time::Instant, tool: Option<String>,
  agent_id: Option<String> }`; `PtyManager` implements it from the handle.
  (`agent_id` scopes the `unblocked` clear to the dialog's own session — the
  2026-08-18 live amendment below.)
- Sweeper pass (same tick, after expiry, before the poke): for every agent
  in `owed` with `blocked_since` older than **90 s**, fail each owed ask
  with `reason_code = "blocked_on_permission"` and reason
  "agent '<id>' has been waiting on a Claude Code permission dialog for
  <tool> for 90 s and cannot reply; add `tools = [...]`/`allow = [...]` for
  it in tempo.toml (or answer the dialog in the pane) and fire again". No
  owed ask → nothing happens, however long it is blocked. `send`s to a
  blocked agent are unaffected (they complete on state transition or TTL as
  today).
- Grace caveat, accepted: `PostToolBatch` fires when the batch's tools
  finish, so a dialog an operator approves at 5 s followed by an 85 s tool
  run reads as blocked for 90 s and fails the ask. That is the
  interactive case where a human is already looking at ⏸ and the reason
  names the tool; serve has no operator. 90 s (not 60) buys headroom.
- An ask arriving at an agent already blocked > 90 s fails on the next
  tick. Intended: the caller learns the allow rule immediately.
- Blocked ⇒ `owed_reply_decision` returns `HoldQuiet` and the queue does
  not `/clear` (typing into a dialog is never right; a leading digit could
  select an option). The gate reads `blocked_since` through the same
  `StateSource`.

**Amendment (2026-08-18, live run).** Verifying §4 against real agents on
Claude Code 2.1.234 found the fail-fast never arming. The parent was idle when
a *subagent's* dialog raised `agent.blocked { tool: Bash }` — the
`PermissionRequest` payload carried `agent_id: a9c81c1e4a5cf2bbe` — and 28 s
later a **different** Claude Code helper agent
(`agent_id: ac3cef2916066bf6d`, `tool_response: "No tools needed for
summary"`) fired `PostToolBatch`, whose `tempo state unblocked` cleared the
flag while the dialog was still on screen; the 90 s grace never started. Fix:
the hook payload's `agent_id` now rides both reports
(`ReportStateRequest.agent_id`, `Blocked.agent_id`, `None` for the main
session, which sends no such field) and `report_unblocked` clears only when
the two match — a non-matching report is a debug log and publishes nothing.
Turn boundaries, restart, exit and shutdown still clear regardless, so the
known gap above is unchanged.

The same run exposed a second defect behind it: after that clear the parent sat
idle with the ask still owed and was never nudged or poked. `owed_reply_decision`
had returned `HoldQuiet` at the idle transition (blocked at the time), so no
`ReplyNudgeState` was created, and `sweep_owed` only poked agents that already
had one — nothing was left to wake it. The poke walk now also pokes an owed
agent with *no* nudge state whose debounced state is `Idle`; the worker runs the
gate, which performs round 1. Two guards on that branch: a blocked agent is
never poked, in the grace or past it (the fail-fast owns that case), and the
never-nudged branch runs the same backoff clock off the oldest owed ask's own
age (derived from its TTL deadline), because an agent handed an ask a moment ago
still reads debounced-idle until its `UserPromptSubmit` hook fires and the
sweeper ticks every second — poking there would nudge it for the message it is
in the act of answering.

### 4.3 Failure reason on the wire (contracts amendment 36)

- `MessageRecord` gains `reason: Option<String>`, `reason_code:
  Option<String>` (serialised, `null` unless `status = failed`). SQLite:
  two nullable columns, additive migration in `store/schema.rs`; existing
  rows read back as `None`.
- `Router::fail_message(id)` → `fail_message(id, FailReason { code:
  &'static str, reason: String })`. Codes: `timeout` (TTL — the code HTTP
  callers already see), `blocked_on_permission`, `agent_exited`,
  `agent_restarted`. `InjectError` maps to `agent_exited`/`agent_restarted`
  (no `inject_failed`). `reconcile_orphans` at open keeps its current
  behaviour and writes `reason_code = "orphaned"`.
- Trigger watcher (`trigger.rs`): the `agent_failed` arm of
  `terminal_completion` prefers the record's `reason`/`reason_code` when
  present; its own synthesised codes (`agent_exited` on lifecycle,
  `timeout` on watcher deadline, `internal`, bus-closed) stay. Inference
  does not go away; the record's code is now the first choice.
- Surfaces: `GET /v1/messages/{id}` and `message.status` carry the fields;
  `tempo status` prints them; `tempo ask` prints `reason` on a failed ask
  instead of a bare "failed"; `clients/js` `ReasonCode` union gains
  `blocked_on_permission` | `agent_restarted` | `orphaned` (it has no
  `MessageRecord` type — nothing else changes there); desktop feed shows
  `reason` on failed rows; `app/src/lib/types.ts` message type gains the
  two optional fields.

### 4.4 Docs

- This section; contracts amendment 36 (record fields, `reconsider`,
  `blocked_since`, `agent.stalled` semantics).
- CLAUDE.md: the edges bullet's "one nudge instead of `/clear`; idle again
  → `agent.stalled` and it is left un-cleared" becomes "…nudged on a
  60/120/240 s backoff while the reply is owed; `agent.stalled` marks each
  idle-after-nudge"; the in-turn-dialog bullet gains "owed asks fail after
  90 s with `blocked_on_permission`; subagent dialogs fire the parent's
  hook and are accepted while idle" and drops "Nothing auto-recovers it"
  in favour of "the agent is never touched".
- `agent.stalled`/`agent.nudged` rustdoc, roster tooltip, JS client README
  reason table, `tempo.example.toml` comment on `ask_timeout_minutes`
  ("…or sooner with `blocked_on_permission`").

### 4.5 Tests

Router (tokio `start_paused`, fake `StateSource` + recording
`InjectionQueue`): first idle nudges once; a second idle at 30 s → stalled
+ HoldQuiet, no nudge; poke at 61 s → nudge #2 and `nudges == 2`; cadence
60/120/240/240; blocked → HoldQuiet even when backoff elapsed; reply drops
state; restart drops state; exited owed asks fail `agent_exited`; a poke on
a non-idle agent is ignored (worker test). Sweeper: expiry precedes poke
(an ask at its deadline is failed, not poked). Blocked: `blocked_since` at
89 s → nothing; 90 s → every owed ask failed with the code and the tool in
the reason, agent state untouched, `send` untouched; no owed ask → nothing
at any age. PtyManager: `blocked` accepted at raw `idle`, dropped at
`starting`/`exited`; idle-while-idle report keeps the flag; `unblocked`
clears; `since` does not move on repeat. Store: migration round-trip,
old rows read `None`. Trigger watcher: record code wins for
`agent_failed`. CLI: `tempo status` prints reason; `tempo ask` shows it.
JS: `ReasonCode` accepts the new literals (type test). Mutation checks:
remove the poke → cadence test fails; remove the blocked branch → its test
fails; remove the `idle` acceptance → subagent test fails.

Real agent (reuse the scratch workflow above): (a) `Agent`-tool fan-out
that finishes — see nudge #2 land ≥ 60 s later and the parent poll and
reply; (b) subagent runs `perl -e` — see `agent.blocked` while the parent
is idle, the ask fail at 90 s with `blocked_on_permission: Bash(perl …)`,
⏸ still up, and no `/clear` typed.

**Amendment (2026-08-18, #63).** The blocked flag now gates the injection
path too, not only `owed_reply_decision`. The handle keeps it as a
`watch::Sender<Option<Blocked>>` and every `QueueWorker` holds the receiver:
`wait_debounced_idle` resolves only at idle **and** unblocked, so a new
ask/send addressed to an idle-but-blocked agent parks instead of being typed
into the dialog; it fails `InjectError::Blocked` → `blocked_on_permission`
once the dialog is 90 s old — the same clock as the sweeper, measured from
`Blocked::since`, and applied at *any* state (the in-turn dialog leaves the
agent working forever, and a message queued behind it must not park without
bound), so a message aimed at an agent already past the grace fails at once
and a `send` (no TTL) can never hang on the park. A dialog that opens in the
`SUBMIT_DELAY` gap between an injection's text and its Enter withholds the
Enter: the injection fails `Blocked` immediately and the text stays in the
input box unsubmitted (warned; the caller re-fires). Known limitation: a
*reply* injected into a blocked asker fails the same way at 90 s and is then
only logged — the record is already `replied` — so the asker never sees that
answer in its pane; safer than typing it into the dialog, but its turn is
stranded until it is answered or restarted. `consult_gate`
returns without typing while blocked (no nudge, no `/clear`) and
`ClearGate::on_stable_idle` returns `HoldQuiet` before it touches the
obligation turn, leaving the turn's one nudge unspent; a blocked→clear flip
at idle re-runs the gate poke-style so that deferred nudge goes out (after a
250 ms settle, skipped if the debounced state moved meanwhile — `report_state`
clears the flag and moves the raw state in one call, and the debounced
Working lands a hop later; drain-then-decide like the transition, so a message
queued while the dialog was up goes out first and the gate is not consulted).
The flag's watch bumps only on real flips (`send_if_modified`), so a clear on
an already-clear flag wakes nobody. The
"never touched" rule is unchanged: nothing is ever typed at a dialog.

**Amendment (2026-08-18, #54).** Enter can be lost even as a separate write
when a cold spawn is still drawing its welcome box, and no hook announces
"prompt ready", so the worker verifies every submit (injection and nudge):
after Enter it waits `SUBMIT_VERIFY` (2 s) for the debounced state to leave
idle — the `UserPromptSubmit` hook — and otherwise resends `\r`, at most
`MAX_ENTER_RESENDS` (2) times, then warns. The `Injected` ack is not held for
the verify. Any debounced change, an epoch bump, or the blocked flag rising
ends the wait: a resent Enter into a dialog would take its default. Reads
cloned receivers, so the run loop still sees every transition itself.

### Out of scope

Answering, escaping or restarting the dialog; configurable backoff/grace;
detecting a subagent that is running but not blocked (nothing to see).

### Delivery

One PR (`fix/owed-ask-watchdog`), closing #55 and #56, in this task order:
record fields + migration + `FailReason` → `blocked: Option<Blocked>` +
idle acceptance + `blocked_since` → `reconsider` + `ReplyNudgeState` +
sweeper pass → trigger/CLI/JS/desktop surfaces → docs → real-agent run.
