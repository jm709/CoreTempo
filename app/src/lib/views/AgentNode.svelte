<script lang="ts">
  import { Handle, Position, type NodeProps } from "@xyflow/svelte";
  import { stateLabel } from "../format";
  import { agentsState } from "../state/agents.svelte";
  import { jumpToAgentTerminal } from "../term/jump";
  import type { AgentModel } from "../types";
  import { nodeTint } from "./nodeState";
  import StatusGlyph from "./StatusGlyph.svelte";

  let { id, data, selected }: NodeProps & { data: { agent: AgentModel } } = $props();

  const agent = $derived(data.agent);
  const incomplete = $derived(agent.dir === "" || agent.prompt === "");
  const live = $derived(agentsState.byId[id]);
  const tint = $derived(nodeTint(live?.state));

  function onDblclick(): void {
    if (live === undefined) return; // no run active
    jumpToAgentTerminal(id);
  }
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<!-- dblclick jumps to this agent's terminal during a run (spec: run-time workflow screen) -->
<div
  class="node mono"
  class:selected
  class:incomplete
  class:busy={tint === "busy"}
  class:info={tint === "info"}
  class:err={tint === "err"}
  ondblclick={onDblclick}
>
  <Handle type="target" position={Position.Left} />
  <div class="title">
    <span>{id}</span>
    {#if live !== undefined}
      <span class="state">
        <StatusGlyph state={live.state} />
        {stateLabel(live.state)}
        {#if agentsState.stalled[id]}
          <span class="stalled" title="idled with unmet workflow steps after a nudge">⚠</span>
        {/if}
      </span>
    {/if}
  </div>
  <div class="sub">{agent.model ?? "default model"}</div>
  <div class="sub dir">{agent.dir === "" ? "no dir set" : agent.dir}</div>
  <Handle type="source" position={Position.Right} />
</div>

<style>
  .node {
    background: var(--panel); border: 1px solid var(--panel-edge);
    padding: 8px 12px; min-width: 150px; font-size: var(--fs-data);
  }
  .node.busy { border-color: var(--busy); }
  .node.info { border-color: var(--info); }
  .node.err { border-color: var(--err); }
  .node.selected { border-color: var(--accent); }
  .node.incomplete { border-style: dashed; border-color: var(--err); }
  .title { display: flex; justify-content: space-between; gap: 12px; color: var(--accent); }
  .state { color: var(--text-dim); font-size: var(--fs-label); }
  .stalled { color: var(--warn); }
  .sub { color: var(--text-dim); font-size: var(--fs-label); }
  .dir { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 200px; }
</style>
