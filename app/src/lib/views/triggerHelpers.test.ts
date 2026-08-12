import { describe, expect, it, test } from "vitest";
import { classify, inlineSchemaSummary, outputPreview } from "./triggerHelpers";

describe("classify", () => {
  it("renders a flat object as a card of scalars", () => {
    const node = classify({ title: "Draft", count: 3, ready: true, note: null });
    expect(node.kind).toBe("card");
    if (node.kind !== "card") return;
    expect(node.entries.map((e) => e.key)).toEqual(["title", "count", "ready", "note"]);
    expect(node.entries[1]?.value).toEqual({ kind: "scalar", text: "3" });
    expect(node.entries[3]?.value).toEqual({ kind: "scalar", text: "—" });
  });

  it("renders an array of objects as a table with the column union", () => {
    const node = classify([{ a: 1, b: 2 }, { b: 3, c: { deep: true } }]);
    expect(node).toEqual({
      kind: "table",
      columns: ["a", "b", "c"],
      rows: [["1", "2", "—"], ["—", "3", '{"deep":true}']],
    });
  });

  it("renders scalar arrays as lists and long strings as prose", () => {
    expect(classify(["x", "y"])).toEqual({
      kind: "list",
      items: [{ kind: "scalar", text: "x" }, { kind: "scalar", text: "y" }],
    });
    expect(classify("line one\nline two").kind).toBe("prose");
    expect(classify("a".repeat(121)).kind).toBe("prose");
    expect(classify("short").kind).toBe("scalar");
  });

  it("falls back to pretty JSON past the depth cap and for mixed arrays", () => {
    const deep = { a: { b: { c: { d: 1 } } } };
    const node = classify(deep);
    expect(JSON.stringify(node)).toContain('"json"');
    expect(classify([1, { a: 2 }]).kind).toBe("json");
  });

  it("handles empty containers", () => {
    expect(classify({})).toEqual({ kind: "scalar", text: "(empty)" });
    expect(classify([])).toEqual({ kind: "scalar", text: "(empty)" });
  });
});

describe("outputPreview", () => {
  test("flat object renders key: value lines", () => {
    expect(outputPreview({ status: "ok", count: 3 })).toEqual(["status: ok", "count: 3"]);
  });

  test("caps at 4 keys and counts the rest", () => {
    const output = { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6 };
    expect(outputPreview(output)).toEqual(["a: 1", "b: 2", "c: 3", "d: 4", "+2 more"]);
  });

  test("values truncate to their first line at 40 chars with an ellipsis", () => {
    const long = "x".repeat(50);
    expect(outputPreview({ note: long })).toEqual([`note: ${"x".repeat(40)}…`]);
    expect(outputPreview({ note: "line one\nline two" })).toEqual(["note: line one"]);
  });

  test("non-object and empty outputs degrade to one line", () => {
    expect(outputPreview("done")).toEqual(["done"]);
    expect(outputPreview([1, 2])).toEqual(["[1,2]"]);
    expect(outputPreview({})).toEqual(["(empty)"]);
  });
});

describe("inlineSchemaSummary", () => {
  test("counts top-level keys of an object schema", () => {
    expect(inlineSchemaSummary({ type: "object", required: ["a"] }))
      .toBe("inline schema — 2 top-level keys");
  });

  test("non-object schemas fall back to the bare label", () => {
    expect(inlineSchemaSummary(true)).toBe("inline schema");
  });
});
