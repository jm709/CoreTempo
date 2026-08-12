import { describe, expect, it } from "vitest";
import { gridClass } from "./gridLayout";

describe("grid auto-layout (spec §9.2: 1 full · 2 side-by-side · 3–4 2×2 · 5–6 3×2)", () => {
  it("maps agent counts to layout classes", () => {
    expect(gridClass(0)).toBe("g1");
    expect(gridClass(1)).toBe("g1");
    expect(gridClass(2)).toBe("g2");
    expect(gridClass(3)).toBe("g4");
    expect(gridClass(4)).toBe("g4");
    expect(gridClass(5)).toBe("g6");
    expect(gridClass(6)).toBe("g6");
  });
});
