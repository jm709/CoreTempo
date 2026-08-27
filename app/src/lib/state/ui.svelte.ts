import type { RunPhase } from "./run.svelte";

export type RunCenter = "graph" | "terminals";

export type DockTab = "feed" | "chat" | "run";

export const uiState = $state({
  focusedAgent: null as string | null,   // pane owning the accent border when captured
  captured: false,                       // keys go to the focused terminal
  maximizedAgent: null as string | null,
  dockTab: "feed" as DockTab,
  flashAgent: null as string | null,     // 200 ms border flash target (feed → terminal linkage)
  hoverFrom: null as string | null,      // roster highlight while hovering a feed item
  hoverTo: null as string | null,
  editorPath: null as string | null,     // workflow file open in the stopped-mode editor
  editorDirty: false,                    // the editor holds edits not yet saved to editorPath
  runCenter: "graph" as RunCenter,       // center view while a run is active
});

let flashTimer: ReturnType<typeof setTimeout> | undefined;

export function focusTerminal(agent: string): void {
  uiState.focusedAgent = agent;
  uiState.captured = true;
}

export function releaseCapture(): void {
  uiState.captured = false;
}

/// The graph view needs an open workflow file; without one the toggle pins
/// to terminals rather than showing an empty center.
export function toggleRunCenter(): void {
  if (uiState.editorPath === null) {
    uiState.runCenter = "terminals";
    return;
  }
  uiState.runCenter = uiState.runCenter === "graph" ? "terminals" : "graph";
}

export function openAgentTerminal(agent: string): void {
  uiState.runCenter = "terminals";
  focusTerminal(agent);
}

export function flashTerminal(agent: string): void {
  uiState.flashAgent = agent;
  clearTimeout(flashTimer);
  flashTimer = setTimeout(() => {
    uiState.flashAgent = null;
  }, 200);
}

export function resetUi(): void {
  uiState.focusedAgent = null;
  uiState.captured = false;
  uiState.maximizedAgent = null;
  uiState.dockTab = "feed";
  uiState.flashAgent = null;
  uiState.hoverFrom = null;
  uiState.hoverTo = null;
  uiState.editorPath = null;
  uiState.editorDirty = false;
  uiState.runCenter = "graph";
}

export interface RunGate {
  disabled: boolean;
  hint: string | null;
}

/// Whether the header's ▶ Run / ■ Stop button is usable, and why not when it
/// is not. A run always starts from the file on disk, so unsaved editor
/// edits block Run (never Stop) until the operator saves them (#89).
export function runGate(
  phase: RunPhase,
  editorPath: string | null,
  editorDirty: boolean,
): RunGate {
  if (phase === "starting" || phase === "stopping") return { disabled: true, hint: null };
  if (phase === "running") return { disabled: false, hint: null };
  if (editorPath === null) return { disabled: true, hint: null };
  if (editorDirty) return { disabled: true, hint: "save the workflow to run it" };
  return { disabled: false, hint: null };
}
