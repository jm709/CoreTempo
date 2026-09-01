import { isAppChord } from "../keys";
import { Backpressure } from "./backpressure";
import { classifyFrameTimes, persistedRenderer, persistRenderer } from "./renderer";
import { terminalOptions } from "./theme";
import type { FitAddon } from "@xterm/addon-fit";
import type { WebglAddon } from "@xterm/addon-webgl";
import type { Terminal } from "@xterm/xterm";

type XtermChunk = typeof import("./xterm-chunk");

/// The PTY plumbing a manager instance drives. Run mode ignores `resume` (its
/// `since_cursor` replays the ring tail); sessions ignore `sinceCursor` (their
/// daemon replays from its own buffer when `resume` is set).
export interface TermTransport {
  subscribe(
    id: string,
    sinceCursor: number | null,
    resume: boolean,
    onChunk: (bytes: Uint8Array) => void,
  ): Promise<() => void>;
  write(id: string, data: Uint8Array): Promise<void>;
  resize(id: string, cols: number, rows: number): Promise<void>;
  pause(id: string, paused: boolean): Promise<void>;
}

export interface TerminalManager {
  ensure(id: string, sinceCursor: number | null, scrollback: number): Promise<void>;
  attach(id: string, el: HTMLElement): void;
  /// Take the terminal off its stream and out of its pane, keeping the xterm and
  /// its screen. `resumeStream` + `attach` bring it back.
  suspend(id: string): void;
  /// Resubscribe a suspended terminal, replaying what it missed.
  resumeStream(id: string): Promise<void>;
  has(id: string): boolean;
  focus(id: string): void;
  blurAll(): void;
  dispose(id: string): void;
  disposeAll(): void;
}

interface Entry {
  term: Terminal;
  fit: FitAddon;
  webgl: WebglAddon | null;
  gauge: Backpressure;
  /// xterm opens into this div, not the pane element: open() is a no-op once the
  /// terminal has an element, so moving between panes means moving the wrapper.
  wrapper: HTMLElement | null;
  container: HTMLElement | null;
  /// xterm's open() has run. `container` cannot stand in for this: a suspend takes
  /// the terminal out of its pane and nulls it, and opening a second time would
  /// build a second renderer over the same screen.
  opened: boolean;
  observer: ResizeObserver | null;
  detach: (() => void) | null;
  /// A subscription is live or in flight. Cleared by suspend so a detach fn that
  /// arrives afterwards is run instead of stored.
  streaming: boolean;
}

const ENC = new TextEncoder();
// Every manager's entries, for the one-shot renderer probe's DOM fallback.
const allEntries = new Set<Map<string, Entry>>();
let chunkPromise: Promise<XtermChunk> | null = null;
let probeStarted = false;

function loadChunk(): Promise<XtermChunk> {
  chunkPromise ??= import("./xterm-chunk");
  return chunkPromise;
}

/// One manager per transport: workflow agents and sessions keep separate entry
/// maps, so stopping a run disposes only the run's terminals.
export function createTerminalManager(transport: TermTransport): TerminalManager {
  const entries = new Map<string, Entry>();
  // Panes whose element mounted before ensure registered the entry: the reactive
  // flush that mounts the grid always beats the xterm chunk import + subscribe.
  const pendingAttach = new Map<string, HTMLElement>();
  // Ids whose ensure is still before its `entries.set`, and the subset of those a
  // suspend has landed on: with no entry yet, `streaming` has nothing to hold the
  // request, so ensure reads it here and skips the subscribe. Only an ensure in
  // flight arms it, so a stray suspend cannot silence some later ensure.
  const ensuring = new Set<string>();
  const suspendedWhileEnsuring = new Set<string>();
  allEntries.add(entries);

  /// Stores the detach fn only while the entry still owns the subscription it
  /// asked for; a dispose or suspend during the await detaches it here instead,
  /// or the channel closure pins the terminal.
  async function subscribe(
    id: string,
    entry: Entry,
    sinceCursor: number | null,
    resume: boolean,
  ): Promise<void> {
    entry.streaming = true;
    const detach = await transport.subscribe(id, sinceCursor, resume, (bytes) => {
      entry.gauge.wrote(bytes.byteLength);
      entry.term.write(bytes, () => {
        entry.gauge.parsed(bytes.byteLength);
      });
    });
    if (entries.get(id) === entry && entry.streaming) entry.detach = detach;
    else detach();
  }

  /// Creates the terminal and starts absorbing PTY bytes immediately (xterm buffers
  /// writes before open()); attach() binds it to a pane element later.
  /// All 2–6 terminals stay mounted and live; hidden ones keep absorbing writes.
  async function ensure(id: string, sinceCursor: number | null, scrollback: number): Promise<void> {
    if (entries.has(id)) return;
    ensuring.add(id);
    try {
      const x = await loadChunk();
      const term = new x.Terminal(terminalOptions(scrollback));
      term.loadAddon(new x.Unicode11Addon());
      term.unicode.activeVersion = "11";
      term.loadAddon(new x.WebLinksAddon());
      term.loadAddon(new x.SearchAddon());
      const fit = new x.FitAddon();
      term.loadAddon(fit);
      // App-scope chords (mod+1..9/`/F/T/E/R/Enter) must escape the captured terminal.
      term.attachCustomKeyEventHandler((ev) => !isAppChord(ev));
      const gauge = new Backpressure((paused) => {
        void transport.pause(id, paused);
      });
      const entry: Entry = {
        term, fit, webgl: null, gauge, wrapper: null, container: null, opened: false,
        observer: null, detach: null, streaming: false,
      };
      term.onData((data) => {
        void transport.write(id, ENC.encode(data));
      });
      term.onResize(({ cols, rows }) => {
        void transport.resize(id, cols, rows);
      });
      // Handlers above must exist before a parked attach runs fit(): the resize event
      // from that fit is what propagates the pane's dimensions to the PTY.
      entries.set(id, entry);
      const parked = pendingAttach.get(id);
      if (parked !== undefined) {
        pendingAttach.delete(id);
        openInPane(entry, parked);
      }
      // A suspend landed while the chunk loaded: keep the terminal, never open the
      // stream. resumeStream is what brings it back.
      if (!suspendedWhileEnsuring.has(id)) await subscribe(id, entry, sinceCursor, false);
    } finally {
      ensuring.delete(id);
      suspendedWhileEnsuring.delete(id);
    }
  }

  function attach(id: string, el: HTMLElement): void {
    const entry = entries.get(id);
    if (entry === undefined) {
      pendingAttach.set(id, el);
      return;
    }
    openInPane(entry, el);
  }

  function suspend(id: string): void {
    const entry = entries.get(id);
    if (entry === undefined) {
      if (ensuring.has(id)) suspendedWhileEnsuring.add(id);
      return;
    }
    entry.streaming = false;
    entry.detach?.();
    entry.detach = null;
    // Out of the pane as well as off the stream. The sessions center hands every
    // session the same pane element, so a wrapper left behind stays a full-height
    // block child and pushes the arriving terminal below the fold. The xterm and
    // its screen live on in the detached wrapper; attach() puts it back.
    entry.observer?.disconnect();
    entry.observer = null;
    entry.wrapper?.remove();
    entry.container = null;
  }

  async function resumeStream(id: string): Promise<void> {
    const entry = entries.get(id);
    if (entry === undefined || entry.streaming) return;
    await subscribe(id, entry, null, true);
  }

  function dispose(id: string): void {
    const entry = entries.get(id);
    pendingAttach.delete(id);
    if (entry === undefined) return;
    entries.delete(id);
    disposeEntry(entry);
  }

  function disposeAll(): void {
    for (const entry of entries.values()) disposeEntry(entry);
    entries.clear();
    pendingAttach.clear();
  }

  return {
    ensure,
    attach,
    suspend,
    resumeStream,
    has: (id) => entries.has(id),
    focus: (id) => entries.get(id)?.term.focus(),
    blurAll: () => {
      for (const entry of entries.values()) entry.term.blur();
    },
    dispose,
    disposeAll,
  };
}

