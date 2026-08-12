// TypeScript mirrors of coretempo-core wire types (contracts §2, §8.1).
// Field names must match the serde JSON exactly: snake_case, nulls explicit.

export type AgentState = "starting" | "idle" | "working" | "exited" | "restarting";
export type MessageKind = "ask" | "send";
/// How a triggered kickoff ended. `timeout` is its own result here, even though the
/// trigger GET status folds it into `failed` — do not collapse the two.
export type CompletionResult = "replied" | "quiesced" | "failed" | "timeout";
export type MessageStatus = "queued" | "injected" | "working" | "replied" | "done" | "failed";
export type LifecyclePhase = "spawned" | "exited" | "restarting";

export function isTerminalStatus(s: MessageStatus): boolean {
  return s === "replied" || s === "done" || s === "failed";
}

export interface MessageRecord {
  id: string;                 // "m-" + 8 lowercase hex
  kind: MessageKind;
  from: string;               // "agent:<id>" | "user" | "http:<req-id>"
  to: string;                 // AgentId
  body: string;
  status: MessageStatus;
  code: number | null;        // 0 | 1; null until replied
  reply: string | null;
  created_at: string;         // RFC 3339 UTC "…Z"
  injected_at: string | null;
  completed_at: string | null;
}

export interface AgentInfo {
  id: string;
  state: AgentState;          // raw (undebounced) — UI shows truth
  pending_asks: number;
  exit_code: number | null;   // set only when state == "exited"
}

export interface AgentDetail extends AgentInfo {
  // serde(flatten): AgentInfo fields sit flat in the JSON object
  dir: string;
  model: string | null;
  permission_mode: string | null;
  auto_clear: boolean;
  pty_cursor: number;
}

interface EventBase {
  seq: number;                // monotonic per run, starts at 1
  ts: string;
}

export type Event =
  | (EventBase & { type: "run.started"; run_id: string; workflow_name: string; started_at: string })
  | (EventBase & { type: "agent.state"; agent: string; state: AgentState })
  | (EventBase & {
      type: "agent.lifecycle"; agent: string; phase: LifecyclePhase; exit_code: number | null;
    })
  | (EventBase & { type: "message.created"; message: MessageRecord })
  | (EventBase & { type: "message.status"; message: MessageRecord })
  | (EventBase & { type: "bus.reset" })
  | (EventBase & { type: "agent.nudged"; agent: string })
  | (EventBase & { type: "agent.stalled"; agent: string })
  | (EventBase & { type: "reply.rejected"; message: string; agent: string; errors: string })
  | (EventBase & {
      type: "workflow.completed"; result: CompletionResult;
      code: number | null; reply: string | null;
      trigger_id: string | null; message: string;
      output: unknown; reason: string | null; reason_code: string | null;
    });

/// Wire form of one trigger's status (serde: internally tagged by `status`,
/// `code`/`reply`/`output` skipped when absent). Mirrors clients/js/src/wire.ts.
export type TriggerView =
  | { trigger_id: string; status: "queued"; position: number }
  | { trigger_id: string; status: "running" }
  | {
      trigger_id: string; status: "completed"; result: "replied" | "quiesced";
      code?: number; reply?: string; output?: unknown;
    }
  | { trigger_id: string; status: "failed"; reason: string; reason_code: string };

export interface RunInfo {
  run_id: string;
  workflow_name: string;
  workflow_path: string;
  started_at: string;
  port: number;
  scrollback: number;               // [workflow] scrollback, frozen per run
}

export interface Snapshot {
  run: RunInfo | null;
  agents: AgentDetail[];
  messages: MessageRecord[];            // most recent 200, created_at DESC
  pty_cursors: Record<string, number>;  // agent id → subscribe_pty since_cursor
  last_seq: number;                     // event dedup floor
  triggers: TriggerView[];              // oldest first, hub-capped at 100
}

export interface ValidationIssue { path: string; message: string }
export interface CmdError { code: string; message: string }

export type EdgeKind = "ask" | "send" | "loop";
export interface EdgeModel { to: string; kind: EdgeKind; max_rounds?: number }
export interface AgentModel {
  dir: string;
  prompt: string;
  model: string | null;
  permission_mode: string | null;
  auto_clear: boolean;
  edges: EdgeModel[];
  tools: string[];
}
export type TriggerType = "on_start" | "webhook";
/// Mirrors OutputConfig's serde form. Exactly one of schema/schema_file is
/// declared (save-time validated); max_repairs is always present in the wire
/// form because serde fills its default on parse.
export interface OutputModel {
  schema?: unknown;
  schema_file?: string;
  max_repairs: number;
}
/// `message` is the static kickoff text for `on_start`; a webhook takes its body
/// from the HTTP request, so it stays null.
export interface TriggerModel {
  type: TriggerType;
  edge: EdgeModel;
  message: string | null;
  output?: OutputModel;
}
export interface WorkflowModel {
  workflow: {
    name: string; db: string; port: number;
    ask_timeout_minutes: number; idle_debounce_seconds: number;
  };
  server: {
    bind: string | null; token_file: string | null;
    allowed_origins: string[]; log: string | null;
  };
  agents: Record<string, AgentModel>;
  trigger: TriggerModel | null;
}
export interface ParseReport { ok: boolean; errors: ValidationIssue[]; model: WorkflowModel | null }
