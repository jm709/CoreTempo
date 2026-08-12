import { describe, expect, it } from "vitest";
import { classifyFrameTimes, persistedRenderer, persistRenderer, RENDERER_KEY } from "./renderer";

function fakeStorage(): Pick<Storage, "getItem" | "setItem"> & { map: Map<string, string> } {
  const map = new Map<string, string>();
  return {
    map,
    getItem: (k) => map.get(k) ?? null,
    setItem: (k, v) => {
      map.set(k, v);
    },
  };
}

describe("renderer persistence and frame-time probe", () => {
  it("round-trips the persisted default and rejects junk", () => {
    const kv = fakeStorage();
    expect(persistedRenderer(kv)).toBeNull();
    persistRenderer(kv, "dom");
    expect(persistedRenderer(kv)).toBe("dom");
    expect(kv.map.get(RENDERER_KEY)).toBe("dom");
    kv.map.set(RENDERER_KEY, "garbage");
    expect(persistedRenderer(kv)).toBeNull();
  });
  it("classifies healthy GPU frame times as webgl", () => {
    expect(classifyFrameTimes(Array.from({ length: 30 }, () => 8))).toBe("webgl");
    expect(classifyFrameTimes([])).toBe("webgl"); // no data → keep the optimistic default
  });
  it("classifies software-rasterized frame times as dom (median > 33 ms)", () => {
    expect(classifyFrameTimes(Array.from({ length: 30 }, () => 55))).toBe("dom");
    // one GC hiccup among fast frames must not flip the default (median, not mean)
    expect(classifyFrameTimes([8, 8, 8, 8, 8, 8, 8, 8, 8, 400])).toBe("webgl");
  });
});
