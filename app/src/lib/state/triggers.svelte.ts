import type { CompletionResult, MessageRecord, TriggerView } from "../types";

export interface Rejection { errors: string; ts: string }

/// One kickoff's lifecycle, assembled from `message.created` (open), `reply.rejected`
/// (repair loop), and the enriched `workflow.completed` (settle) — or seeded whole
/// from `Snapshot.triggers`, where the kickoff message fills in the request side.
export interface TriggerLifecycle {
  id: string;                 // "t-" + 8 lowercase hex
  messageId: string | null;   // null when snapshot-seeded and the message aged out
  agent: string | null;
  body: string | null;
  startedAt: string | null;
  phase: "working" | "completed" | "failed";
  rejections: Rejection[];
  result: CompletionResult | null;
  code: number | null;
  reply: string | null;
  output: unknown;            // non-null only when a [flows.<name>.output] schema validated
  reason: string | null;
  reasonCode: string | null;
  completedAt: string | null;
}

export const triggersState = $state({
  list: [] as TriggerLifecycle[],     // oldest first, mirroring hub insertion order
  selectedId: null as string | null,  // history selection; null = latest
});

function blank(id: string): TriggerLifecycle {
  return {
    id, messageId: null, agent: null, body: null, startedAt: null,
    phase: "working", rejections: [], result: null, code: null, reply: null,
    output: null, reason: null, reasonCode: null, completedAt: null,
  };
}

/// Opens a lifecycle for a flow kickoff. `from` is "trigger:<hex>" where the
/// trigger id is "t-<hex>" (contracts amendment 38); everything else — including
/// a plain "http:<hex>" message such as a manual `tempo ask` — is ignored.
export function beginTrigger(m: MessageRecord): void {
  if (!m.from.startsWith("trigger:")) return;
  const id = `t-${m.from.slice("trigger:".length)}`;
  if (triggersState.list.some((t) => t.id === id)) return;
  triggersState.list.push({
    ...blank(id), messageId: m.id, agent: m.to, body: m.body, startedAt: m.created_at,
  });
}

export function addRejection(messageId: string, errors: string, ts: string): void {
  const t = triggersState.list.find((x) => x.messageId === messageId);
  if (t !== undefined) t.rejections.push({ errors, ts });
}

/// The enriched workflow.completed payload plus its bus timestamp.
export interface CompletionEvent {
  trigger_id: string | null;
  message: string;
  result: CompletionResult;
  code: number | null;
  reply: string | null;
  output: unknown;
  reason: string | null;
  reason_code: string | null;
  ts: string;
}

export function completeTrigger(ev: CompletionEvent): void {
  const t =
    triggersState.list.find((x) => ev.trigger_id !== null && x.id === ev.trigger_id) ??
    triggersState.list.find((x) => x.messageId === ev.message);
  if (t === undefined) return;
  t.phase = ev.result === "replied" || ev.result === "quiesced" ? "completed" : "failed";
  t.result = ev.result;
  t.code = ev.code;
  t.reply = ev.reply;
  t.output = ev.output ?? null;
  t.reason = ev.reason;
  t.reasonCode = ev.reason_code;
  t.completedAt = ev.ts;
}

export function seedTriggers(views: TriggerView[], messages: MessageRecord[]): void {
  triggersState.list = views.map((v) => {
    const hex = v.trigger_id.slice("t-".length);
    const m = messages.find((x) => x.from === `trigger:${hex}`);
    const t = blank(v.trigger_id);
    if (m !== undefined) {
      t.messageId = m.id;
      t.agent = m.to;
      t.body = m.body;
      t.startedAt = m.created_at;
    }
    if (v.status === "completed") {
      t.phase = "completed";
      t.result = v.result;
      t.code = v.code ?? null;
      t.reply = v.reply ?? null;
      t.output = v.output ?? null;
    } else if (v.status === "failed") {
      t.phase = "failed";
      t.reason = v.reason;
      t.reasonCode = v.reason_code;
    }
    return t;
  });
  triggersState.selectedId = null;
}

export function selectTrigger(id: string | null): void {
  triggersState.selectedId = id;
}

export function resetTriggers(): void {
  triggersState.list = [];
  triggersState.selectedId = null;
}
