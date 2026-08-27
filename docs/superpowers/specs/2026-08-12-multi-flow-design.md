# Multiple flows per workflow file

Date: 2026-08-12
Status: approved design, pre-implementation

A tempo.toml today declares at most one `[trigger]`, so one file gives one
webhook endpoint, and every triggered run spawns the entire roster even when
the kickoff touches a single agent. This adds named **flows**: one file holds a
shared `[agents.*]` pool plus `[flows.<name>]` sections, each an independent
sub-workflow — its own agent subset, trigger edge, and output schema. Serve
mode runs flows concurrently under a global cap, with per-agent
readers/writers locks giving the author explicit control over what may
overlap.

Two motivating cases (readers/writers):

- **Writer**: a social-media-manager agent receives webhooks from many sites
  and must handle one at a time; other hooks queue. Serialization is the point.
- **Reader**: an agent that never mutates its working directory — it reads
  context and produces output. Duplicating it (several live sessions at once)
  is safe enough and desirable for throughput.

## Goals

- One tempo.toml declares any number of flows; each webhook flow is its own
  HTTP endpoint with its own output contract.
- A triggered run spawns **only the flow's agent subset**, and its completion
  rules (`ask` → terminal reply, `send` → quiescence) are scoped to that subset.
- Serve mode runs different flows concurrently, bounded by
  `max_concurrent_runs`; the same flow stays FIFO unless all of its agents are
  shared.
- Per-agent `concurrency = "exclusive" | "shared"` (default `exclusive`)
  declares whether an agent's sessions may overlap across runs. Exclusive
  agents serialize every flow that includes them; shared agents overlap freely.
- The old single `[trigger]` section is removed, not deprecated. One config
  shape.

## Non-goals

- Per-flow overlap caps, queue priorities, or cancel-in-progress. The global
  cap is the only throughput knob.
- Per-flow-membership access modes (`agents = [{ id, access }]`) — the
  asymmetric writer-here-reader-there case. If it materializes, the per-agent
  field is replaced by per-membership modes in one release; the migration is
  mechanical (`concurrency = "shared"` on the agent becomes `access = "read"`
  at each use site).
- Durable trigger queues (unchanged: in-memory, lost on daemon restart).
- Canvas editing of flow agent-subsets. The node editor renders flows and edits
  their triggers (§8); subset membership is edited in TOML.
- Cross-flow messaging. Flows in one file share agent *definitions*, never a
  live run.

## 1. Config shape

```toml
[agents.social-media-manager]
dir = "./work/smm"
prompt = "..."
# concurrency = "exclusive" is the default: one live session anywhere,
# every flow that includes this agent serializes against it.

[agents.classifier]
dir = "./work/kb"
prompt = "Answer questions about the knowledge base. Never modify files."
concurrency = "shared"

[flows.post]
agents = ["social-media-manager"]
trigger = { type = "webhook", edge = { to = "social-media-manager", kind = "ask" } }

[flows.classify]
agents = ["classifier"]
trigger = { type = "webhook", edge = { to = "classifier", kind = "ask" } }
[flows.classify.output]
schema_file = "./classify.schema.json"
```

- `[agents.*]` remains the single pool of definitions and gains one field:
  `concurrency = "exclusive" | "shared"`, default `exclusive`. The mode lives
  on the agent — next to the prompt that determines whether it mutates
  anything — so it cannot be declared inconsistently across flows.
- `[flows.<name>]`: `agents = [...]` (non-empty subset of the pool),
  `trigger = { type, edge, message? }` (the existing `TriggerConfig` shape,
  minus its own `output` field), optional `[flows.<name>.output]` (existing
  `OutputConfig`: exactly one of `schema`/`schema_file`, `max_repairs`).
- Flow names use the agent-id charset. Top-level `[trigger]` is **removed**.
  `WorkflowFile` is `deny_unknown_fields`, so the deletion alone would yield
  serde's generic unknown-field error; the loader special-cases that error for
  `trigger` and appends the `[flows.<name>]` rewrite. No dual config shape —
  just a better error.
- `[server]` gains `max_concurrent_runs` (default 2, range 1..=16): the
  ceiling on simultaneously live runs in serve mode. Each run is a roster of
  real `claude` sessions — RAM and token spend — so the default is
  conservative. File-only: it does not join the flags > env > file server
  resolution (it is workflow-shaped, not deployment-shaped).
