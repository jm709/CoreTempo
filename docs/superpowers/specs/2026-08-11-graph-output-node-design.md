# Graph output node — design

**Date:** 2026-08-11
**Status:** Approved
**Feature:** An `output` node on the workflow graph canvas that declares, shows,
and lights up the `[trigger.output]` contract

## Motivation

The trigger output widget (2026-08-07 design) renders validated `[trigger.output]`
results in the Run dock tab — but the workflow graph, the app's picture of "what
this workflow is", never shows that the workflow produces a structured answer at
all. Declaring the contract is also the last graph-invisible piece of workflow
authoring: agents and triggers are added from the canvas toolbar and edited in the
inspector, while `[trigger.output]` can only be typed in the toml view.

This feature adds a fourth node type — **output** — to the graph canvas. The box
appears whenever the workflow declares `[trigger.output]`, can be created from the
toolbar and edited in the inspector, and overlays live run data the same way agent
nodes overlay live agent state. The Run tab stays; the split is graph box for
at-a-glance, Run tab for history and repair detail.

The authoring flow this encodes, deliberately: the accepting side comes first.
The webhook consumer's JSON Schema exists (usually a `schema.json` written to the
caller's needs) → the toml declares it → the box appears → runs light it up. The
graph affordance points at an existing schema file; it does not author schemas.

## Decisions

Settled during brainstorming, with the chosen option first:

- **Visibility:** the node appears iff `[trigger.output]` is declared in the open
  workflow — config-node-with-live-overlay, the `AgentNode` pattern. (Rejected:
  runtime-only widget; showing it for any webhook.)
- **Size:** compact node like every other node; double-click jumps to the Run tab
  the way `AgentNode` double-click jumps to its terminal. (Rejected: embedding
  the full `OutputRenderer` in the node; an expandable node.)
- **Wiring:** a display-only edge from the kickoff target agent to the output
  node, so the canvas reads webhook → (ask) → agent → output. (Rejected: edge
  from the trigger; a floating box.)
- **Editability:** creatable from the toolbar and editable in the inspector —
  `schema_file` and `max_repairs` only. Inline `schema` is shown read-only with a
  pointer to the toml view; authoring JSON Schema in a 260 px panel is worse than
  toml. (Revised from an earlier display-only decision after walking the
  authoring flow.)

## Scope

In scope:

- `output` node type in `graphModel.ts` (`OUTPUT_NODE_ID`, projection in
  `toFlow`, `addOutput`/`removeOutput` helpers) plus a display-only edge.
- `OutputNode.svelte` with edit-time contract summary and run-time lifecycle
  overlay from the triggers store.
- `+ output` toolbar button in `GraphCanvas.svelte`.
- An Output form in `Inspector.svelte`.
- TS `TriggerModel` gains `output` — a type catch-up, not a wire change: the
  model `workflow_parse` returns is the full Rust `WorkflowFile`, whose
  `TriggerConfig.output` already serializes when present, and `merge.rs` already
  adds/updates/removes `[trigger.output]` (tests exist). No contracts amendment.
- One small core change: `validate_workflow` rejects an **empty** `schema_file`
  (today `""` satisfies exactly-one-of and only fails at run start; the toolbar
  stub makes that state reachable from the UI, so it must be save-blocking).
- Docs: correct the contracts doc's stale `TriggerConfig` shape — amendment 15
  predates `output`, which shipped with the 2026-08-06 design and never got a
  shape update. A doc correction for an already-shipped field, no wire change.

Out of scope (deliberately):

- Schema authoring UI. Inline `schema` editing stays in the toml view.
- Run tab changes.
- Backend or wire changes beyond the empty-`schema_file` validation line; the
  contracts doc edit above corrects a stale shape, it changes no contract.
- **Follow-up, filed with this design:** dangling `schema_file` — a non-empty
  path pointing at no file — still passes editor validation and fails at run
  start, because `validate_workflow` is text-only with no base path.
  `workflow_parse` has no path either; the fix is threading the workflow's
  directory into parse-time validation (or a dedicated check command), its own
  slice. Until then the node shows the declared path without confirming it
  exists.

## Graph model and projection

`FlowNode.type` gains `"output"`; `OUTPUT_NODE_ID = "§output"` joins the two
existing `§` ids. `toFlow` emits the node when `model.trigger?.output` is
present:

