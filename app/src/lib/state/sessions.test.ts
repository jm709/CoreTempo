import { beforeEach, describe, expect, test } from "vitest";
import type { ProjectView, SessionEvent, SessionView } from "../types";
import {
  applyGitPoll,
  applySessionEvent,
  blockedCount,
  byProject,
  selectSession,
  sessionsState,
  setLists,
} from "./sessions.svelte";

const proj = (id: string, name: string): ProjectView => ({
  id,
  path: `/home/u/${name}`,
  name,
  created_at: "2026-08-27T10:00:00Z",
});

const sess = (id: string, project: string, createdAt: string): SessionView => ({
  id,
  project,
  cwd: `/home/u/${project}`,
  worktree: null,
  title: id,
  claude_session_id: null,
  model: null,
  permission_mode: null,
  isolated_config: false,
  prompt: null,
  created_at: createdAt,
  stopped_at: null,
  state: "idle",
  blocked: null,
  exit: null,
  pty_cursor: 0,
  branch: null,
  changed_files: null,
  ahead: null,
  worktree_status: "none",
});

function resetStore(): void {
  sessionsState.conn = "idle";
  sessionsState.projects = [];
  sessionsState.sessions = {};
  sessionsState.selected = null;
  sessionsState.lastSeq = 0;
  sessionsState.streamErrors = {};
}

describe("setLists / byProject", () => {
  beforeEach(resetStore);

  test("populates projects and sessions, and groups newest-first per project", () => {
    const p1 = proj("p-1", "alpha");
    const p2 = proj("p-2", "beta");
    const s1 = sess("s-1", "p-1", "2026-08-27T10:00:00Z");
    const s2 = sess("s-2", "p-1", "2026-08-27T12:00:00Z");
    const s3 = sess("s-3", "p-2", "2026-08-27T09:00:00Z");
    setLists([p1, p2], [s1, s2, s3]);

    expect(sessionsState.projects).toEqual([p1, p2]);
    // oxlint-disable-next-line no-array-sort -- fresh Object.keys() copy, sorting in place is safe
    expect(Object.keys(sessionsState.sessions).sort()).toEqual(["s-1", "s-2", "s-3"]);

    const groups = byProject();
    expect(groups.map((g) => g.project.id)).toEqual(["p-1", "p-2"]);
    expect(groups[0]?.sessions.map((s) => s.id)).toEqual(["s-2", "s-1"]);
    expect(groups[1]?.sessions.map((s) => s.id)).toEqual(["s-3"]);
  });

  test("resets lastSeq to 0", () => {
    sessionsState.lastSeq = 42;
    setLists([], []);
    expect(sessionsState.lastSeq).toBe(0);
  });
});

describe("applySessionEvent: agent.state", () => {
  beforeEach(() => {
    resetStore();
    setLists([proj("p-1", "alpha")], [sess("s-1", "p-1", "2026-08-27T10:00:00Z")]);
  });

  test("maps restarting to starting and applies to the row", () => {
    const ev: SessionEvent = {
      seq: 1, ts: "2026-08-27T10:00:01Z", type: "agent.state", agent: "s-1", state: "restarting",
    };
    applySessionEvent(ev);
    expect(sessionsState.sessions["s-1"]?.state).toBe("starting");
  });

  test("applies a known state directly", () => {
    const ev: SessionEvent = {
      seq: 1, ts: "2026-08-27T10:00:01Z", type: "agent.state", agent: "s-1", state: "working",
    };
    applySessionEvent(ev);
    expect(sessionsState.sessions["s-1"]?.state).toBe("working");
  });

  test("unknown agent id is a no-op", () => {
    const ev: SessionEvent = {
      seq: 1, ts: "2026-08-27T10:00:01Z", type: "agent.state", agent: "s-missing", state: "working",
    };
    expect(() => applySessionEvent(ev)).not.toThrow();
    expect(sessionsState.sessions["s-missing"]).toBeUndefined();
  });

  test("stale seq (<= lastSeq) is dropped", () => {
    sessionsState.lastSeq = 5;
    const ev: SessionEvent = {
      seq: 5, ts: "2026-08-27T10:00:01Z", type: "agent.state", agent: "s-1", state: "working",
    };
    applySessionEvent(ev);
    expect(sessionsState.sessions["s-1"]?.state).toBe("idle"); // unchanged
  });
});

describe("applySessionEvent: agent.blocked / blockedCount", () => {
  beforeEach(() => {
    resetStore();
    setLists(
      [proj("p-1", "alpha")],
      [sess("s-1", "p-1", "2026-08-27T10:00:00Z"), sess("s-2", "p-1", "2026-08-27T11:00:00Z")],
    );
  });

  test("sets row.blocked on blocked: true", () => {
    const ev: SessionEvent = {
      seq: 1, ts: "2026-08-27T10:05:00Z", type: "agent.blocked",
      agent: "s-1", blocked: true, tool: "Bash",
    };
    applySessionEvent(ev);
    expect(sessionsState.sessions["s-1"]?.blocked).toEqual({
      tool: "Bash", since: "2026-08-27T10:05:00Z",
    });
    expect(blockedCount()).toBe(1);
  });

  test("clears row.blocked on blocked: false", () => {
    sessionsState.sessions["s-1"]!.blocked = { tool: "Bash", since: "2026-08-27T10:05:00Z" };
    const ev: SessionEvent = {
      seq: 1, ts: "2026-08-27T10:06:00Z", type: "agent.blocked",
      agent: "s-1", blocked: false, tool: null,
    };
    applySessionEvent(ev);
    expect(sessionsState.sessions["s-1"]?.blocked).toBeNull();
    expect(blockedCount()).toBe(0);
  });
});

