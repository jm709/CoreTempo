import { describe, expect, it } from "vitest";
import { loadRecents, pushRecent, RECENTS_KEY } from "./recents";

function fakeStorage(): Pick<Storage, "getItem" | "setItem"> {
  const map = new Map<string, string>();
  return {
    getItem: (k) => map.get(k) ?? null,
    setItem: (k, v) => {
      map.set(k, v);
    },
  };
}

describe("recent workflow files", () => {
  it("starts empty and survives junk", () => {
    const kv = fakeStorage();
    expect(loadRecents(kv)).toEqual([]);
    kv.setItem(RECENTS_KEY, "not json");
    expect(loadRecents(kv)).toEqual([]);
  });
  it("push is MRU-first, deduped, capped at 8", () => {
    const kv = fakeStorage();
    for (let i = 0; i < 10; i += 1) pushRecent(kv, `/w/${i}.toml`);
    pushRecent(kv, "/w/5.toml");
    const list = loadRecents(kv);
    expect(list).toHaveLength(8);
    expect(list[0]).toBe("/w/5.toml");
    expect(list.filter((p) => p === "/w/5.toml")).toHaveLength(1);
  });
});
