import { describe, expect, it } from "vitest";
import { cardActions, cardLine2, resumeDisabled } from "./railHelpers";
import type { SessionState, SessionView } from "../types";

function makeSession(overrides: Partial<SessionView> = {}): SessionView {
  return {
    id: "s1", project: "p1", cwd: "/home/user/project", worktree: null,
    title: "fix the thing", claude_session_id: null, model: null,
    permission_mode: null, isolated_config: false, prompt: null,
    created_at: "2026-09-01T00:00:00Z", stopped_at: null,
    state: "idle", blocked: null, exit: null, pty_cursor: 0,
    branch: null, changed_files: null, ahead: null, worktree_status: "none",
    ...overrides,
  };
}

describe("cardLine2", () => {
  it("uses the branch when set", () => {
    expect(cardLine2(makeSession({ branch: "session/fix-thing" }))).toBe("session/fix-thing");
  });

  it("falls back to the last path segment of cwd when there is no branch", () => {
    expect(cardLine2(makeSession({ branch: null, cwd: "/home/user/project" }))).toBe("project");
  });

  it("omits counts when both are zero or null", () => {
    expect(cardLine2(makeSession({ branch: "main", changed_files: 0, ahead: 0 }))).toBe("main");
    expect(cardLine2(makeSession({ branch: "main", changed_files: null, ahead: null })))
      .toBe("main");
  });

  it("appends ±N and ↑N with exact spacing when set", () => {
    expect(cardLine2(makeSession({ branch: "main", changed_files: 2, ahead: 1 })))
      .toBe("main ±2 ↑1");
    expect(cardLine2(makeSession({ branch: "main", changed_files: 3, ahead: 0 })))
      .toBe("main ±3");
    expect(cardLine2(makeSession({ branch: "main", changed_files: 0, ahead: 4 })))
      .toBe("main ↑4");
  });
});

describe("cardActions", () => {
  const cases: [SessionState, ("stop" | "resume" | "rm")[]][] = [
    ["starting", ["stop"]],
    ["idle", ["stop"]],
    ["working", ["stop"]],
    ["stopped", ["resume", "rm"]],
    ["exited", ["resume", "rm"]],
  ];
  for (const [state, expected] of cases) {
    it(`returns ${JSON.stringify(expected)} for ${state}`, () => {
      expect(cardActions(makeSession({ state }))).toEqual(expected);
    });
  }
});

describe("resumeDisabled", () => {
  it("flags a missing worktree with the reason", () => {
    expect(resumeDisabled(makeSession({ worktree_status: "missing" })))
      .toBe("worktree is gone; rm (delete) is the valid action");
  });

  it("is enabled (null) for present or none", () => {
    expect(resumeDisabled(makeSession({ worktree_status: "present" }))).toBeNull();
    expect(resumeDisabled(makeSession({ worktree_status: "none" }))).toBeNull();
  });
});
