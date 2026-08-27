import { beforeEach, describe, expect, it } from "vitest";
import type { MessageRecord } from "../types";
import {
  addRejection, beginTrigger, completeTrigger, resetTriggers, seedTriggers, triggersState,
} from "./triggers.svelte";

function kickoff(over: Partial<MessageRecord> = {}): MessageRecord {
  return {
    id: "m-a3f91c2e", kind: "ask", from: "trigger:11223344", to: "translator",
    body: "translate this", status: "working", code: null, reply: null,
    created_at: "2026-08-07T10:00:00Z", injected_at: null, completed_at: null,
    reason: null, reason_code: null,
    ...over,
  };
}

beforeEach(() => resetTriggers());

describe("trigger lifecycle assembly", () => {
  it("opens a lifecycle from a trigger-origin message and ignores the rest", () => {
    beginTrigger(kickoff());
    beginTrigger(kickoff({ id: "m-2", from: "agent:planner" }));
    beginTrigger(kickoff({ id: "m-3", from: "http:99999999" }));  // a manual `tempo ask` (#24)
    beginTrigger(kickoff());  // duplicate: same trigger id
    expect(triggersState.list).toHaveLength(1);
    const t = triggersState.list[0];
    expect(t?.id).toBe("t-11223344");
    expect(t?.messageId).toBe("m-a3f91c2e");
    expect(t?.agent).toBe("translator");
    expect(t?.phase).toBe("working");
  });

  it("appends rejections by kickoff message id", () => {
    beginTrigger(kickoff());
    addRejection("m-a3f91c2e", "at /name: required", "2026-08-07T10:00:05Z");
    addRejection("m-unknown", "ignored", "2026-08-07T10:00:06Z");
    expect(triggersState.list[0]?.rejections).toEqual([
      { errors: "at /name: required", ts: "2026-08-07T10:00:05Z" },
    ]);
  });

  it("settles a lifecycle from the enriched completion", () => {
    beginTrigger(kickoff());
    completeTrigger({
      trigger_id: "t-11223344", message: "m-a3f91c2e", result: "replied",
      code: 0, reply: '{"ok":true}', output: { ok: true },
      reason: null, reason_code: null, ts: "2026-08-07T10:01:00Z",
    });
    const t = triggersState.list[0];
    expect(t?.phase).toBe("completed");
    expect(t?.output).toEqual({ ok: true });
    expect(t?.completedAt).toBe("2026-08-07T10:01:00Z");
  });

  it("marks failed and timeout results as failed, correlating by message id", () => {
    beginTrigger(kickoff());
    completeTrigger({
      trigger_id: null, message: "m-a3f91c2e", result: "timeout",
      code: null, reply: null, output: null, reason: null, reason_code: null,
      ts: "2026-08-07T10:30:00Z",
    });
    expect(triggersState.list[0]?.phase).toBe("failed");
    expect(triggersState.list[0]?.result).toBe("timeout");
  });

  it("settles a quiesced completion as completed, not failed", () => {
    beginTrigger(kickoff());
    completeTrigger({
      trigger_id: "t-11223344", message: "m-a3f91c2e", result: "quiesced",
      code: null, reply: null, output: null, reason: null, reason_code: null,
      ts: "2026-08-07T10:02:00Z",
    });
    const t = triggersState.list[0];
    expect(t?.phase).toBe("completed");
    expect(t?.code).toBeNull();
    expect(t?.reply).toBeNull();
    expect(t?.output).toBeNull();
  });

  it("seeds from a snapshot, enriching from the message list", () => {
    seedTriggers(
      [
        { trigger_id: "t-11223344", status: "completed", result: "replied",
          code: 0, reply: '{"ok":true}', output: { ok: true } },
        { trigger_id: "t-55667788", status: "running" },
      ],
      [kickoff(), kickoff({ id: "m-9", from: "trigger:55667788", body: "second" })],
    );
    expect(triggersState.list).toHaveLength(2);
    expect(triggersState.list[0]?.phase).toBe("completed");
    expect(triggersState.list[0]?.body).toBe("translate this");
    expect(triggersState.list[1]?.phase).toBe("working");
    expect(triggersState.list[1]?.body).toBe("second");
  });

  it("seeds a failed trigger with its reason and reason code", () => {
    seedTriggers(
      [{ trigger_id: "t-99887766", status: "failed",
        reason: "validation failed after max repairs", reason_code: "schema_invalid" }],
      [],
    );
    const t = triggersState.list[0];
    expect(t?.phase).toBe("failed");
    expect(t?.reason).toBe("validation failed after max repairs");
    expect(t?.reasonCode).toBe("schema_invalid");
  });
});
