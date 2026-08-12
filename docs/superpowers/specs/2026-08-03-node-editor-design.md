# Node-based workflow editor

Date: 2026-08-03
Status: approved design, pre-implementation

Replaces the raw-toml textarea with an n8n-style node/edge canvas, gives edges
real semantics (deterministic delegation steps enforced by the server), and
fixes the workflow-file path handling that made `~/CoreTempoWorkflows/tempo.toml`
fail with ENOENT.

## Goals

- Edit `tempo.toml` through nodes (agents) and edges (delegation steps), not raw
  text. Raw text stays available as a toggle.
- An edge is a deterministic workflow step: the user chooses `ask` or `send` per
  edge; the agent is told exactly which command to run. Workflows should happen
  the same way every time.
- The server knows each agent's outgoing obligations and refuses to auto-`/clear`
  an agent that idles without completing them — it nudges once instead.
- Opening/saving workflow files works with `~` paths, creates missing parent
  directories, and offers a native file picker on first launch.

## Non-goals

- Live run visualization on the canvas (the editor remains stopped-state only;
  the roster is frozen during a run).
- Runtime message *blocking* outside the graph. Edges compose prompts and drive
  the idle gate; the router does not reject out-of-graph messages.
- Persisting node positions. Layout is computed on open; drags last the session.

## 1. Data model and core changes

### 1.1 `edges` field

`AgentConfig` gains an optional ordered list (default empty):

```toml
[agents.planner]
dir = "~/projects/x"
prompt = "You break work into tasks..."
edges = [
  { to = "builder", kind = "ask" },     # delegate, wait for the reply
  { to = "notifier", kind = "send" },   # fire-and-forget
]
```

- `to`: an agent id that must exist in the roster.
- `kind`: `"ask"` (reply expected) or `"send"` (no reply).
- Order is meaningful: it is the order the agent is instructed to perform the
  steps, and the order the UI shows.

Validation (`validate_workflow`) rejects: unknown `to` (error names the bad
target and the roster), self-edges, duplicate edges (same `to` + `kind` — the
presence-based check in §2 could not distinguish them), and any `kind` outside
`ask`/`send`. Error messages follow the errors-are-read-by-LLMs convention:
name the input, the valid values, the fix.

### 1.3 Contract amendments

This design touches four frozen contracts; each lands as a reconciliation
amendment in `docs/superpowers/plans/2026-08-01-contracts.md`:

1. `AgentConfig` gains the optional `edges` field (additive; old tomls valid).
2. The system-prompt composition (existing amendment 3) is extended with the
   per-edge imperative steps of §1.2.
3. `EventPayload` gains `agent.nudged` and `agent.stalled` variants (§2).
4. `ClearGate` is extended so the queue worker can ask not just "may I clear?"
   but "what is unmet?" — the nudge decision happens inside the worker (§2).

### 1.2 Prompt composition at freeze

When `edges` is non-empty, `load_workflow` appends an imperative paragraph to
that agent's *frozen* prompt (the file keeps role prompt and wiring separate).
Agent asks are turn-ending by frozen contract — the reply arrives later as a
new prompt — so the composed steps must never say "wait":

> When your work is complete you must perform these steps, in order:
> 1. `tempo ask builder "<your delegation>"` — then end your turn; the reply
>    will arrive as a new prompt. Continue with the remaining steps after it
>    does.
> 2. `tempo send notifier "<your notification>"`.
> Do not skip these steps.

The agent does not choose between ask and send — the edge kind chooses. An
agent with no edges gets its prompt byte-for-byte untouched. Headless
`coretempod` runs behave identically to the app because composition happens in
`core`, at freeze.

The reply direction needs no edge: replies to an ask are always permitted, and
reply instructions already travel with the injected ask.

## 2. Obligation tracking (backend failsafe)

Core knows each agent's outgoing edges from the frozen workflow. The check
lives where the drain/clear ordering is already decided: inside the serialized
queue worker (`drain_then_maybe_clear`, `core/src/pty/queue.rs`), consulting
the `Router` through the extended `ClearGate` (§1.3). It is a third condition
beyond "zero pending asks" and "empty queue". The nudge itself is a queue
injection like `/clear` — server plumbing, not a `MessageRecord` — paired with
an `agent.nudged` bus event so the UI, HTTP observers, and the feed see it.

The turn state machine, per agent:

