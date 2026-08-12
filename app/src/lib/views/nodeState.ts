import type { AgentState } from "../types";

export type NodeTint = "busy" | "info" | "err";

/// Border tint for an agent node during a run (spec: run-time workflow screen).
/// null = default chrome — idle agents and the stopped-mode editor.
export function nodeTint(state: AgentState | undefined): NodeTint | null {
  switch (state) {
    case "working": return "busy";
    case "starting": return "info";
    case "restarting": return "info";
    case "exited": return "err";
    case "idle": return null;
    case undefined: return null;
  }
}