function disposeEntry(entry: Entry): void {
  entry.streaming = false;
  entry.observer?.disconnect();
  entry.detach?.();
  entry.term.dispose();
  entry.wrapper?.remove();
}

function openInPane(entry: Entry, el: HTMLElement): void {
  if (entry.container === el) return;
  entry.container = el;
  entry.wrapper ??= newWrapper();
  el.append(entry.wrapper); // moves it out of the pane it was attached to before
  if (!entry.opened) {
    entry.term.open(entry.wrapper);
    entry.opened = true;
    enableWebgl(entry);
  }
  entry.observer?.disconnect();
  entry.fit.fit();
  entry.observer = new ResizeObserver(() => {
    if (el.offsetParent !== null) entry.fit.fit(); // skip display:none panes
  });
  entry.observer.observe(el);
  void runProbeOnce();
}

function newWrapper(): HTMLElement {
  const wrapper = document.createElement("div");
  wrapper.style.cssText = "width:100%;height:100%;";
  return wrapper;
}

/// WebGL with DOM fallback: failed init → stay on DOM; context loss → dispose addon,
/// xterm reverts to its DOM renderer automatically.
function enableWebgl(entry: Entry): void {
  if (chunkPromise === null || persistedRenderer(localStorage) === "dom") return;
  void chunkPromise.then((x) => {
    try {
      const addon = new x.WebglAddon();
      addon.onContextLoss(() => {
        addon.dispose();
        entry.webgl = null;
      });
      entry.term.loadAddon(addon);
      entry.webgl = addon;
    } catch {
      entry.webgl = null;
    }
  });
}

function fallbackAllToDom(): void {
  for (const entries of allEntries) {
    for (const entry of entries.values()) {
      entry.webgl?.dispose();
      entry.webgl = null;
    }
  }
}

/// One-time frame-time probe (spec §9.1): an offscreen terminal renders a text storm
/// under WebGL; a software-rasterized context shows as slow frames. The verdict is
/// persisted so the probe never runs again on this install.
async function runProbeOnce(): Promise<void> {
  if (probeStarted || persistedRenderer(localStorage) !== null) return;
  probeStarted = true;
  const x = await loadChunk();
  const host = document.createElement("div");
  host.style.cssText = "position:fixed;left:-10000px;top:0;width:640px;height:384px;";
  document.body.append(host);
  const probe = new x.Terminal(terminalOptions(1_000));
  probe.open(host);
  let webgl: WebglAddon | null = null;
  try {
    webgl = new x.WebglAddon();
    probe.loadAddon(webgl);
  } catch {
    webgl = null;
  }
  if (webgl === null) {
    persistRenderer(localStorage, "dom");
    fallbackAllToDom();
  } else {
    const line = "\u001b[32mprobe \u001b[33m▉▉▉▉▉▉▉▉ \u001b[36m0123456789 abcdefghij\u001b[0m\r\n";
    const deltas: number[] = [];
    let last = performance.now();
    for (let i = 0; i < 30; i += 1) {
      probe.write(line.repeat(40));
      await new Promise<void>((resolve) => {
        requestAnimationFrame(() => {
          resolve();
        });
      });
      const now = performance.now();
      deltas.push(now - last);
      last = now;
    }
    const verdict = classifyFrameTimes(deltas);
    persistRenderer(localStorage, verdict);
    if (verdict === "dom") fallbackAllToDom();
  }
  probe.dispose();
  host.remove();
}
