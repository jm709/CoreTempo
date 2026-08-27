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

/// Closing a workflow unmounts its editor, and with it any unsaved edits.
export function confirmDiscard(path: string): Promise<boolean> {
  return ask(`${path} has unsaved changes. Close it anyway?`, {
    title: "Discard unsaved changes?",
    kind: "warning",
    okLabel: "Discard and close",
    cancelLabel: "Keep editing",
  });
}
