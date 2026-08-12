// Helpers shared by the graph canvas and the inspector. The editing rules themselves live in
// graphModel.ts; this module adapts them to what the canvas hands over.

import type { EdgeKind, WorkflowModel } from "../types";
import {
  addEdge,
  OUTPUT_NODE_ID,
  setTriggerTarget,
  TRIGGER_NODE_ID,
  WORKFLOW_NODE_ID,
} from "./graphModel";

interface Point { x: number; y: number }

const NUDGE_Y = 140;
const OVERLAP = 40;

/** Keeps a freshly laid-out node off nodes the user has already placed: the auto-layout
 * slot for a new agent is often the slot an older agent was dragged away from, and two
 * nodes at identical coordinates hide each other completely. Nudges down until free. */
export function freeSlot(position: Point, taken: Point[]): Point {
  let y = position.y;
  while (taken.some((p) => Math.abs(p.x - position.x) < OVERLAP && Math.abs(p.y - y) < OVERLAP)) {
    y += NUDGE_Y;
  }
  return { x: position.x, y };
}

/** Applies a canvas connection to the model, returning an error message or null on success.
 * New edges are `ask` (spec §3.1). The workflow node is decoration — it has no handles and
 * no edges in the model — so a connection touching it is refused rather than added. The
 * output node is a read-only projection of [trigger.output] for the same reason. A drag
 * from the trigger node re-aims the trigger's single edge instead of appending one: the
 * trigger has exactly one target, so the new drag replaces the old target. */
export function connectAgents(model: WorkflowModel, source: string, target: string): string | null {
  if (source === WORKFLOW_NODE_ID || target === WORKFLOW_NODE_ID) {
    return `'${WORKFLOW_NODE_ID}' is not an agent; drag between two agent nodes`;
  }
  if (source === OUTPUT_NODE_ID || target === OUTPUT_NODE_ID) {
    return (
      `'${OUTPUT_NODE_ID}' is a projection of [trigger.output]; it takes no drawn ` +
      "edges — its edge follows the trigger's kickoff target"
    );
  }
  if (source === TRIGGER_NODE_ID) return setTriggerTarget(model, target);
  if (target === TRIGGER_NODE_ID) {
    return `'${TRIGGER_NODE_ID}' has no inbound edges; it starts the workflow, so drag from it, not into it`;
  }
  return addEdge(model, source, target, "ask");
}

/** Coerces a number input's raw text, rejecting blanks and non-numbers so a half-typed
 * value never overwrites a workflow field. Range checks stay with workflow_parse. */
export function coerceNumber(raw: string): number | null {
  if (raw.trim() === "") return null;
  const n = Number(raw);
  return Number.isFinite(n) ? n : null;
}

/** Guards `setEdgeKind`, which flips every edge between two agents: flipping onto a kind
 * that already exists would collapse two edges into one duplicate. Returns the message to
 * show, or null when the flip is safe. */
export function duplicateEdgeError(
  model: WorkflowModel,
  from: string,
  to: string,
  kind: EdgeKind,
): string | null {
  const exists = model.agents[from]?.edges.some((e) => e.to === to && e.kind === kind) ?? false;
  if (!exists) return null;
  return (
    `'${from}' -> '${to}' already has an edge of kind '${kind}'; ` +
    `delete one of the two edges before changing this one's kind`
  );
}

/** Click-cycle order for edge kinds. Trigger edges skip loop: a trigger cannot
 * supervise rounds, and validation rejects it. */
export function nextEdgeKind(kind: EdgeKind, allowLoop: boolean): EdgeKind {
  if (kind === "ask") return "send";
  if (kind === "send") return allowLoop ? "loop" : "ask";
  return "ask";
}

/** The output edge is a projection of [trigger.output], not an EdgeModel. */
export function isProjectedEdge(label: EdgeKind | "output"): label is "output" {
  return label === "output";
}
