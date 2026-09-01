<script lang="ts">
  import { elapsed } from "./lib/format";
  import { restartAgent } from "./lib/ipc";
  import { isMac, modLabel, resolveKey } from "./lib/keys";
  import { boot, startRun, stopRun } from "./lib/session";
  import { agentsState, runningCount } from "./lib/state/agents.svelte";
  import { runState } from "./lib/state/run.svelte";
  import { blockedCount as sessionsBadge } from "./lib/state/sessions.svelte";
  import {
    closeWorkflow,
    releaseCapture,
    runGate,
    toggleRunCenter,
    uiState,
  } from "./lib/state/ui.svelte";
  import { jumpToAgentTerminal } from "./lib/term/jump";
  import { workflowTerm } from "./lib/term/instances";
  import { confirmDiscard } from "./lib/dialogs";
  import Dock from "./lib/views/Dock.svelte";
  import NoWorkflowCard from "./lib/views/NoWorkflowCard.svelte";
  import Roster from "./lib/views/Roster.svelte";
  import TerminalGrid from "./lib/views/TerminalGrid.svelte";
  import WorkflowEditor from "./lib/views/WorkflowEditor.svelte";

  const mac = isMac();
  const mod = modLabel(mac);

  let now = $state(Date.now());
  let runError = $state<string | null>(null);

  const showGrid = $derived(runState.phase === "running" || runState.phase === "stopping");
  const gate = $derived(runGate(runState.phase, uiState.editorPath, uiState.editorDirty));
  // Graph needs an open workflow file; a run adopted at boot always sets
  // editorPath from the snapshot, so this only pins terminals in edge cases.
  const runView = $derived(uiState.editorPath === null ? "terminals" : uiState.runCenter);
  // `timeout` is its own outcome on the bus even though the trigger's GET status folds it
  // into `failed`; both read as "did not finish cleanly" here.
  const completionFailed = $derived(
    runState.completed?.result === "failed" || runState.completed?.result === "timeout",
  );

  $effect(() => {
    void boot();
  });
  $effect(() => {
    const t = setInterval(() => {
      now = Date.now();
    }, 1000);
    return () => clearInterval(t);
  });
  // Hiding the grid drops the xterm textarea's DOM focus, so a capture that
  // survived the toggle would draw a border over a terminal keys no longer reach.
  $effect(() => {
    if (showGrid && runView !== "terminals") releaseCapture();
  });

  function toggleMaximize(): void {
    if (uiState.focusedAgent === null) return;
    uiState.maximizedAgent = uiState.maximizedAgent === uiState.focusedAgent
      ? null
      : uiState.focusedAgent;
  }

  function onKeydown(ev: KeyboardEvent): void {
    const action = resolveKey(ev, mac);
    if (action === null) return;
    if (uiState.mode === "sessions" && action.kind !== "release") return;
    ev.preventDefault();
    switch (action.kind) {
      case "focus-terminal": {
        const id = agentsState.order[action.index];
        if (id !== undefined && showGrid) jumpToAgentTerminal(id);
        break;
      }
      case "release":
        releaseCapture();
        workflowTerm.blurAll();
        break;
      case "dock-feed":
        uiState.dockTab = "feed";
        break;
      case "dock-chat":
        uiState.dockTab = "chat";
        break;
      case "edit-workflow":
        // While running, mod+E flips graph/terminals; when stopped the center
        // is structurally the editor already (spec §9.2), so nothing to do.
        if (showGrid) toggleRunCenter();
        break;
      case "restart-focused":
        if (uiState.focusedAgent !== null) void restartAgent(uiState.focusedAgent);
        break;
      case "toggle-maximize":
        toggleMaximize();
        break;
    }
  }

  async function onRunClick(): Promise<void> {
    runError = null;
    try {
      if (showGrid) await stopRun();
      else if (uiState.editorPath !== null) await startRun(uiState.editorPath);
    } catch (e: unknown) {
      runError = e instanceof Error ? e.message : (e as { message: string }).message;
    }
  }

  // A run adopts its workflow file at start, so the way back only exists while
  // stopped; the phase guard keeps it out of the starting/stopping window.
  const canClose = $derived(uiState.editorPath !== null && runState.phase === "stopped");

  async function onCloseClick(): Promise<void> {
    const path = uiState.editorPath;
    if (path === null) return;
    if (uiState.editorDirty && !(await confirmDiscard(path))) return;
    closeWorkflow();
  }

  function clock(ms: number): string {
    const d = new Date(ms);
    return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
  }
</script>

<svelte:window onkeydown={onKeydown} />

