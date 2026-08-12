import type { CompletionResult, RunInfo } from "../types";

export type RunPhase = "stopped" | "starting" | "running" | "stopping";

/// How this run's trigger kickoff ended; `code`/`reply` are set only for `replied`.
export interface Completion {
  result: CompletionResult;
  code: number | null;
  reply: string | null;
}

export const runState = $state({
  phase: "stopped" as RunPhase,
  info: null as RunInfo | null,
  lastSeq: 0,
  completed: null as Completion | null,
});

export function resetRun(): void {
  runState.phase = "stopped";
  runState.info = null;
  runState.lastSeq = 0;
  runState.completed = null;
}
