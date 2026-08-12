import type { StandardSchemaV1 } from "./standard-schema.js";
import type { ReasonCode, TerminalView } from "./wire.js";

/** The agent replied and (if a schema was supplied) the output conformed. */
export interface CompletedOutcome<T> {
  status: "completed";
  triggerId: string;
  result: "replied" | "quiesced";
  reply?: string;
  /** Typed when a schema was supplied; the raw wire `output` (or undefined) otherwise. */
  output: T;
}

/** The agent answered `--code <n>` with n > 0: it could not produce the shape. */
export interface DeclinedOutcome {
  status: "declined";
  triggerId: string;
  code: number;
  reply: string;
}

/** The trigger failed server-side; branch on `reasonCode`, never on `reason` prose. */
export interface FailedOutcome {
  status: "failed";
  triggerId: string;
  reason: string;
  reasonCode: ReasonCode;
}

/**
 * The server said completed but the caller's schema rejected `output` — the
 * schema passed to the client has drifted from the workflow's `schema_file`.
 */
export interface OutputMismatchOutcome {
  status: "output_mismatch";
  triggerId: string;
  reply?: string;
  raw: unknown;
  issues: ReadonlyArray<StandardSchemaV1.Issue>;
}

export type TriggerOutcome<T> =
  | CompletedOutcome<T>
  | DeclinedOutcome
  | FailedOutcome
  | OutputMismatchOutcome;

/** Maps a terminal wire view into the caller-facing outcome union. */
export async function toOutcome<T = unknown>(
  view: TerminalView,
  schema?: StandardSchemaV1<unknown, T>,
): Promise<TriggerOutcome<T>> {
  if (view.status === "failed") {
    return {
      status: "failed",
      triggerId: view.trigger_id,
      reason: view.reason,
      reasonCode: view.reason_code,
    };
  }
  if (typeof view.code === "number" && view.code !== 0) {
    return {
      status: "declined",
      triggerId: view.trigger_id,
      code: view.code,
      reply: view.reply ?? "",
    };
  }
  if (schema === undefined) {
    return {
      status: "completed",
      triggerId: view.trigger_id,
      result: view.result,
      ...(view.reply === undefined ? {} : { reply: view.reply }),
      output: view.output as T,
    };
  }
  if (!("output" in view) || view.output === undefined) {
    return {
      status: "output_mismatch",
      triggerId: view.trigger_id,
      ...(view.reply === undefined ? {} : { reply: view.reply }),
      raw: undefined,
      issues: [
        {
          message:
            "a schema was supplied but the server reply carried no parsed output — " +
            "declare [trigger.output] in tempo.toml so the server validates and emits it",
        },
      ],
    };
  }
  const result = await schema["~standard"].validate(view.output);
  if (result.issues !== undefined) {
    return {
      status: "output_mismatch",
      triggerId: view.trigger_id,
      ...(view.reply === undefined ? {} : { reply: view.reply }),
      raw: view.output,
      issues: result.issues,
    };
  }
  return {
    status: "completed",
    triggerId: view.trigger_id,
    result: view.result,
    ...(view.reply === undefined ? {} : { reply: view.reply }),
    output: result.value,
  };
}
