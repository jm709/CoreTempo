import type { AgentDetail, AgentExit, AgentState, LifecyclePhase, Refusal } from "../types";

export const agentsState = $state({
  byId: {} as Record<string, AgentDetail>,
  order: [] as string[],
  stalled: {} as Record<string, boolean>,
  blocked: {} as Record<string, string | null>,
  refused: {} as Record<string, Refusal>,
});

/** The hook refused a tool call on the agent's behalf; the latest one stays until a resync. */
export function setRefused(id: string, refusal: Refusal): void {
  agentsState.refused[id] = refusal;
}

/** Badge title for the refused ⛔ glyph: what was refused and how to allow it next time. */
export function refusedTitle(r: Refusal): string {
  const what = r.tool === null ? "a tool call" : r.tool;
  const detail = r.input === null ? "" : `: ${r.input}`;
  return `refused ${what}${detail} — no allow rule matched; add one to tools = […] or allow = […] if the agent needs it`;
}

export function setStalled(id: string, on: boolean): void {
  if (on) agentsState.stalled[id] = true;
  // oxlint-disable-next-line no-dynamic-delete -- keyed rune map, ids are roster-bounded
  else delete agentsState.stalled[id];
}

export function setBlocked(id: string, tool: string | null): void {
  agentsState.blocked[id] = tool;
}

export function clearBlocked(id: string): void {
  // oxlint-disable-next-line no-dynamic-delete -- keyed rune map, ids are roster-bounded
  delete agentsState.blocked[id];
}

/** Badge title for the blocked ⏸ glyph: names the tool when the hook gave one. */
export function blockedTitle(tool: string | null): string {
  const suffix = "answer it in the terminal or add an allow rule";
  return tool === null
    ? `waiting on a Claude Code permission dialog — ${suffix}`
    : `waiting on a Claude Code permission dialog for ${tool} — ${suffix}`;
}

export function setAgents(list: AgentDetail[]): void {
  const byId: Record<string, AgentDetail> = {};
  for (const a of list) byId[a.id] = a;
  agentsState.byId = byId;
  // oxlint-disable-next-line no-array-sort -- ES2022 lib; sorting a fresh map() copy is safe
  agentsState.order = list.map((a) => a.id).sort();
  agentsState.stalled = {};
  agentsState.blocked = {};
  agentsState.refused = {};
  for (const a of list) if (a.blocked) agentsState.blocked[a.id] = null;
}

export function applyAgentState(id: string, state: AgentState): void {
  const a = agentsState.byId[id];
  if (a === undefined) return; // roster frozen; snapshot precedes events (see wireEvents)
  a.state = state;
  if (state !== "exited") a.exit = null;
  if (state === "working") setStalled(id, false);
}

export function applyLifecycle(id: string, phase: LifecyclePhase, exit: AgentExit | null): void {
  const a = agentsState.byId[id];
  if (a === undefined) return;
  if (phase === "spawned") {
    a.state = "starting";
    a.exit = null;
  } else if (phase === "exited") {
    a.state = "exited";
    a.exit = exit;
  } else {
    a.state = "restarting";
    a.exit = null;
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
  agentsState.blocked = {};
  agentsState.refused = {};
}
