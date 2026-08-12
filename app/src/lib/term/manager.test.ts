import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";

const opened: { el: unknown }[] = [];
const FIT_COLS = 56;
const FIT_ROWS = 54;

vi.mock("./xterm-chunk", () => {
  type ResizeHandler = (size: { cols: number; rows: number }) => void;
  class Terminal {
    cols = 80;
    rows = 24;
    unicode = { activeVersion: "6" };
    resizeHandler: ResizeHandler | null = null;
    loadAddon(addon: { activate?: (term: Terminal) => void }): void {
      addon.activate?.(this);
    }
    attachCustomKeyEventHandler(): void {}
    onData(): void {}
    onResize(handler: ResizeHandler): void {
      this.resizeHandler = handler;
    }
    // Mirrors xterm: resize fires onResize, and only when dimensions change.
    resize(cols: number, rows: number): void {
      if (cols === this.cols && rows === this.rows) return;
      this.cols = cols;
      this.rows = rows;
      this.resizeHandler?.({ cols, rows });
    }
    write(_data: Uint8Array, done?: () => void): void {
      done?.();
    }
    open(el: unknown): void {
      opened.push({ el });
    }
    focus(): void {}
    blur(): void {}
    dispose(): void {}
  }
  class FitAddon {
    term: Terminal | null = null;
    activate(term: Terminal): void {
      this.term = term;
    }
    fit(): void {
      this.term?.resize(FIT_COLS, FIT_ROWS);
    }
    proposeDimensions(): { cols: number; rows: number } {
      return { cols: FIT_COLS, rows: FIT_ROWS };
    }
    dispose(): void {}
  }
  class Addon {
    dispose(): void {}
    onContextLoss(): void {}
  }
  return {
    Terminal,
    FitAddon,
    SearchAddon: Addon,
    Unicode11Addon: Addon,
    WebLinksAddon: Addon,
    WebglAddon: Addon,
  };
});

const { detach, gate } = vi.hoisted(() => ({
  detach: vi.fn(),
  // Lets a test hold subscribePty pending: arm() parks the mock until release().
  gate: {
    pending: null as Promise<void> | null,
    release: () => {},
    arm(): void {
      this.pending = new Promise<void>((resolve) => {
        this.release = resolve;
      });
    },
  },
}));

vi.mock("../ipc", () => ({
  subscribePty: vi.fn(async () => {
    if (gate.pending !== null) await gate.pending;
    return detach;
  }),
  writePty: vi.fn(async () => {}),
  resizePty: vi.fn(async () => {}),
  pausePty: vi.fn(async () => {}),
}));

import { resizePty, subscribePty } from "../ipc";
import { attachTerminal, disposeAllTerminals, ensureTerminal } from "./manager";

describe("terminal attach ordering", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    opened.length = 0;
    gate.pending = null;
    // "dom" verdict: skips the WebGL addon and the frame-time probe, neither of
    // which can run outside a browser.
    vi.stubGlobal("localStorage", {
      getItem: () => "dom",
      setItem: () => {},
    });
    vi.stubGlobal(
      "ResizeObserver",
      class {
        observe(): void {}
        disconnect(): void {}
      },
    );
  });

  afterEach(() => {
    disposeAllTerminals();
    vi.unstubAllGlobals();
  });

  test("pane mounting before ensureTerminal completes still opens the terminal", async () => {
    const el = { offsetParent: null };
    // The app's real ordering: applySnapshot flips phase to "running", Svelte
    // mounts the pane (use:host -> attachTerminal) while openTerminals is still
    // awaiting the xterm chunk import and the subscribe_pty round-trip.
    const pending = ensureTerminal("builder", null, 5_000);
    attachTerminal("builder", el as unknown as HTMLElement);
    await pending;
    expect(opened.map((o) => o.el)).toEqual([el]);
  });

  test("pane mounting after ensureTerminal completes opens the terminal", async () => {
    const el = { offsetParent: null };
    await ensureTerminal("builder", null, 5_000);
    attachTerminal("builder", el as unknown as HTMLElement);
    expect(opened.map((o) => o.el)).toEqual([el]);
  });

  test("the fit on attach reaches the PTY regardless of attach order", async () => {
    // The fit() inside the attach resizes xterm; that resize event must arrive
    // after onResize is registered, or the PTY stays at its 120x40 spawn size
    // and every agent renders for the wrong width.
    const early = ensureTerminal("builder", null, 5_000);
    attachTerminal("builder", { offsetParent: null } as unknown as HTMLElement);
    await early;
    expect(vi.mocked(resizePty)).toHaveBeenCalledWith("builder", 56, 54);

    await ensureTerminal("planner", null, 5_000);
    attachTerminal("planner", { offsetParent: null } as unknown as HTMLElement);
    expect(vi.mocked(resizePty)).toHaveBeenCalledWith("planner", 56, 54);
  });

  test("disposeAllTerminals detaches the PTY channel callback", async () => {
    await ensureTerminal("builder", null, 5_000);
    expect(detach).not.toHaveBeenCalled();
    disposeAllTerminals();
    expect(detach).toHaveBeenCalledTimes(1);
  });

  test("a dispose mid-subscribe still detaches the PTY channel callback", async () => {
    // Stopping a run while openTerminals is still awaiting subscribe_pty: the
    // entry is gone by the time the detach fn arrives, so nothing would ever
    // call it and the channel closure would pin the terminal for the process.
    gate.arm();
    const pending = ensureTerminal("builder", null, 5_000);
    await vi.waitFor(() => {
      expect(vi.mocked(subscribePty)).toHaveBeenCalled();
    });
    disposeAllTerminals();
    gate.release();
    await pending;
    expect(detach).toHaveBeenCalledTimes(1);
  });
});
