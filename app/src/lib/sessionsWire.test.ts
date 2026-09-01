import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ProjectView, SessionEvent, SessionView } from "./types";

// vi.mock hoists above every const, so both doubles are hoisted with it. `captured`
// holds the callbacks the wire hands to the listener registrars, which is how a
// test plays a daemon event or a status announcement at it.
const h = vi.hoisted(() => {
  const captured: {
    event: ((ev: unknown) => void) | null;
    status: ((s: { state: string }) => void) | null;
  } = { event: null, status: null };
  return {
    captured,
    ipc: {
      sessionsStatus: vi.fn(async () => ({ state: "idle", health: null })),
      sessionList: vi.fn(async (): Promise<SessionView[]> => []),
      projectList: vi.fn(async (): Promise<ProjectView[]> => []),
      onSessionEvent: vi.fn(async (cb: (ev: unknown) => void) => {
        captured.event = cb;
        return () => {};
      }),
      onSessionsStatus: vi.fn(async (cb: (s: { state: string }) => void) => {
        captured.status = cb;
        return () => {};
      }),
      toCmdError: (e: unknown) => e as { code: string; message: string },
    },
    term: {
      ensure: vi.fn(async () => {}),
      attach: vi.fn(),
      suspend: vi.fn(),
      resumeStream: vi.fn(async () => {}),
      has: vi.fn(() => false),
      dispose: vi.fn(),
      disposeAll: vi.fn(),
    },
  };
});

vi.mock("./ipcSessions", () => h.ipc);
vi.mock("./term/instances", () => ({ sessionTerm: h.term }));

type Wire = typeof import("./sessionsWire");
type Store = typeof import("./state/sessions.svelte");
type Ui = typeof import("./state/ui.svelte");

let wire: Wire;
let store: Store;
let ui: Ui;

const proj = (id: string): ProjectView => ({
  id, path: `/home/u/${id}`, name: id, created_at: "2026-09-01T00:00:00Z",
});

const sess = (id: string, overrides: Partial<SessionView> = {}): SessionView => ({
  id, project: "p-1", cwd: "/home/u/p-1", worktree: null, title: id,
  claude_session_id: null, model: null, permission_mode: null, isolated_config: false,
  prompt: null, created_at: "2026-09-01T00:00:00Z", stopped_at: null,
  state: "idle", blocked: null, exit: null, pty_cursor: 0,
  branch: null, changed_files: null, ahead: null, worktree_status: "none",
  ...overrides,
});

/// Flushes the microtask the refetch debounce parks on, whether or not the fake
/// clock also owns `queueMicrotask`.
async function flush(): Promise<void> {
  await vi.advanceTimersByTimeAsync(0);
}

/// The wire keeps `started`/`lastConn` in module state, so every test needs its own
/// copy of the module — and of the store it writes, or the two would disagree.
/// Fake timers throughout: `useRealTimers` then discards the poll interval that
/// `enterSessionsMode` starts, which would otherwise outlive its module.
beforeEach(async () => {
  vi.clearAllMocks();
  vi.resetModules();
  vi.useFakeTimers();
  h.captured.event = null;
  h.captured.status = null;
  h.ipc.sessionsStatus.mockResolvedValue({ state: "idle", health: null });
  h.ipc.projectList.mockResolvedValue([]);
  h.ipc.sessionList.mockResolvedValue([]);
  [wire, store, ui] = await Promise.all([
    import("./sessionsWire"),
    import("./state/sessions.svelte"),
    import("./state/ui.svelte"),
  ]);
});

afterEach(() => {
  vi.useRealTimers();
});

