import { describe, expect, it } from "vitest";
import { modLabel, resolveKey } from "./keys";

function ev(key: string, opts: Partial<{ meta: boolean; ctrl: boolean; alt: boolean }> = {}) {
  return { key, metaKey: opts.meta ?? false, ctrlKey: opts.ctrl ?? false, altKey: opts.alt ?? false };
}

describe("resolveKey (spec §9.2 focus model)", () => {
  it("mod+1..9 focuses terminals by index", () => {
    expect(resolveKey(ev("1", { meta: true }), true)).toEqual({ kind: "focus-terminal", index: 0 });
    expect(resolveKey(ev("9", { meta: true }), true)).toEqual({ kind: "focus-terminal", index: 8 });
    expect(resolveKey(ev("3", { ctrl: true }), false)).toEqual({ kind: "focus-terminal", index: 2 });
  });
  it("mod+` releases capture — never Esc (Claude Code uses it)", () => {
    expect(resolveKey(ev("`", { meta: true }), true)).toEqual({ kind: "release" });
    expect(resolveKey(ev("Escape", { meta: true }), true)).toBeNull();
    expect(resolveKey(ev("Escape"), true)).toBeNull();
  });
  it("app-scope chords F/T/E/R and Enter", () => {
    expect(resolveKey(ev("f", { meta: true }), true)).toEqual({ kind: "dock-feed" });
    expect(resolveKey(ev("t", { meta: true }), true)).toEqual({ kind: "dock-chat" });
    expect(resolveKey(ev("e", { meta: true }), true)).toEqual({ kind: "edit-workflow" });
    expect(resolveKey(ev("r", { meta: true }), true)).toEqual({ kind: "restart-focused" });
    expect(resolveKey(ev("Enter", { meta: true }), true)).toEqual({ kind: "toggle-maximize" });
  });
  it("wrong modifier / plain keys / alt combos pass through to the terminal", () => {
    expect(resolveKey(ev("1", { ctrl: true }), true)).toBeNull();  // ctrl on mac ≠ mod
    expect(resolveKey(ev("f"), true)).toBeNull();
    expect(resolveKey(ev("f", { meta: true, alt: true }), true)).toBeNull();
    expect(resolveKey(ev("x", { meta: true }), true)).toBeNull();
  });
  it("labels the modifier per platform", () => {
    expect(modLabel(true)).toBe("⌘");
    expect(modLabel(false)).toBe("Ctrl+");
  });
});
