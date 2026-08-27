import { onCoreEvent } from "./ipc";
import {
  applyAgentState, applyLifecycle, clearBlocked, setAgents, setBlocked, setRefused, setStalled,
} from "./state/agents.svelte";
import { setMessages, upsertMessage } from "./state/messages.svelte";
import { runState } from "./state/run.svelte";
import {
  addRejection, beginTrigger, completeTrigger, resetTriggers, seedTriggers,
} from "./state/triggers.svelte";
import type { Event, Snapshot } from "./types";

const noop = (): void => {};

export function applySnapshot(snap: Snapshot): void {
  runState.lastSeq = snap.last_seq;
  runState.info = snap.run;
  if (snap.run !== null) runState.phase = "running";
  else if (runState.phase !== "stopping") runState.phase = "stopped";
  setAgents(snap.agents);
  setMessages(snap.messages);
  seedTriggers(snap.triggers, snap.messages);
}

/// The single reducer: one Event in, rune state mutated. Returns false iff the
/// event was dropped by the seq dedup floor (contracts §8.2: dedup by seq > last_seq).
export function applyEvent(ev: Event, onReset: () => void = noop): boolean {
  if (ev.type === "bus.reset") {
    // Synthesized on bridge lag; its seq equals the latest published seq, so it must
    // bypass the floor. The only recovery is a fresh snapshot.
    runState.lastSeq = Math.max(runState.lastSeq, ev.seq);
    onReset();
    return true;
  }
  if (ev.seq <= runState.lastSeq) return false;
  runState.lastSeq = ev.seq;
  switch (ev.type) {
    case "run.started":
      runState.phase = "running";
      runState.completed = null; // a fresh run: the previous run's outcome is stale
      resetTriggers();
      // A run we did not initiate (or a reload race): the roster is unknown → resync.
      if (runState.info === null || runState.info.run_id !== ev.run_id) onReset();
      break;
    case "agent.state":
      applyAgentState(ev.agent, ev.state);
      break;
    case "agent.lifecycle":
      applyLifecycle(ev.agent, ev.phase, ev.exit);
      break;
    case "message.created":
      upsertMessage(ev.message);
      beginTrigger(ev.message);
      break;
    case "message.status":
      upsertMessage(ev.message);
      break;
    case "agent.nudged":
      break; // logged server-side; the stall badge is the durable signal
    case "agent.stalled":
      setStalled(ev.agent, true);
      break;
    case "agent.blocked":
      if (ev.blocked) setBlocked(ev.agent, ev.tool);
      else clearBlocked(ev.agent);
      break;
    case "agent.permission_refused":
      setRefused(ev.agent, { tool: ev.tool, input: ev.input, ts: ev.ts });
      break;
    case "reply.rejected":
      addRejection(ev.message, ev.errors, ev.ts);
      break;
    case "workflow.completed":
      runState.completed = { result: ev.result, code: ev.code, reply: ev.reply };
      completeTrigger({
        trigger_id: ev.trigger_id, message: ev.message, result: ev.result, code: ev.code,
        reply: ev.reply, output: ev.output, reason: ev.reason, reason_code: ev.reason_code,
        ts: ev.ts,
      });
      break;
  }
  return true;
}

export async function wireEvents(resync: () => void): Promise<() => void> {
  return await onCoreEvent((ev) => {
    applyEvent(ev, resync);
  });
}