- **Position:** one column right of the kickoff target agent's slot, so the flow
  reads left to right; `freeSlot` handles collisions as it does for every node.
  If `trigger.edge.to` names no existing agent, fall back to one column right
  of the trigger node and suppress the edge (its source node would not exist;
  SvelteFlow would drop it with a console warning). This branch is unreachable
  through the model helpers today — `removeAgent` nulls the trigger, renames
  rewrite the edge — so it is purely defensive; the projection must never throw
  on a half-edited model.
- **Edge:** `<kickoff-agent> → §output`, id `${edge.to}>§output:output`, label
  `output`. It is a projection of the declaration, not an `EdgeModel`:
  `cycleKind` returns early for edges targeting `OUTPUT_NODE_ID`, and the node
  sets `connectable: false` so drags can neither start nor end on it (verified
  against the pinned @xyflow/svelte 1.6.2: an unconnectable handle gets no
  pointer events and fails `isValidHandle`).

Type changes this implies, enumerated: `FlowEdge.label` widens from `EdgeKind`
to `EdgeKind | "output"`, and `cycleKind`'s early return comes before its
`nextEdgeKind(edge.label, …)` call so the label narrows back to `EdgeKind`
there; `FlowNode` gains an optional `connectable` passed through to SvelteFlow
and an output-node `data` shape of `{ output: OutputModel }`. `connectAgents`
also gains an explicit `OUTPUT_NODE_ID` case so a drag that ever reaches it
(library regression) reports a real message instead of the roster-lookup
fallback ("no agent named '§output'…"), matching the `§trigger`/`§workflow`
cases.

`addOutput(model)` mirrors `addTrigger`: precondition trigger exists, type
`webhook`, kickoff kind `ask`, no output declared (the toolbar disables the
button in these cases; the helper still returns an error string for defense and
tests). It writes the stub `{ schema_file: "", max_repairs: 2 }`.
`removeOutput(model)` deletes the property (`delete`, per
`exactOptionalPropertyTypes`).

TS types (`types.ts`), mirroring `OutputConfig`'s serde form:

```ts
export interface OutputModel {
  schema?: unknown;
  schema_file?: string;
  max_repairs: number;
}
// TriggerModel gains: output?: OutputModel
```

`max_repairs` is always present in the wire form (serde default fills it on
parse), so it is non-optional.

## OutputNode.svelte

Follows `TriggerNode`/`AgentNode`: compact panel, target handle only, `mono`
styling, existing tokens.

Edit time (no live lifecycle):

