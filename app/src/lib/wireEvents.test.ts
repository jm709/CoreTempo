import { beforeEach, describe, expect, it, vi } from "vitest";
import { recordedRun, snapshotMidRun, snapshotRunning } from "./fixtures/recorded";
import { agentsState, resetAgents } from "./state/agents.svelte";
import { messagesState, resetMessages } from "./state/messages.svelte";
import { resetRun, runState } from "./state/run.svelte";
import { resetTriggers, triggersState } from "./state/triggers.svelte";
import type { Event, MessageRecord } from "./types";
import { applyEvent, applySnapshot } from "./wireEvents";

beforeEach(() => {
  resetRun();
  resetAgents();
  resetMessages();
  resetTriggers();
});

/// Seeds a one-agent roster via a bare snapshot (last_seq: 0), so `mkEvent()` below can
/// mint valid next-seq events against it without pulling in the full recorded fixture.
function seedRosterWith(id: string): void {
  applySnapshot({
    run: null,
    agents: [{
      id, state: "idle", dir: "/tmp", pending_asks: 0, exit_code: null,
      model: null, permission_mode: null, auto_clear: true, pty_cursor: 0,
    }],
    messages: [], pty_cursors: {}, last_seq: 0, triggers: [],
  });
}

// Plain `Omit` doesn't distribute over a union — it would collapse `Event` to only
// the keys common to every variant. This preserves each variant's own extra fields.
type DistributiveOmit<T, K extends keyof T> = T extends unknown ? Omit<T, K> : never;

/// Fills seq/ts boilerplate; seq tracks runState.lastSeq so events stay valid
/// regardless of how many precede them in a given test.
function mkEvent(partial: DistributiveOmit<Event, "seq" | "ts">): Event {
  return { seq: runState.lastSeq + 1, ts: "2026-08-03T00:00:00Z", ...partial } as Event;
}

describe("wireEvents reduction (recorded stream)", () => {
  it("reduces the full stream into agents, messages, and run state", () => {
    applySnapshot(snapshotRunning);
    for (const ev of recordedRun) expect(applyEvent(ev)).toBe(true);

    expect(runState.phase).toBe("running");
    expect(runState.lastSeq).toBe(12);
    expect(agentsState.byId["builder"]?.state).toBe("idle");
    expect(agentsState.byId["planner"]?.state).toBe("idle");
    expect(agentsState.byId["docs"]?.state).toBe("exited");
    expect(agentsState.byId["docs"]?.exit_code).toBe(1);
    expect(messagesState.list.map((m) => [m.id, m.status])).toEqual([
      ["m-a3f91c2e", "replied"],
      ["m-b7c21d0e", "failed"],
    ]);
    expect(messagesState.list[0]?.code).toBe(0);
    expect(messagesState.list[0]?.reply).toBe("Yes, migration 004 applied and tested.");
  });

  it("drops replayed seqs without corrupting state", () => {
    applySnapshot(snapshotRunning);
    for (const ev of recordedRun) applyEvent(ev);
    for (const ev of recordedRun.slice(2, 7)) expect(applyEvent(ev)).toBe(false);
    expect(messagesState.list).toHaveLength(2);
    expect(messagesState.list[0]?.status).toBe("replied");
    expect(runState.lastSeq).toBe(12);
  });

  it("honors the snapshot dedup floor on reload mid-run", () => {
    applySnapshot(snapshotMidRun); // last_seq = 8; ask already replied in snapshot
    for (const ev of recordedRun.slice(0, 8)) expect(applyEvent(ev)).toBe(false);
    for (const ev of recordedRun.slice(8)) expect(applyEvent(ev)).toBe(true);
    expect(messagesState.list).toHaveLength(2);
    expect(agentsState.byId["docs"]?.state).toBe("exited");
  });

  it("bus.reset triggers resync and never dedups", () => {
    applySnapshot(snapshotRunning);
    for (const ev of recordedRun) applyEvent(ev);
    const resync = vi.fn();
    expect(applyEvent({ seq: 12, ts: "2026-08-01T17:05:20Z", type: "bus.reset" }, resync))
      .toBe(true);
    expect(resync).toHaveBeenCalledOnce();
    expect(runState.lastSeq).toBe(12);
  });

  it("run.started for an unknown run id requests a resync (fresh roster needed)", () => {
    const resync = vi.fn();
    const first = recordedRun[0];
    if (first === undefined) throw new Error("fixture empty");
    applyEvent(first, resync); // runState.info is null → resync
    expect(resync).toHaveBeenCalledOnce();
    expect(runState.phase).toBe("running");
  });

  it("applySnapshot with run=null lands in stopped phase but keeps the feed history", () => {
    applySnapshot({ ...snapshotMidRun, run: null });
    expect(runState.phase).toBe("stopped");
    expect(messagesState.list).toHaveLength(1);
  });

  it("agent.stalled sets the badge and working clears it", () => {
    seedRosterWith("planner");
    applyEvent(mkEvent({ type: "agent.stalled", agent: "planner" }));
    expect(agentsState.stalled["planner"]).toBe(true);
    applyEvent(mkEvent({ type: "agent.state", agent: "planner", state: "working" }));
    expect(agentsState.stalled["planner"]).toBeUndefined();
  });

  it("agent.nudged is accepted and does not throw", () => {
    seedRosterWith("planner");
    expect(applyEvent(mkEvent({ type: "agent.nudged", agent: "planner" }))).toBe(true);
  });
});

