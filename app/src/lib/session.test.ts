import { beforeEach, describe, expect, test, vi } from "vitest";
import type { Snapshot } from "./types";

vi.mock("./ipc", () => ({
  snapshot: vi.fn(),
  runStart: vi.fn(),
  runStop: vi.fn(async () => {}),
  runUntrustedDirs: vi.fn(async () => []),
  toCmdError: (e: unknown) => e,
}));

vi.mock("./dialogs", () => ({ confirmTrust: vi.fn(async () => true) }));

vi.mock("./term/instances", () => ({
  workflowTerm: { ensure: vi.fn(async () => {}), disposeAll: vi.fn() },
}));

vi.mock("./wireEvents", () => ({
  applySnapshot: vi.fn(),
  wireEvents: vi.fn(async () => {}),
}));

import { runStart, runUntrustedDirs, snapshot } from "./ipc";
import { boot, startRun } from "./session";
import { resetRun, runState } from "./state/run.svelte";
import { uiState } from "./state/ui.svelte";
import { workflowTerm } from "./term/instances";
import { confirmTrust } from "./dialogs";

const midRunSnapshot: Snapshot = {
  run: {
    run_id: "r-1764eb3f",
    workflow_name: "example",
    workflow_path: "/w/tempo.toml",
    started_at: "2026-08-04T23:25:00Z",
    port: 4820,
    scrollback: 20_000,
  },
  agents: [
    {
      id: "builder",
      state: "idle",
      pending_asks: 0,
      exit: null,
      dir: "/w",
      model: null,
      permission_mode: null,
      auto_clear: true,
      isolated_config: false,
      skills: [],
      pty_cursor: 62_336,
      blocked: false,
    },
  ],
  messages: [],
  pty_cursors: { builder: 62_336 },
  last_seq: 7,
  triggers: [],
};

describe("terminal subscription cursors", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  test("boot subscribes fresh terminals with a null cursor to replay the ring tail", async () => {
    // Contracts §8.2: pty_cursors are end-of-stream positions; a terminal that
    // does not already hold the screen must pass null or it joins mid-stream
    // and shows a torn, mostly blank pane until the agent redraws.
    vi.mocked(snapshot).mockResolvedValue(midRunSnapshot);
    await boot();
    expect(vi.mocked(workflowTerm.ensure)).toHaveBeenCalledWith("builder", null, 20_000);
  });
});

describe("run start view reset", () => {
  test("startRun returns the center to the graph view", async () => {
    vi.mocked(runStart).mockResolvedValue({
      run_id: "r-1764eb3f",
      workflow_name: "example",
      workflow_path: "/w/tempo.toml",
      started_at: "2026-08-04T23:25:00Z",
      port: 4820,
      scrollback: 20_000,
    });
    vi.mocked(snapshot).mockResolvedValue(midRunSnapshot);
    uiState.runCenter = "terminals";
    await startRun("/w/tempo.toml");
    expect(uiState.runCenter).toBe("graph");
  });
});

describe("startRun trust preflight", () => {
  beforeEach(() => {
    resetRun();
    vi.mocked(runUntrustedDirs).mockResolvedValue([]);
    vi.mocked(confirmTrust).mockClear();
    vi.mocked(runStart).mockClear();
    vi.mocked(runStart).mockResolvedValue(midRunSnapshot.run!);
    vi.mocked(snapshot).mockResolvedValue(midRunSnapshot);
  });

  test("starts without a dialog when every root is trusted", async () => {
    await startRun("/w/tempo.toml");
    expect(confirmTrust).not.toHaveBeenCalled();
    expect(runStart).toHaveBeenCalledWith("/w/tempo.toml", false);
    expect(runState.info).toEqual(midRunSnapshot.run);
  });

  test("asks, then starts with the confirmation when the user agrees", async () => {
    vi.mocked(runUntrustedDirs).mockResolvedValue(["/w/one", "/w/two"]);
    vi.mocked(confirmTrust).mockResolvedValue(true);
    await startRun("/w/tempo.toml");
    expect(confirmTrust).toHaveBeenCalledWith(["/w/one", "/w/two"]);
    expect(runStart).toHaveBeenCalledWith("/w/tempo.toml", true);
    expect(runState.info).toEqual(midRunSnapshot.run);
  });

  test("declining starts nothing", async () => {
    vi.mocked(runUntrustedDirs).mockResolvedValue(["/w/one"]);
    vi.mocked(confirmTrust).mockResolvedValue(false);
    await startRun("/w/tempo.toml");
    expect(runStart).not.toHaveBeenCalled();
    expect(runState.phase).toBe("stopped");
    expect(runState.info).toBeNull();
  });
});
