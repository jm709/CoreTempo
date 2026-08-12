import { describe, expect, it } from "vitest";
import { askQueued, askReplied, askWorking, sendFailed, sendQueued } from "./fixtures/recorded";
import {
  elapsed, feedTime, isChat, isExternal, lifecycleGlyph, originAgent, originLabel,
  STATE_GLYPHS, stateLabel,
} from "./format";

describe("status glyphs (spec §9.3, exact characters)", () => {
  it("maps agent states to glyphs", () => {
    expect(STATE_GLYPHS.working).toBe("●");
    expect(STATE_GLYPHS.idle).toBe("◌");
    expect(STATE_GLYPHS.starting).toBe("◐");
    expect(STATE_GLYPHS.restarting).toBe("◐");
    expect(STATE_GLYPHS.exited).toBe("✕");
  });
  it("labels exited as dead everywhere in the UI", () => {
    expect(stateLabel("exited")).toBe("dead");
    expect(stateLabel("working")).toBe("working");
  });
  it("maps message lifecycle ○ → ⟳ ∅0|∅1 ✓ ✕", () => {
    expect(lifecycleGlyph(askQueued)).toBe("○");
    expect(lifecycleGlyph({ ...askQueued, status: "injected" })).toBe("→");
    expect(lifecycleGlyph(askWorking)).toBe("⟳");
    expect(lifecycleGlyph(askReplied)).toBe("∅0");
    expect(lifecycleGlyph({ ...askReplied, code: 1 })).toBe("∅1");
    expect(lifecycleGlyph({ ...sendQueued, status: "done" })).toBe("✓");
    expect(lifecycleGlyph(sendFailed)).toBe("✕");
  });
});

describe("origins and times", () => {
  it("renders origin labels", () => {
    expect(originLabel("agent:planner")).toBe("planner");
    expect(originLabel("user")).toBe("you");
    expect(originLabel("http:1f2e3d4c")).toBe("external");
  });
  it("extracts the agent from an agent origin only", () => {
    expect(originAgent("agent:planner")).toBe("planner");
    expect(originAgent("user")).toBeNull();
    expect(originAgent("http:1f2e3d4c")).toBeNull();
  });
  it("flags external origins and chat traffic", () => {
    expect(isExternal("http:1f2e3d4c")).toBe(true);
    expect(isExternal("agent:planner")).toBe(false);
    expect(isChat({ ...askQueued, from: "user" })).toBe(true);
    expect(isChat(askQueued)).toBe(false);
  });
  it("formats feed times and run elapsed", () => {
    expect(feedTime("2026-08-01T17:03:11Z")).toBe("17:03:11");
    const t0 = "2026-08-01T17:00:00Z";
    expect(elapsed(t0, Date.parse("2026-08-01T17:14:02Z"))).toBe("14m 02s");
    expect(elapsed(t0, Date.parse("2026-08-01T18:04:30Z"))).toBe("1h 04m");
    expect(elapsed(t0, Date.parse("2026-08-01T16:59:00Z"))).toBe("0m 00s");
  });
});
