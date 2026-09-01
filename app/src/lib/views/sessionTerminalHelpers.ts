import { exitLabel } from "../format";
import { clearStreamError } from "../state/sessions.svelte";
import { sessionTerm } from "../term/instances";
import type { SessionView } from "../types";

/// Sessions run long and are read back by hand; the run-mode figure is per-workflow
/// (`RunInfo.scrollback`), sessions have no such knob. Shared with sessionsWire's
/// re-ensure after a reconnect so both open the same buffer.
export const SESSION_SCROLLBACK = 5000;

/// The overlay text for a session with no live process, or null while it has one.
export function bannerFor(s: SessionView): string | null {
  if (s.state === "stopped") return "stopped";
  if (s.state === "exited") return exitLabel(s.exit);
  return null;
}

/// Show `id` in the sessions center: a terminal we already hold only needs its
/// stream back (its screen survived the suspend); a new one is built from scratch.
export async function openSelected(id: string, pane: HTMLElement): Promise<void> {
  if (sessionTerm.has(id)) await sessionTerm.resumeStream(id);
  else await sessionTerm.ensure(id, null, SESSION_SCROLLBACK);
  sessionTerm.attach(id, pane);
}

/// The `retry` action on the stream-error bar. The old terminal is discarded
/// rather than resumed: the error means its subscription is gone, and its screen
/// stops where the stream died, so a fresh one replaying the daemon's buffer is
/// the only way back to the truth.
export async function retryStream(id: string, pane: HTMLElement | null): Promise<void> {
  clearStreamError(id);
  sessionTerm.dispose(id);
  await sessionTerm.ensure(id, null, SESSION_SCROLLBACK);
  if (pane !== null) sessionTerm.attach(id, pane);
}