- **Arming.** A turn opens when the agent receives an injected `ask` or `send`
  (any origin — agent, chat, HTTP; the first agent in a chain is armed by
  whatever kicks it off). Obligations are the agent's full edge list.
- **Continuation.** While a turn is open, two kinds of incoming injection do
  *not* open a new turn:
  - A *reply* to the agent's own ask continues the current turn — remaining
    obligations still stand. A reply never opens a turn of its own (the
    loop-prevention rule: otherwise an agent reviewing the final reply would
    be nudged to re-delegate forever).
  - A further `ask`/`send` while the turn is open merges into it: the met-set
    carries over, and the nudge budget (below) resets.
- **Check** (each debounced working→idle transition, in the same worker pass
  as the drain): compare messages the agent emitted since the turn opened
  against its obligations, matching target and kind. Presence-based, not
  order-based — edge order instructs the agent (§1.2); enforcement only
  requires that each step happened. An obligation counts as met when the
  message is created. Duplicates are impossible (§1.1 rejects duplicate
  edges).
- **In-progress ask** → if an unmet-or-met obligation ask is still awaiting
  its reply, the turn is simply in progress: no nudge, no `stalled`, no clear
  (the existing pending-asks gate already holds). This is what makes
  `[ask builder, send notifier]` work — the agent asks, correctly ends its
  turn, and is *not* nudged about the notifier before builder's reply arrives.
- **All met** → the turn closes; normal drain-then-clear, unchanged.
- **Unmet, no ask in flight** → skip `/clear`, inject one nudge naming the
  exact missing commands: `You have not completed your required steps:
  tempo send notifier "…"`.
- **Still unmet at the next idle** → leave the agent un-cleared, emit
  `agent.stalled`. One nudge per turn (budget resets on merge-arming); no
  infinite loop, no silent clear that destroys the evidence.
- **Drain precedence.** If the worker drained a queued injection at this
  transition, no nudge is considered — the drained message arms or continues
  a turn and the agent is about to work again. Mirrors the existing
  `drained || !auto_clear` short-circuit.
- **Disarm.** Agent restart, run stop, and PTY shutdown discard the agent's
  turn state entirely — a restarted session has no memory of the arming
  message and must not be judged against it.
- **`auto_clear = false` agents** get the same tracking: the check, nudge, and
  `stalled` event all run; only the clear step is skipped, as it already is.
  The check therefore runs before the worker's `!auto_clear` early return.

All of this lives in `core`, so headless runs get the same failsafe.

## 3. Editor UI

### 3.1 Canvas

Built on `@xyflow/svelte` (Svelte Flow, exact-pinned) — the one new frontend
dependency, justified because drag-to-connect edge interaction, pan/zoom, and
node dragging are the fiddliest parts of the UI and exactly what it provides.
Custom Svelte node components keep the app's visual language (panel background,
mono font, status glyphs).

- **Agent node**: agent id as title; `model` and `dir` as compact subtitle
  lines; one source handle (right), one target handle (left).
- **Workflow node**: a single fixed node for `[workflow]`/`[server]` settings
  (name, port, timeouts, debounce). No handles.
- **Edges**: created by dragging source→target. Labeled `ask` or `send`;
  clicking an edge shows controls to flip the kind or delete it. New edges
  default to `ask` (completion is observable, so it is the safer default).
  Edge order within an agent = creation order, reorderable in the inspector.
- **Add agent**: toolbar button creates a node with a generated unique id,
  editable in the inspector.

The canvas replaces the textarea as the default view in the slot
`WorkflowEditor` occupies today (workflow loaded, no run active).

### 3.2 Inspector panel

Right-side panel (the current "Planned roster" sidebar slot). Selecting an
agent node shows: `prompt` (multiline editor), `dir`, `model`,
`permission_mode`, `auto_clear`, and the agent's ordered edge list. Selecting
the workflow node shows workflow/server fields.

### 3.3 Raw toggle

A `graph | toml` toggle in the editor toolbar. The toml view is the existing
editable textarea. Switching text→graph re-parses; on a parse error the switch
is blocked and the validation message shown — no silent data loss. The
debounced `workflow_validate` runs in both views.

### 3.4 Auto-layout

Left-to-right layering computed from the edge topology (roots at the left,
breadth-first ranks). No layout library; workflows are a handful of nodes.
Positions are session-only.

