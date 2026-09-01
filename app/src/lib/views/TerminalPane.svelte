<script lang="ts">
  import { restartAgent, toCmdError } from "../ipc";
  import { agentsState } from "../state/agents.svelte";
  import { focusTerminal, uiState } from "../state/ui.svelte";
  import { workflowTerm } from "../term/instances";
  import { exitLabel, stateLabel } from "../format";
  import StatusGlyph from "./StatusGlyph.svelte";

  let { agentId }: { agentId: string } = $props();

  const agent = $derived(agentsState.byId[agentId]);
  const hidden = $derived(uiState.maximizedAgent !== null && uiState.maximizedAgent !== agentId);

  function host(el: HTMLElement): void {
    workflowTerm.attach(agentId, el);
  }

  function capture(): void {
    focusTerminal(agentId);
    workflowTerm.focus(agentId);
  }

  function toggleMaximize(): void {
    uiState.maximizedAgent = uiState.maximizedAgent === agentId ? null : agentId;
  }

  function restart(): void {
    restartAgent(agentId).catch((e: unknown) => {
      console.error("restart failed:", toCmdError(e).message);
    });
  }
</script>

<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<!-- mousedown only mirrors the click-to-focus the xterm textarea inside already handles -->
<section
  class="pane"
  class:hidden
  class:maximized={uiState.maximizedAgent === agentId}
  class:captured={uiState.captured && uiState.focusedAgent === agentId}
  class:flash={uiState.flashAgent === agentId}
  class:dead={agent?.state === "exited"}
  onmousedown={capture}
  role="group"
  aria-label="terminal {agentId}"
>
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <!-- dblclick is a pointer shortcut for the mod+Enter toggle-maximize chord (keys.ts) -->
  <header class="mono" ondblclick={toggleMaximize}>
    <span class="title">▸ {agentId}</span>
    {#if agent !== undefined}
      <span class="state">
        <StatusGlyph state={agent.state} />
        {stateLabel(agent.state)}
      </span>
    {/if}
  </header>
  <div class="term-host" use:host></div>
  {#if agent?.state === "exited"}
    <div class="overlay mono">
      <span>[{exitLabel(agent.exit)}]</span>
      <button class="restart" onclick={restart}>restart</button>
    </div>
  {/if}
</section>

<style>
  .pane {
    display: flex; flex-direction: column; position: relative; min-width: 0; min-height: 0;
    background: var(--terminal-bg);
    outline: 1px solid transparent; outline-offset: -1px;
  }
  .pane.hidden { display: none; }
  .pane.captured { outline-color: var(--accent); }
  .pane.flash { outline: 2px solid var(--accent); }        /* 200 ms via flashTerminal timer */
  .pane.maximized { animation: ct-fade-in var(--t-maximize) ease-out; }
  header {
    display: flex; justify-content: space-between; align-items: center;
    padding: 2px 8px; background: var(--panel); border-bottom: 1px solid var(--panel-edge);
    font-size: var(--fs-data); color: var(--text-dim); cursor: default;
  }
  .term-host { flex: 1; min-height: 0; padding: 4px; }
  .pane.dead .term-host { opacity: 0.4; }                  /* last screen dimmed 60% */
  .overlay {
    position: absolute; inset: 0; display: flex; gap: 12px;
    align-items: center; justify-content: center; color: var(--err);
  }
  .overlay .restart {
    color: var(--accent); border: 1px solid var(--panel-edge); padding: 2px 10px;
    background: var(--panel);
  }
</style>
