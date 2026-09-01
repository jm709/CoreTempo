import { pausePty, resizePty, subscribePty, writePty } from "../ipc";
import {
  pauseSessionPty, resizeSessionPty, subscribeSessionPty, writeSessionPty,
} from "../ipcSessions";
import { createTerminalManager } from "./manager";

/// Run-mode agents. `resume` has no meaning here: `subscribe_pty` replays from
/// the ring cursor the caller passes.
export const workflowTerm = createTerminalManager({
  subscribe: (id, since, _resume, onChunk) => subscribePty(id, since, onChunk),
  write: writePty,
  resize: resizePty,
  pause: pausePty,
});

/// Sessions. The daemon's stream endpoint has no cursor; `resume` is what
/// replays the buffered output after a suspend.
export const sessionTerm = createTerminalManager({
  subscribe: (id, _since, resume, onChunk) => subscribeSessionPty(id, resume, onChunk),
  write: writeSessionPty,
  resize: resizeSessionPty,
  pause: pauseSessionPty,
});
