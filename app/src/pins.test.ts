import { describe, expect, it } from "vitest";
import pkg from "../package.json";

describe("dependency pins", () => {
  it("every dependency is an exact version", () => {
    const all: Record<string, string> = { ...pkg.dependencies, ...pkg.devDependencies };
    for (const [name, version] of Object.entries(all)) {
      expect(version, `${name} must be pinned exactly`).toMatch(/^\d+\.\d+\.\d+$/);
    }
  });
  it("the 5.x-era canvas addon is absent (spec §15 trap)", () => {
    const all: Record<string, string> = { ...pkg.dependencies, ...pkg.devDependencies };
    expect(Object.keys(all)).not.toContain("@xterm/addon-canvas");
  });
});
