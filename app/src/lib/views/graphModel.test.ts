import { describe, expect, it, test } from "vitest";
import type { FlowModel, WorkflowModel } from "../types";
import {
  addAgent,
  addEdge,
  addFlow,
  addOutput,
  layoutPositions,
  moveEdge,
  outputNodeId,
  removeAgent,
  removeEdge,
  removeFlow,
  removeOutput,
  renameAgent,
  setEdgeKind,
  setTriggerTarget,
  toFlow,
  triggerNodeFlow,
  triggerNodeId,
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
          { to: "notifier", kind: "send" },
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
      notifier: {
        dir: "/n",
        prompt: "notify",
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

function triggered(): WorkflowModel {
  const m = model();
  m.flows = {
    main: {
      agents: ["planner", "builder", "notifier"],
      trigger: { type: "webhook", edge: { to: "planner", kind: "ask" }, message: null },
    },
  };
  return m;
}

function withOutput(): WorkflowModel {
  const m = triggered();
  m.flows["main"]!.output = { schema_file: "schema.json", max_repairs: 2 };
  return m;
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
    const trigger = nodes.find((n) => n.id === triggerNodeId("main"));
    expect(trigger?.type).toBe("trigger");
    expect(trigger?.position).toEqual({ x: 0, y: 190 });
    expect(trigger?.data.trigger?.type).toBe("webhook");
    const edge = edges.find((e) => e.id === "§trigger:main>planner:ask");
    expect(edge).toBeDefined();
    expect(edge?.source).toBe(triggerNodeId("main"));
    expect(edge?.target).toBe("planner");
    expect(edge?.label).toBe("ask");
  });

  test("emits neither node nor edge when there are no flows", () => {
    const { nodes, edges } = toFlow(model());
    expect(nodes.some((n) => n.type === "trigger")).toBe(false);
    expect(edges.some((e) => triggerNodeFlow(e.source) !== null)).toBe(false);
  });

  test("leaves the trigger out of the agent auto-layout", () => {
    const positions = layoutPositions(triggered());
    expect(positions[triggerNodeId("main")]).toBeUndefined();
    // oxlint-disable-next-line no-array-sort -- ES2022 lib; Object.keys returns a fresh array
    expect(Object.keys(positions).sort()).toEqual(["builder", "notifier", "planner"]);
  });
});

describe("toFlow output projection", () => {
  test("no output node or edge without a declaration", () => {
    const { nodes, edges } = toFlow(triggered());
    expect(nodes.some((n) => n.id === outputNodeId("main"))).toBe(false);
    expect(edges.some((e) => e.target === outputNodeId("main"))).toBe(false);
  });

  test("declared output projects an unconnectable node right of the kickoff agent", () => {
    const { nodes, edges } = toFlow(withOutput());
    const node = nodes.find((n) => n.id === outputNodeId("main"));
    expect(node?.type).toBe("output");
    expect(node?.position).toEqual({ x: 520, y: 60 });
    expect(node?.connectable).toBe(false);
    expect(node?.data.output).toEqual({ schema_file: "schema.json", max_repairs: 2 });
    expect(edges).toContainEqual({
      id: `planner>${outputNodeId("main")}:output`,
      source: "planner",
      target: outputNodeId("main"),
      label: "output",
    });
  });

  test("a dangling kickoff target falls back beside the trigger and suppresses the edge", () => {
    const m = withOutput();
    m.flows["main"]!.trigger.edge.to = "ghost";
    const { nodes, edges } = toFlow(m);
    expect(nodes.find((n) => n.id === outputNodeId("main"))?.position).toEqual({
      x: 260,
      y: 190,
    });
    expect(edges.some((e) => e.target === outputNodeId("main"))).toBe(false);
  });
});

it("toFlow renders one trigger node per flow, stacked, with per-flow edges", () => {
  const m = twoAgentModel({
    post: webhookFlow(["a"], "a"),
    classify: webhookFlow(["b"], "b"),
  });
  const { nodes, edges } = toFlow(m);
  const triggers = nodes.filter((n) => n.type === "trigger");
  expect(triggers.map((n) => n.id)).toEqual([
    triggerNodeId("classify"),
    triggerNodeId("post"),
  ]); // name order
  expect(triggers[0]!.position.y).not.toBe(triggers[1]!.position.y);
  expect(triggers.map((n) => n.data.flowName)).toEqual(["classify", "post"]);
  expect(edges.filter((e) => triggerNodeFlow(e.source) !== null).map((e) => e.target))
    .toEqual(["b", "a"]);
});

