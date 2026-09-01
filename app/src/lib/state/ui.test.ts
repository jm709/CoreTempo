import { afterEach, beforeEach, describe, expect, test } from "vitest";
import {
  closeWorkflow,
  openAgentTerminal,
  resetUi,
  runGate,
  toggleRunCenter,
  uiState,
} from "./ui.svelte";

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

describe("run gate", () => {
  beforeEach(() => {
    resetUi();
  });

  test("runs a saved workflow", () => {
    expect(runGate("stopped", "/w/tempo.toml", false)).toEqual({ disabled: false, hint: null });
  });

  test("disables Run while the editor has unsaved edits and says why", () => {
    expect(runGate("stopped", "/w/tempo.toml", true)).toEqual({
      disabled: true,
      hint: "save the workflow to run it",
    });
  });

  test("disables Run with no workflow open, silently", () => {
    expect(runGate("stopped", null, false)).toEqual({ disabled: true, hint: null });
  });

  test("disables the button while a run is starting or stopping", () => {
    expect(runGate("starting", "/w/tempo.toml", false).disabled).toBe(true);
    expect(runGate("stopping", "/w/tempo.toml", false).disabled).toBe(true);
  });

  test("unsaved edits never block Stop", () => {
    expect(runGate("running", "/w/tempo.toml", true)).toEqual({ disabled: false, hint: null });
  });

  test("resetUi clears the dirty flag", () => {
    uiState.editorDirty = true;
    resetUi();
    expect(uiState.editorDirty).toBe(false);
  });
});

describe("mode (chrome, not run state)", () => {
  afterEach(() => {
    uiState.mode = "workflows";
  });

  test("defaults to workflows", () => {
    expect(uiState.mode).toBe("workflows");
  });

  test("resetUi does not touch mode", () => {
    uiState.mode = "sessions";
    resetUi();
    expect(uiState.mode).toBe("sessions");
  });
});

describe("closeWorkflow", () => {
  beforeEach(() => {
    resetUi();
    uiState.editorPath = "/w/tempo.toml";
  });

  test("returns to the no-workflow card and forgets edits", () => {
    uiState.editorDirty = true;
    uiState.runCenter = "terminals";
    closeWorkflow();
    expect(uiState.editorPath).toBeNull();
    expect(uiState.editorDirty).toBe(false);
    expect(uiState.runCenter).toBe("graph");
  });

  test("leaves the dock and terminal focus alone", () => {
    uiState.dockTab = "chat";
    uiState.focusedAgent = "builder";
    closeWorkflow();
    expect(uiState.dockTab).toBe("chat");
    expect(uiState.focusedAgent).toBe("builder");
  });
});