## 4. Saving and toml round-trip

The graph never regenerates the file from scratch; comments and formatting on
untouched keys survive byte-identical.

- New Tauri command `workflow_merge(text, model)` in the app crate: takes the
  current file text plus the structured model (agents, fields, edges,
  workflow/server settings) and returns merged text, implemented with
  `toml_edit`. The frontend then writes via the existing `workflow_save`.
- Merge semantics: changed scalars set in place; new agents append as
  `[agents.<id>]` tables in template order (`dir`, `prompt`, then optionals);
  deleting a node removes its table (comments attached to it go with it);
  `edges` is written in the inline-table-array form of §1.1. Merging an
  unchanged model is a no-op diff.
- Save flow unchanged: explicit Save button with dirty marker, no autosave.
  Save runs merge → validate → write; validation errors block the write.
- The raw-toml view bypasses merge and saves its text verbatim, as today.

### 4.1 Path fixes

- `workflow_open`, `workflow_save`, and `run_start` expand a leading `~`
  against the home directory before touching the filesystem — the same rule
  core applies to agent `dir` values (core's `expand_tilde` is currently
  private in `workflow.rs`; export it rather than duplicating). (Root cause of
  the reported bug:
  `app/src-tauri/src/commands.rs` used typed paths verbatim, so
  `~/CoreTempoWorkflows/tempo.toml` was a relative `./~/…` path → ENOENT.)
- `workflow_save` runs `create_dir_all` on the parent before writing, so a new
  file in a new folder works.

## 5. First-open flow and file picker

`NoWorkflowCard` gains a native picker via `tauri-plugin-dialog` (exact-pinned;
GTK chooser on Linux/WSL under X11):

- **Browse…** — native open dialog filtered to `.toml`; picking an existing
  file opens the graph editor.
- **New workflow…** — native directory picker; the chosen directory gets
  `<dir>/tempo.toml` seeded from `WORKFLOW_TEMPLATE`. If `tempo.toml` already
  exists there it is opened, never overwritten.
- The typed-path input stays as the keyboard fallback, now tilde-safe. Path
  exists → open. Path missing → the editor seeds the template in memory and the
  first Save creates the file, directories included.
- Recents list unchanged (localStorage, capped).

Behavior fix: today the "seed template" fallback triggers on *any* open error,
including unreadable real files (permissions, invalid UTF-8), which could let a
user unknowingly save a template over a real file. The open handler will
distinguish not-found (seed) from other IO errors (show the error, do not
seed).

## 6. Testing

Core (TDD, failing test first):

- `workflow.rs`: `edges` parse/validate — unknown target, self-edge, bad kind,
  order preserved. Freeze: composed prompt contains the imperative steps in
  edge order; an edge-free agent's prompt is byte-for-byte untouched.
- Obligation tracking, driving the same raw-state channel the debouncer tests
  use: armed by ask/send; a reply continues the turn without opening one; an
  in-flight obligation ask means no nudge (the `[ask, send]` chain does not
  misfire); met → clear proceeds; unmet with no ask in flight → one nudge and
  `/clear` suppressed; second idle while short → `agent.stalled`, no second
  nudge; merge-arming resets the nudge budget and keeps the met-set; restart
  and run stop disarm; drained injection at the same transition suppresses the
  nudge; `auto_clear = false` agents still get nudge and `stalled`.

App crate:

- `workflow_merge` round-trips: untouched keys/comments byte-identical; agent
  add/remove; edge rewrite; unchanged model → no-op diff.
- Paths: `~` expansion, parent-dir creation, not-found vs other-IO distinction.

Frontend (vitest, colocated):

- graph↔model mapping both directions, auto-layout ranks, edge-kind default,
  blocked text→graph toggle on parse error. Canvas interaction itself is
  Svelte Flow's tested behavior; we test our mapping, not the library.

Real-agent check (required — this touches injection): a two-agent `ask`-edge
workflow under `coretempod run`; confirm the composed prompt produces the
delegation, then starve the obligation (an agent told to do nothing) and watch
the nudge arrive instead of `/clear`. Trivial prompts to keep token cost down.

## 7. Error handling summary

- Validation blocks save; issues name the path, the value, and the fix.
- IO errors name the operation, the path, and the cause.
- Stalled agents surface as an event and UI badge, never a silent clear.
- Parse errors block the text→graph toggle with the message shown.