function flowWithOutput(to: string): FlowModel {
  return {
    agents: [to],
    trigger: { type: "webhook", edge: { to, kind: "ask" }, message: null },
    output: { schema_file: "s.json", max_repairs: 2 },
  };
}

it("each flow with an output renders its own output node anchored on its target", () => {
  const m = twoAgentModel({ one: flowWithOutput("a"), two: flowWithOutput("b") });
  const { nodes, edges } = toFlow(m);
  const outputs = nodes.filter((n) => n.type === "output");
  expect(outputs.map((n) => n.id)).toEqual([outputNodeId("one"), outputNodeId("two")]);
  expect(edges.filter((e) => e.label === "output").map((e) => [e.source, e.target]))
    .toEqual([["a", outputNodeId("one")], ["b", outputNodeId("two")]]);
});

it("addFlow always creates flow-N spanning the roster, skipping taken names", () => {
  const m = twoAgentModel({});
  const first = addFlow(m, "webhook");
  expect(first).toEqual({ name: "flow-1" });
  expect(m.flows["flow-1"]!.agents).toEqual(["a", "b"]);
  const second = addFlow(m, "on_start"); // NOT refused: multi-flow spec §8
  expect(second).toEqual({ name: "flow-2" });
  expect(m.flows["flow-2"]!.trigger.message).toBe("");
  expect(addFlow(twoAgentModel({}), "webhook")).toEqual({ name: "flow-1" });
});

it("addFlow with no agents reports the fix", () => {
  const empty = twoAgentModel({});
  empty.agents = {};
  const result = addFlow(empty, "webhook");
  expect("error" in result && result.error).toContain("add an agent");
});

it("removeFlow deletes exactly that flow section", () => {
  const m = twoAgentModel({ keep: webhookFlow(["a"], "a"), drop: webhookFlow(["b"], "b") });
  removeFlow(m, "drop");
  expect(Object.keys(m.flows)).toEqual(["keep"]);
});

it("setTriggerTarget is flow-scoped and keeps the target a member", () => {
  const m = twoAgentModel({ one: webhookFlow(["a"], "a"), two: webhookFlow(["b"], "b") });
  expect(setTriggerTarget(m, "one", "b")).toBeNull();
  expect(m.flows["one"]!.trigger.edge.to).toBe("b");
  expect(m.flows["one"]!.agents).toContain("b");
  expect(m.flows["two"]!.trigger.edge.to).toBe("b"); // untouched
  expect(setTriggerTarget(m, "gone", "a")).toContain("gone");
});

it("setEdgeKind on a trigger node id flips only that flow's edge", () => {
  const m = twoAgentModel({ one: webhookFlow(["a"], "a"), two: webhookFlow(["a"], "a") });
  setEdgeKind(m, triggerNodeId("one"), "a", "send");
  expect(m.flows["one"]!.trigger.edge.kind).toBe("send");
  expect(m.flows["two"]!.trigger.edge.kind).toBe("ask");
});

it("addOutput and removeOutput are flow-scoped with the phase-2 guards", () => {
  const m = twoAgentModel({ hook: webhookFlow(["a"], "a") });
  expect(addOutput(m, "hook")).toBeNull();
  expect(m.flows["hook"]!.output).toEqual({ schema_file: "", max_repairs: 2 });
  expect(addOutput(m, "hook")).toContain("already");
  removeOutput(m, "hook");
  expect(m.flows["hook"]!.output).toBeUndefined();
  m.flows["hook"]!.trigger.edge.kind = "send";
  expect(addOutput(m, "hook")).toContain("ask");
});

describe("setTriggerTarget", () => {
  test("rewires the edge, keeping the kind", () => {
    const m = triggered();
    m.flows["main"]!.trigger.edge.kind = "send";
    expect(setTriggerTarget(m, "main", "notifier")).toBeNull();
    expect(m.flows["main"]!.trigger.edge).toEqual({ to: "notifier", kind: "send" });
  });

  test("rejects an unknown target, naming the roster", () => {
    const m = triggered();
    const err = setTriggerTarget(m, "main", "ghost");
    expect(err).toContain("ghost");
    expect(err).toContain("builder");
    expect(err).toContain("notifier");
    expect(m.flows["main"]!.trigger.edge.to).toBe("planner");
  });

  test("rejects a rewire when the flow is unknown", () => {
    const m = model();
    expect(setTriggerTarget(m, "main", "planner")).toContain("no flow named");
  });
});

