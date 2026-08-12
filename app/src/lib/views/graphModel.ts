// Pure model <-> flow-graph mapping, auto-layout, and mutation helpers for the node editor.
// No Svelte Flow imports here — FlowNode/FlowEdge are this project's own shapes; Svelte Flow
// itself is only consumed by the canvas component that renders them.

import type {
  AgentModel,
  EdgeKind,
  OutputModel,
  TriggerModel,
  TriggerType,
  WorkflowModel,
} from "../types";

// "§" cannot collide with an agent id: ids are restricted to [a-z0-9_-].
export const WORKFLOW_NODE_ID = "§workflow";
export const TRIGGER_NODE_ID = "§trigger";
export const OUTPUT_NODE_ID = "§output";

const AGENT_ID_PATTERN = /^[a-z0-9][a-z0-9_-]{0,31}$/;
const RANK_X_STEP = 260;
const ROW_Y_STEP = 130;
const ROW_Y_BASE = 60;
const WORKFLOW_NODE_POSITION = { x: 0, y: 60 };
// Fixed, like the workflow node: both sit left of rank 0, outside the agent auto-layout.
const TRIGGER_NODE_POSITION = { x: 0, y: 190 };

export interface FlowNode {
  id: string;
  type: "agent" | "workflow" | "trigger" | "output";
  position: { x: number; y: number };
  data: {
    agent?: AgentModel;
    model?: WorkflowModel;
    trigger?: TriggerModel;
    output?: OutputModel;
  };
  connectable?: boolean;
}

export interface FlowEdge {
  id: string; // `${from}>${to}:${kind}` — or `:output` for the display-only output edge
  source: string;
  target: string;
  label: EdgeKind | "output";
}

/** Read-only projection of the workflow model into Svelte-Flow-shaped nodes and edges. */
export function toFlow(model: WorkflowModel): { nodes: FlowNode[]; edges: FlowEdge[] } {
  const positions = layoutPositions(model);
  const nodes: FlowNode[] = [
    { id: WORKFLOW_NODE_ID, type: "workflow", position: WORKFLOW_NODE_POSITION, data: { model } },
  ];
  const edges: FlowEdge[] = [];
  if (model.trigger !== null) {
    const trigger = model.trigger;
    nodes.push({
      id: TRIGGER_NODE_ID,
      type: "trigger",
      position: TRIGGER_NODE_POSITION,
      data: { trigger },
    });
    edges.push({
      id: `${TRIGGER_NODE_ID}>${trigger.edge.to}:${trigger.edge.kind}`,
      source: TRIGGER_NODE_ID,
      target: trigger.edge.to,
      label: trigger.edge.kind,
    });
  }
  for (const [id, agent] of Object.entries(model.agents)) {
    const position = positions[id] ?? { x: 0, y: 0 };
    nodes.push({ id, type: "agent", position, data: { agent } });
    for (const edge of agent.edges) {
      edges.push({
        id: `${id}>${edge.to}:${edge.kind}`,
        source: id,
        target: edge.to,
        label: edge.kind,
      });
    }
  }
  // Pushed after the agent loop so freeSlot's collision reconciler nudges this projection
  // box on first layout instead of displacing a real agent that landed on the same slot.
  if (model.trigger !== null && model.trigger.output !== undefined) {
    // The kickoff target anchors the box; positions covers exactly the roster, so a
    // missing anchor means a dangling target — park beside the trigger, no edge (its
    // source node would not exist and SvelteFlow would drop it with a warning).
    const anchor = positions[model.trigger.edge.to];
    nodes.push({
      id: OUTPUT_NODE_ID,
      type: "output",
      position: anchor === undefined
        ? { x: TRIGGER_NODE_POSITION.x + RANK_X_STEP, y: TRIGGER_NODE_POSITION.y }
        : { x: anchor.x + RANK_X_STEP, y: anchor.y },
      data: { output: model.trigger.output },
      connectable: false,
    });
    if (anchor !== undefined) {
      edges.push({
        id: `${model.trigger.edge.to}>${OUTPUT_NODE_ID}:output`,
        source: model.trigger.edge.to,
        target: OUTPUT_NODE_ID,
        label: "output",
      });
    }
  }
  return { nodes, edges };
}

/** BFS rank-based auto-layout, read-only. Roots (no incoming edges) start at rank 0; a cycle
 * with no root falls back to the lexicographically first agent id so layout still terminates. */
