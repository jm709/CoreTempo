import { beforeEach, describe, expect, it, vi } from "vitest";

// vi.mock is hoisted above every const in the module, so the double the factory
// returns has to be hoisted with it.
const term = vi.hoisted(() => ({
  ensure: vi.fn(async () => {}),
  attach: vi.fn(),
  resumeStream: vi.fn(async () => {}),
  suspend: vi.fn(),
  has: vi.fn(() => false),
  dispose: vi.fn(),
}));

vi.mock("../term/instances", () => ({ sessionTerm: term }));

import { sessionsState } from "../state/sessions.svelte";
import type { AgentExit, SessionState, SessionView } from "../types";
import {
  bannerFor, openSelected, retryStream, SESSION_SCROLLBACK, syncSelection,
} from "./sessionTerminalHelpers";

function makeSession(state: SessionState, exit: AgentExit | null = null): SessionView {
  return {
    id: "s-1", project: "p-1", cwd: "/home/u/repo", worktree: null, title: "fix it",
    claude_session_id: null, model: null, permission_mode: null, isolated_config: false,
    prompt: null, created_at: "2026-09-01T00:00:00Z", stopped_at: null,
    state, blocked: null, exit, pty_cursor: 0,
    branch: null, changed_files: null, ahead: null, worktree_status: "none",
  };
}

describe("bannerFor", () => {
  const live: SessionState[] = ["starting", "idle", "working"];
  for (const state of live) {
    it(`is null while ${state}`, () => {
      expect(bannerFor(makeSession(state))).toBeNull();
    });
  }

  it("reads 'stopped' for a stopped session", () => {
    expect(bannerFor(makeSession("stopped"))).toBe("stopped");
  });

  it("reads the exit label for an exited session", () => {
    expect(bannerFor(makeSession("exited", { code: 3 }))).toBe("exited 3");
  });

  it("names the signal that killed an exited session", () => {
    expect(bannerFor(makeSession("exited", { signal: "Terminated" }))).toBe("killed: Terminated");
  });

  it("falls back to 'exited ?' when the snapshot has no exit yet", () => {
    expect(bannerFor(makeSession("exited", null))).toBe("exited ?");
  });
});

describe("openSelected", () => {
  const pane = {} as HTMLElement;

  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("ensures a terminal it has never seen, then attaches it to the pane", async () => {
    term.has.mockReturnValue(false);
    await openSelected("s-1", pane);
    expect(term.ensure).toHaveBeenCalledWith("s-1", null, SESSION_SCROLLBACK);
    expect(term.resumeStream).not.toHaveBeenCalled();
    expect(term.attach).toHaveBeenCalledWith("s-1", pane);
  });

  it("resumes the stream of a terminal it already holds instead of re-ensuring", async () => {
    term.has.mockReturnValue(true);
    await openSelected("s-1", pane);
    expect(term.resumeStream).toHaveBeenCalledWith("s-1");
    expect(term.ensure).not.toHaveBeenCalled();
    expect(term.attach).toHaveBeenCalledWith("s-1", pane);
  });
});

describe("syncSelection", () => {
  const pane = {} as HTMLElement;

  beforeEach(() => {
    vi.clearAllMocks();
    term.has.mockReturnValue(false);
  });

  it("suspends the terminal being left and opens the one arriving", async () => {
    expect(await syncSelection("s-1", "s-2", pane, true)).toBe("s-2");
    expect(term.suspend).toHaveBeenCalledWith("s-1");
    expect(term.ensure).toHaveBeenCalledWith("s-2", null, SESSION_SCROLLBACK);
    expect(term.attach).toHaveBeenCalledWith("s-2", pane);
  });

  it("suspends nothing when the selection has not moved", async () => {
    term.has.mockReturnValue(true);
    await syncSelection("s-1", "s-1", pane, true);
    expect(term.suspend).not.toHaveBeenCalled();
    expect(term.resumeStream).toHaveBeenCalledWith("s-1");
  });

  it("still suspends the old terminal when the selection is cleared", async () => {
    expect(await syncSelection("s-1", null, pane, true)).toBeNull();
    expect(term.suspend).toHaveBeenCalledWith("s-1");
    expect(term.ensure).not.toHaveBeenCalled();
  });

  it("opens nothing while the daemon is unreachable", async () => {
    await syncSelection("s-1", "s-2", pane, false);
    expect(term.suspend).toHaveBeenCalledWith("s-1");
    expect(term.ensure).not.toHaveBeenCalled();
    expect(term.attach).not.toHaveBeenCalled();
  });

  it("opens nothing before the pane is mounted", async () => {
    await syncSelection(null, "s-1", null, true);
    expect(term.ensure).not.toHaveBeenCalled();
  });
});

describe("retryStream", () => {
  const pane = {} as HTMLElement;

  beforeEach(() => {
    vi.clearAllMocks();
    sessionsState.streamErrors = {};
  });

  it("clears the error and resubscribes from a fresh terminal", async () => {
    sessionsState.streamErrors["s-1"] = "the PTY stream ended";
    term.has.mockReturnValue(true); // stale entry: retry must not resume it
    await retryStream("s-1", pane);
    expect(sessionsState.streamErrors["s-1"]).toBeUndefined();
    expect(term.dispose).toHaveBeenCalledWith("s-1");
    expect(term.ensure).toHaveBeenCalledWith("s-1", null, SESSION_SCROLLBACK);
    expect(term.resumeStream).not.toHaveBeenCalled();
    expect(term.attach).toHaveBeenCalledWith("s-1", pane);
  });

  it("skips the attach when the pane is not mounted", async () => {
    await retryStream("s-1", null);
    expect(term.ensure).toHaveBeenCalledWith("s-1", null, SESSION_SCROLLBACK);
    expect(term.attach).not.toHaveBeenCalled();
  });
});