describe("enterSessionsMode", () => {
  it("registers both listeners and asks for the status once", async () => {
    await wire.enterSessionsMode();
    expect(h.ipc.onSessionsStatus).toHaveBeenCalledTimes(1);
    expect(h.ipc.onSessionEvent).toHaveBeenCalledTimes(1);
    expect(h.ipc.sessionsStatus).toHaveBeenCalledTimes(1);
  });

  it("is idempotent: a second entry adds no listener and no status call", async () => {
    await wire.enterSessionsMode();
    await wire.enterSessionsMode();
    expect(h.ipc.onSessionsStatus).toHaveBeenCalledTimes(1);
    expect(h.ipc.onSessionEvent).toHaveBeenCalledTimes(1);
    expect(h.ipc.sessionsStatus).toHaveBeenCalledTimes(1);
  });

  it("retries registration after a first entry that failed to register", async () => {
    // Leaving `started` set on a failed registration bricks sessions mode for the
    // life of the app: no listeners, no poll, and every later switch returns early.
    const quiet = vi.spyOn(console, "error").mockImplementation(() => {});
    h.ipc.onSessionsStatus.mockRejectedValueOnce(new Error("listen failed"));

    await wire.enterSessionsMode();
    expect(h.ipc.onSessionEvent).not.toHaveBeenCalled();
    expect(h.ipc.sessionsStatus).not.toHaveBeenCalled();
    // The connection never happened, so the topbar must say so rather than sit on
    // "starting…" for as long as the operator stays in the mode.
    expect(store.sessionsState.conn).toBe("unreachable");

    await wire.enterSessionsMode();
    expect(h.ipc.onSessionsStatus).toHaveBeenCalledTimes(2);
    expect(h.ipc.onSessionEvent).toHaveBeenCalledTimes(1);
    expect(h.ipc.sessionsStatus).toHaveBeenCalledTimes(1);
    quiet.mockRestore();
  });

  it("registers each listener once across a retry", async () => {
    const quiet = vi.spyOn(console, "error").mockImplementation(() => {});
    h.ipc.sessionsStatus.mockRejectedValueOnce(new Error("daemon spawn failed"));
    await wire.enterSessionsMode();
    await wire.enterSessionsMode();
    expect(h.ipc.onSessionsStatus).toHaveBeenCalledTimes(1);
    expect(h.ipc.onSessionEvent).toHaveBeenCalledTimes(1);
    expect(h.ipc.sessionsStatus).toHaveBeenCalledTimes(2); // the connect is retried
    quiet.mockRestore();
  });

  it("applies the status the command reports", async () => {
    h.ipc.sessionsStatus.mockResolvedValue({ state: "starting", health: null });
    await wire.enterSessionsMode();
    expect(store.sessionsState.conn).toBe("starting");
  });

  it("routes a status announcement through the same reducer", async () => {
    h.ipc.projectList.mockResolvedValue([proj("p-1")]);
    await wire.enterSessionsMode();
    h.captured.status?.({ state: "connected" });
    await flush();
    expect(store.sessionsState.conn).toBe("connected");
    expect(store.sessionsState.projects).toEqual([proj("p-1")]);
  });
});

describe("onStatus", () => {
  it("fetches both lists on connected and applies them", async () => {
    h.ipc.projectList.mockResolvedValue([proj("p-1")]);
    h.ipc.sessionList.mockResolvedValue([sess("s-1")]);
    store.sessionsState.lastSeq = 42;

    await wire.onStatus({ state: "connected" });

    expect(store.sessionsState.conn).toBe("connected");
    expect(store.sessionsState.projects).toEqual([proj("p-1")]);
    expect(store.sessionsState.sessions["s-1"]).toBeDefined();
    expect(store.sessionsState.lastSeq).toBe(0); // fresh bus: the old floor is gone
  });

  it("fetches nothing while starting or unreachable", async () => {
    await wire.onStatus({ state: "starting" });
    await wire.onStatus({ state: "unreachable" });
    expect(h.ipc.sessionList).not.toHaveBeenCalled();
    expect(h.ipc.projectList).not.toHaveBeenCalled();
    expect(store.sessionsState.conn).toBe("unreachable");
  });

  it("drops every terminal when connected follows a drop", async () => {
    await wire.onStatus({ state: "unreachable" });
    await wire.onStatus({ state: "connected" });
    expect(h.term.disposeAll).toHaveBeenCalledTimes(1);
  });

  it("drops them before conn goes connected, so the reopen sees an empty manager", async () => {
    let connAtDispose: string | null = null;
    h.term.disposeAll.mockImplementation(() => {
      connAtDispose = store.sessionsState.conn;
    });
    await wire.onStatus({ state: "unreachable" });
    await wire.onStatus({ state: "connected" });
    expect(connAtDispose).toBe("unreachable");
  });

  it("keeps the terminals when connected is not preceded by a drop", async () => {
    await wire.onStatus({ state: "starting" });
    await wire.onStatus({ state: "connected" });
    expect(h.term.disposeAll).not.toHaveBeenCalled();
  });
});

