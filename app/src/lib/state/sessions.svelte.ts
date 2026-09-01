import type { ProjectView, SessionEvent, SessionsConnState, SessionView } from "../types";

export const sessionsState = $state({
  conn: "idle" as SessionsConnState,
  projects: [] as ProjectView[],
  sessions: {} as Record<string, SessionView>,
  selected: null as string | null,
  lastSeq: 0,
  /// Shell-originated: session id -> the message from its last `pty.stream_error`.
  streamErrors: {} as Record<string, string>,
});

/// Wholesale replace of both lists AND lastSeq = 0: a reconnected daemon numbers
/// events from a fresh bus, so the old floor would drop everything.
export function setLists(projects: ProjectView[], sessions: SessionView[]): void {
  sessionsState.projects = projects;
  const byId: Record<string, SessionView> = {};
  for (const s of sessions) byId[s.id] = s;
  sessionsState.sessions = byId;
  sessionsState.lastSeq = 0;
}

/// Git-poll refresh (spec §4.3): only the four derived git fields move, and only
/// on rows the poll still names — a poll row's `state` is never live, so it must
/// not overwrite the row's actual state.
export function applyGitPoll(views: SessionView[]): void {
  const seen = new Set<string>();
  for (const v of views) {
    seen.add(v.id);
    const row = sessionsState.sessions[v.id];
    if (row === undefined) {
      sessionsState.sessions[v.id] = v;
      continue;
    }
    row.branch = v.branch;
    row.changed_files = v.changed_files;
    row.ahead = v.ahead;
    row.worktree_status = v.worktree_status;
  }
  for (const id of Object.keys(sessionsState.sessions)) {
    // oxlint-disable-next-line no-dynamic-delete -- keyed rune map, ids are poll-bounded
    if (!seen.has(id)) delete sessionsState.sessions[id];
  }
}

/// Mid-stream refresh after a `session.*`/`project.*` event. Unlike setLists it
/// keeps the dedup floor — the bus that numbered the triggering event is still the
/// one we are reading — and puts the sessions through applyGitPoll, so a row's live
/// state survives a list snapshot the daemon took before the last state event.
export function refreshLists(projects: ProjectView[], sessions: SessionView[]): void {
  sessionsState.projects = projects;
  applyGitPoll(sessions);
}

export function clearStreamError(id: string): void {
  // oxlint-disable-next-line no-dynamic-delete -- keyed rune map, ids are session-bounded
  delete sessionsState.streamErrors[id];
}

/// Returns "refetch" when the event demands a list refetch (session.*/project.*),
/// null otherwise. Dedup by seq; reset lastSeq to 0 on every setLists.
export function applySessionEvent(ev: SessionEvent): "refetch" | null {
  if (ev.type === "pty.stream_error") {
    // Shell-originated; carries no seq, so it bypasses the dedup floor below.
    sessionsState.streamErrors[ev.agent] = ev.message;
    return null;
  }
  if (ev.seq <= sessionsState.lastSeq) return null;
  sessionsState.lastSeq = ev.seq;
  switch (ev.type) {
    case "agent.state": {
      const row = sessionsState.sessions[ev.agent];
      if (row === undefined) return null;
      row.state = ev.state === "restarting" ? "starting" : ev.state;
      return null;
    }
    case "agent.lifecycle": {
      const row = sessionsState.sessions[ev.agent];
      if (row === undefined) return null;
      if (ev.phase === "exited") {
        row.state = "exited";
        row.exit = ev.exit;
      } else {
        row.state = "starting";
        row.exit = null;
      }
      return null;
    }
    case "agent.blocked": {
      const row = sessionsState.sessions[ev.agent];
      if (row === undefined) return null;
      row.blocked = ev.blocked ? { tool: ev.tool, since: ev.ts } : null;
      return null;
    }
    case "session.created":
    case "session.stopped":
    case "session.resumed":
    case "session.deleted":
      if (ev.type === "session.deleted" && sessionsState.selected === ev.agent) {
        sessionsState.selected = null;
      }
      return "refetch";
    case "project.registered":
    case "project.forgotten":
      return "refetch";
  }
}

export function blockedCount(): number {
  let n = 0;
  for (const row of Object.values(sessionsState.sessions)) {
    if (row.blocked !== null) n += 1;
  }
  return n;
}

export interface ProjectGroup { project: ProjectView; sessions: SessionView[] }

export function byProject(): ProjectGroup[] {
  const groups: ProjectGroup[] = [];
  for (const project of sessionsState.projects) {
    const sessions = Object.values(sessionsState.sessions).filter((s) => s.project === project.id);
    // oxlint-disable-next-line no-array-sort -- ES2022 lib; sorting a fresh filter() copy is safe
    sessions.sort((a, b) => b.created_at.localeCompare(a.created_at));
    groups.push({ project, sessions });
  }
  return groups;
}

export function selectSession(id: string | null): void {
  sessionsState.selected = id;
}
