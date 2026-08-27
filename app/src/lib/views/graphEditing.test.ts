import { describe, expect, it, test } from "vitest";
import type { FlowModel, WorkflowModel } from "../types";
import {
  coerceNumber, connectAgents, duplicateEdgeError, freeSlot, isProjectedEdge, nextEdgeKind,
} from "./graphEditing";
import { outputNodeId, triggerNodeId, WORKFLOW_NODE_ID } from "./graphModel";

function model(): WorkflowModel {
  return {
    workflow: {
      name: "t",
      db: "./tempo.db",
      port: 4820,
      ask_timeout_minutes: 30,
      idle_debounce_seconds: 2,
    },
    server: {
      bind: null, token_file: null, log: null, max_concurrent_runs: 2, trust_agent_dirs: false,
    },
    agents: {
      planner: {
        dir: "/p",
        prompt: "plan",
        model: null,
        permission_mode: null,
        auto_clear: true,
        edges: [
          { to: "builder", kind: "ask" },
          { to: "builder", kind: "send" },
        ],
        tools: [],
        allow: [],
        mcp: [],
        concurrency: "exclusive",
        isolated_config: false,
        skills: [],
      },
      builder: {
        dir: "/b",
        prompt: "build",
        model: null,
        permission_mode: null,
        auto_clear: true,
        edges: [],
        tools: [],
        allow: [],
        mcp: [],
        concurrency: "exclusive",
        isolated_config: false,
        skills: [],
      },
    },
    flows: {},
  };
}

function agent(): WorkflowModel["agents"][string] {
  return {
    dir: "/tmp", prompt: "p", model: null, permission_mode: null,
    auto_clear: true, edges: [], tools: [], allow: [], mcp: [], concurrency: "exclusive",
    isolated_config: false, skills: [],
  };
}

function twoAgentModel(flows: Record<string, FlowModel>): WorkflowModel {
  return {
    workflow: {
      name: "x", db: "./tempo.db", port: 4820,
      ask_timeout_minutes: 30, idle_debounce_seconds: 2,
    },
    server: {
      bind: null, token_file: null, log: null, max_concurrent_runs: 2, trust_agent_dirs: false,
    },
    agents: { a: agent(), b: agent() },
    flows,
  };
}

function webhookFlow(agents: string[], to: string): FlowModel {
  return { agents, trigger: { type: "webhook", edge: { to, kind: "ask" }, message: null } };
}

describe("coerceNumber", () => {
  test("parses digits", () => {
    expect(coerceNumber("4820")).toBe(4820);
    expect(coerceNumber(" 12 ")).toBe(12);
    expect(coerceNumber("0")).toBe(0);
  });

  test("rejects blanks so an emptied field does not become 0", () => {
    expect(coerceNumber("")).toBeNull();
    expect(coerceNumber("   ")).toBeNull();
  });

  test("rejects non-numbers and infinities", () => {
    expect(coerceNumber("12abc")).toBeNull();
    expect(coerceNumber("Infinity")).toBeNull();
    expect(coerceNumber("NaN")).toBeNull();
  });
});

