<script lang="ts">
  import { elapsed } from "./lib/format";
  import { restartAgent } from "./lib/ipc";
  import { isMac, modLabel, resolveKey } from "./lib/keys";
  import { boot, startRun, stopRun } from "./lib/session";
  import { agentsState, runningCount } from "./lib/state/agents.svelte";
  import { runState } from "./lib/state/run.svelte";
  import { releaseCapture, toggleRunCenter, uiState } from "./lib/state/ui.svelte";
  import { jumpToAgentTerminal } from "./lib/term/jump";
  import { blurAllTerminals } from "./lib/term/manager";
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
    ev.preventDefault();
    switch (action.kind) {
      case "focus-terminal": {
        const id = agentsState.order[action.index];
        if (id !== undefined && showGrid) jumpToAgentTerminal(id);
        break;
      }
      case "release":
        releaseCapture();
        blurAllTerminals();
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

  function clock(ms: number): string {
    const d = new Date(ms);
    return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
  }
</script>

<svelte:window onkeydown={onKeydown} />

<div class="shell">
  <header class="topbar panel mono">
    <span class="brand">◉ CoreTempo</span>
    <span class="wf" title={uiState.editorPath ?? ""}>
      {uiState.editorPath ?? runState.info?.workflow_path ?? "no workflow"}
    </span>
    {#if runError !== null}<span class="err">{runError}</span>{/if}
    <button
      class="runbtn"
      class:running={showGrid}
      disabled={runState.phase === "starting" ||
        runState.phase === "stopping" ||
        (!showGrid && uiState.editorPath === null)}
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
  </header>

  <aside class="rail panel"><Roster /></aside>

  <main class="center">
    <!-- The editor mounts once for the life of the open file: branching it on
         showGrid would destroy it (and any unsaved edits) when a run stops. -->
    {#if uiState.editorPath !== null}
      <div class="view" class:offscreen={showGrid && runView !== "graph"}>
        <WorkflowEditor />
      </div>
    {:else if !showGrid}
      <NoWorkflowCard />
    {/if}
    {#if showGrid}
      <div class="view" class:offscreen={runView !== "terminals"}>
        <TerminalGrid />
      </div>
    {/if}
  </main>

  <aside class="dock panel"><Dock /></aside>

  <footer class="statusbar panel mono">
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
  </footer>
</div>

<style>
  .shell {
    display: grid; height: 100%; gap: 1px; background: var(--panel-edge);
    grid-template-columns: 180px 1fr 320px;
    grid-template-rows: 36px 1fr 24px;
    grid-template-areas: "top top top" "rail center dock" "foot foot foot";
  }
  .topbar {
    grid-area: top; display: flex; align-items: center; gap: 12px; padding: 0 10px;
    font-size: var(--fs-data); border: none;
  }
  .brand { color: var(--accent); }
  .wf { color: var(--text-dim); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .err { color: var(--err); }
  .runbtn { margin-left: auto; color: var(--accent); }
  .runbtn.running { color: var(--err); }
  .meta { color: var(--text-dim); }
  .viewtoggle { display: flex; gap: 2px; }
  .viewtoggle button { color: var(--text-dim); }
  .viewtoggle button.active { color: var(--accent); }
  .view { height: 100%; min-width: 0; min-height: 0; }
  .view.offscreen { display: none; }
  .rail { grid-area: rail; border: none; min-height: 0; }
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
