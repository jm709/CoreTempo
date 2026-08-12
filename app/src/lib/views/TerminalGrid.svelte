<script lang="ts">
  import { agentsState } from "../state/agents.svelte";
  import { runState } from "../state/run.svelte";
  import { uiState } from "../state/ui.svelte";
  import { gridClass } from "./gridLayout";
  import TerminalPane from "./TerminalPane.svelte";

  const layout = $derived(
    uiState.maximizedAgent !== null ? "g1" : gridClass(agentsState.order.length),
  );
</script>

<div
  class="grid {layout}"
  class:stopping={runState.phase === "stopping"}
>
  {#each agentsState.order as id (id)}
    <TerminalPane agentId={id} />
  {/each}
</div>

<style>
  .grid {
    display: grid; gap: 1px; height: 100%;
    background: var(--panel-edge);
  }
  .grid.g1 { grid-template-columns: 1fr; }
  .grid.g2 { grid-template-columns: 1fr 1fr; }
  .grid.g4 { grid-template-columns: 1fr 1fr; grid-template-rows: 1fr 1fr; }
  .grid.g6 { grid-template-columns: repeat(3, 1fr); grid-template-rows: 1fr 1fr; }
  .grid.stopping { opacity: 0.5; }   /* stopping dims panes until confirmed (spec §9.2) */
</style>
