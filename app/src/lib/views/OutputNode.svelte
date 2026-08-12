<script lang="ts">
  import { Handle, Position, type NodeProps } from "@xyflow/svelte";
  import { triggersState } from "../state/triggers.svelte";
  import { uiState } from "../state/ui.svelte";
  import type { OutputModel } from "../types";
  import { outputPreview } from "./triggerHelpers";

  let { data, selected }: NodeProps & { data: { output: OutputModel } } = $props();

  const output = $derived(data.output);
  // Stub state from '+ output': dashed red until the inspector fills the path; the
  // empty-schema_file validation issue blocks the save meanwhile.
  const incomplete = $derived(output.schema === undefined && (output.schema_file ?? "") === "");
  const source = $derived(
    output.schema !== undefined
      ? "inline schema"
      : (output.schema_file ?? "") === ""
        ? "no schema file set"
        : (output.schema_file ?? ""),
  );
  // Same lifecycle the Run tab shows (history selection wins, else latest), and the same
  // persistence: the store outlives a stopped run until the next run.started/bus.reset —
  // a deliberate divergence from AgentNode, whose overlay dies at stop.
  const live = $derived(
    triggersState.list.find((t) => t.id === triggersState.selectedId)
      ?? triggersState.list.at(-1)
      ?? null,
  );
  const tint = $derived(
    live === null ? null
    : live.phase === "working" ? "busy"
    : live.phase === "failed" ? "err"
    : "ok",
  );
  const preview = $derived(
    live !== null && live.phase === "completed" && live.output !== null
      ? outputPreview(live.output)
      : [],
  );
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<!-- dblclick jumps to the Run tab, the node's detail view (spec 2026-08-11) -->
<div
  class="node mono"
  class:selected
  class:incomplete
  class:busy={tint === "busy"}
  class:ok={tint === "ok"}
  class:err={tint === "err"}
  ondblclick={() => {
    uiState.dockTab = "run";
  }}
>
  <Handle type="target" position={Position.Left} />
  <div class="title">⇥ output</div>
  <div class="sub">{source}</div>
  <div class="sub">max repairs {output.max_repairs}</div>
  {#if live !== null}
    {#if live.phase === "working"}
      {@const n = live.rejections.length}
      <div class="sub state">
        in progress…{n > 0 ? ` · ${n} repair${n === 1 ? "" : "s"}` : ""}
      </div>
    {:else if preview.length > 0}
      <div class="preview">
        {#each preview as line, i (i)}<div class="sub">{line}</div>{/each}
      </div>
    {:else if live.phase === "completed"}
      <div class="sub state">
        {live.result ?? "completed"}{live.code !== null ? ` (code ${live.code})` : ""}
      </div>
    {:else}
      <div class="sub state fail">{live.reasonCode ?? "failed"}</div>
    {/if}
  {/if}
</div>

<style>
  .node {
    background: var(--panel); border: 1px solid var(--panel-edge);
    padding: 8px 12px; min-width: 150px; font-size: var(--fs-data);
  }
  .node.busy { border-color: var(--busy); }
  .node.ok { border-color: var(--ok); }
  .node.err { border-color: var(--err); }
  .node.selected { border-color: var(--accent); }
  .node.incomplete { border-style: dashed; border-color: var(--err); }
  .title { color: var(--ok); }
  .sub {
    color: var(--text-dim); font-size: var(--fs-label);
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 200px;
  }
  .preview { margin-top: 4px; border-top: 1px solid var(--panel-edge); padding-top: 4px; }
  .state { margin-top: 4px; }
  .fail { color: var(--err); }
</style>