<div class="shell" class:sessions={uiState.mode === "sessions"}>
  <header class="topbar panel mono">
    <span class="brand">◉ CoreTempo</span>
    <span class="modeswitch">
      <button
        class:active={uiState.mode === "workflows"}
        onclick={() => {
          uiState.mode = "workflows";
        }}
      >
        workflows
      </button>
      <button
        class:active={uiState.mode === "sessions"}
        onclick={() => {
          uiState.mode = "sessions";
        }}
      >
        sessions{#if sessionsBadge() > 0}<span class="badge">{sessionsBadge()}</span>{/if}
      </button>
    </span>
    {#if uiState.mode === "workflows"}
      {#if canClose}
        <button
          class="closewf"
          title="Close workflow"
          aria-label="Close workflow"
          onclick={() => {
            void onCloseClick();
          }}
        >
          ←
        </button>
      {/if}
      <span class="wf" title={uiState.editorPath ?? ""}>
        {uiState.editorPath ?? runState.info?.workflow_path ?? "no workflow"}
      </span>
      {#if runError !== null}<span class="err">{runError}</span>{/if}
      {#if gate.hint !== null}<span class="hint">{gate.hint}</span>{/if}
      <button
        class="runbtn"
        class:running={showGrid}
        disabled={gate.disabled}
        onclick={() => {
          void onRunClick();
        }}
      >
        {showGrid ? "■ Stop" : "▶ Run"}
      </button>
      {#if showGrid}
        <span class="viewtoggle">
          <button
            class:active={runView === "graph"}
            disabled={uiState.editorPath === null}
            onclick={() => {
              uiState.runCenter = "graph";
            }}
          >
            graph
          </button>
          <button
            class:active={runView === "terminals"}
            onclick={() => {
              uiState.runCenter = "terminals";
            }}
          >
            terminals
          </button>
        </span>
      {/if}
      <span class="meta">agents {runningCount()}/{agentsState.order.length} · {clock(now)}</span>
    {/if}
  </header>

  <aside class="rail panel">
    {#if uiState.mode === "workflows"}
      <Roster />
    {:else}
      <div class="label heading">Sessions</div>
    {/if}
  </aside>

  <main class="center">
    <!-- The editor mounts once for the life of the open file: branching it on
         showGrid would destroy it (and any unsaved edits) when a run stops. -->
    {#if uiState.editorPath !== null}
      <div
        class="view"
        class:offscreen={uiState.mode !== "workflows" || (showGrid && runView !== "graph")}
      >
        <WorkflowEditor />
      </div>
    {:else if !showGrid && uiState.mode === "workflows"}
      <NoWorkflowCard />
    {/if}
    {#if showGrid}
      <div
        class="view"
        class:offscreen={uiState.mode !== "workflows" || runView !== "terminals"}
      >
        <TerminalGrid />
      </div>
    {/if}
  </main>

  <aside class="dock panel">
    {#if uiState.mode === "workflows"}<Dock />{/if}
  </aside>

  <footer class="statusbar panel mono">
    {#if uiState.mode === "workflows"}
      <span>
        {mod}1–9 focus terminal · {mod}` release ·
        {mod}E {showGrid ? "graph/terminals" : "edit workflow"}
      </span>
      {#if runState.completed !== null}
        <span class="done" class:bad={completionFailed}>
          workflow {runState.completed.result}{runState.completed.code !== null
            ? ` (code ${runState.completed.code})`
            : ""}
        </span>
      {/if}
      {#if runState.info !== null}
        <span class="elapsed">⏺ run {elapsed(runState.info.started_at, now)}</span>
      {/if}
    {/if}
  </footer>
</div>

<style>
  .shell {
    display: grid; height: 100%; gap: 1px; background: var(--panel-edge);
    grid-template-columns: 180px 1fr 320px;
    grid-template-rows: 36px 1fr 24px;
    grid-template-areas: "top top top" "rail center dock" "foot foot foot";
  }
  .shell.sessions { grid-template-columns: 220px 1fr 0; }
  .topbar {
    grid-area: top; display: flex; align-items: center; gap: 12px; padding: 0 10px;
    font-size: var(--fs-data); border: none;
  }
  .brand { color: var(--accent); }
  .closewf { color: var(--text-dim); }
  .closewf:hover { color: var(--text); }
  .wf { color: var(--text-dim); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .err { color: var(--err); }
  .hint { margin-left: auto; color: var(--text-dim); }
  .runbtn { margin-left: auto; color: var(--accent); }
  .hint + .runbtn { margin-left: 0; }
  .runbtn.running { color: var(--err); }
  .meta { color: var(--text-dim); }
  .viewtoggle { display: flex; gap: 2px; }
  .viewtoggle button { color: var(--text-dim); }
  .viewtoggle button.active { color: var(--accent); }
  .modeswitch { display: flex; gap: 2px; }
  .modeswitch button { color: var(--text-dim); }
  .modeswitch button.active { color: var(--accent); }
  .badge {
    color: var(--accent); margin-left: 4px;
    border: 1px solid var(--panel-edge); border-radius: 2px; padding: 0 4px;
  }
  .view { height: 100%; min-width: 0; min-height: 0; }
  .view.offscreen { display: none; }
  .rail { grid-area: rail; border: none; min-height: 0; }
  .heading { padding: 0 10px 6px; }
  .center { grid-area: center; min-width: 0; min-height: 0; background: var(--bg); }
  .dock { grid-area: dock; border: none; min-width: 0; min-height: 0; }
  .statusbar {
    grid-area: foot; display: flex; justify-content: space-between; align-items: center;
    padding: 0 10px; font-size: var(--fs-label); color: var(--text-dim); border: none;
  }
  .elapsed { color: var(--accent); }
  .done { color: var(--ok); }
  .done.bad { color: var(--err); }
</style>
