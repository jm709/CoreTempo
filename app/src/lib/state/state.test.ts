import { beforeEach, describe, expect, it, vi } from "vitest";
import { askQueued, askReplied, sendQueued, snapshotRunning } from "../fixtures/recorded";
import { agentsState, applyAgentState, applyLifecycle, resetAgents, runningCount, setAgents }
  from "./agents.svelte";
import { messagesState, pendingAsksFor, resetMessages, setMessages, upsertMessage }
  from "./messages.svelte";
import { resetRun } from "./run.svelte";
import { flashTerminal, resetUi, uiState } from "./ui.svelte";

beforeEach(() => {
  resetRun();
  resetAgents();
  resetMessages();
  resetUi();
});

describe("agents state", () => {
  it("setAgents indexes by id and orders lexicographically", () => {
    // oxlint-disable-next-line no-array-reverse -- ES2022 lib; spread copy is safe
    setAgents([...snapshotRunning.agents].reverse());
    expect(agentsState.order).toEqual(["builder", "docs", "planner"]);
    expect(agentsState.byId["planner"]?.model).toBe("opus");
  });
  it("lifecycle exited records the exit code; spawned clears it", () => {
    setAgents(snapshotRunning.agents);
    applyLifecycle("docs", "exited", { code: 1 });
    expect(agentsState.byId["docs"]?.state).toBe("exited");
    expect(agentsState.byId["docs"]?.exit).toEqual({ code: 1 });
    applyLifecycle("docs", "spawned", null);
    expect(agentsState.byId["docs"]?.state).toBe("starting");
    expect(agentsState.byId["docs"]?.exit).toBeNull();
  });
  it("unknown agent ids are ignored (roster is frozen; snapshot precedes events)", () => {
    setAgents(snapshotRunning.agents);
    applyAgentState("ghost", "working");
    expect(agentsState.byId["ghost"]).toBeUndefined();
  });
  it("runningCount excludes exited and restarting agents", () => {
    setAgents(snapshotRunning.agents);
    applyLifecycle("docs", "exited", { code: 1 });
    expect(runningCount()).toBe(2);
  });
});

describe("messages state", () => {
  it("setMessages reverses snapshot DESC into ascending feed order", () => {
    setMessages([sendQueued, askReplied]); // DESC by created_at
    expect(messagesState.list.map((m) => m.id)).toEqual(["m-a3f91c2e", "m-b7c21d0e"]);
  });
  it("upsert replaces by id and appends unknown ids", () => {
    upsertMessage(askQueued);
    upsertMessage(askReplied);
    expect(messagesState.list).toHaveLength(1);
    expect(messagesState.list[0]?.status).toBe("replied");
    upsertMessage(sendQueued);
    expect(messagesState.list).toHaveLength(2);
  });
  it("pendingAsksFor counts non-terminal asks sent by the agent", () => {
    upsertMessage(askQueued);
    expect(pendingAsksFor("planner")).toBe(1);
    upsertMessage(askReplied);
    expect(pendingAsksFor("planner")).toBe(0);
    upsertMessage(sendQueued); // a send never counts
    expect(pendingAsksFor("planner")).toBe(0);
  });
});

describe("ui state", () => {
  it("flashTerminal sets the target and clears it after 200 ms", () => {
    vi.useFakeTimers();
    flashTerminal("builder");
    expect(uiState.flashAgent).toBe("builder");
    vi.advanceTimersByTime(200);
    expect(uiState.flashAgent).toBeNull();
    vi.useRealTimers();
  });
});