- A file with zero flows is valid and behaves exactly like today's
  non-triggered workflow: warm run until ctrl-c.

Validation (freeze-time, errors written for LLMs — name the roster, the valid
values, the fix):

- Every flow member exists in the pool; the trigger edge target is a member.
- The subset is **edge-closed**: if member A declares an edge to B, B must be
  listed. The error names the flow, the edge, and the missing agent.
- `type` ∈ {on_start, webhook}; `on_start` requires non-empty `message`;
  `webhook` rejects `message` (unchanged rules, now per flow).
- Each webhook flow's output schema compiles (unchanged, now per flow).
- `concurrency` ∈ {exclusive, shared} (serde enum; parse error names both).

## 2. Freeze

`FrozenWorkflow` gains a `flows` map: per flow, the resolved member set, edge,
trigger type, kickoff message (on_start), and compiled `OutputContract`. The
content hash covers the tempo.toml bytes plus every flow's `schema_file` bytes
appended in flow-name order, so serve mode's edit-refusal
(`workflow_changed`) works unchanged.

A subset run (serve cold-start, `run --flow`) gets a **derived**
`FrozenWorkflow` whose `agents` map is the flow's member set — hash and source
path unchanged. Everything that iterates the roster (`create_message` target
validation, the system prompt's "Other agents:" list, settings-file
generation, `spawn_all`, the quiescence roster) then sees only the members: a
subset run's prompts never name non-spawned teammates, and
`tempo ask <excluded-agent>` fails validation naming the run's actual roster
instead of injecting into a nonexistent PTY.

## 3. Store run-scoping (prerequisite — ships first)

Concurrent runs share one SQLite file (`per_run_server` clones the daemon's
`db` path), but `messages` and `agent_events` have no run column, and the
restart sweep (`Router::on_agent_restarted` → `pending_to_agent` /
`pending_asks`) fails every non-terminal row addressed to an agent id. With
two live runs sharing an agent id, a restart in run B would fail run A's
in-flight messages. Therefore, before any concurrency ships:

- The store gains schema versioning — `PRAGMA user_version`, today absent
  (the schema is bare `CREATE TABLE IF NOT EXISTS`, so adding columns to the
  DDL silently does nothing to existing files and inserts then fail at
  runtime). Migration 1: `ALTER TABLE ... ADD COLUMN run_id` on `messages`
  and `agent_events`, nullable; legacy rows stay NULL and are excluded from
  run-scoped queries. v1.0.0 dbs exist in the wild — a test must open a
  pre-migration db file and round-trip.
- `run_id` threads through inserts and queries; `pending_to_agent`,
  `pending_asks`, and the restart sweep scope to the calling run.
- `MessageId` widens from 8 to 16 hex chars (32 → 64 bits). The current
  entropy over a shared persistent primary key reaches ~50% collision odds
  around 77k accumulated rows; widen while the schema is open.
- The desktop history view keeps reading one db file across runs (this is why
  run-scoping beats per-run db files).

## 4. Serve-mode scheduling

Per-flow FIFO queues replace the single queue; the `TriggerHub` stays global
(trigger ids are unique across flows), but its single `in_flight` slot —
which underpins serve's `begin` and warm mode's 409 — becomes per-flow keyed.
A trigger's lifecycle:

1. Dequeue from its flow's queue (per-flow order preserved).
2. Acquire the flow's per-agent locks **in sorted agent-id order** — one
   `tokio::sync::RwLock<()>` per pool agent, `read()` for shared members,
   `write()` for exclusive members. Sorted acquisition prevents deadlock;
   tokio's RwLock is FIFO-fair and write-preferring, so queue order holds and
   writers never starve behind readers.
3. Acquire a `max_concurrent_runs` semaphore permit. **Locks before permit**:
   a worker blocked on a busy exclusive agent holds nothing the cap counts,
   so a queued writer flow never parks a permit a disjoint flow could use.
   The order is deadlock-free — a permit holder never waits on locks — and a
   lock holder waiting for a permit only extends serialization its contended
   agent already imposed.
4. Cold-start a run spawning **only the flow's members** (the derived
   `FrozenWorkflow`, §2); kickoff, watch, tear down; release locks and permit.

The flow worker spawns step 4 without awaiting it, so a flow whose members are
all `shared` overlaps with itself; any `exclusive` member self-serializes the
flow through its own write lock. Flow self-concurrency therefore needs no
config axis of its own — the same locks yield both the writer-queueing and
reader-pool behaviors.

