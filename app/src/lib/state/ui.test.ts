import { beforeEach, describe, expect, test } from "vitest";
import { openAgentTerminal, resetUi, toggleRunCenter, uiState } from "./ui.svelte";

describe("run center view", () => {
  beforeEach(() => {
    resetUi();
    uiState.editorPath = "/w/tempo.toml";
  });

  test("starts on the graph view", () => {
    expect(uiState.runCenter).toBe("graph");
  });

  test("toggleRunCenter flips between graph and terminals", () => {
    toggleRunCenter();
    expect(uiState.runCenter).toBe("terminals");
    toggleRunCenter();
    expect(uiState.runCenter).toBe("graph");
  });

  test("toggleRunCenter stays on terminals when no workflow file is open", () => {
    uiState.editorPath = null;
    toggleRunCenter();
    expect(uiState.runCenter).toBe("terminals");
    toggleRunCenter();
    expect(uiState.runCenter).toBe("terminals");
  });

  test("openAgentTerminal switches view and captures the agent", () => {
    openAgentTerminal("builder");
    expect(uiState.runCenter).toBe("terminals");
    expect(uiState.focusedAgent).toBe("builder");
    expect(uiState.captured).toBe(true);
  });

  test("resetUi returns to the graph view", () => {
    toggleRunCenter();
    resetUi();
    expect(uiState.runCenter).toBe("graph");
  });
});
