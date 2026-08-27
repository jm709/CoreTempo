import { runStart, runStop, runUntrustedDirs, snapshot, toCmdError } from "./ipc";
import { resetAgents } from "./state/agents.svelte";
import { runState } from "./state/run.svelte";
import { releaseCapture, uiState } from "./state/ui.svelte";
import { disposeAllTerminals, ensureTerminal } from "./term/manager";
import { confirmTrust } from "./dialogs";
import { applySnapshot, wireEvents } from "./wireEvents";
import type { Snapshot } from "./types";

/// bus.reset recovery: control plane only. PTY channels are contiguous byte cursors
/// and never desync, so terminals are left alone (contracts §8.2).
export async function resyncControlPlane(): Promise<void> {
  applySnapshot(await snapshot());
}

async function openTerminals(snap: Snapshot): Promise<void> {
  if (snap.run === null) return;
  for (const agent of snap.agents) {
    // Contracts §8.2: a fresh terminal passes null to replay the full ring tail.
    // pty_cursors are end-of-stream positions, for clients that already hold the
    // screen; subscribing there joins mid-stream and shows a torn or blank pane.
    await ensureTerminal(agent.id, null, snap.run.scrollback);
  }
}

/// App start (fresh or reload mid-run): snapshot → subscribe events (seq dedup)
/// → PTY channels with the snapshot's cursors. Core replays ring tails: no gap,
/// no duplicate.
export async function boot(): Promise<void> {
  const snap = await snapshot();
  applySnapshot(snap);
  await wireEvents(() => {
    void resyncControlPlane();
  });
  if (snap.run !== null) {
    uiState.editorPath = snap.run.workflow_path;
    await openTerminals(snap);
  }
}

export async function startRun(configPath: string): Promise<void> {
  runState.phase = "starting";
  uiState.runCenter = "graph";
  try {
    const roots = await runUntrustedDirs(configPath);
    let trustConfirmed = false;
    if (roots.length > 0) {
      trustConfirmed = await confirmTrust(roots);
      if (!trustConfirmed) {
        runState.phase = "stopped";
        return;
      }
    }
    runState.info = await runStart(configPath, trustConfirmed);
    const snap = await snapshot(); // events before this are deduped by last_seq
    applySnapshot(snap);
    await openTerminals(snap);
  } catch (e: unknown) {
    runState.phase = "stopped";
    throw toCmdError(e);
  }
}

/// Stopping dims panes until confirmed, then the center switches to the editor
/// (spec §9.2 edge states).
export async function stopRun(): Promise<void> {
  runState.phase = "stopping";
  try {
    await runStop();
  } finally {
    runState.phase = "stopped";
    runState.info = null;
    runState.completed = null;
    releaseCapture();
    uiState.maximizedAgent = null;
    disposeAllTerminals();
    resetAgents();
    // messages stay: the traffic feed is history (SQLite-backed), not run state
  }
}