describe("session events", () => {
  beforeEach(async () => {
    await wire.enterSessionsMode();
    store.setLists([proj("p-1")], [sess("s-1")]);
  });

  it("applies a state event to the store", () => {
    const ev: SessionEvent = {
      seq: 1, ts: "2026-09-01T00:00:01Z", type: "agent.state", agent: "s-1", state: "working",
    };
    h.captured.event?.(ev);
    expect(store.sessionsState.sessions["s-1"]?.state).toBe("working");
  });

  it("refetches both lists once for two refetch verdicts in the same tick", async () => {
    h.ipc.sessionList.mockResolvedValue([sess("s-1"), sess("s-2")]);
    h.captured.event?.({
      seq: 1, ts: "2026-09-01T00:00:01Z", type: "session.created", agent: "s-2",
    } satisfies SessionEvent);
    h.captured.event?.({
      seq: 2, ts: "2026-09-01T00:00:02Z", type: "session.created", agent: "s-3",
    } satisfies SessionEvent);
    await flush();

    expect(h.ipc.sessionList).toHaveBeenCalledTimes(1);
    expect(h.ipc.projectList).toHaveBeenCalledTimes(1);
    expect(store.sessionsState.sessions["s-2"]).toBeDefined();
  });

  it("keeps the dedup floor across a refetch", async () => {
    h.captured.event?.({
      seq: 7, ts: "2026-09-01T00:00:01Z", type: "session.created", agent: "s-2",
    } satisfies SessionEvent);
    await flush();
    expect(store.sessionsState.lastSeq).toBe(7);
  });

  it("leaves a live state alone when the refetched row is stale", async () => {
    h.captured.event?.({
      seq: 1, ts: "2026-09-01T00:00:01Z", type: "agent.state", agent: "s-1", state: "working",
    } satisfies SessionEvent);
    h.ipc.sessionList.mockResolvedValue([sess("s-1", { state: "idle", branch: "main" })]);
    h.captured.event?.({
      seq: 2, ts: "2026-09-01T00:00:02Z", type: "session.created", agent: "s-2",
    } satisfies SessionEvent);
    await flush();

    expect(store.sessionsState.sessions["s-1"]?.branch).toBe("main");
    expect(store.sessionsState.sessions["s-1"]?.state).toBe("working");
  });
});

describe("git poll", () => {
  async function enterConnected(): Promise<void> {
    h.ipc.sessionsStatus.mockResolvedValue({ state: "connected", health: null });
    h.ipc.sessionList.mockResolvedValue([sess("s-1")]);
    await wire.enterSessionsMode();
    h.ipc.sessionList.mockClear();
  }

  it("polls the session list every 5 s while connected in sessions mode", async () => {
    ui.uiState.mode = "sessions";
    await enterConnected();
    h.ipc.sessionList.mockResolvedValue([sess("s-1", { branch: "session/otter", ahead: 2 })]);

    await vi.advanceTimersByTimeAsync(5000);
    expect(h.ipc.sessionList).toHaveBeenCalledTimes(1);
    expect(store.sessionsState.sessions["s-1"]?.branch).toBe("session/otter");
    expect(store.sessionsState.sessions["s-1"]?.ahead).toBe(2);

    await vi.advanceTimersByTimeAsync(5000);
    expect(h.ipc.sessionList).toHaveBeenCalledTimes(2);
  });

  it("does not poll in workflows mode", async () => {
    ui.uiState.mode = "workflows";
    await enterConnected();
    await vi.advanceTimersByTimeAsync(15_000);
    expect(h.ipc.sessionList).not.toHaveBeenCalled();
  });

  it("does not poll while the daemon is unreachable", async () => {
    ui.uiState.mode = "sessions";
    await enterConnected();
    await wire.onStatus({ state: "unreachable" });
    h.ipc.sessionList.mockClear();
    await vi.advanceTimersByTimeAsync(15_000);
    expect(h.ipc.sessionList).not.toHaveBeenCalled();
  });
});
