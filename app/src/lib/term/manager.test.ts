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

import { createTerminalManager, type TerminalManager, type TermTransport } from "./manager";

/// The manager parks each terminal in a wrapper div it re-parents between panes,
/// so the tests need enough of a DOM to observe parentage. `environment: "node"`
/// gives us none: these are the three operations the manager performs.
interface FakeNode {
  style: { cssText: string };
  parent: FakeEl | null;
  remove(): void;
}

interface FakeEl {
  offsetParent: null;
  children: FakeNode[];
  append(child: FakeNode): void;
}

function fakeNode(): FakeNode {
  const node: FakeNode = {
    style: { cssText: "" },
    parent: null,
    remove(): void {
      if (node.parent === null) return;
      node.parent.children = node.parent.children.filter((c) => c !== node);
      node.parent = null;
    },
  };
  return node;
}

function fakeEl(): FakeEl {
  const el: FakeEl = {
    offsetParent: null,
    children: [],
    append(child: FakeNode): void {
      child.remove();
      child.parent = el;
      el.children.push(child);
    },
  };
  return el;
}

function asEl(el: FakeEl): HTMLElement {
  return el as unknown as HTMLElement;
}

interface Harness {
  transport: TermTransport;
  subscribe: ReturnType<typeof vi.fn>;
  resize: ReturnType<typeof vi.fn>;
  detach: ReturnType<typeof vi.fn>;
  // Lets a test hold subscribe pending: arm() parks the mock until release().
  gate: { pending: Promise<void> | null; release: () => void; arm: () => void };
}

function harness(): Harness {
  const detach = vi.fn();
  const gate = {
    pending: null as Promise<void> | null,
    release: () => {},
    arm(): void {
      this.pending = new Promise<void>((resolve) => {
        this.release = resolve;
      });
    },
  };
  const subscribe = vi.fn(
    async (
      _id: string,
      _since: number | null,
      _resume: boolean,
      _onChunk: (bytes: Uint8Array) => void,
    ): Promise<() => void> => {
      if (gate.pending !== null) await gate.pending;
      return detach;
    },
  );
  const resize = vi.fn(async () => {});
  const transport: TermTransport = {
    subscribe,
    write: vi.fn(async () => {}),
    resize,
    pause: vi.fn(async () => {}),
  };
  return { transport, subscribe, resize, detach, gate };
}

