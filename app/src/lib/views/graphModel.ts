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

// "§" cannot collide with an agent id: ids are restricted to [a-z0-9_-]. ":" is outside the
// flow-name charset too, so a trigger/output node id parses back into its flow unambiguously.
export const WORKFLOW_NODE_ID = "§workflow";
export const TRIGGER_NODE_PREFIX = "§trigger:";
export const OUTPUT_NODE_PREFIX = "§output:";

export function triggerNodeId(flow: string): string {
  return `${TRIGGER_NODE_PREFIX}${flow}`;
}
export function outputNodeId(flow: string): string {
  return `${OUTPUT_NODE_PREFIX}${flow}`;
}
/** The flow name a trigger node id carries, or null for any other id. */
export function triggerNodeFlow(id: string): string | null {
  return id.startsWith(TRIGGER_NODE_PREFIX) ? id.slice(TRIGGER_NODE_PREFIX.length) : null;
}
/** The flow name an output node id carries, or null for any other id. */
export function outputNodeFlow(id: string): string | null {
  return id.startsWith(OUTPUT_NODE_PREFIX) ? id.slice(OUTPUT_NODE_PREFIX.length) : null;
}

export function sortedFlowNames(model: WorkflowModel): string[] {
  // oxlint-disable-next-line no-array-sort -- fresh local array
  return Object.keys(model.flows).sort();
}

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
    flowName?: string;
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
  sortedFlowNames(model).forEach((name, index) => {
    const flow = model.flows[name]!;
    const id = triggerNodeId(name);
    nodes.push({
      id,
      type: "trigger",
      position: { x: TRIGGER_NODE_POSITION.x, y: TRIGGER_NODE_POSITION.y + ROW_Y_STEP * index },
      data: { trigger: flow.trigger, flowName: name },
    });
    edges.push({
      id: `${id}>${flow.trigger.edge.to}:${flow.trigger.edge.kind}`,
      source: id,
      target: flow.trigger.edge.to,
      label: flow.trigger.edge.kind,
    });
  });
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
  for (const name of sortedFlowNames(model)) {
    const flow = model.flows[name]!;
    if (flow.output === undefined) continue;
    // The kickoff target anchors the box; positions covers exactly the roster, so a
    // missing anchor means a dangling target — park beside the trigger, no edge (its
    // source node would not exist and SvelteFlow would drop it with a warning).
    const anchor = positions[flow.trigger.edge.to];
    nodes.push({
      id: outputNodeId(name),
      type: "output",
      position: anchor === undefined
        ? { x: TRIGGER_NODE_POSITION.x + RANK_X_STEP, y: TRIGGER_NODE_POSITION.y }
        : { x: anchor.x + RANK_X_STEP, y: anchor.y },
      data: { output: flow.output },
      connectable: false,
    });
    if (anchor !== undefined) {
      edges.push({
        id: `${flow.trigger.edge.to}>${outputNodeId(name)}:output`,
        source: flow.trigger.edge.to,
        target: outputNodeId(name),
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
 * null on success. Mirrors workflow_parse's validation client-side for instant feedback.
 * Every flow the source belongs to gains the target as a member: a member with an edge to a
 * non-member fails the edge-closure check at save time, and membership is TOML-only until
 * multi-flow phase 4a, so any other outcome dead-ends the user in the canvas. */
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
  for (const flow of Object.values(model.flows)) {
    if (flow.agents.includes(from) && !flow.agents.includes(to)) flow.agents.push(to);
  }
  return null;
}

/** Removes exactly the (to, kind) pair; a no-op if no such edge exists. */
export function removeEdge(model: WorkflowModel, from: string, to: string, kind: EdgeKind): void {
  const agent = model.agents[from];
  if (agent === undefined) return;
  agent.edges = agent.edges.filter((edge) => !(edge.to === to && edge.kind === kind));
}

/** Flips an existing (from, to) edge's kind in place, preserving array order. A trigger node
 * id for `from` flips that flow's own edge — the one edge no agent owns. */
export function setEdgeKind(
  model: WorkflowModel,
  from: string,
  to: string,
  kind: EdgeKind,
): void {
  const flowName = triggerNodeFlow(from);
  if (flowName !== null) {
    const flow = model.flows[flowName];
    if (flow !== undefined && flow.trigger.edge.to === to) flow.trigger.edge.kind = kind;
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
    allow: [],
    mcp: [],
    concurrency: "exclusive",
    isolated_config: false,
    skills: [],
  };
  return id;
}

/** Removes an agent and strips every other agent's edges pointing at it. A flow whose trigger
 * is aimed at it goes too: the trigger *is* its edge, and workflow_parse refuses an off-roster
 * target, so keeping it would leave a file that cannot be saved. Other flows keep the agent's
 * former membership slot empty. */
export function removeAgent(model: WorkflowModel, id: string): void {
  // oxlint-disable-next-line no-dynamic-delete -- id is a validated agent-map key, not user HTML
  delete model.agents[id];
  for (const agent of Object.values(model.agents)) {
    agent.edges = agent.edges.filter((edge) => edge.to !== id);
  }
  for (const [name, flow] of Object.entries(model.flows)) {
    if (flow.trigger.edge.to === id) {
      // oxlint-disable-next-line no-dynamic-delete -- name is a validated flow-map key
      delete model.flows[name];
      continue;
    }
    flow.agents = flow.agents.filter((member) => member !== id);
  }
}

/** Creates a new flow named flow-N spanning the full roster (multi-flow spec
 * §8); the author narrows agents = [...] in the TOML. Never refused for
 * "already has a trigger" — flows are plural now. */
export function addFlow(
  model: WorkflowModel,
  type: TriggerType,
): { name: string } | { error: string } {
  const ids = sortedAgentIds(model);
  const first = ids[0];
  if (first === undefined) {
    return { error: `no agents to trigger; add an agent before adding a '${type}' flow` };
  }
  let n = 1;
  while (`flow-${n}` in model.flows) n += 1;
  const name = `flow-${n}`;
  model.flows[name] = {
    agents: ids,
    trigger: { type, edge: { to: first, kind: "ask" }, message: type === "on_start" ? "" : null },
  };
  return { name };
}

/** Deletes the flow section; its agents are untouched. */
export function removeFlow(model: WorkflowModel, name: string): void {
  // oxlint-disable-next-line no-dynamic-delete -- name is a validated flows-map key
  delete model.flows[name];
}

/** Re-aims a named flow's trigger edge, keeping its kind, and keeps the target a member of
 * the flow. Returns an error message naming the input, the rule, and the fix — or null on
 * success. */
export function setTriggerTarget(model: WorkflowModel, name: string, to: string): string | null {
  const flow = model.flows[name];
  if (flow === undefined) {
    return `no flow named '${name}'; flows: ${sortedFlowNames(model).join(", ")}`;
  }
  if (model.agents[to] === undefined) {
    return `no agent named '${to}'; roster: ${sortedAgentIds(model).join(", ")}`;
  }
  flow.trigger.edge.to = to;
  // Validation requires the target to be a member; membership editing is TOML-only.
  if (!flow.agents.includes(to)) flow.agents.push(to);
  return null;
}

/** Declares `[flows.<name>.output]` with a stub the inspector completes. Save-time
 * validation is the authority on the schema_file being non-empty. Returns an error
 * message naming the input, the rule, and the fix — or null on success. */
export function addOutput(model: WorkflowModel, name: string): string | null {
  const flow = model.flows[name];
  if (flow === undefined) {
    return `no flow named '${name}'; flows: ${sortedFlowNames(model).join(", ")}`;
  }
  if (flow.trigger.type !== "webhook") {
    return (
      "an output schema requires a 'webhook' trigger — only an HTTP caller can " +
      "receive the structured result; change the trigger type first"
    );
  }
  if (flow.trigger.edge.kind !== "ask") {
    return (
      "an output schema requires the kickoff kind 'ask' — a send kickoff carries " +
      "no reply body to validate; change the kind first"
    );
  }
  if (flow.output !== undefined) {
    return `flow '${name}' already declares [flows.${name}.output]; edit or delete it in the inspector`;
  }
  flow.output = { schema_file: "", max_repairs: 2 };
  return null;
}

/** Drops the output declaration; the flow itself is untouched. */
export function removeOutput(model: WorkflowModel, name: string): void {
  const flow = model.flows[name];
  if (flow === undefined) return;
  delete flow.output;
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
  for (const flow of Object.values(model.flows)) {
    flow.agents = flow.agents.map((member) => (member === oldId ? newId : member));
    if (flow.trigger.edge.to === oldId) flow.trigger.edge.to = newId;
  }
  return null;
}
