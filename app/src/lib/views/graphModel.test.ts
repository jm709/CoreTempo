import { describe, expect, test } from "vitest";
import type { WorkflowModel } from "../types";
import {
  addAgent,
  addEdge,
  addOutput,
  addTrigger,
  layoutPositions,
  moveEdge,
  OUTPUT_NODE_ID,
  removeAgent,
  removeEdge,
  removeOutput,
  removeTrigger,
  renameAgent,
  setEdgeKind,
  setTriggerTarget,
  toFlow,
  TRIGGER_NODE_ID,
  WORKFLOW_NODE_ID,
} from "./graphModel";

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
          { to: "notifier", kind: "send" },
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
      notifier: {
        dir: "/n",
        prompt: "notify",
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

function triggered(): WorkflowModel {
  const m = model();
  m.trigger = { type: "webhook", edge: { to: "planner", kind: "ask" }, message: null };
  return m;
}

function withOutput(): WorkflowModel {
  const m = triggered();
  m.trigger!.output = { schema_file: "schema.json", max_repairs: 2 };
  return m;
}

describe("toFlow", () => {
  test("emits one workflow node and one node per agent", () => {
    const { nodes } = toFlow(model());
    expect(nodes).toHaveLength(4);
    expect(nodes.filter((n) => n.type === "workflow")).toHaveLength(1);
    expect(nodes.find((n) => n.id === WORKFLOW_NODE_ID)?.type).toBe("workflow");
    const agentIds = nodes.filter((n) => n.type === "agent").map((n) => n.id);
    // oxlint-disable-next-line no-array-sort -- ES2022 lib; agentIds is a fresh local array
    expect(agentIds.sort()).toEqual(["builder", "notifier", "planner"]);
  });

  test("emits edges with matching source, target, and label", () => {
    const { edges } = toFlow(model());
    expect(edges).toHaveLength(2);
    const ask = edges.find((e) => e.id === "planner>builder:ask");
    expect(ask).toBeDefined();
    expect(ask?.source).toBe("planner");
    expect(ask?.target).toBe("builder");
    expect(ask?.label).toBe("ask");
    const send = edges.find((e) => e.id === "planner>notifier:send");
    expect(send).toBeDefined();
    expect(send?.source).toBe("planner");
    expect(send?.target).toBe("notifier");
    expect(send?.label).toBe("send");
  });

  test("positions agent nodes per layoutPositions and fixes the workflow node", () => {
    const m = model();
    const { nodes } = toFlow(m);
    const positions = layoutPositions(m);
    for (const node of nodes.filter((n) => n.type === "agent")) {
      expect(node.position).toEqual(positions[node.id]);
    }
    const planner = nodes.find((n) => n.id === "planner");
    const builder = nodes.find((n) => n.id === "builder");
    expect(planner?.position.x).toBeLessThan(builder!.position.x);
    expect(nodes.find((n) => n.id === WORKFLOW_NODE_ID)?.position).toEqual({ x: 0, y: 60 });
  });
});

describe("toFlow with a trigger", () => {
  test("emits the trigger node below the workflow node plus its edge", () => {
    const { nodes, edges } = toFlow(triggered());
    const trigger = nodes.find((n) => n.id === TRIGGER_NODE_ID);
    expect(trigger?.type).toBe("trigger");
    expect(trigger?.position).toEqual({ x: 0, y: 190 });
    expect(trigger?.data.trigger?.type).toBe("webhook");
    const edge = edges.find((e) => e.id === "§trigger>planner:ask");
    expect(edge).toBeDefined();
    expect(edge?.source).toBe(TRIGGER_NODE_ID);
    expect(edge?.target).toBe("planner");
    expect(edge?.label).toBe("ask");
  });

  test("emits neither node nor edge when trigger is null", () => {
    const { nodes, edges } = toFlow(model());
    expect(nodes.find((n) => n.id === TRIGGER_NODE_ID)).toBeUndefined();
    expect(edges.some((e) => e.source === TRIGGER_NODE_ID)).toBe(false);
  });

  test("leaves the trigger out of the agent auto-layout", () => {
    const positions = layoutPositions(triggered());
    expect(positions[TRIGGER_NODE_ID]).toBeUndefined();
    // oxlint-disable-next-line no-array-sort -- ES2022 lib; Object.keys returns a fresh array
    expect(Object.keys(positions).sort()).toEqual(["builder", "notifier", "planner"]);
  });
});

describe("toFlow output projection", () => {
  test("no output node or edge without a declaration", () => {
    const { nodes, edges } = toFlow(triggered());
    expect(nodes.some((n) => n.id === OUTPUT_NODE_ID)).toBe(false);
    expect(edges.some((e) => e.target === OUTPUT_NODE_ID)).toBe(false);
  });

  test("declared output projects an unconnectable node right of the kickoff agent", () => {
    const { nodes, edges } = toFlow(withOutput());
    const node = nodes.find((n) => n.id === OUTPUT_NODE_ID);
    expect(node?.type).toBe("output");
    expect(node?.position).toEqual({ x: 520, y: 60 });
    expect(node?.connectable).toBe(false);
    expect(node?.data.output).toEqual({ schema_file: "schema.json", max_repairs: 2 });
    expect(edges).toContainEqual({
      id: "planner>§output:output",
      source: "planner",
      target: OUTPUT_NODE_ID,
      label: "output",
    });
  });

  test("a dangling kickoff target falls back beside the trigger and suppresses the edge", () => {
    const m = withOutput();
    m.trigger!.edge.to = "ghost";
    const { nodes, edges } = toFlow(m);
    expect(nodes.find((n) => n.id === OUTPUT_NODE_ID)?.position).toEqual({ x: 260, y: 190 });
    expect(edges.some((e) => e.target === OUTPUT_NODE_ID)).toBe(false);
  });
});

describe("addTrigger", () => {
  test("on_start seeds an empty message and an ask edge to the first agent", () => {
    const m = model();
    expect(addTrigger(m, "on_start")).toBeNull();
    expect(m.trigger).toEqual({
      type: "on_start",
      edge: { to: "builder", kind: "ask" },
      message: "",
    });
  });

  test("webhook seeds a null message", () => {
    const m = model();
    expect(addTrigger(m, "webhook")).toBeNull();
    expect(m.trigger?.message).toBeNull();
  });

  test("a second trigger is refused, naming the one-trigger rule", () => {
    const m = triggered();
    const err = addTrigger(m, "on_start");
    expect(err).toContain("one trigger");
    expect(m.trigger?.type).toBe("webhook");
  });

  test("an empty roster is refused rather than pointing the edge at nothing", () => {
    const m = model();
    m.agents = {};
    const err = addTrigger(m, "webhook");
    expect(err).toContain("no agents");
    expect(m.trigger).toBeNull();
  });
});

describe("setTriggerTarget", () => {
  test("rewires the edge, keeping the kind", () => {
    const m = triggered();
    m.trigger!.edge.kind = "send";
    expect(setTriggerTarget(m, "notifier")).toBeNull();
    expect(m.trigger?.edge).toEqual({ to: "notifier", kind: "send" });
  });

  test("rejects an unknown target, naming the roster", () => {
    const m = triggered();
    const err = setTriggerTarget(m, "ghost");
    expect(err).toContain("ghost");
    expect(err).toContain("builder");
    expect(err).toContain("notifier");
    expect(m.trigger?.edge.to).toBe("planner");
  });

  test("rejects a rewire when no trigger exists", () => {
    const m = model();
    expect(setTriggerTarget(m, "planner")).toContain("no trigger");
  });
});

describe("removeTrigger", () => {
  test("clears the trigger and leaves the agents alone", () => {
    const m = triggered();
    removeTrigger(m);
    expect(m.trigger).toBeNull();
    expect(Object.keys(m.agents)).toHaveLength(3);
  });
});

describe("setEdgeKind on the trigger edge", () => {
  test("flips the trigger's own kind", () => {
    const m = triggered();
    setEdgeKind(m, TRIGGER_NODE_ID, "planner", "send");
    expect(m.trigger?.edge.kind).toBe("send");
  });

  test("ignores a flip aimed at a target the trigger does not point to", () => {
    const m = triggered();
    setEdgeKind(m, TRIGGER_NODE_ID, "builder", "send");
    expect(m.trigger?.edge).toEqual({ to: "planner", kind: "ask" });
  });
});

describe("layoutPositions", () => {
  test("puts roots left of their targets, same-rank nodes share x with distinct y", () => {
    const positions = layoutPositions(model());
    expect(positions["planner"]).toBeDefined();
    expect(positions["builder"]).toBeDefined();
    expect(positions["notifier"]).toBeDefined();
    expect(positions["planner"]!.x).toBeLessThan(positions["builder"]!.x);
    expect(positions["builder"]!.x).toBe(positions["notifier"]!.x);
    expect(positions["builder"]!.y).not.toBe(positions["notifier"]!.y);
  });

  test("a cycle still terminates and assigns every node a position", () => {
    const m = model();
    m.agents["builder"]!.edges.push({ to: "planner", kind: "ask" });
    const positions = layoutPositions(m);
    // oxlint-disable-next-line no-array-sort -- ES2022 lib; Object.keys returns a fresh array
    expect(Object.keys(positions).sort()).toEqual(["builder", "notifier", "planner"]);
    for (const id of ["planner", "builder", "notifier"]) {
      expect(positions[id]).toBeDefined();
      expect(Number.isFinite(positions[id]!.x)).toBe(true);
      expect(Number.isFinite(positions[id]!.y)).toBe(true);
    }
  });
});

describe("addEdge", () => {
  test("appends in order", () => {
    const m = model();
    expect(addEdge(m, "builder", "notifier", "ask")).toBeNull();
    expect(addEdge(m, "builder", "planner", "send")).toBeNull();
    expect(m.agents["builder"]!.edges).toEqual([
      { to: "notifier", kind: "ask" },
      { to: "planner", kind: "send" },
    ]);
  });

  test("rejects an unknown target, mentioning the roster", () => {
    const m = model();
    const err = addEdge(m, "planner", "ghost", "ask");
    expect(err).not.toBeNull();
    expect(err).toContain("ghost");
    expect(err).toContain("planner");
    expect(err).toContain("builder");
    expect(err).toContain("notifier");
  });

  test("rejects a self-edge", () => {
    const m = model();
    const err = addEdge(m, "planner", "planner", "ask");
    expect(err).not.toBeNull();
    expect(err).toContain("itself");
  });

  test("rejects a duplicate (to, kind) pair", () => {
    const m = model();
    const err = addEdge(m, "planner", "builder", "ask");
    expect(err).not.toBeNull();
    expect(err).toContain("duplicate");
  });

  test("allows the same target with a different kind", () => {
    const m = model();
    expect(addEdge(m, "planner", "builder", "send")).toBeNull();
    expect(m.agents["planner"]!.edges).toContainEqual({ to: "builder", kind: "send" });
  });
});

describe("setEdgeKind", () => {
  test("flips the kind in place, preserving order", () => {
    const m = model();
    setEdgeKind(m, "planner", "builder", "send");
    expect(m.agents["planner"]!.edges).toEqual([
      { to: "builder", kind: "send" },
      { to: "notifier", kind: "send" },
    ]);
  });
});

describe("removeEdge", () => {
  test("removes exactly the matching pair", () => {
    const m = model();
    removeEdge(m, "planner", "builder", "ask");
    expect(m.agents["planner"]!.edges).toEqual([{ to: "notifier", kind: "send" }]);
  });
});

describe("moveEdge", () => {
  test("swaps two adjacent edges", () => {
    const m = model();
    moveEdge(m, "planner", 1, -1);
    expect(m.agents["planner"]!.edges).toEqual([
      { to: "notifier", kind: "send" },
      { to: "builder", kind: "ask" },
    ]);
  });

  test("out-of-range deltas no-op", () => {
    const m = model();
    const before = [...m.agents["planner"]!.edges];
    moveEdge(m, "planner", 0, -1);
    expect(m.agents["planner"]!.edges).toEqual(before);
    moveEdge(m, "planner", 1, 1);
    expect(m.agents["planner"]!.edges).toEqual(before);
  });
});

describe("addAgent", () => {
  test("returns agent-1, then agent-2, with template defaults", () => {
    const m = model();
    const first = addAgent(m);
    expect(first).toBe("agent-1");
    expect(m.agents["agent-1"]).toBeDefined();
    expect(m.agents["agent-1"]!.auto_clear).toBe(true);
    expect(m.agents["agent-1"]!.edges).toEqual([]);
    expect(m.agents["agent-1"]!.tools).toEqual([]);

    const second = addAgent(m);
    expect(second).toBe("agent-2");
    expect(m.agents["agent-2"]).toBeDefined();
  });
});

describe("AgentModel tools", () => {
  test("preserves tools through toFlow", () => {
    const m = model();
    m.agents["planner"]!.tools = ["pat"];
    const { nodes } = toFlow(m);
    const planner = nodes.find((n) => n.id === "planner");
    expect(planner?.data.agent?.tools).toEqual(["pat"]);
  });
});

describe("removeAgent", () => {
  test("removes the agent and strips planner's edge to it", () => {
    const m = model();
    removeAgent(m, "builder");
    expect(m.agents["builder"]).toBeUndefined();
    expect(m.agents["planner"]!.edges).toEqual([{ to: "notifier", kind: "send" }]);
  });
});

describe("removeAgent with a trigger", () => {
  test("drops a trigger aimed at it — an off-roster target would not validate", () => {
    const m = triggered();
    removeAgent(m, "planner");
    expect(m.trigger).toBeNull();
  });

  test("keeps a trigger aimed elsewhere", () => {
    const m = triggered();
    removeAgent(m, "notifier");
    expect(m.trigger?.edge.to).toBe("planner");
  });
});

describe("addOutput / removeOutput", () => {
  test("declares the stub on a webhook+ask trigger and reports each precondition", () => {
    const ok = triggered();
    expect(addOutput(ok)).toBeNull();
    expect(ok.trigger!.output).toEqual({ schema_file: "", max_repairs: 2 });

    expect(addOutput(model())).toContain("no trigger");

    const onStart = model();
    onStart.trigger = { type: "on_start", edge: { to: "planner", kind: "ask" }, message: "go" };
    expect(addOutput(onStart)).toContain("webhook");

    const send = triggered();
    send.trigger!.edge.kind = "send";
    expect(addOutput(send)).toContain("ask");

    expect(addOutput(ok)).toContain("already");
  });

  test("removeOutput deletes the property; absent trigger is a no-op", () => {
    const m = withOutput();
    removeOutput(m);
    expect("output" in m.trigger!).toBe(false);
    removeOutput(model()); // must not throw
  });

  test("deleting the trigger or its kickoff agent discards the declaration", () => {
    const viaTrigger = withOutput();
    removeTrigger(viaTrigger);
    expect(viaTrigger.trigger).toBeNull();
    const viaAgent = withOutput();
    removeAgent(viaAgent, "planner");
    expect(viaAgent.trigger).toBeNull();
  });
});

describe("renameAgent", () => {
  test("rewrites the trigger's target", () => {
    const m = triggered();
    expect(renameAgent(m, "planner", "chief")).toBeNull();
    expect(m.trigger?.edge.to).toBe("chief");
  });

  test("moves the config and rewrites planner's edge target", () => {
    const m = model();
    const err = renameAgent(m, "builder", "smith");
    expect(err).toBeNull();
    expect(m.agents["builder"]).toBeUndefined();
    expect(m.agents["smith"]).toBeDefined();
    expect(m.agents["smith"]!.dir).toBe("/b");
    expect(m.agents["planner"]!.edges).toContainEqual({ to: "smith", kind: "ask" });
  });

  test("rejects an invalid id, naming the rule", () => {
    const m = model();
    expect(renameAgent(m, "builder", "Builder2")).not.toBeNull();
    expect(renameAgent(m, "builder", "-abc")).not.toBeNull();
    expect(renameAgent(m, "builder", "a".repeat(33))).not.toBeNull();
  });

  test("rejects a collision with an existing agent", () => {
    const m = model();
    const err = renameAgent(m, "builder", "planner");
    expect(err).not.toBeNull();
    expect(err).toContain("planner");
  });
});
