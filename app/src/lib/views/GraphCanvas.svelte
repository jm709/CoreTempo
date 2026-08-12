<script lang="ts">
  import { untrack } from "svelte";
  import {
    Background, Controls, SvelteFlow, type Connection, type NodeTypes,
  } from "@xyflow/svelte";
  import "@xyflow/svelte/dist/style.css";
  import type { TriggerType, WorkflowModel } from "../types";
  import AgentNode from "./AgentNode.svelte";
  import FitOnAdd from "./FitOnAdd.svelte";
  import OutputNode from "./OutputNode.svelte";
  import TriggerNode from "./TriggerNode.svelte";
  import WorkflowNode from "./WorkflowNode.svelte";
  import {
    connectAgents, duplicateEdgeError, freeSlot, isProjectedEdge, nextEdgeKind,
  } from "./graphEditing";
  import {
    addAgent, addOutput, addTrigger, setEdgeKind, toFlow, OUTPUT_NODE_ID, TRIGGER_NODE_ID,
    type FlowEdge, type FlowNode,
  } from "./graphModel";

  let {
    model, selected = $bindable(null), onchanged,
  }: { model: WorkflowModel; selected: string | null; onchanged: () => void } = $props();

  let graphError = $state<string | null>(null);
  let nodes = $state.raw<FlowNode[]>([]);
  let edges = $state.raw<FlowEdge[]>([]);
  const nodeTypes: NodeTypes = {
    agent: AgentNode, workflow: WorkflowNode, trigger: TriggerNode, output: OutputNode,
  };
  // Svelte Flow deletes on Backspace by default, but it only deletes from its own arrays:
  // the agent would survive in the model, its inbound edges would never be stripped, and
  // nothing would be marked dirty. Deletion belongs to the inspector, which goes through
  // removeAgent/removeEdge and calls onchanged.
  const DELETE_KEY = null;
  const lastPositions = new Map<string, { x: number; y: number }>();

  // Rebuilt whenever the roster or the edges change (toFlow reads nothing else, so inspector
  // field edits leave the canvas alone). Where the node was already on screen its position
  // wins over the fresh auto-layout: adding one edge must not teleport the whole graph.
  $effect(() => {
    const next = toFlow(model);
    for (const node of untrack(() => nodes)) lastPositions.set(node.id, node.position);
    const placed: { x: number; y: number }[] = [];
    nodes = next.nodes.map((node) => {
      const position = lastPositions.get(node.id) ?? freeSlot(node.position, placed);
      placed.push(position);
      return { ...node, position };
    });
    edges = next.edges;
  });

  /** Runs before Svelte Flow inserts an edge of its own, and always vetoes that insert by
   * returning false: `edges` is a projection of the model, so a refused connection must
   * leave nothing behind, and an accepted one is rendered by the reconciler above with the
   * model's id. Validating in `onconnect` instead would be too late — the library adds its
   * edge first, and a rejected drag would leave that phantom on the canvas. */
  function onbeforeconnect(conn: Connection): false {
    graphError = connectAgents(model, conn.source, conn.target);
    if (graphError === null) onchanged();
    return false;
  }

  /** Edge click cycles ask -> send -> loop; the inspector owns reorder and delete. The
   * trigger edge takes the same path but skips loop (validation rejects loop triggers),
   * and `duplicateEdgeError` finds no agent under `§trigger`, so it reports no clash. */
  function cycleKind(edge: FlowEdge): void {
    // Nothing to cycle on a projected edge. isProjectedEdge also narrows the type for
    // nextEdgeKind below.
    if (isProjectedEdge(edge.label)) return;
    const kind = nextEdgeKind(edge.label, edge.source !== TRIGGER_NODE_ID);
    const clash = duplicateEdgeError(model, edge.source, edge.target, kind);
    if (clash !== null) {
      graphError = clash;
      return;
    }
    setEdgeKind(model, edge.source, edge.target, kind);
    graphError = null;
    onchanged();
  }

  function addTriggerNode(type: TriggerType): void {
    graphError = addTrigger(model, type);
    if (graphError !== null) return;
    selected = TRIGGER_NODE_ID;
    onchanged();
  }

  function addOutputNode(): void {
    graphError = addOutput(model);
    if (graphError !== null) return;
    selected = OUTPUT_NODE_ID;
    onchanged();
  }
</script>

<div class="canvas">
  <div class="toolbar mono">
    <button
      onclick={() => {
        selected = addAgent(model);
        graphError = null;
        onchanged();
      }}>+ agent</button
    >
    <button
      disabled={model.trigger !== null}
      title="start the workflow when the run begins"
      onclick={() => {
        addTriggerNode("on_start");
      }}>+ on-start</button
    >
    <button
      disabled={model.trigger !== null}
      title="start the workflow from POST /v1/trigger"
      onclick={() => {
        addTriggerNode("webhook");
      }}>+ webhook</button
    >
    <button
      disabled={model.trigger === null ||
        model.trigger.type !== "webhook" ||
        model.trigger.edge.kind !== "ask" ||
        model.trigger.output !== undefined}
      title="declare the [trigger.output] reply contract (webhook trigger with an ask kickoff)"
      onclick={addOutputNode}>+ output</button
    >
    {#if graphError !== null}<span class="err">{graphError}</span>{/if}
  </div>
  <div class="flow">
    <SvelteFlow
      bind:nodes
      bind:edges
      {nodeTypes}
      {onbeforeconnect}
      colorMode="dark"
      fitView
      deleteKey={DELETE_KEY}
      zoomOnDoubleClick={false}
      onnodeclick={({ node }) => {
        selected = node.id;
      }}
      onedgeclick={({ edge }) => {
        cycleKind(edge);
      }}
      onpaneclick={() => {
        selected = null;
      }}
    >
      <Background />
      <Controls showLock={false} />
      <FitOnAdd count={nodes.length} />
    </SvelteFlow>
  </div>
</div>

<style>
  .canvas { display: flex; flex-direction: column; height: 100%; min-height: 0; }
  .toolbar {
    display: flex; gap: 8px; align-items: center; padding: 4px 10px;
    border-bottom: 1px solid var(--panel-edge);
  }
  .flow { flex: 1; min-height: 0; background: var(--terminal-bg); }
  /* Edge labels render in Svelte Flow's overlay layer, not inside the edge's own <g>, so a
     click on the label never reaches onedgeclick — the label would be a dead spot right in
     the middle of the edge. Svelte Flow sets pointer-events inline, hence !important. */
  .flow :global(.svelte-flow__edge-label) { pointer-events: none !important; }
  .flow :global(.svelte-flow__attribution) {
    background: transparent; color: var(--text-dim); font-size: var(--fs-label);
  }
  .err { color: var(--err); }
</style>
