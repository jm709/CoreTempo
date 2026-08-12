import { describe, expect, test } from "vitest";
import type { WorkflowModel } from "../types";
import {
  coerceNumber, connectAgents, duplicateEdgeError, freeSlot, isProjectedEdge, nextEdgeKind,
} from "./graphEditing";
import { OUTPUT_NODE_ID, TRIGGER_NODE_ID, WORKFLOW_NODE_ID } from "./graphModel";

function model(): WorkflowModel {
  return {
    workflow: {
      name: "t",
      db: "./tempo.db",
      port: 4820,
      ask_timeout_minutes: 30,
      idle_debounce_seconds: 2,
    },
    server: { bind: null, token_file: null, allowed_origins: [], log: null },
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
      },
      builder: {
        dir: "/b",
        prompt: "build",
        model: null,
        permission_mode: null,
        auto_clear: true,
        edges: [],
        tools: [],
      },
    },
    trigger: null,
  };
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
    m.trigger = { type: "webhook", edge: { to: "planner", kind: "send" }, message: null };
    expect(connectAgents(m, TRIGGER_NODE_ID, "builder")).toBeNull();
    expect(m.trigger.edge).toEqual({ to: "builder", kind: "send" });
    expect(m.agents["planner"]!.edges).toHaveLength(2); // agent edges untouched
  });

  test("refuses a connection touching the trigger node from the wrong side", () => {
    const m = model();
    m.trigger = { type: "webhook", edge: { to: "planner", kind: "ask" }, message: null };
    expect(connectAgents(m, TRIGGER_NODE_ID, WORKFLOW_NODE_ID)).toContain("is not an agent");
    expect(connectAgents(m, "planner", TRIGGER_NODE_ID)).toContain("has no inbound edges");
    expect(m.trigger.edge.to).toBe("planner");
  });

  test("connections touching the output node are refused without mutating", () => {
    const m = model();
    m.trigger = {
      type: "webhook", edge: { to: "planner", kind: "ask" }, message: null,
      output: { schema_file: "s.json", max_repairs: 2 },
    };
    expect(connectAgents(m, "planner", OUTPUT_NODE_ID)).toContain("[trigger.output]");
    expect(connectAgents(m, OUTPUT_NODE_ID, "planner")).toContain("[trigger.output]");
    expect(m.agents["planner"]!.edges).toHaveLength(2);
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
