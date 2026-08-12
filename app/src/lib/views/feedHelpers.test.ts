import { describe, expect, it } from "vitest";
import { isAtBottom } from "./feedHelpers";

describe("stick-to-bottom detection", () => {
  it("is at bottom when offset + viewport reaches scrollSize within slack", () => {
    expect(isAtBottom(900, 100, 1000)).toBe(true);
    expect(isAtBottom(894, 100, 1000)).toBe(true); // within default 8 px slack
    expect(isAtBottom(600, 100, 1000)).toBe(false);
  });
  it("short lists are always at bottom", () => {
    expect(isAtBottom(0, 500, 200)).toBe(true);
  });
});
