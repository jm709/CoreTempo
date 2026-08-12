<script lang="ts">
  import type { NodeProps } from "@xyflow/svelte";
  import type { WorkflowModel } from "../types";

  let { data, selected }: NodeProps & { data: { model: WorkflowModel } } = $props();

  const workflow = $derived(data.model.workflow);
</script>

<div class="node mono" class:selected>
  <div class="title">[workflow]</div>
  <div class="sub">{workflow.name}</div>
  <div class="sub">port {workflow.port}</div>
</div>

<style>
  .node {
    background: var(--panel); border: 1px dashed var(--panel-edge);
    padding: 8px 12px; min-width: 150px; font-size: var(--fs-data);
  }
  .node.selected { border-color: var(--accent); border-style: solid; }
  .title { color: var(--text); }
  .sub { color: var(--text-dim); font-size: var(--fs-label); }
</style>
