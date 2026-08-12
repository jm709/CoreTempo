# Run-time workflow screen

2026-08-04. Approved design for keeping the workflow graph on screen during a
run, with live agent state on the nodes and the terminal grid one toggle away.

## Problem

Starting a run replaces the center pane with the terminal grid
(`App.svelte`: `showGrid` derived from `runState.phase`). The workflow graph —
the mental model of who delegates to whom — disappears exactly when it becomes
interesting, and there is no way to see agent idle/working state without
reading individual terminal headers.

## Behavior

- Starting a run keeps the center on the workflow screen. A run has two center
  views: **Graph** (default, every run) and **Terminals**.
- Toggle between them with the toolbar segmented control (visible only while a
  run is active), the `mod+E` chord, or by double-clicking an agent node —
  which also focuses that agent's terminal.
- Stopping behaves as today: center returns to the editor, and the
  `stopping` dim still applies to the terminal grid.
- The workflow editor stays fully editable during a run. Edits go to the file
  and take effect next run; the frozen-roster model is unchanged.

## Center wiring

- `uiState.runCenter: "graph" | "terminals"` in `lib/state/ui.svelte.ts`,
  reset to `"graph"` in `startRun`. Not persisted across runs.
- While `phase` is `running` or `stopping`, `App.svelte` mounts **both**
  `WorkflowEditor` and `TerminalGrid` and hides the inactive one with CSS.
  Hidden terminals keep absorbing PTY writes (xterm buffers off-DOM); the
  existing `offsetParent` guard skips `display:none` panes and the
  ResizeObserver refits on re-show. No terminal re-attach machinery.
- When stopped, center selection is unchanged from today
  (editor if `editorPath`, else the empty card).
- The `edit-workflow` key action toggles `runCenter` during a run; its
  stopped-state meaning is unchanged. Statusbar hints gain the toggle.

## Live agent nodes

- `AgentNode` looks up `agentsState.byId[id]`. During a run it renders:
  - the existing `StatusGlyph` + `stateLabel` in the node header, and
  - a border tint by state class: `working` = `--busy`, `starting` /
    `restarting` = `--info`, `exited` = `--err`, `idle` = default chrome;
  - the roster's `⚠` stalled badge, driven by `agentsState.stalled`.
- When no run is active `byId` is empty: nodes render exactly as today,
  including the dashed-red `incomplete` state.
- Edges are untouched: ask/send labels, click-to-cycle, no activity animation.

## Testing

- Pure helpers carry the logic and get vitest coverage: the
  state-to-class mapping for node tint, and `resolveKey` routing for the
  toggle. TDD per repo convention (failing test first).
- Real-agent check per `CLAUDE.md`: `./dev`, start a run, toggle both ways,
  double-click a node, confirm sizing and focus — fake agents cannot exercise
  PTY/TUI behavior.

## Out of scope

- Edge activity flashes when messages travel.
- A live-runtime inspector (state/pending-asks detail on node click).
- Persisting the Graph/Terminals choice across runs.

## Sequencing

Branch `feature/run-workflow-screen` based on `fix/terminal-pane-attach`
(PR #11): the feature is only verifiable with visible terminals. Merge after
#11 lands.
