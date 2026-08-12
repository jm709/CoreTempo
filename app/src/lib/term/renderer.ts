export type Renderer = "webgl" | "dom";
export const RENDERER_KEY = "coretempo.renderer";

type KV = Pick<Storage, "getItem" | "setItem">;

export function persistedRenderer(kv: KV): Renderer | null {
  const v = kv.getItem(RENDERER_KEY);
  return v === "webgl" || v === "dom" ? v : null;
}

export function persistRenderer(kv: KV, r: Renderer): void {
  kv.setItem(RENDERER_KEY, r);
}

/// Spec §9.1: webkitgtk can hand out a WebGL context that is secretly software-
/// rasterized. Median frame time > 33 ms (< ~30 fps) under text load ⇒ DOM renderer.
/// Median (not mean) so a single GC pause cannot flip the verdict.
export function classifyFrameTimes(deltasMs: number[]): Renderer {
  if (deltasMs.length === 0) return "webgl";
  // oxlint-disable-next-line no-array-sort -- ES2022 lib; sorting a fresh spread copy is safe
  const sorted = [...deltasMs].sort((a, b) => a - b);
  const median = sorted[Math.floor(sorted.length / 2)] ?? 0;
  return median > 33 ? "dom" : "webgl";
}