- Title `⇥ output`.
- Sub-line: the `schema_file` name, `no schema file set` when it is empty
  (mirroring the trigger node's `no message set`), or `inline schema` when
  `schema` is the source.
- Sub-line: `max repairs <n>`.
- `incomplete` (dashed red border) when `schema_file === ""` — the on-start
  trigger-without-message pattern; save is blocked by validation (below) and the
  reason appears in the editor's footer.

Run time: the node derives the same lifecycle the Run tab shows —
`triggersState.selectedId ?? latest` — one source of truth, so picking a history
entry in the Run tab changes what the box shows. States:

- **working:** busy border tint, `in progress…`, plus a repair-rejection count
  when `rejections.length > 0`.
- **completed with `output`:** ok border tint, up to 4 top-level `key: value`
  preview lines via a new pure helper `outputPreview(output)` in
  `triggerHelpers.ts` (truncated values; non-object outputs render as one
  truncated line; a `+n more` line when keys overflow). Not the full
  `OutputRenderer` — the Run tab owns that.
- **completed without `output`** (declined or plain reply): ok tint, the result
  word (`replied (code 1)` style, matching the status bar's vocabulary).
- **failed:** err border tint, `reason_code` when present.

Double-click sets `uiState.dockTab = "run"`. The dock is always visible, so this
works in both stopped-editor and running-graph modes.

The overlay **persists after a run stops**, mirroring the Run tab: `stopRun`
resets agent state but not the triggers store, which clears on the next
`run.started`/`bus.reset`. This deliberately diverges from `AgentNode`, whose
overlay dies at stop — an agent's working/idle state is meaningless without a
run, but the trigger's result is the workflow's outcome and stays worth reading
in the stopped editor. The box and the Run tab always agree.

Like the agent nodes' live overlay, the box keys off the current run's store
regardless of whether the on-disk file still matches the frozen run config;
that is existing graph behaviour, not new to this node. The overlay
correlates kickoffs the same way the Run tab does — on the `trigger:<hex>`
origin (contracts amendment 38), so a plain authenticated `POST /v1/messages`
never opens one.

## Toolbar and inspector

`GraphCanvas.svelte` toolbar gains `+ output` after `+ webhook`:

- Enabled iff `model.trigger` is `webhook`, kickoff kind `ask`, and no output is
  declared. Disabled state carries a `title` explaining the precondition, like
  the trigger buttons.
- Click: `addOutput(model)`, select `§output`, `onchanged()`.

`Inspector.svelte` gains an Output form for `selected === OUTPUT_NODE_ID`
(joining the two existing `§` cases):

- `schema_file` text input, `missing` (red) styling while empty, placeholder
  `schema.json`. The input only appears when `schema_file` is the declared
  source; switching a workflow between inline and file schemas is a toml-view
  edit, not an inspector affordance.
- When inline `schema` is the source: a read-only row — `inline schema — <n>
  top-level keys` for object schemas, plain `inline schema` otherwise (a JSON
  Schema may legally be a boolean) — and the hint `edit the inline schema in
  the toml view`.
- `max_repairs` number input. The handler blocks only what a `u32` cannot
  represent — non-numeric input via `coerceNumber` (like the workflow form's
  numeric fields), non-integers, and negatives — since those would fail `u32`
  deserialization inside `workflow_merge` as an opaque IPC error. Out-of-range
  values (6+) reach the model and are answered by save-time validation
  (`max_repairs must be 0..=5`) in the footer, the same convention `setPort`
  already follows for the workflow form.
- `delete output` danger button: `removeOutput(model)`, deselect, `onchanged()`.

Flipping the trigger to `send` or `on_start` while output is declared stays
legal in the model and is blocked at save by core's existing validation
messages ("an output schema requires edge kind 'ask' …") through the editor's
footer — no new UI logic.

`delete trigger`, and deleting the kickoff agent (which nulls the trigger),
silently discard the output declaration along with the trigger. Output lives on
the trigger, so this containment is intended; graphModel tests pin it.

## Core validation change

`validate_workflow` (`core/src/workflow.rs`, `validate_trigger`) gains: if
`schema_file` is present and empty, emit a `trigger.output.schema_file` issue —
"schema_file is empty; point it at a JSON Schema file relative to the
tempo.toml, or use an inline schema". Errors are read by LLMs: state the fix.
This is the only non-frontend change.

## Testing

TDD throughout; everything here is exercisable without a real agent (no PTY or
TUI behaviour), with one live desktop pass at the end for the overlay.

- `graphModel.test.ts` (vitest): node + edge emitted iff output declared;
  position right of the kickoff agent; dangling-target fallback; `addOutput`
  preconditions (no trigger / on_start / send kickoff / already declared) and
  stub shape; `removeOutput` deletes the property; `removeTrigger` and
  kickoff-agent deletion discard the declaration (pinning the silent
  containment); edge id/label stable; edge suppressed in the dangling-target
  fallback.
- `graphEditing`/canvas logic: `cycleKind` ignores edges targeting `§output`
  (unit-test the guard's helper if extracted, otherwise cover via the
  interaction path).
- `triggerHelpers.test.ts`: `outputPreview` — flat object, > 4 keys (`+n
  more`), long values truncated, non-object output, empty object.
- Core (`core/tests/workflow_validate.rs`, beside
  `output_requires_exactly_one_schema_source`): empty `schema_file` rejected
  with the actionable message; existing exactly-one-of, kind, type, and
  `max_repairs` cases still pass.
- Merge: already covered (`adding_output_via_model_writes_the_section`,
  `removing_output_via_model_deletes_the_section`,
  `inline_output_schema_survives_unrelated_merge`); no new merge code.
- Mutation-proof per repo convention: where a test lands with its
  implementation, break the code and watch it fail.
- Live desktop pass: declare an output via `+ output` + inspector, save, run
  the webhook workflow from PR #23's live check, and verify the box's
  working → completed transition, the preview lines, dblclick → Run tab, and
  history-selection mirroring.
