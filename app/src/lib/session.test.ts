import { beforeEach, describe, expect, test, vi } from "vitest";
import type { Snapshot } from "./types";

vi.mock("./ipc", () => ({
  snapshot: vi.fn(),
  runStart: vi.fn(),
  runStop: vi.fn(async () => {}),
  toCmdError: (e: unknown) => e,
}));

vi.mock("./term/manager", () => ({
  ensureTerminal: vi.fn(async () => {}),
  disposeAllTerminals: vi.fn(),
}));

vi.mock("./wireEvents", () => ({
  applySnapshot: vi.fn(),
  wireEvents: vi.fn(async () => {}),
}));

import { runStart, snapshot } from "./ipc";
import { boot, startRun } from "./session";
import { uiState } from "./state/ui.svelte";
import { ensureTerminal } from "./term/manager";

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
      exit_code: null,
      dir: "/w",
      model: null,
      permission_mode: null,
      auto_clear: true,
      pty_cursor: 62_336,
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
    expect(vi.mocked(ensureTerminal)).toHaveBeenCalledWith("builder", null, 20_000);
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