describe("setEdgeKind on the trigger edge", () => {
  test("flips the trigger's own kind", () => {
    const m = triggered();
    setEdgeKind(m, triggerNodeId("main"), "planner", "send");
    expect(m.flows["main"]!.trigger.edge.kind).toBe("send");
  });

  test("ignores a flip aimed at a target the trigger does not point to", () => {
    const m = triggered();
    setEdgeKind(m, triggerNodeId("main"), "builder", "send");
    expect(m.flows["main"]!.trigger.edge).toEqual({ to: "planner", kind: "ask" });
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

  test("pulls the new target into every flow the source belongs to", () => {
    const m = twoAgentModel({
      main: webhookFlow(["a"], "a"),
      other: webhookFlow(["b"], "b"),
    });
    expect(addEdge(m, "a", "b", "ask")).toBeNull();
    expect(m.flows["main"]!.agents).toEqual(["a", "b"]);
    // 'b' already belonged to 'other', and 'a' does not, so 'other' is untouched.
    expect(m.flows["other"]!.agents).toEqual(["b"]);
  });

  test("leaves membership alone when the source is not a member", () => {
    const m = twoAgentModel({ main: webhookFlow(["b"], "b") });
    expect(addEdge(m, "a", "b", "ask")).toBeNull();
    expect(m.flows["main"]!.agents).toEqual(["b"]);
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
    expect(m.agents["agent-1"]!.allow).toEqual([]);
    expect(m.agents["agent-1"]!.mcp).toEqual([]);
    expect(m.agents["agent-1"]!.concurrency).toBe("exclusive");

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

describe("AgentModel allow", () => {
  test("preserves allow through toFlow", () => {
    const m = model();
    m.agents["planner"]!.allow = ["WebSearch"];
    const { nodes } = toFlow(m);
    expect(nodes.find((n) => n.id === "planner")?.data.agent?.allow).toEqual(["WebSearch"]);
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
  test("drops a flow whose trigger is aimed at it — an off-roster target would not validate", () => {
    const m = triggered();
    removeAgent(m, "planner");
    expect(m.flows["main"]).toBeUndefined();
  });

  test("keeps a trigger aimed elsewhere", () => {
    const m = triggered();
    removeAgent(m, "notifier");
    expect(m.flows["main"]!.trigger.edge.to).toBe("planner");
  });

  test("deletes flows targeting the removed agent and strips membership elsewhere", () => {
    const m = twoAgentModel({ main: webhookFlow(["a", "b"], "a") });
    removeAgent(m, "b");
    expect(m.flows["main"]!.agents).toEqual(["a"]);
    removeAgent(m, "a");
    expect(m.flows["main"]).toBeUndefined();
  });
});

describe("addOutput / removeOutput", () => {
  test("declares the stub on a webhook+ask trigger and reports each precondition", () => {
    const ok = triggered();
    expect(addOutput(ok, "main")).toBeNull();
    expect(ok.flows["main"]!.output).toEqual({ schema_file: "", max_repairs: 2 });

    expect(addOutput(model(), "main")).toContain("no flow named");

    const onStart = model();
    onStart.flows = {
      main: {
        agents: ["planner"],
        trigger: { type: "on_start", edge: { to: "planner", kind: "ask" }, message: "go" },
      },
    };
    expect(addOutput(onStart, "main")).toContain("webhook");

    const send = triggered();
    send.flows["main"]!.trigger.edge.kind = "send";
    expect(addOutput(send, "main")).toContain("ask");

    expect(addOutput(ok, "main")).toContain("already");
  });

  test("removeOutput deletes the property; unknown flow is a no-op", () => {
    const m = withOutput();
    removeOutput(m, "main");
    expect("output" in m.flows["main"]!).toBe(false);
    removeOutput(model(), "main"); // must not throw
  });

  test("deleting the flow or its kickoff agent discards the declaration", () => {
    const viaFlow = withOutput();
    removeFlow(viaFlow, "main");
    expect(viaFlow.flows["main"]).toBeUndefined();
    const viaAgent = withOutput();
    removeAgent(viaAgent, "planner");
    expect(viaAgent.flows["main"]).toBeUndefined();
  });
});

describe("renameAgent", () => {
  test("rewrites the trigger's target", () => {
    const m = triggered();
    expect(renameAgent(m, "planner", "chief")).toBeNull();
    expect(m.flows["main"]!.trigger.edge.to).toBe("chief");
  });

  test("rewrites flow membership and targets", () => {
    const m = twoAgentModel({ main: webhookFlow(["a", "b"], "a") });
    expect(renameAgent(m, "a", "a2")).toBeNull();
    expect(m.flows["main"]!.agents).toEqual(["a2", "b"]);
    expect(m.flows["main"]!.trigger.edge.to).toBe("a2");
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
