import type {
  AgentExit, AgentState, MessageRecord, SessionsConnState, SessionState,
} from "./types";

// Spec §9.3: ● working (pulsing) · ◌ idle · ◐ starting · ✕ dead. Restarting reuses ◐.
// ◻ stopped is sessions-only (spec §2): a session parked without a live process,
// distinct from exited's ✕ (a session that ended unexpectedly).
export const STATE_GLYPHS: Record<AgentState | SessionState, string> = {
  working: "●",
  idle: "◌",
  starting: "◐",
  restarting: "◐",
  exited: "✕",
  stopped: "◻",
};

export function stateLabel(s: AgentState): string {
  return s === "exited" ? "dead" : s;
}

// Sessions topbar: what the daemon connection is doing. `idle` only exists before
// the first status lands, and entering sessions mode always kicks the supervisor,
// so it reads as starting rather than as a state of its own.
export function connLabel(conn: SessionsConnState): string {
  switch (conn) {
    case "connected": return "connected";
    case "unreachable": return "daemon unreachable — retrying";
    case "idle":
    case "starting": return "starting…";
  }
}

// Pane overlay for a dead agent: `exited 3`, or `killed: Terminated` when a
// signal ended it; `exited ?` while the snapshot has not caught up.
export function exitLabel(exit: AgentExit | null): string {
  if (exit === null) return "exited ?";
  if ("signal" in exit) return `killed: ${exit.signal}`;
  return `exited ${exit.code}`;
}

// Spec §9.3 lifecycle: ○ → ⟳ ∅0|✓ ; codes render as mono chips ∅0/∅1.
export function lifecycleGlyph(m: MessageRecord): string {
  switch (m.status) {
    case "queued": return "○";
    case "injected": return "→";
    case "working": return "⟳";
    case "replied": return m.code === 1 ? "∅1" : "∅0";
    case "done": return "✓";
    case "failed": return "✕";
  }
}

export function feedTime(ts: string): string {
  return ts.slice(11, 19);
}

export function originAgent(from: string): string | null {
  return from.startsWith("agent:") ? from.slice(6) : null;
}

export function originLabel(from: string): string {
  const agent = originAgent(from);
  if (agent !== null) return agent;
  if (from === "user") return "you";
  return from.startsWith("trigger:") ? "trigger" : "external";
}

// Anything that entered over the API rather than from an agent or the in-process
// user: a plain HTTP message or a flow kickoff.
export function isExternal(from: string): boolean {
  return from.startsWith("http:") || from.startsWith("trigger:");
}

// Chat = the feed filtered to human ↔ agent (spec §9.2). `to` is always an agent,
// so human traffic is exactly the messages originated by the in-process user.
export function isChat(m: MessageRecord): boolean {
  return m.from === "user";
}

// Feed fade plays only for rows entering on state change, not on virtua remounts
// during scroll (spec §9.3: zero animation on insertion of old content).
export function isFresh(createdAt: string, nowMs: number): boolean {
  return nowMs - Date.parse(createdAt) < 1000;
}

export function elapsed(fromTs: string, nowMs: number): string {
  const total = Math.max(0, Math.floor((nowMs - Date.parse(fromTs)) / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  if (h > 0) return `${h}h ${String(m).padStart(2, "0")}m`;
  return `${m}m ${String(s).padStart(2, "0")}s`;
}
