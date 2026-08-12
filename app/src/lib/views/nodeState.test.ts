import { describe, expect, test } from "vitest";
import { nodeTint } from "./nodeState";

describe("nodeTint", () => {
  test("working agents tint busy", () => {
    expect(nodeTint("working")).toBe("busy");
  });

  test("starting and restarting agents tint info", () => {
    expect(nodeTint("starting")).toBe("info");
    expect(nodeTint("restarting")).toBe("info");
  });

  test("exited agents tint err", () => {
    expect(nodeTint("exited")).toBe("err");
  });

  test("idle and absent agents keep default chrome", () => {
    expect(nodeTint("idle")).toBeNull();
    expect(nodeTint(undefined)).toBeNull();
  });
});
