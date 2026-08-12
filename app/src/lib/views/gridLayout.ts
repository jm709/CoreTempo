export type GridClass = "g1" | "g2" | "g4" | "g6";

export function gridClass(count: number): GridClass {
  if (count <= 1) return "g1";
  if (count === 2) return "g2";
  if (count <= 4) return "g4";
  return "g6";
}
