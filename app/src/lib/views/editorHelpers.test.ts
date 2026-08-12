import { describe, expect, it } from "vitest";
import { plannedRoster } from "./editorHelpers";

const SAMPLE = `
[workflow]
name = "core-tempo-dev"

[agents.planner]
dir = "~/p"
prompt = "plan"

[agents.builder]   # trailing comment
dir = "~/p"

[agents.builder]
dir = "~/dup"

[agents.planner.extra]
nested = true

[server]
log = "info"
`;

describe("plannedRoster", () => {
  it("extracts agent table headers, deduped and sorted", () => {
    expect(plannedRoster(SAMPLE)).toEqual(["builder", "planner"]);
  });
  it("ignores nested agent sub-tables and non-agent sections", () => {
    expect(plannedRoster("[agents.a.b]\n[server]\n")).toEqual([]);
  });
  it("returns empty for empty text", () => {
    expect(plannedRoster("")).toEqual([]);
  });
});
