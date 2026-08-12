# Workflow triggers: on-start and webhook

Date: 2026-08-03
Status: approved design, pre-implementation

Workflows currently start only when a human types the first message into chat
or `tempo ask`. This adds trigger nodes: **On-Start** (kick off automatically
when the run launches, batch-style) and **Wait-For-API** (webhook — a standing
daemon cold-starts the workflow per inbound HTTP call). Primarily for exported
deployments (`tempo export` → systemd/docker).

## Goals

- A workflow declares its own entry point as a trigger node on the canvas,
  serialized in `tempo.toml`.
- `on_start`: `coretempod run` becomes a batch job — launch, kick off with a
  configured message, exit 0/1 when the workflow completes. The desktop app's
  ▶ Run fires the same kickoff.
- `webhook`: a new `coretempod serve` mode listens with **no agents spawned**;
  each API call cold-starts a run, the request body is the kickoff message,
  triggers queue one-at-a-time, and agents are torn down on completion.
- Completion is inferred from the workflow itself: an `ask` kickoff completes
  when that ask goes terminal (reply = the HTTP result); a `send` kickoff
  completes on global quiescence.

## Non-goals

- Trigger chaining (`On-Start → Wait-For-API` composes nothing today — a
  webhook necessarily waits from launch). Trigger nodes are siblings; at most
  one per workflow in v1.
- Other trigger kinds (cron, file-watch). The schema is shaped for them; none
  are built.
- Parallel runs per daemon. One run at a time, FIFO.
- Durable trigger queue. The serve queue is in-memory and lost on daemon
  restart (documented tradeoff; systemd restarts the listener, not the queue).

## 1. Trigger declaration

New optional top-level section:

```toml
[trigger]
type = "on_start"                       # "on_start" | "webhook"
edge = { to = "planner", kind = "ask" } # same Edge shape as agent edges
message = "Plan the implementation"     # on_start only (required, non-empty)
```

- `edge` reuses the existing `Edge { to, kind }` type: `to` is the kickoff
  target, `kind` selects the completion rule (§2). Default kind in the UI is
  `ask`.
- `webhook` has no `message` — the HTTP request body is the message, verbatim.
  Instructions belong in the target agent's prompt.
- Validation (`validate_workflow`): `type` ∈ {on_start, webhook} (error names
  the valid kinds); `edge.to` must exist in the roster (same error style as
  agent edges); no self-referential concerns (triggers are not agents);
  `on_start` requires non-empty `message`; `webhook` rejects a `message` key
  (deny-unknown-style clarity: it would silently never be used).
- A workflow without `[trigger]` behaves exactly as today. The section is
  additive; `coretempod run` without a trigger still just runs until ctrl-c.

### Canvas

Two node types alongside the workflow node:

- **On-Start**: inspector edits the multiline `message`. One source handle.
- **Wait-For-API**: inspector shows the read-only endpoint path and a hint
  that the request body becomes the message. One source handle.

The node's single outgoing edge is `[trigger].edge`; drawing/deleting it edits
the section, and its ask/send kind is toggled like any agent edge. A toolbar
"+ trigger" control offers both types, disabled when a trigger exists.
Validation enforces at most one trigger node.

## 2. Completion

A completion watcher lives in `core` (shared by `run`, `serve`, and the app),
started when a kickoff is injected. It resolves when:

- **ask kickoff** → the kickoff message reaches terminal status (subscribes to
  `message.status` on the bus, the same machinery as `wait_terminal`). The
  reply body/code is the workflow's result.
- **send kickoff** → **global quiescence**: every agent debounced-idle, zero
  pending asks anywhere, all injection queues empty, no open obligation turns,
  held for one `idle_debounce` dwell.

**Arming guard (load-bearing).** Quiescence checking begins only after the
kickoff message is observed at status `working` — at the instant of injection
the whole predicate is already true (the system was idle before the trigger),
and a swallowed kickoff (the known unsubmitted-Enter failure mode) would
otherwise dwell straight to a false `quiesced` success with the payload
silently lost. A kickoff that never reaches `working` ends via the deadline as
`timeout`. (Safe interaction, verified: the auto-`/clear` session restart does
not flap debounced state — `SessionStart` reports `idle` onto an already-idle
channel — so `/clear` cannot reset the dwell.)

**State sources.** Debounced agent state and `pending_asks` are observable
today; queue emptiness and open obligation turns are not — the design adds two
read-only accessors (a per-agent queue-empty query on the PTY manager and an
open-turn query on the router), listed in §8. Failure fast-path: any agent
exiting while a kickoff is in flight fails the trigger immediately
(`agent.lifecycle` exited) instead of waiting out the deadline.