export function layoutPositions(model: WorkflowModel): Record<string, { x: number; y: number }> {
  const ids = sortedAgentIds(model);
  const indegree = new Map<string, number>(ids.map((id) => [id, 0]));
  for (const id of ids) {
    for (const edge of model.agents[id]!.edges) {
      if (indegree.has(edge.to)) indegree.set(edge.to, indegree.get(edge.to)! + 1);
    }
  }

  const roots = ids.filter((id) => indegree.get(id) === 0);
  const starts = roots.length > 0 ? roots : ids.slice(0, 1);

  const rankOf = new Map<string, number>();
  const ranks: string[][] = [];
  let frontier = starts;
  let rank = 0;
  while (frontier.length > 0) {
    const nextFrontier: string[] = [];
    ranks[rank] ??= [];
    for (const id of frontier) {
      if (rankOf.has(id)) continue;
      rankOf.set(id, rank);
      ranks[rank]!.push(id);
      for (const edge of model.agents[id]!.edges) {
        if (!rankOf.has(edge.to) && !nextFrontier.includes(edge.to)) nextFrontier.push(edge.to);
      }
    }
    frontier = nextFrontier;
    rank += 1;
  }

  // Cycle remnants: agents no BFS from a root/fallback start ever reached.
  const remaining = ids.filter((id) => !rankOf.has(id));
  if (remaining.length > 0) {
    ranks[rank] ??= [];
    for (const id of remaining) {
      rankOf.set(id, rank);
      ranks[rank]!.push(id);
    }
  }

  const positions: Record<string, { x: number; y: number }> = {};
  for (const [r, idsInRank] of ranks.entries()) {
    idsInRank.forEach((id, index) => {
      positions[id] = { x: RANK_X_STEP * (r + 1), y: ROW_Y_BASE + ROW_Y_STEP * index };
    });
  }
  return positions;
}

function sortedAgentIds(model: WorkflowModel): string[] {
  const ids = Object.keys(model.agents);
  // oxlint-disable-next-line no-array-sort -- ES2022 lib; ids is a fresh local array
  return ids.sort();
}

function agentOrError(model: WorkflowModel, id: string): AgentModel | string {
  const agent = model.agents[id];
  if (agent === undefined) {
    return `no agent named '${id}'; roster: ${sortedAgentIds(model).join(", ")}`;
  }
  return agent;
}

/** Appends an edge, returning an error message naming the input, the rule, and the fix — or
 * null on success. Mirrors workflow_parse's validation client-side for instant feedback. */
export function addEdge(
  model: WorkflowModel,
  from: string,
  to: string,
  kind: EdgeKind,
): string | null {
  const source = agentOrError(model, from);
  if (typeof source === "string") return source;
  if (model.agents[to] === undefined) {
    return `no agent named '${to}'; roster: ${sortedAgentIds(model).join(", ")}`;
  }
  if (from === to) {
    return `agent '${from}' cannot edge to itself; choose a different target`;
  }
  if (source.edges.some((edge) => edge.to === to && edge.kind === kind)) {
    return `duplicate edge '${from}' -> '${to}' (${kind}); remove the existing edge or pick a different kind`;
  }
  source.edges.push({ to, kind });
  return null;
}

/** Removes exactly the (to, kind) pair; a no-op if no such edge exists. */
export function removeEdge(model: WorkflowModel, from: string, to: string, kind: EdgeKind): void {
  const agent = model.agents[from];
  if (agent === undefined) return;
  agent.edges = agent.edges.filter((edge) => !(edge.to === to && edge.kind === kind));
}

/** Flips an existing (from, to) edge's kind in place, preserving array order. `from ===
 * TRIGGER_NODE_ID` flips the trigger's own edge — the one edge no agent owns. */
export function setEdgeKind(
  model: WorkflowModel,
  from: string,
  to: string,
  kind: EdgeKind,
): void {
  if (from === TRIGGER_NODE_ID) {
    if (model.trigger !== null && model.trigger.edge.to === to) model.trigger.edge.kind = kind;
    return;
  }
  const agent = model.agents[from];
  if (agent === undefined) return;
  for (const edge of agent.edges) {
    if (edge.to === to) edge.kind = kind;
  }
}

/** Swaps the edge at `index` with its neighbor `index + delta`; out-of-range deltas no-op. */
export function moveEdge(model: WorkflowModel, from: string, index: number, delta: -1 | 1): void {
  const agent = model.agents[from];
  if (agent === undefined) return;
  const target = index + delta;
  if (index < 0 || index >= agent.edges.length) return;
  if (target < 0 || target >= agent.edges.length) return;
  const edges = agent.edges;
  [edges[index], edges[target]] = [edges[target]!, edges[index]!];
}

/** Inserts a new agent under the first free `agent-N` id, with template defaults. */
export function addAgent(model: WorkflowModel): string {
  let n = 1;
  while (`agent-${n}` in model.agents) n += 1;
  const id = `agent-${n}`;
  model.agents[id] = {
    dir: "",
    prompt: "",
    model: null,
    permission_mode: null,
    auto_clear: true,
    edges: [],
    tools: [],
  };
  return id;
}

