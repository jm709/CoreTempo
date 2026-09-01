import { Channel, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { toCmdError } from "./ipc";
import type {
  CreateSessionRequest, DeleteSessionResponse, ProjectView, ResumeResponse, SessionEvent,
  SessionsConnState, SessionView,
} from "./types";

export { toCmdError };

export const SESSIONS_STATUS_EVENT = "coretempo:sessions-status";
export const SESSION_EVENT = "coretempo:session-event";

/// `sessions_status`'s connection state is the full four-value union (the
/// command can observe `idle`, before the supervisor is even spawned); the
/// `coretempo:sessions-status` event it emits afterward never carries `idle`
/// (`announce` in the shell's supervisor only fires for starting/connected/
/// unreachable) — see `onSessionsStatus` below.
export interface SessionsStatusView {
  state: SessionsConnState;
  health: unknown;
}

export function sessionsStatus(): Promise<SessionsStatusView> {
  return invoke("sessions_status");
}

export function sessionList(): Promise<SessionView[]> {
  return invoke("session_list");
}

export function sessionCreate(req: CreateSessionRequest): Promise<SessionView> {
  return invoke("session_create", { req });
}

export function sessionStop(session: string): Promise<SessionView> {
  return invoke("session_stop", { session });
}

export function sessionResume(session: string): Promise<ResumeResponse> {
  return invoke("session_resume", { session });
}

export function sessionDelete(
  session: string,
  removeWorktree: boolean,
  force: boolean,
): Promise<DeleteSessionResponse> {
  return invoke("session_delete", { session, remove_worktree: removeWorktree, force });
}

export function projectList(): Promise<ProjectView[]> {
  return invoke("project_list");
}

export function projectRegister(path: string, name?: string): Promise<ProjectView> {
  return invoke("project_register", { path, name: name ?? null });
}

export function projectForget(project: string): Promise<void> {
  return invoke("project_forget", { project });
}

/// PTY bytes: same wire form as run mode's `subscribePty` (contracts §8.2), plus
/// the `resume` flag the sessions daemon's stream endpoint takes to replay
/// buffered output. Returns a detach fn for the same reason `subscribePty` does.
export async function subscribeSessionPty(
  session: string,
  resume: boolean,
  onChunk: (bytes: Uint8Array) => void,
): Promise<() => void> {
  const channel = new Channel<ArrayBuffer>();
  // oxlint-disable-next-line unicorn/prefer-add-event-listener
  channel.onmessage = (buf) => {
    onChunk(new Uint8Array(buf));
  };
  try {
    await invoke("session_subscribe_pty", { session, resume, channel });
  } catch (e) {
    // oxlint-disable-next-line unicorn/prefer-add-event-listener
    channel.onmessage = () => {};
    throw e;
  }
  return () => {
    // oxlint-disable-next-line unicorn/prefer-add-event-listener
    channel.onmessage = () => {};
    void invoke("session_unsubscribe_pty", { session });
  };
}

export function writeSessionPty(session: string, data: Uint8Array): Promise<void> {
  return invoke("session_write_pty", { session, data: Array.from(data) });
}

export function resizeSessionPty(session: string, cols: number, rows: number): Promise<void> {
  return invoke("session_resize_pty", { session, cols, rows });
}

export function pauseSessionPty(session: string, paused: boolean): Promise<void> {
  return invoke("session_pause_pty", { session, paused });
}

export async function onSessionEvent(cb: (ev: SessionEvent) => void): Promise<() => void> {
  return await listen<SessionEvent>(SESSION_EVENT, (e) => {
    cb(e.payload);
  });
}

/// The event payload is the 3-state subset the shell's `announce` actually
/// emits (`starting | connected | unreachable`) — narrower than
/// `SessionsStatusView.state`, which the `sessions_status` command can also
/// report as `idle` before a supervisor exists.
export async function onSessionsStatus(
  cb: (s: { state: "starting" | "connected" | "unreachable" }) => void,
): Promise<() => void> {
  return await listen<{ state: "starting" | "connected" | "unreachable" }>(
    SESSIONS_STATUS_EVENT,
    (e) => {
      cb(e.payload);
    },
  );
}