describe("connectAgents", () => {
  test("adds an ask edge between two agents", () => {
    const m = model();
    m.agents["planner"]!.edges = [];
    expect(connectAgents(m, "planner", "builder")).toBeNull();
    expect(m.agents["planner"]!.edges).toEqual([{ to: "builder", kind: "ask" }]);
  });

  test("refuses a self-edge and leaves the model alone", () => {
    const m = model();
    const before = m.agents["builder"]!.edges.length;
    expect(connectAgents(m, "builder", "builder")).toContain("cannot edge to itself");
    expect(m.agents["builder"]!.edges).toHaveLength(before);
  });

  test("refuses a duplicate ask edge", () => {
    const m = model();
    m.agents["planner"]!.edges = [{ to: "builder", kind: "ask" }];
    expect(connectAgents(m, "planner", "builder")).toContain("duplicate edge");
    expect(m.agents["planner"]!.edges).toHaveLength(1);
  });

  test("refuses a connection touching the workflow node", () => {
    const m = model();
    expect(connectAgents(m, WORKFLOW_NODE_ID, "builder")).toContain("is not an agent");
    expect(connectAgents(m, "planner", WORKFLOW_NODE_ID)).toContain("is not an agent");
    expect(m.agents["planner"]!.edges).toHaveLength(2);
  });

  test("refuses an unknown endpoint", () => {
    const m = model();
    expect(connectAgents(m, "planner", "ghost")).toContain("no agent named 'ghost'");
  });

  test("a drag from the trigger node rewires the trigger instead of adding an agent edge", () => {
    const m = model();
    m.flows = {
      main: {
        agents: ["planner", "builder"],
        trigger: { type: "webhook", edge: { to: "planner", kind: "send" }, message: null },
      },
    };
    expect(connectAgents(m, triggerNodeId("main"), "builder")).toBeNull();
    expect(m.flows["main"]!.trigger.edge).toEqual({ to: "builder", kind: "send" });
    expect(m.agents["planner"]!.edges).toHaveLength(2); // agent edges untouched
  });

  test("refuses a connection touching the trigger node from the wrong side", () => {
    const m = model();
    m.flows = {
      main: {
        agents: ["planner", "builder"],
        trigger: { type: "webhook", edge: { to: "planner", kind: "ask" }, message: null },
      },
    };
    expect(connectAgents(m, triggerNodeId("main"), WORKFLOW_NODE_ID)).toContain("is not an agent");
    expect(connectAgents(m, "planner", triggerNodeId("main"))).toContain("has no inbound edges");
    expect(m.flows["main"]!.trigger.edge.to).toBe("planner");
  });

  test("connections touching the output node are refused without mutating", () => {
    const m = model();
    m.flows = {
      main: {
        agents: ["planner", "builder"],
        trigger: { type: "webhook", edge: { to: "planner", kind: "ask" }, message: null },
        output: { schema_file: "s.json", max_repairs: 2 },
      },
    };
    expect(connectAgents(m, "planner", outputNodeId("main"))).toContain("[flows.<name>.output]");
    expect(connectAgents(m, outputNodeId("main"), "planner")).toContain("[flows.<name>.output]");
    expect(m.agents["planner"]!.edges).toHaveLength(2);
  });

  it("a drag from a trigger node re-aims exactly that flow", () => {
    const m = twoAgentModel({ one: webhookFlow(["a"], "a"), two: webhookFlow(["a"], "a") });
    expect(connectAgents(m, triggerNodeId("one"), "b")).toBeNull();
    expect(m.flows["one"]!.trigger.edge.to).toBe("b");
    expect(m.flows["two"]!.trigger.edge.to).toBe("a");
  });
});

describe("freeSlot", () => {
  test("keeps the layout slot when nothing occupies it", () => {
    expect(freeSlot({ x: 260, y: 60 }, [{ x: 260, y: 320 }])).toEqual({ x: 260, y: 60 });
  });

  test("nudges down off an occupied slot", () => {
    expect(freeSlot({ x: 260, y: 60 }, [{ x: 260, y: 60 }])).toEqual({ x: 260, y: 200 });
  });

  test("keeps nudging past a stack of occupied slots", () => {
    const taken = [
      { x: 260, y: 60 },
      { x: 260, y: 200 },
    ];
    expect(freeSlot({ x: 260, y: 60 }, taken)).toEqual({ x: 260, y: 340 });
  });

  test("ignores nodes in other columns", () => {
    expect(freeSlot({ x: 260, y: 60 }, [{ x: 520, y: 60 }])).toEqual({ x: 260, y: 60 });
  });
});

describe("duplicateEdgeError", () => {
  test("null when the target kind does not exist yet", () => {
    const m = model();
    m.agents["planner"]!.edges = [{ to: "builder", kind: "ask" }];
    expect(duplicateEdgeError(m, "planner", "builder", "send")).toBeNull();
  });

  test("names both endpoints and the fix when the kind is taken", () => {
    const message = duplicateEdgeError(model(), "planner", "builder", "send");
    expect(message).toContain("'planner' -> 'builder'");
    expect(message).toContain("send");
    expect(message).toContain("delete one of the two edges");
  });

  test("null for an unknown source agent", () => {
    expect(duplicateEdgeError(model(), "ghost", "builder", "ask")).toBeNull();
  });
});

describe("nextEdgeKind", () => {
  test("agent edges cycle ask -> send -> loop -> ask", () => {
    expect(nextEdgeKind("ask", true)).toBe("send");
    expect(nextEdgeKind("send", true)).toBe("loop");
    expect(nextEdgeKind("loop", true)).toBe("ask");
  });

  test("trigger edges skip loop (triggers reject it)", () => {
    expect(nextEdgeKind("ask", false)).toBe("send");
    expect(nextEdgeKind("send", false)).toBe("ask");
  });
});

describe("isProjectedEdge", () => {
  test("true only for the output projection's label", () => {
    expect(isProjectedEdge("output")).toBe(true);
    expect(isProjectedEdge("ask")).toBe(false);
    expect(isProjectedEdge("send")).toBe(false);
    expect(isProjectedEdge("loop")).toBe(false);
  });
});
