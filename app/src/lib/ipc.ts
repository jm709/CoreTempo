import { Channel, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  CmdError, Event, MessageKind, MessageRecord, ParseReport, RunInfo, Snapshot, WorkflowModel,
} from "./types";

export const CORE_EVENT = "coretempo:event";

export function isCmdError(e: unknown): e is CmdError {
  return typeof e === "object" && e !== null && "code" in e && "message" in e;
}

export function toCmdError(e: unknown): CmdError {
  return isCmdError(e) ? e : { code: "internal", message: String(e) };
}

export function snapshot(): Promise<Snapshot> {
  return invoke("snapshot");
}

export function runStart(configPath: string): Promise<RunInfo> {
  // Wire arg keys are snake_case: the shell declares rename_all = "snake_case".
  return invoke("run_start", { config_path: configPath });
}

export function runStop(): Promise<void> {
  return invoke("run_stop");
}

export function restartAgent(agent: string): Promise<void> {
  return invoke("restart_agent", { agent });
}

/// PTY bytes: contracts §8.2 — Channel receives InvokeResponseBody::Raw, i.e. an
/// ArrayBuffer in JS. Wrap in Uint8Array and hand straight to term.write (no decode).
/// Returns a detach fn: Tauri's global callback map strongly references the channel
/// closure, which would otherwise pin the terminal (and its buffers) until backend
/// teardown completes.
export async function subscribePty(
  agent: string,
  sinceCursor: number | null,
  onChunk: (bytes: Uint8Array) => void,
): Promise<() => void> {
  const channel = new Channel<ArrayBuffer>();
  // Tauri Channel is not an EventTarget; `onmessage` is its only handler API.
  // oxlint-disable-next-line unicorn/prefer-add-event-listener
  channel.onmessage = (buf) => {
    onChunk(new Uint8Array(buf));
  };
  try {
    await invoke("subscribe_pty", { agent, since_cursor: sinceCursor, channel });
  } catch (e) {
    // A rejected subscribe leaves no detach fn for the caller to run, so clear
    // the handler here rather than leave the closure registered.
    // oxlint-disable-next-line unicorn/prefer-add-event-listener
    channel.onmessage = () => {};
    throw e;
  }
  return () => {
    // oxlint-disable-next-line unicorn/prefer-add-event-listener
    channel.onmessage = () => {};
  };
}

export function writePty(agent: string, data: Uint8Array): Promise<void> {
  // Keystrokes are tiny; JSON-safe number[] avoids typed-array serialization pitfalls.
  return invoke("write_pty", { agent, data: Array.from(data) });
}

export function resizePty(agent: string, cols: number, rows: number): Promise<void> {
  return invoke("resize_pty", { agent, cols, rows });
}

export function pausePty(agent: string, paused: boolean): Promise<void> {
  return invoke("pause_pty", { agent, paused });
}

export function workflowOpen(path: string): Promise<string> {
  return invoke("workflow_open", { path });
}

export function workflowSave(path: string, text: string): Promise<void> {
  return invoke("workflow_save", { path, text });
}

export function workflowParse(text: string): Promise<ParseReport> {
  return invoke("workflow_parse", { text });
}

export function workflowMerge(text: string, model: WorkflowModel): Promise<string> {
  return invoke("workflow_merge", { text, model });
}

export function sendChat(to: string, kind: MessageKind, body: string): Promise<MessageRecord> {
  return invoke("send_chat", { to, kind, body });
}

export async function onCoreEvent(handler: (ev: Event) => void): Promise<() => void> {
  return await listen<Event>(CORE_EVENT, (e) => {
    handler(e.payload);
  });
}
