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
  uiState.runCenter = "graph";
}