/** Removes an agent and strips every other agent's edges pointing at it. A trigger aimed at
 * it goes too: the trigger *is* its edge, and workflow_parse refuses an off-roster target,
 * so keeping it would leave a file that cannot be saved. */
export function removeAgent(model: WorkflowModel, id: string): void {
  // oxlint-disable-next-line no-dynamic-delete -- id is a validated agent-map key, not user HTML
  delete model.agents[id];
  for (const agent of Object.values(model.agents)) {
    agent.edges = agent.edges.filter((edge) => edge.to !== id);
  }
  if (model.trigger?.edge.to === id) model.trigger = null;
}

/** Adds the workflow's single trigger, aimed at the lexicographically first agent with kind
 * `ask`. `on_start` seeds an empty message for the inspector to fill; save-time validation,
 * not this function, is the authority on whether that text is required. Returns an error
 * message naming the input, the rule, and the fix — or null on success. */
export function addTrigger(model: WorkflowModel, type: TriggerType): string | null {
  if (model.trigger !== null) {
    return `a workflow has at most one trigger (this one is '${model.trigger.type}'); delete it before adding a '${type}' trigger`;
  }
  const first = sortedAgentIds(model)[0];
  if (first === undefined) {
    return `no agents to trigger; add an agent before adding a '${type}' trigger`;
  }
  model.trigger = {
    type,
    edge: { to: first, kind: "ask" },
    message: type === "on_start" ? "" : null,
  };
  return null;
}

/** Drops the trigger; the agents it pointed at are untouched. */
export function removeTrigger(model: WorkflowModel): void {
  model.trigger = null;
}

/** Re-aims the trigger's edge, keeping its kind. Returns an error message naming the input,
 * the rule, and the fix — or null on success. */
export function setTriggerTarget(model: WorkflowModel, to: string): string | null {
  if (model.trigger === null) {
    return `no trigger to re-aim at '${to}'; add one with '+ on-start' or '+ webhook' first`;
  }
  if (model.agents[to] === undefined) {
    return `no agent named '${to}'; roster: ${sortedAgentIds(model).join(", ")}`;
  }
  model.trigger.edge.to = to;
  return null;
}

/** Declares `[trigger.output]` with a stub the inspector completes. Save-time validation is
 * the authority on the schema_file being non-empty (Task: empty path is a validation issue).
 * Returns an error message naming the input, the rule, and the fix — or null on success. */
export function addOutput(model: WorkflowModel): string | null {
  if (model.trigger === null) {
    return "no trigger to declare output on; add one with '+ webhook' first";
  }
  if (model.trigger.type !== "webhook") {
    return (
      "an output schema requires a 'webhook' trigger — only an HTTP caller can " +
      "receive the structured result; change the trigger type first"
    );
  }
  if (model.trigger.edge.kind !== "ask") {
    return (
      "an output schema requires the kickoff kind 'ask' — a send kickoff carries " +
      "no reply body to validate; change the kind first"
    );
  }
  if (model.trigger.output !== undefined) {
    return "the trigger already declares [trigger.output]; edit or delete it in the inspector";
  }
  model.trigger.output = { schema_file: "", max_repairs: 2 };
  return null;
}

/** Drops the output declaration; the trigger itself is untouched. */
export function removeOutput(model: WorkflowModel): void {
  if (model.trigger === null) return;
  delete model.trigger.output;
}

/** Renames an agent, moving its config and rewriting every edge that targeted it. Returns an
 * error message naming the input, the rule, and the fix — or null on success. */
export function renameAgent(model: WorkflowModel, oldId: string, newId: string): string | null {
  const agent = model.agents[oldId];
  if (agent === undefined) {
    return `no agent named '${oldId}'; roster: ${sortedAgentIds(model).join(", ")}`;
  }
  if (!AGENT_ID_PATTERN.test(newId)) {
    return `invalid agent id '${newId}'; ids must match ${AGENT_ID_PATTERN.source} (lowercase letters, digits, '_', '-', starting with a letter or digit, max 32 chars) — rename to fit that pattern`;
  }
  if (newId !== oldId && model.agents[newId] !== undefined) {
    return `agent '${newId}' already exists; choose a different id`;
  }
  // oxlint-disable-next-line no-dynamic-delete -- oldId is a validated agent-map key
  delete model.agents[oldId];
  model.agents[newId] = agent;
  for (const other of Object.values(model.agents)) {
    for (const edge of other.edges) {
      if (edge.to === oldId) edge.to = newId;
    }
  }
  if (model.trigger?.edge.to === oldId) model.trigger.edge.to = newId;
  return null;
}
