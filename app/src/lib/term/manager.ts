import { pausePty, resizePty, subscribePty, writePty } from "../ipc";
import { isAppChord } from "../keys";
import { Backpressure } from "./backpressure";
import { classifyFrameTimes, persistedRenderer, persistRenderer } from "./renderer";
import { terminalOptions } from "./theme";
import type { FitAddon } from "@xterm/addon-fit";
import type { WebglAddon } from "@xterm/addon-webgl";
import type { Terminal } from "@xterm/xterm";

type XtermChunk = typeof import("./xterm-chunk");

interface Entry {
  term: Terminal;
  fit: FitAddon;
  webgl: WebglAddon | null;
  gauge: Backpressure;
  container: HTMLElement | null;
  observer: ResizeObserver | null;
  detach: (() => void) | null;
}

const ENC = new TextEncoder();
const entries = new Map<string, Entry>();
// Panes whose element mounted before ensureTerminal registered the entry: the reactive
// flush that mounts the grid always beats the xterm chunk import + subscribe round-trip.
const pendingAttach = new Map<string, HTMLElement>();
let chunkPromise: Promise<XtermChunk> | null = null;
let probeStarted = false;

function loadChunk(): Promise<XtermChunk> {
  chunkPromise ??= import("./xterm-chunk");
  return chunkPromise;
}

/// Creates the terminal and starts absorbing PTY bytes immediately (xterm buffers
/// writes before open()); attachTerminal() binds it to a pane element later.
/// All 2–6 terminals stay mounted and live; hidden ones keep absorbing writes.
export async function ensureTerminal(
  agent: string,
  sinceCursor: number | null,
  scrollback: number,
): Promise<void> {
  if (entries.has(agent)) return;
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
    void pausePty(agent, paused);
  });
  const entry: Entry = {
    term, fit, webgl: null, gauge, container: null, observer: null, detach: null,
  };
  term.onData((data) => {
    void writePty(agent, ENC.encode(data));
  });
  term.onResize(({ cols, rows }) => {
    void resizePty(agent, cols, rows);
  });
  // Handlers above must exist before a parked attach runs fit(): the resize event
  // from that fit is what propagates the pane's dimensions to the PTY.
  entries.set(agent, entry);
  const parked = pendingAttach.get(agent);
  if (parked !== undefined) {
    pendingAttach.delete(agent);
    openInPane(entry, parked);
  }
  const detach = await subscribePty(agent, sinceCursor, (bytes) => {
    entry.gauge.wrote(bytes.byteLength);
    entry.term.write(bytes, () => {
      entry.gauge.parsed(bytes.byteLength);
    });
  });
  // A disposeAllTerminals landing during that await drops the entry before the
  // detach fn exists; detach here or the channel closure pins the terminal.
  if (entries.get(agent) === entry) entry.detach = detach;
  else detach();
}

export function attachTerminal(agent: string, el: HTMLElement): void {
  const entry = entries.get(agent);
  if (entry === undefined) {
    pendingAttach.set(agent, el);
    return;
  }
  openInPane(entry, el);
}

function openInPane(entry: Entry, el: HTMLElement): void {
  if (entry.container !== null) return;
  entry.container = el;
  entry.term.open(el);
  enableWebgl(entry);
  entry.fit.fit();
  entry.observer = new ResizeObserver(() => {
    if (el.offsetParent !== null) entry.fit.fit(); // skip display:none panes
  });
  entry.observer.observe(el);
  void runProbeOnce();
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
  for (const entry of entries.values()) {
    entry.webgl?.dispose();
    entry.webgl = null;
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

export function focusAgentTerminal(agent: string): void {
  entries.get(agent)?.term.focus();
}

export function blurAllTerminals(): void {
  for (const entry of entries.values()) entry.term.blur();
}

export function disposeAllTerminals(): void {
  for (const entry of entries.values()) {
    entry.observer?.disconnect();
    entry.detach?.();
    entry.term.dispose();
  }
  entries.clear();
  pendingAttach.clear();
}