Downstream idleness never triggers anything by itself: quiescence requires the
*whole* system to settle simultaneously, so an agent idling while an upstream
agent works keeps the run alive and costs nothing.

**Residual risk (send kickoff).** The arming guard covers the kickoff itself;
a mid-workflow send between agents has an inherent hand-off window — the
message is injected (queue depth 0) before the receiver's `working` hook
lands, so a snapshot taken inside that window looks quiescent. The dwell,
then re-verify, catches it whenever the receiver starts working within one
`idle_debounce`, and open obligation turns catch a sender still owed a
step; a receiver slower than the dwell with no outstanding obligation is the
residual, accepted risk.

**Deadline.** One wall-clock deadline per kickoff, counted from kickoff
*creation* (matching the existing ask TTL, which also runs from creation),
reusing `ask_timeout_minutes`. Creation-based counting matters: injection can
be delayed indefinitely (an agent stuck in `Starting` — the trust-dialog
gotcha — never reaches idle), and an injection-based deadline would never arm,
hanging serve's one-at-a-time worker forever. Result labeling: if the deadline
elapsed, the result is `timeout` regardless of the underlying message status
(the TTL sweeper also marks expired asks `failed`; the wrapper's own clock
owns the label); a kickoff that fails before the deadline is `failed`. A
stalled agent (nudged, still short) never satisfies quiescence; the deadline
converts that to `timeout` rather than hanging.

**Event.** Completion publishes `workflow.completed { result, code?, reply? }`
on the bus, `result` ∈ {replied, quiesced, failed, timeout}. The app shows it
in the feed.

## 3. `coretempod serve` (webhook mode)

`coretempod serve <tempo.toml>` — valid only when the workflow declares a
`webhook` trigger (error otherwise, naming `run` as the alternative). Owns the
standing HTTP listener on the configured port; holds no agents between
triggers.

Worker loop per trigger:

1. Dequeue from a bounded in-memory FIFO (cap 32; overflow → 429 with queue
   depth in the message). Payload cap 64 KB (413 above — the payload is typed
   into a TUI prompt; megabytes are not plausible input). Payloads are
   normalized before injection: CRLF and lone `\r` become `\n` (a raw `\r` is
   Enter to the queue and would submit the prompt mid-payload).
2. Re-load and freeze the toml, verifying its hash against the **hash taken
   at serve startup** — an edited file fails the *trigger* (not the daemon)
   until the daemon restarts. Deterministic, mirrors the frozen-roster rule;
   per-trigger adoption of edits is explicitly rejected (an edit could remove
   the trigger or break validation mid-queue).
3. `Run::start` with the run's `/v1` API bound to an **ephemeral loopback
   port**. This requires restructuring `Run::start` to bind the listener
   *before* constructing `AgentEnv` — today the env port is fixed pre-bind,
   and agents prefer `CORETEMPO_PORT` over `api.json`, so a port-0 bind would
   hand agents a dead port. The ephemeral port is injected programmatically
   (the file-level `port` stays validated non-zero); the public port stays
   with the trigger server. Serve-mode runs do not repoint the
   `~/.coretempo/runs/current` symlink (agents get everything via env), and
   teardown deletes the run's artifact directory — a long-lived daemon must
   not accumulate one dir per trigger forever.
4. Inject the kickoff (normalized payload as message, per `edge.kind`).
5. Await the completion watcher or the deadline.
6. `run.stop()` teardown, record the result, next trigger.

**Ctrl-c in serve mode:** stop the in-flight run, fail queued triggers with a
`daemon_shutdown` reason (observable via any still-connected `?wait`
long-poll, which returns that failure), then exit.

**Shutdown-hang fix (in scope, shared).** Teardown and daemon ctrl-c must
complete with SSE clients attached — the known graceful-shutdown hang
(`/v1/events` and PTY streams never end, so graceful shutdown awaits forever)
is fixed in the shared `ApiServerHandle::shutdown` path: streams are
closed/aborted on shutdown rather than awaited. This deliberately changes
desktop-app `Run::stop` behavior too (it currently inherits the same hang);
a serve-only fork of the shutdown path is explicitly rejected.

## 4. `on_start` in run mode and the app

- `coretempod run` with an `on_start` trigger: after `spawn_all`, inject the
  configured `message` to the target, await completion, exit — code 0 for
  `replied` (code 0 reply) / `quiesced`, 1 for `failed`/`timeout`/reply code 1.
  Without a trigger, `run` behaves as today (until ctrl-c).
