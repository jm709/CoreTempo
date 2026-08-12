import type { AgentDetail, AgentState, LifecyclePhase } from "../types";

export const agentsState = $state({
  byId: {} as Record<string, AgentDetail>,
  order: [] as string[],
  stalled: {} as Record<string, boolean>,
});

export function setStalled(id: string, on: boolean): void {
  if (on) agentsState.stalled[id] = true;
  // oxlint-disable-next-line no-dynamic-delete -- keyed rune map, ids are roster-bounded
  else delete agentsState.stalled[id];
}

export function setAgents(list: AgentDetail[]): void {
  const byId: Record<string, AgentDetail> = {};
  for (const a of list) byId[a.id] = a;
  agentsState.byId = byId;
  // oxlint-disable-next-line no-array-sort -- ES2022 lib; sorting a fresh map() copy is safe
  agentsState.order = list.map((a) => a.id).sort();
  agentsState.stalled = {};
}

export function applyAgentState(id: string, state: AgentState): void {
  const a = agentsState.byId[id];
  if (a === undefined) return; // roster frozen; snapshot precedes events (see wireEvents)
  a.state = state;
  if (state !== "exited") a.exit_code = null;
  if (state === "working") setStalled(id, false);
}

export function applyLifecycle(id: string, phase: LifecyclePhase, exitCode: number | null): void {
  const a = agentsState.byId[id];
  if (a === undefined) return;
  if (phase === "spawned") {
    a.state = "starting";
    a.exit_code = null;
  } else if (phase === "exited") {
    a.state = "exited";
    a.exit_code = exitCode;
  } else {
    a.state = "restarting";
    a.exit_code = null;
  }
}

export function runningCount(): number {
  let n = 0;
  for (const id of agentsState.order) {
    const s = agentsState.byId[id]?.state;
    if (s === "starting" || s === "idle" || s === "working") n += 1;
  }
  return n;
}

export function resetAgents(): void {
  agentsState.byId = {};
  agentsState.order = [];
  agentsState.stalled = {};
}