Completion becomes per-flow: a `send` kickoff completes on quiescence of the
flow's member set, not the pool. Concretely: the router's
`total_pending_asks`/`open_turns` counters gain member-set-scoped variants,
and `WatchInputs.roster` becomes the flow's member set (in serve mode the
derived roster makes this automatic; warm mode must pass the subset
explicitly). The armed-only-after-`working` guard is unchanged.

Shutdown gains two arms for the new blocking points. Lock and permit
acquisition race the shutdown signal — a worker parked on either settles its
dequeued trigger as `daemon_shutdown` immediately instead of burning the
drain grace. And because step 4 is spawned un-awaited, the run task settles
its trigger and releases its outstanding slot on **every** exit path —
`Run::start` failure and panic included — via a settle-on-drop guard.
In-flight runs get the drain grace; everything still queued fails with
`daemon_shutdown`, as today.

## 5. API surface

| Route | Behavior |
|---|---|
| `POST /v1/flows/{name}/trigger` | Fires the named webhook flow. Body is the kickoff verbatim; `?wait=<secs>` long-polls; 202 + `trigger_id`/`position` otherwise. Unknown name → 404 listing declared flows; an `on_start` flow → 400 explaining it fires at launch via `run --flow`. |
| `GET /v1/trigger/{id}` | Unchanged; ids are global across flows. |
| `GET /v1/flows` | Lists flows: name, trigger type, target, queue depth, running count. |
| `GET /v1/health` | Gains per-flow queue depths and a total running count. |

Bare `POST /v1/trigger` is removed; its 404 names the declared flows and the
new route. `queue_full` (429) is per-flow, `QUEUE_CAP` per flow. Once the
daemon is interrupted the listener keeps answering until the drain finishes,
and a trigger arriving in that window gets 503 `shutting_down` — not a 429,
whose "retry once one completes" advice would send the caller back at a daemon
that is going away.

**Warm runs** (desktop, `coretempod run`): the same routes exist on the run's
own `/v1` API and fire against the live roster. A warm run has exactly one
live instance of each agent, so duplication does not apply there: one
in-flight trigger per flow (409 `trigger_in_flight` otherwise), and a warm
trigger holds per-agent locks for its duration — two flows sharing an
`exclusive` agent serialize in warm mode too, rather than interleaving
conversations in its one live session. The warm lock table is the run's own
(living in `ApiContext`), a separate instance from the daemon scheduler's —
each guards its own roster. `shared` members take read locks, so all-shared
flows still overlap across flows (never within one — the per-flow 409
stands). One conservative wrinkle: two overlapping flows sharing an agent
each count it in their quiescence roster, so flow A's `send` completion can
wait out flow B's activity on that agent — a delay, never a false
completion, since arming is per-kickoff. Duplication only ever happens in
serve mode's cold-started runs.

## 6. Run mode and the desktop app

- Bare `coretempod run` / desktop ▶ Run: warm whole-pool run — every pool
  agent spawns, all webhook flows' routes are armed, nothing auto-fires. The
  development view: watch every flow in one window.
- The desktop Run tab gains a per-flow fire control for `on_start` flows: it
  injects the flow's configured message into the warm pool through the
  existing desktop kickoff machinery. Without it, `run --flow` would be the
  only way to execute an on_start flow — the canvas could create flows the
  desktop cannot run.
- `tempo export` re-bases on flows: any webhook flow → a `serve` unit
  (unchanged shape); `tempo export --flow <name>` emits an on_start batch
  unit whose `ExecStart` is `coretempod run --flow <name>`; a file with no
  flows keeps today's plain run unit. Exporting a file whose only flows are
  on_start without `--flow` fails, naming the flows to pick from.
