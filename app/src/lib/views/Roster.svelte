<script lang="ts">
  import { restartAgent, toCmdError } from "../ipc";
  import { agentsState } from "../state/agents.svelte";
  import { pendingAsksFor } from "../state/messages.svelte";
  import { uiState } from "../state/ui.svelte";
  import { jumpToAgentTerminal } from "../term/jump";
  import StatusGlyph from "./StatusGlyph.svelte";

  function restart(id: string): void {
    restartAgent(id).catch((e: unknown) => {
      console.error("restart failed:", toCmdError(e).message);
    });
  }
</script>

<nav class="roster">
  <div class="label heading">Agents</div>
  {#each agentsState.order as id (id)}
    {@const a = agentsState.byId[id]}
    {#if a !== undefined}
      <div class="row" class:hl={uiState.hoverFrom === id || uiState.hoverTo === id}>
        <button class="mono name" onclick={() => jumpToAgentTerminal(id)} title={a.dir}>
          <StatusGlyph state={a.state} />
          <span>{id}</span>
          {#if agentsState.stalled[id]}
            <span class="stalled" title="idled with unmet workflow steps after a nudge">⚠</span>
          {/if}
          {#if pendingAsksFor(id) > 0}
            <span class="pending mono">{pendingAsksFor(id)}</span>
          {/if}
        </button>
        {#if a.state === "exited"}
          <button class="mono restart" onclick={() => restart(id)}>restart</button>
        {/if}
      </div>
    {/if}
  {/each}
</nav>

<style>
  .roster { padding: 8px 0; overflow-y: auto; height: 100%; }
  .heading { padding: 0 10px 6px; }
  .row { display: flex; flex-direction: column; }
  .row.hl { background: color-mix(in srgb, var(--accent) 12%, transparent); }
  .name {
    display: flex; align-items: center; gap: 8px;
    padding: 4px 10px; text-align: left; width: 100%;
  }
  .stalled { color: var(--warn); }
  .pending {
    margin-left: auto; color: var(--accent);
    border: 1px solid var(--panel-edge); border-radius: 2px; padding: 0 4px;
  }
  .restart { color: var(--accent); text-align: left; padding: 0 10px 4px 30px; }
</style>
