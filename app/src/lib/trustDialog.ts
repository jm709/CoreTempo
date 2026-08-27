import { ask } from "@tauri-apps/plugin-dialog";

/// Spec 2026-08-17 §1: CoreTempo never grants Claude Code trust silently. When
/// neither config surface grants it, the desktop asks once, listing the roots;
/// the answer is carried into the run as its trust policy.
export function confirmTrust(roots: string[]): Promise<boolean> {
  const list = roots.map((root) => `  ${root}`).join("\n");
  return ask(
    `Claude Code has not trusted these folders:\n\n${list}\n\n` +
      "CoreTempo will mark them trusted in ~/.claude.json so the agents can start. Continue?",
    {
      title: "Trust these folders?",
      kind: "warning",
      okLabel: "Trust and run",
      cancelLabel: "Cancel",
    },
  );
}