describe("applySessionEvent: agent.lifecycle", () => {
  beforeEach(() => {
    resetStore();
    setLists([proj("p-1", "alpha")], [sess("s-1", "p-1", "2026-08-27T10:00:00Z")]);
  });

  test("phase exited sets state exited and exit", () => {
    const ev: SessionEvent = {
      seq: 1, ts: "2026-08-27T10:05:00Z", type: "agent.lifecycle",
      agent: "s-1", phase: "exited", exit: { code: 1 },
    };
    applySessionEvent(ev);
    expect(sessionsState.sessions["s-1"]?.state).toBe("exited");
    expect(sessionsState.sessions["s-1"]?.exit).toEqual({ code: 1 });
  });
});

describe("applySessionEvent: session.* / project.*", () => {
  beforeEach(() => {
    resetStore();
    setLists([proj("p-1", "alpha")], [sess("s-1", "p-1", "2026-08-27T10:00:00Z")]);
  });

  test("session.deleted returns refetch and clears selected when it names it", () => {
    selectSession("s-1");
    const ev: SessionEvent = {
      seq: 1, ts: "2026-08-27T10:05:00Z", type: "session.deleted", agent: "s-1",
    };
    expect(applySessionEvent(ev)).toBe("refetch");
    expect(sessionsState.selected).toBeNull();
  });

  test("session.deleted leaves a different selection alone", () => {
    selectSession("s-2");
    const ev: SessionEvent = {
      seq: 1, ts: "2026-08-27T10:05:00Z", type: "session.deleted", agent: "s-1",
    };
    expect(applySessionEvent(ev)).toBe("refetch");
    expect(sessionsState.selected).toBe("s-2");
  });

  test("session.created returns refetch", () => {
    const ev: SessionEvent = {
      seq: 1, ts: "2026-08-27T10:05:00Z", type: "session.created", agent: "s-9",
    };
    expect(applySessionEvent(ev)).toBe("refetch");
  });

  test("project.registered returns refetch", () => {
    const ev: SessionEvent = { seq: 1, ts: "2026-08-27T10:05:00Z", type: "project.registered" };
    expect(applySessionEvent(ev)).toBe("refetch");
  });
});

describe("applySessionEvent: pty.stream_error", () => {
  beforeEach(() => {
    resetStore();
    setLists([proj("p-1", "alpha")], [sess("s-1", "p-1", "2026-08-27T10:00:00Z")]);
  });

  test("stores the message keyed by agent, bypassing seq dedup", () => {
    sessionsState.lastSeq = 999; // any normal event would be dropped
    const ev: SessionEvent = {
      type: "pty.stream_error", agent: "s-1", message: "could not open the PTY stream",
    };
    expect(applySessionEvent(ev)).toBeNull();
    expect(sessionsState.streamErrors["s-1"]).toBe("could not open the PTY stream");
  });

  test("clearStreamError removes it", async () => {
    const { clearStreamError } = await import("./sessions.svelte");
    sessionsState.streamErrors["s-1"] = "boom";
    clearStreamError("s-1");
    expect(sessionsState.streamErrors["s-1"]).toBeUndefined();
  });
});

describe("applyGitPoll", () => {
  beforeEach(() => {
    resetStore();
    setLists(
      [proj("p-1", "alpha")],
      [sess("s-1", "p-1", "2026-08-27T10:00:00Z"), sess("s-2", "p-1", "2026-08-27T11:00:00Z")],
    );
  });

  test("overwrites only the four git fields on existing rows, ignoring a stale state", () => {
    sessionsState.sessions["s-1"]!.state = "working"; // live state, must survive the poll
    const polled = sess("s-1", "p-1", "2026-08-27T10:00:00Z");
    polled.state = "idle"; // stale — the poll's view of state, must NOT overwrite
    polled.branch = "session/brisk-otter";
    polled.changed_files = 3;
    polled.ahead = 2;
    polled.worktree_status = "present";
    applyGitPoll([polled, sess("s-2", "p-1", "2026-08-27T11:00:00Z")]);

    const row = sessionsState.sessions["s-1"];
    expect(row?.state).toBe("working"); // untouched
    expect(row?.branch).toBe("session/brisk-otter");
    expect(row?.changed_files).toBe(3);
    expect(row?.ahead).toBe(2);
    expect(row?.worktree_status).toBe("present");
  });

  test("adds unknown rows wholesale", () => {
    const s3 = sess("s-3", "p-1", "2026-08-27T12:00:00Z");
    applyGitPoll([
      sess("s-1", "p-1", "2026-08-27T10:00:00Z"),
      sess("s-2", "p-1", "2026-08-27T11:00:00Z"),
      s3,
    ]);
    expect(sessionsState.sessions["s-3"]).toEqual(s3);
  });

  test("removes rows absent from the poll", () => {
    applyGitPoll([sess("s-1", "p-1", "2026-08-27T10:00:00Z")]);
    expect(sessionsState.sessions["s-2"]).toBeUndefined();
    expect(sessionsState.sessions["s-1"]).toBeDefined();
  });
});

describe("selectSession", () => {
  beforeEach(resetStore);

  test("sets and clears the selection", () => {
    selectSession("s-1");
    expect(sessionsState.selected).toBe("s-1");
    selectSession(null);
    expect(sessionsState.selected).toBeNull();
  });
});