describe("terminal manager", () => {
  const live: TerminalManager[] = [];

  function manager(t: TermTransport): TerminalManager {
    const m = createTerminalManager(t);
    live.push(m);
    return m;
  }

  beforeEach(() => {
    vi.clearAllMocks();
    opened.length = 0;
    // "dom" verdict: skips the WebGL addon and the frame-time probe, neither of
    // which can run outside a browser.
    vi.stubGlobal("localStorage", {
      getItem: () => "dom",
      setItem: () => {},
    });
    vi.stubGlobal("document", { createElement: () => fakeNode() });
    vi.stubGlobal(
      "ResizeObserver",
      class {
        observe(): void {}
        disconnect(): void {}
      },
    );
  });

  afterEach(() => {
    for (const m of live) m.disposeAll();
    live.length = 0;
    vi.unstubAllGlobals();
  });

  test("pane mounting before ensure completes still opens the terminal", async () => {
    const h = harness();
    const term = manager(h.transport);
    const el = fakeEl();
    // The app's real ordering: applySnapshot flips phase to "running", Svelte
    // mounts the pane (use:host -> attach) while openTerminals is still awaiting
    // the xterm chunk import and the subscribe round-trip.
    const pending = term.ensure("builder", null, 5_000);
    term.attach("builder", asEl(el));
    await pending;
    expect(el.children).toHaveLength(1);
    expect(opened.map((o) => o.el)).toEqual(el.children);
  });

  test("pane mounting after ensure completes opens the terminal", async () => {
    const h = harness();
    const term = manager(h.transport);
    const el = fakeEl();
    await term.ensure("builder", null, 5_000);
    term.attach("builder", asEl(el));
    expect(el.children).toHaveLength(1);
    expect(opened.map((o) => o.el)).toEqual(el.children);
  });

  test("attaching to a second element re-parents the terminal", async () => {
    // The sessions center reuses one pane element for every session, so a
    // terminal is attached to an element it was not opened in. xterm's open()
    // returns early once the terminal has an element, so the wrapper moves.
    const h = harness();
    const term = manager(h.transport);
    const first = fakeEl();
    const second = fakeEl();
    await term.ensure("s-1", null, 5_000);
    term.attach("s-1", asEl(first));
    const wrapper = first.children[0];
    term.attach("s-1", asEl(second));
    expect(first.children).toHaveLength(0);
    expect(second.children).toEqual([wrapper]);
    expect(opened).toHaveLength(1);
  });

  test("the fit on attach reaches the PTY regardless of attach order", async () => {
    // The fit() inside the attach resizes xterm; that resize event must arrive
    // after onResize is registered, or the PTY stays at its 120x40 spawn size
    // and every agent renders for the wrong width.
    const h = harness();
    const term = manager(h.transport);
    const early = term.ensure("builder", null, 5_000);
    term.attach("builder", asEl(fakeEl()));
    await early;
    expect(h.resize).toHaveBeenCalledWith("builder", 56, 54);

    await term.ensure("planner", null, 5_000);
    term.attach("planner", asEl(fakeEl()));
    expect(h.resize).toHaveBeenCalledWith("planner", 56, 54);
  });

  test("ensure subscribes without resume", async () => {
    const h = harness();
    const term = manager(h.transport);
    await term.ensure("builder", 4_096, 5_000);
    expect(h.subscribe).toHaveBeenCalledWith("builder", 4_096, false, expect.any(Function));
  });

  test("disposeAll detaches the PTY channel callback", async () => {
    const h = harness();
    const term = manager(h.transport);
    await term.ensure("builder", null, 5_000);
    expect(h.detach).not.toHaveBeenCalled();
    term.disposeAll();
    expect(h.detach).toHaveBeenCalledTimes(1);
    expect(term.has("builder")).toBe(false);
  });

  test("a dispose mid-subscribe still detaches the PTY channel callback", async () => {
    // Stopping a run while openTerminals is still awaiting subscribe: the entry
    // is gone by the time the detach fn arrives, so nothing would ever call it
    // and the channel closure would pin the terminal for the process.
    const h = harness();
    const term = manager(h.transport);
    h.gate.arm();
    const pending = term.ensure("builder", null, 5_000);
    await vi.waitFor(() => {
      expect(h.subscribe).toHaveBeenCalled();
    });
    term.disposeAll();
    h.gate.release();
    await pending;
    expect(h.detach).toHaveBeenCalledTimes(1);
  });

  test("dispose drops one entry and leaves the rest alive", async () => {
    const h = harness();
    const term = manager(h.transport);
    await term.ensure("builder", null, 5_000);
    await term.ensure("planner", null, 5_000);
    term.dispose("builder");
    expect(h.detach).toHaveBeenCalledTimes(1);
    expect(term.has("builder")).toBe(false);
    expect(term.has("planner")).toBe(true);
  });

  test("suspend detaches the stream once and keeps the terminal", async () => {
    const h = harness();
    const term = manager(h.transport);
    await term.ensure("s-1", null, 5_000);
    term.suspend("s-1");
    term.suspend("s-1");
    expect(h.detach).toHaveBeenCalledTimes(1);
    expect(term.has("s-1")).toBe(true);
  });

  test("a suspend mid-subscribe still detaches the arriving stream", async () => {
    const h = harness();
    const term = manager(h.transport);
    h.gate.arm();
    const pending = term.ensure("s-1", null, 5_000);
    await vi.waitFor(() => {
      expect(h.subscribe).toHaveBeenCalled();
    });
    term.suspend("s-1");
    h.gate.release();
    await pending;
    expect(h.detach).toHaveBeenCalledTimes(1);
  });

  test("resumeStream resubscribes with resume", async () => {
    const h = harness();
    const term = manager(h.transport);
    await term.ensure("s-1", null, 5_000);
    term.suspend("s-1");
    await term.resumeStream("s-1");
    expect(h.subscribe).toHaveBeenLastCalledWith("s-1", null, true, expect.any(Function));
    expect(h.subscribe).toHaveBeenCalledTimes(2);
  });

  test("resumeStream on a live stream does not subscribe twice", async () => {
    const h = harness();
    const term = manager(h.transport);
    await term.ensure("s-1", null, 5_000);
    await term.resumeStream("s-1");
    expect(h.subscribe).toHaveBeenCalledTimes(1);
  });

  test("stopRun must not kill session terminals", async () => {
    // Two managers, two transports, two entry maps: stopRun's disposeAll on the
    // workflow manager must leave every session terminal streaming.
    const workflow = harness();
    const sessions = harness();
    const workflowTerm = manager(workflow.transport);
    const sessionTerm = manager(sessions.transport);
    await workflowTerm.ensure("builder", null, 5_000);
    await sessionTerm.ensure("s-1", null, 5_000);
    workflowTerm.disposeAll();
    expect(workflow.detach).toHaveBeenCalledTimes(1);
    expect(sessions.detach).not.toHaveBeenCalled();
    expect(workflowTerm.has("builder")).toBe(false);
    expect(sessionTerm.has("s-1")).toBe(true);
  });
});
