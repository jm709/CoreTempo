<script lang="ts">
  import { Handle, Position, type NodeProps } from "@xyflow/svelte";
  import type { TriggerModel } from "../types";

  let { data, selected }: NodeProps & { data: { trigger: TriggerModel } } = $props();

  const trigger = $derived(data.trigger);
  // No target handle: the trigger starts the workflow, so nothing edges into it.
  const onStart = $derived(trigger.type === "on_start");
  // An on_start trigger with no message will not validate on save; show that here the way
  // an agent missing its dir does, rather than letting the save fail unexplained.
  const incomplete = $derived(onStart && (trigger.message ?? "") === "");
  const sub = $derived(
    onStart
      ? ((trigger.message ?? "").split("\n")[0] ?? "") || "no message set"
      : "POST /v1/trigger",
  );
</script>

<div class="node mono" class:selected class:incomplete>
  <div class="title">⚡ {onStart ? "on-start" : "webhook"}</div>
  <div class="sub">{sub}</div>
  <Handle type="source" position={Position.Right} />
</div>

<style>
  .node {
    background: var(--panel); border: 1px solid var(--panel-edge);
    padding: 8px 12px; min-width: 150px; font-size: var(--fs-data);
  }
  .node.selected { border-color: var(--accent); }
  .node.incomplete { border-style: dashed; border-color: var(--err); }
  .title { color: var(--ok); }
  .sub {
    color: var(--text-dim); font-size: var(--fs-label);
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 200px;
  }
</style>