- `coretempod run --flow <name>`: spawns only that flow's members. If the flow
  is `on_start`, injects its message at launch and exits 0/1 on completion
  (this replaces today's whole-file on_start behavior). If it is `webhook`,
  the run is warm with just that flow's route armed.
- `coretempod serve` requires ≥1 webhook flow and refuses otherwise, naming
  what it found (on_start-only or zero flows).

## 7. Clients

`@coretempo/client` targets `/v1/flows/{name}/trigger`; the flow name is a
constructor option. Major version bump, no compatibility shim. The `tempo` CLI
is untouched — agents always talk within their own run.

## 8. Canvas (node editor)

The singleton trigger node generalizes: one trigger node per flow (node id
carries the flow name), each with its single outgoing edge to its target and
its own output node when `[flows.<name>.output]` exists. The "+ trigger"
toolbar control creates a new `[flows.<name>]` with a generated name
(`flow-N`, skipping names already taken) spanning the full roster (the author
narrows `agents = [...]` in TOML); it is no longer disabled after the first
trigger. Deleting a trigger node deletes its flow section. Subset editing in
the inspector is a non-goal (§Non-goals).

Blast radius beyond the graph model: the TOML round-trip writer (the
src-tauri merge layer), the wire types, and the trigger state store /
trigger-node / inspector / Run-tab components all carry the flow name today
as an implicit singleton and become keyed by it.

## 9. What `shared` promises

`concurrency = "shared"` is the author asserting the agent does not mutate its
working directory and does not rely on directory-scoped Claude Code state.
Concurrent sessions in one directory are officially unsupported by Claude
Code; the documented sharp edges are shared chat history/context bleed
(claude-code#7702), `.claude/plans/` files clobbering each other
(claude-code#27311), and `.claude/settings.local.json` races when sessions
record new permission grants (claude-code#41259 — largely mitigated by
CoreTempo's pre-allowlisted generated settings). Session transcripts are keyed
by session id and do not collide. `~/.claude.json` corruption under concurrent
*processes* (claude-code#29051) exists today with any multi-agent roster;
flows change its magnitude, not its existence. In warm mode the analogous
opt-in hazard is different in kind: cross-flow overlap on a `shared` agent
interleaves two callers' prompts in one live session — context bleed between
conversations, not directory races. The default is `exclusive` precisely so
nobody opts into either accidentally.

The lock key is the **agent id**, not the directory: two distinct pool agents
sharing a dir run concurrently today (the shipped example config does it) and
that stays the author's judgment call.

## 10. Testing

- **Unit**: config parse and validation errors (missing member, edge-closure,
  bad `concurrency` value, leftover `[trigger]` rewrite hint); freeze hash
  covering multiple schema files; sorted lock acquisition.
- **Integration (scripted fake agents)**: two disjoint flows overlap; an
  exclusive agent shared by two flows serializes them FIFO; an all-shared flow
  overlaps with itself up to the cap; the cap blocks run N+1; a restart in one
  run does not sweep another run's messages (store scoping); per-flow
  `queue_full`; shutdown fails queued triggers across all flows; shutdown
  while a worker is lock-blocked settles its trigger within the grace; a
  `Run::start` failure in the spawned task settles the trigger and frees its
  outstanding slot; a pre-migration `tempo.db` opens, migrates, and
  round-trips. Warm mode: two flows sharing an exclusive agent serialize on
  one live roster; a second trigger to the same flow 409s.
- **Real-agent smoke** (repo convention — fakes cannot catch PTY timing):
  a two-flow serve config with one shared reader; fire both endpoints
  concurrently, check both round-trip. Trivial prompts; real tokens.
- TDD throughout: failing test first, per repo convention.

## 11. Sequencing

1. **Store run-scoping + schema versioning + MessageId widening** (§3) —
   independent, lands green alone, removes the correctness blocker.
2. **Config + freeze + consumer rewrite** (§1–2): `[flows]`, `concurrency`,
   validation, hash, the derived subset `FrozenWorkflow` — **and** the
   mechanical rewrite of every `WorkflowFile.trigger` consumer to read the
   flows map with single-flow-equivalent behavior: the on_start kickoff and
   warm trigger handler in core, serve's edge resolution, `run_until_interrupt`,
   the desktop kickoff command, export's template choice, and the canvas wire
   types. `[trigger]` cannot be removed piecemeal — the field is read across
   core, daemon, app, and export, so removing it and rewriting its readers is
   one change. `tempo.example.toml` and docs move here too.
3. **Serve scheduler** (§4): per-flow queues, RwLocks, semaphore, per-flow
   completion scoping (member-scoped counters), settle-on-drop.
4. **API routes + `run --flow` + fire control + clients + canvas UI + export
   `--flow`** (§5–8), plus the docs sweep: the CLAUDE.md `[trigger]`
   paragraph, README, a contracts-doc reconciliation amendment for the
   `WorkflowFile`/`FrozenWorkflow`/`MessageId` shape changes, and the
   protocol primer's example message ids (they display the widened form).

Each phase passes `./dev check` on its own.