- Desktop app: ▶ Run on an `on_start` workflow fires the same kickoff after
  spawn. The run stays open for inspection; `workflow.completed` appears in
  the feed (no auto-stop in the app).
- Any warm run of a `webhook` workflow — the desktop app or a plain
  `coretempod run` — mounts `POST /v1/trigger` on the run's own `/v1` API:
  warm testing via `curl`, no export needed. No cold start; it injects the
  kickoff into the live roster and shares the same watcher/result reporting.
  Warm triggers do not queue: while a kickoff is in flight the endpoint
  returns 409 (`trigger_in_flight`, naming the active trigger id). (`run` on
  a webhook workflow otherwise behaves as today: agents up, waiting, until
  ctrl-c.)
- `coretempod run` + `on_start` interrupted by ctrl-c mid-await: stop the run
  and exit 130 (interrupted), distinct from the 0/1 completion codes.

## 5. API surface (serve mode)

- `POST /v1/trigger` — body = kickoff payload (any content type; must be
  valid UTF-8, else 400 — this route is exempt from the run API's JSON-only
  content-type guard, which would otherwise 415 text/plain webhook bodies).
  `202 {trigger_id, position}` immediately; with `?wait=<seconds>` long-polls
  and returns the completion result if it arrives in time, else the 202
  shape. Auth: the same bearer token scheme as the rest of `/v1` (exports
  already require a provisioned token off-loopback) — the token check is
  extracted into a narrower auth state, since serve mode has no run-scoped
  `ApiContext` between runs.
- `GET /v1/trigger/{id}` — `queued {position} | running | completed {result,
  code?, reply?} | failed {reason}`. In-memory records, last 100 kept.
- `GET /v1/health` — daemon up, queue depth, current run id if any.

Error messages follow the errors-are-read-by-LLMs convention throughout.

## 6. Export

`tempo export` reflects the trigger in both templates (systemd unit `ExecStart`
and dockerfile `ENTRYPOINT`):

- `webhook` → `coretempod serve`.
- `on_start` → `coretempod run` with `Restart=always` (not `on-failure` — a
  successful batch run exits 0, which `on-failure` would leave stopped) plus a
  comment explaining the re-running-batch-worker semantics.
- No trigger → unchanged output.

## 7. Testing

Core (TDD):

- Validation: bad `type`, unknown `edge.to`, empty/missing `on_start` message,
  `message` on webhook, two triggers (canvas-level, but the model helper
  enforces it too).
- Completion watcher unit tests on the same fake state channels the
  queue/debouncer tests use: ask-terminal path; quiescence path including
  "downstream idle while upstream works ≠ quiescent"; dwell required; stall →
  deadline → `timeout`; `workflow.completed` wire-form test.

Daemon integration (scripted fake agent):

- serve: trigger → cold start → completion → teardown → second queued trigger
  runs next; queue overflow → 429; hash-mismatch toml → failed trigger,
  daemon alive; teardown completes with an SSE client attached.
- run + on_start: kicks off with the configured message and exits with the
  mapped code.

Frontend (vitest): trigger node ↔ `[trigger]` mapping and merge round-trip;
single-trigger validation; edge-kind toggle on the trigger edge.

Real-agent check (required — touches spawn and injection): an `on_start`
workflow under `coretempod run` that exits 0 on its own; a webhook `curl`
round-trip against `serve` including a second trigger queued behind the
first. Trivial prompts.

## 8. Contracts amendments

1. `WorkflowFile` gains the optional `trigger` section (`TriggerConfig {
   type, edge: Edge, message: Option<String> }`).
2. `EventPayload` gains `workflow.completed { result, code?, reply? }`.
3. New endpoints `POST /v1/trigger`, `GET /v1/trigger/{id}` (serve mode and,
   for webhook workflows, the run API; the trigger POST is exempt from the
   JSON content-type guard). Serve-mode health has its own shape (daemon up,
   queue depth, current run id if any) — a distinct type, not a bent `Health`.
4. `coretempod` gains the `serve` subcommand; `run` gains on-start batch
   semantics (exit-on-completion when a trigger is declared; 130 on ctrl-c).
5. Two read-only quiescence accessors: per-agent queue-emptiness on the PTY
   manager, open-obligation-turn query on the router.
6. `Run::start` binds the API listener before constructing `AgentEnv` so
   agents receive the actually-bound port (enables the ephemeral serve-mode
   port); a run-options input selects ephemeral vs configured port and
   whether the `current` symlink is repointed.
7. `ApiServerHandle::shutdown` aborts SSE streams instead of awaiting them
   (shared fix; changes desktop `Run::stop` behavior too).
