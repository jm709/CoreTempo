# Trigger output widget — design

**Date:** 2026-08-07
**Status:** Approved
**Feature:** In-app trigger lifecycle tab rendering structured `[trigger.output]` results

## Motivation

The `[trigger.output]` feature (2026-08-06 design) delivers a validated, parsed
`output` object to HTTP callers — but the desktop app is a black box while that
happens. The only in-app signal is a one-line status-bar banner ("workflow replied
(code 0)") with no reply text, no `output`, and no visibility into the schema-repair
loop; each trigger overwrites it, and a reload loses even that. The adding-to-DC
integration exercises the full trigger path today and gets nothing visual in
CoreTempo.

This feature adds a third dock tab that shows each trigger's full lifecycle and
renders the validated `output` visually. The design constraint that shaped every
choice: **zero adoption ceremony**. A workflow author who already has
`[trigger.output]` in their `tempo.toml` gets the widget for free — no new config
keys, no new API calls, no UI-specific declarations. This is distinct from issue
#18 (an embeddable widget for external project websites, still deferred).

## Scope

In scope:

- Additive enrichment of the `workflow.completed` bus event (contracts amendment).
- Trigger state in the boot snapshot; `Run` retains the `TriggerHub`.
- Desktop `on_start` kickoffs registered in the hub.
- A third dock tab: latest trigger lifecycle + in-memory session history.
- Value-driven generic rendering of the `output` object.
- Reducer handling for `reply.rejected` (declared in the TS event union today,
  silently dropped).
- `tempo.example.toml` fix: the `[trigger.output]` example currently sits under an
  `on_start` example and fails load validation if uncommented as written.

Out of scope (deliberately):

- Issue #18's external embeddable widget.
- Schema editing (or display) in the Inspector — a read-only `[trigger.output]`
  row is a flagged follow-up.
- An in-app "fire trigger" affordance. A real adoption win (authors could see the
  widget without curl/token ceremony) but new command surface; its own slice.
- Schema-informed field ordering (see Renderer).
- Persisting rejection history. Across reloads, rejections are best-effort via
  bus-ring replay; accepted loss.

## Wire: enrich `workflow.completed`

Verified: `workflow.completed` already fires **once per trigger kickoff**, not once
per run — every warm-run `POST /v1/trigger` spawns its own `watch_completion`
(`core/src/api/trigger.rs`), as do serve-mode cold starts (`daemon/src/serve.rs`)
and the desktop `on_start` path (`app/src-tauri/src/commands.rs`), and
`watch_completion` itself publishes the event (`core/src/trigger.rs`). The kickoff
is already observable in the UI as `message.created` with `from = "http:<hex>"`
(trigger id `t-<hex>`). The only data missing from the bus is terminal.

`WorkflowCompleted` gains additive fields:

- `trigger_id: Option<String>` — `None` for desktop `on_start` kickoffs until they
  register in the hub (this slice makes them register, so in practice always set).
- `message: MessageId` — the kickoff message id, correlating the completion to the
  feed and to the lifecycle the reducer has already assembled.
- `output: Option<Value>` — present only when a schema was declared and the final
  body validated.
- `reason: Option<String>`, `reason_code: Option<&'static str>` — failure detail,
  mirroring what the HTTP wire already carries.

Everything except the trigger id is already in scope inside `watch_completion`;
the id becomes one new `WatchInputs` field passed by its three callers. The event
union is not frozen — additive amendment is the established precedent
(`agent.nudged`/`agent.stalled`, `workflow.completed` itself, `reply.rejected`,
`reason_code`). This lands as a new reconciliation amendment in
`docs/superpowers/plans/2026-08-01-contracts.md`. A separate `trigger.completed`
event was considered and rejected: kickoff receipt is already `message.created`,
so a second event adds surface without adding information. Riding the existing bus
also means headless observers get the enrichment through SSE `/v1/events` for free
— the plumbing is reusable infrastructure, not desktop-only.

The `http:<hex>` origin is not exclusive to trigger kickoffs: any authenticated
`POST /v1/messages` without an `X-CoreTempo-Agent` header also gets
`Origin::Http(<request-id>)` (`core/src/api/auth.rs`), so the UI's kickoff
correlation can open a lifecycle row for a non-trigger HTTP message until a
dedicated origin discriminator lands (tracked follow-up; a reload clears such
rows since the snapshot reseeds from the hub).

## Snapshot: surviving reload

The boot snapshot (snapshot-then-subscribe, `app/src/lib/session.ts`) carries no
trigger state; today even the completion banner dies on reload. Changes:

- `Run` keeps an `Arc<TriggerHub>` clone. Currently the hub is moved into
  `ApiContext` at construction (`core/src/run.rs`) and is unreachable from `Run`;
  a `triggers()` accessor exposes the records.
- `Snapshot` gains `triggers: Vec<TriggerView>` (already `Serialize`), sourced from
  the hub's insertion-ordered records. The hub's existing 100-record cap **is** the
  in-memory session history the tab shows — no new storage invented.
- The desktop `on_start` path registers its kickoff in the hub so non-webhook runs
  populate the tab identically.

Snapshot seeding reconstructs latest + history after a reload; live events take
over from there. Mid-trigger reloads reconstruct "working" from the snapshotted
kickoff message.

## UI: the Run tab

A third dock tab — **Run** — beside Feed and Chat, following `Dock.svelte`'s
always-mounted pattern. Layout:

- **Latest trigger, prominent.** Lifecycle states: kickoff received → agent
  working → one entry per repair rejection (`reply.rejected`, with its validation
  errors) → terminal state. Terminal states render distinctly: validated `output`
  (the visual widget), agent declined (`code 1`, the agent's prose explanation),
  failed (`reason_code` + reason).
- **Session history.** A compact list of earlier triggers (id, terminal state,
  time); clicking one shows its record in the main area. In-memory only.

New state module `app/src/lib/state/triggers.svelte.ts` (module-level `$state` +
mutation functions + `reset*()`, matching every other store). The reducer
(`wireEvents.ts`) gains cases for `reply.rejected` and the enriched
`workflow.completed`, plus kickoff detection off `message.created` with
`from: "http:*"`; `applySnapshot` seeds from `snapshot.triggers`.

## Renderer: value-driven, not schema-driven

The workspace pins `serde_json` without `preserve_order`, so every `Value` object
— schema and output alike — reaches the wire with keys alphabetized. Schema-driven
field ordering is therefore unbuildable without a core-wide `preserve_order`
change that would perturb prompt composition and error rendering for cosmetics.
The renderer works from the validated value alone:

- Object → key/value card.
- Array of objects → table (column union across rows).
- Array of scalars → list.
- Long string (multi-line or past a length threshold) → prose block.
- Nesting beyond a depth cap, or anything unrecognised → `<pre>` JSON fallback.

Keys are deterministic (alphabetical), so layout is stable across runs. The
node-type decision logic lives in a plain-TS helper (`triggerHelpers.ts`) so it is
unit-testable like `feedHelpers.ts`; `OutputRenderer.svelte` recurses over the
helper's classification. Styling via existing tokens (`styles/tokens.css`) and
scoped CSS — no new dependencies. Calibration target: adding-to-DC's output is a
flat-ish object, so the key/value card is the primary form; tables, prose blocks
and the fallback keep other shapes presentable rather than optimal.

## Docs

- `tempo.example.toml`: move the `[trigger.output]` block under a webhook trigger
  variant so uncommenting it loads.
- `CLAUDE.md`: the "18 amendments" count is stale (23 before this design); update
  alongside the new amendment.

## Testing

TDD throughout; the scripted fake agent covers everything (no PTY timing or TUI
behaviour involved).

- Wire: enriched `WorkflowCompleted` serialization; `trigger_id` threaded through
  all three `watch_completion` call sites; `output` present only on validated
  success; `reason_code` on failure kinds.
- Snapshot: `triggers` populated from the hub; `on_start` kickoff registered;
  ordering and cap respected.
- Reducer/state (vitest): `reply.rejected` appends to the open trigger's
  rejection list; enriched completion terminalises it; kickoff detection from
  `from: "http:*"`; snapshot seeding; `bus.reset` resync.
- Renderer helper (vitest): classification per shape — flat object, array of
  objects with ragged keys, scalar array, long string, deep nesting → fallback;
  empty object/array edges.
- Component: covered by helper/store unit tests plus live desktop verification of
  the replied path, history, selection and snapshot reseeding; the failure,
  decline, and repair-rejection branches are covered by unit tests only.
