/**
 * Raw JSON shapes from the CoreTempo /v1/trigger endpoints, exactly as core
 * serializes them (core/src/trigger.rs). snake_case is the wire's, not ours.
 */

/** Failure kinds core emits today; open-ended so a new server code still flows through. */
export type ReasonCode =
  | "schema_validation_failed"
  | "agent_failed"
  | "timeout"
  | "agent_exited"
  | "kickoff_rejected"
  | "internal"
  | "workflow_changed"
  | (string & {});

/** 202 body: the result arrives later via GET /v1/trigger/{id}. */
export interface TriggerAccepted {
  trigger_id: string;
  position: number;
}

export type TriggerView =
  | { trigger_id: string; status: "queued"; position: number }
  | { trigger_id: string; status: "running" }
  | {
      trigger_id: string;
      status: "completed";
      result: "replied" | "quiesced";
      code?: number;
      reply?: string;
      output?: unknown;
    }
  | { trigger_id: string; status: "failed"; reason: string; reason_code: ReasonCode };

/** The two views a poll loop stops on. */
export type TerminalView = Extract<TriggerView, { status: "completed" | "failed" }>;

/** Every non-2xx body: {"error":{"code","message"}}. */
export interface ApiErrorBody {
  error: { code: string; message: string };
}
