import {
  onSessionEvent,
  onSessionsStatus,
  projectList,
  sessionList,
  sessionsStatus,
  toCmdError,
} from "./ipcSessions";
import {
  applyGitPoll, applySessionEvent, refreshLists, sessionsState, setLists,
} from "./state/sessions.svelte";
import { uiState } from "./state/ui.svelte";
import { sessionTerm } from "./term/instances";
import type { SessionEvent, SessionsConnState } from "./types";

/// The git columns are derived server-side per request, so nothing pushes them —
/// they are only ever as fresh as the last poll (spec §3).
const POLL_MS = 5000;

let started = false;
let lastConn: SessionsConnState = "idle";
let refetchQueued = false;

function report(operation: string, error: unknown): void {
  console.error(`sessions ${operation} failed:`, toCmdError(error).message);
}

function handleStatus(s: { state: SessionsConnState }): void {
  onStatus(s).catch((error: unknown) => {
    report("status", error);
  });
}

/// Idempotent; the first call registers the listeners and kicks `sessions_status`,
/// which is what starts the shell's supervisor. Safe to call on every switch into
/// sessions mode. The listeners and the poll then live for the app's life: leaving
/// the mode does not disconnect, so coming back is instant.
export async function enterSessionsMode(): Promise<void> {
  if (started) return;
  started = true;
  try {
    await onSessionsStatus(handleStatus);
    await onSessionEvent(onEvent);
    setInterval(() => {
      void poll();
    }, POLL_MS);
    await onStatus(await sessionsStatus());
  } catch (error) {
    report("connect", error);
  }
}

/// The reducer-side effects of one status transition. Exported for tests and for
/// the listener above; nothing else calls it.
export async function onStatus(s: { state: SessionsConnState }): Promise<void> {
  if (s.state !== "connected") {
    sessionsState.conn = s.state;
    lastConn = s.state;
    return;
  }
  // A daemon we lost and found again is a new process: every session's PTY stream
  // died with it and no xterm we hold is attached to anything. They are dropped
  // *before* the UI is told it is back, so the reopen that SessionTerminal runs off
  // this transition builds fresh ones rather than resuming corpses.
  if (lastConn === "unreachable") sessionTerm.disposeAll();
  sessionsState.conn = "connected";
  lastConn = "connected";
  const [projects, sessions] = await Promise.all([projectList(), sessionList()]);
  setLists(projects, sessions);
}

function onEvent(ev: SessionEvent): void {
  if (applySessionEvent(ev) !== "refetch") return;
  // One fetch per tick: creating a session emits its own event and (through the
  // spawn) a lifecycle one, and a project registration lands with the session
  // events of whatever it brought along.
  if (refetchQueued) return;
  refetchQueued = true;
  queueMicrotask(() => {
    refetchQueued = false;
    void refetch();
  });
}

async function refetch(): Promise<void> {
  try {
    const [projects, sessions] = await Promise.all([projectList(), sessionList()]);
    refreshLists(projects, sessions);
  } catch (error) {
    report("list refresh", error);
  }
}

async function poll(): Promise<void> {
  if (uiState.mode !== "sessions" || sessionsState.conn !== "connected") return;
  try {
    applyGitPoll(await sessionList());
  } catch (error) {
    report("git poll", error);
  }
}
