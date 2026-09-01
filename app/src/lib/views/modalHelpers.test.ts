import { describe, expect, it } from "vitest";
import { buildCreateRequest } from "./modalHelpers";

function baseForm(overrides: Partial<Parameters<typeof buildCreateRequest>[0]> = {}) {
  return {
    project: "p1", worktree: true, cwd: "", title: "", prompt: "",
    model: "", permissionMode: "default" as const, isolatedConfig: false,
    ...overrides,
  };
}

describe("buildCreateRequest", () => {
  it("omits empty-string fields entirely (key absent, not undefined-valued)", () => {
    const req = buildCreateRequest(baseForm());
    expect("cwd" in req).toBe(false);
    expect("title" in req).toBe(false);
    expect("prompt" in req).toBe(false);
    expect("model" in req).toBe(false);
    expect("permission_mode" in req).toBe(false);
    expect(req).toEqual({ project: "p1", worktree: true, isolated_config: false });
  });

  it("maps permissionMode 'default' to an absent permission_mode", () => {
    const req = buildCreateRequest(baseForm({ permissionMode: "default" }));
    expect("permission_mode" in req).toBe(false);
  });

  it("passes non-empty string fields through under their wire names", () => {
    const req = buildCreateRequest(baseForm({
      cwd: "/tmp/x", title: "fix it", prompt: "do the thing",
      model: "claude-fable-5", permissionMode: "bypassPermissions",
    }));
    expect(req).toEqual({
      project: "p1", worktree: true, isolated_config: false,
      cwd: "/tmp/x", title: "fix it", prompt: "do the thing",
      model: "claude-fable-5", permission_mode: "bypassPermissions",
    });
  });

  it("passes worktree and isolated_config booleans through as given", () => {
    expect(buildCreateRequest(baseForm({ worktree: false, isolatedConfig: true })))
      .toEqual({ project: "p1", worktree: false, isolated_config: true });
  });
});
