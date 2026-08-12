import type { Event, MessageRecord, Snapshot } from "../types";

const askBase = {
  id: "m-a3f91c2e",
  kind: "ask" as const,
  from: "agent:planner",
  to: "builder",
  body: "Is the schema migration done?",
};

export const askQueued: MessageRecord = {
  ...askBase, status: "queued", code: null, reply: null,
  created_at: "2026-08-01T17:03:11Z", injected_at: null, completed_at: null,
};
export const askInjected: MessageRecord = {
  ...askQueued, status: "injected", injected_at: "2026-08-01T17:03:12Z",
};
export const askWorking: MessageRecord = { ...askInjected, status: "working" };
export const askReplied: MessageRecord = {
  ...askWorking, status: "replied", code: 0,
  reply: "Yes, migration 004 applied and tested.", completed_at: "2026-08-01T17:04:40Z",
};

export const sendQueued: MessageRecord = {
  id: "m-b7c21d0e", kind: "send", from: "agent:planner", to: "docs",
  body: "Document the /v1/messages endpoint.", status: "queued", code: null, reply: null,
  created_at: "2026-08-01T17:05:02Z", injected_at: null, completed_at: null,
};
export const sendFailed: MessageRecord = {
  ...sendQueued, status: "failed", completed_at: "2026-08-01T17:05:10Z",
};

export const recordedRun: Event[] = [
  { seq: 1, ts: "2026-08-01T17:00:00Z", type: "run.started", run_id: "r-1f2e3d4c",
    workflow_name: "core-tempo-dev", started_at: "2026-08-01T17:00:00Z" },
  { seq: 2, ts: "2026-08-01T17:03:10Z", type: "agent.state", agent: "planner", state: "working" },
  { seq: 3, ts: "2026-08-01T17:03:11Z", type: "message.created", message: askQueued },
  { seq: 4, ts: "2026-08-01T17:03:12Z", type: "message.status", message: askInjected },
  { seq: 5, ts: "2026-08-01T17:03:13Z", type: "agent.state", agent: "builder", state: "working" },
  { seq: 6, ts: "2026-08-01T17:03:14Z", type: "message.status", message: askWorking },
  { seq: 7, ts: "2026-08-01T17:04:40Z", type: "message.status", message: askReplied },
  { seq: 8, ts: "2026-08-01T17:04:42Z", type: "agent.state", agent: "builder", state: "idle" },
  { seq: 9, ts: "2026-08-01T17:05:02Z", type: "message.created", message: sendQueued },
  { seq: 10, ts: "2026-08-01T17:05:09Z", type: "agent.lifecycle", agent: "docs",
    phase: "exited", exit_code: 1 },
  { seq: 11, ts: "2026-08-01T17:05:10Z", type: "message.status", message: sendFailed },
  { seq: 12, ts: "2026-08-01T17:05:11Z", type: "agent.state", agent: "planner", state: "idle" },
];

const agentDefaults = {
  pending_asks: 0, exit_code: null, model: null, permission_mode: null,
  auto_clear: true, pty_cursor: 0,
};

export const snapshotRunning: Snapshot = {
  run: { run_id: "r-1f2e3d4c", workflow_name: "core-tempo-dev",
    workflow_path: "/home/user/dev/tempo.toml", started_at: "2026-08-01T17:00:00Z", port: 4820,
    scrollback: 5_000 },
  agents: [
    { id: "builder", state: "idle", dir: "/home/user/dev", ...agentDefaults,
      permission_mode: "acceptEdits" },
    { id: "docs", state: "idle", dir: "/home/user/dev", ...agentDefaults },
    { id: "planner", state: "idle", dir: "/home/user/dev", ...agentDefaults, model: "opus" },
  ],
  messages: [],
  pty_cursors: { builder: 0, docs: 0, planner: 0 },
  last_seq: 0,
  triggers: [],
};

// State of the world as of seq 8 (ask replied, send not yet created, docs still alive).
export const snapshotMidRun: Snapshot = {
  run: snapshotRunning.run,
  agents: [
    { id: "builder", state: "idle", dir: "/home/user/dev", ...agentDefaults,
      permission_mode: "acceptEdits", pty_cursor: 18321 },
    { id: "docs", state: "idle", dir: "/home/user/dev", ...agentDefaults, pty_cursor: 921 },
    { id: "planner", state: "working", dir: "/home/user/dev", ...agentDefaults,
      model: "opus", pty_cursor: 40217 },
  ],
  messages: [askReplied],
  pty_cursors: { builder: 18321, docs: 921, planner: 40217 },
  last_seq: 8,
  triggers: [],
};
