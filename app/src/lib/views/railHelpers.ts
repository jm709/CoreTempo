import type { SessionView } from "../types";

function lastPathSegment(path: string): string {
  const segments = path.split("/").filter((s) => s.length > 0);
  return segments.at(-1) ?? path;
}

export function cardLine2(s: SessionView): string {
  let line = s.branch ?? lastPathSegment(s.cwd);
  if ((s.changed_files ?? 0) > 0) line += ` ±${s.changed_files}`;
  if ((s.ahead ?? 0) > 0) line += ` ↑${s.ahead}`;
  return line;
}

export function cardActions(s: SessionView): ("stop" | "resume" | "rm")[] {
  if (s.state === "starting" || s.state === "idle" || s.state === "working") return ["stop"];
  return ["resume", "rm"];
}

export function resumeDisabled(s: SessionView): string | null {
  if (s.worktree_status !== "missing") return null;
  return "worktree is gone; rm (delete) is the valid action";
}