describe("workflow.completed", () => {
  it("stores the result, code, and reply", () => {
    seedRosterWith("planner");
    const accepted = applyEvent(mkEvent({
      type: "workflow.completed", result: "replied", code: 0, reply: "done",
      trigger_id: null, message: "m-0", output: null, reason: null, reason_code: null,
    }));
    expect(accepted).toBe(true);
    expect(runState.completed).toEqual({ result: "replied", code: 0, reply: "done" });
  });

  it("keeps a codeless result (quiesced, failed, timeout) distinguishable", () => {
    seedRosterWith("planner");
    applyEvent(mkEvent({
      type: "workflow.completed", result: "timeout", code: null, reply: null,
      trigger_id: null, message: "m-0", output: null, reason: null, reason_code: null,
    }));
    expect(runState.completed?.result).toBe("timeout");
    expect(runState.completed?.code).toBeNull();
  });

  it("a later run.started clears the previous run's completion", () => {
    seedRosterWith("planner");
    applyEvent(mkEvent({
      type: "workflow.completed", result: "failed", code: null, reply: null,
      trigger_id: null, message: "m-0", output: null, reason: null, reason_code: null,
    }));
    applyEvent(mkEvent({
      type: "run.started", run_id: "r-2", workflow_name: "w", started_at: "2026-08-03T00:00:01Z",
    }));
    expect(runState.completed).toBeNull();
  });

  it("resetRun clears it", () => {
    seedRosterWith("planner");
    applyEvent(mkEvent({
      type: "workflow.completed", result: "quiesced", code: null, reply: null,
      trigger_id: null, message: "m-0", output: null, reason: null, reason_code: null,
    }));
    resetRun();
    expect(runState.completed).toBeNull();
  });
});

describe("trigger lifecycle reduction", () => {
  it("assembles kickoff → rejection → completion from the stream", () => {
    seedRosterWith("translator");
    const kickoff: MessageRecord = {
      id: "m-a3f91c2e", kind: "ask", from: "http:11223344", to: "translator",
      body: "translate this", status: "working", code: null, reply: null,
      created_at: "2026-08-07T10:00:00Z", injected_at: null, completed_at: null,
    };
    applyEvent(mkEvent({ type: "message.created", message: kickoff }));
    applyEvent(mkEvent({
      type: "reply.rejected", message: "m-a3f91c2e", agent: "translator",
      errors: "at /name: required",
    }));
    applyEvent(mkEvent({
      type: "workflow.completed", result: "replied", code: 0, reply: '{"ok":true}',
      trigger_id: "t-11223344", message: "m-a3f91c2e", output: { ok: true },
      reason: null, reason_code: null,
    }));
    const t = triggersState.list[0];
    expect(t?.id).toBe("t-11223344");
    expect(t?.rejections).toHaveLength(1);
    expect(t?.phase).toBe("completed");
    expect(t?.output).toEqual({ ok: true });
    expect(runState.completed?.result).toBe("replied");
  });

  it("seeds trigger history from the snapshot and clears it on run.started", () => {
    applySnapshot({
      run: null, agents: [], messages: [], pty_cursors: {}, last_seq: 0,
      triggers: [{ trigger_id: "t-11223344", status: "running" }],
    });
    expect(triggersState.list).toHaveLength(1);
    applyEvent(mkEvent({
      type: "run.started", run_id: "r-1f2e3d4c", workflow_name: "example",
      started_at: "2026-08-07T11:00:00Z",
    }));
    expect(triggersState.list).toHaveLength(0);
  });
});
