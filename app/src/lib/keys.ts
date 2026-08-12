export type AppAction =
  | { kind: "focus-terminal"; index: number }
  | { kind: "release" }
  | { kind: "dock-feed" }
  | { kind: "dock-chat" }
  | { kind: "edit-workflow" }
  | { kind: "restart-focused" }
  | { kind: "toggle-maximize" };

export function isMac(nav: { platform?: string } = globalThis.navigator ?? {}): boolean {
  return (nav.platform ?? "").toLowerCase().includes("mac");
}

export function modLabel(mac: boolean): string {
  return mac ? "⌘" : "Ctrl+";
}

/// Spec §9.2: click captures keys; mod+` releases (NEVER Esc — Claude Code uses it);
/// mod+1–9 / F / T / E / R are app-scope; mod+Enter maximizes.
export function resolveKey(
  ev: Pick<KeyboardEvent, "key" | "metaKey" | "ctrlKey" | "altKey">,
  mac: boolean,
): AppAction | null {
  const mod = mac ? ev.metaKey : ev.ctrlKey;
  if (!mod || ev.altKey) return null;
  if (ev.key === "`") return { kind: "release" };
  if (ev.key >= "1" && ev.key <= "9" && ev.key.length === 1) {
    return { kind: "focus-terminal", index: Number(ev.key) - 1 };
  }
  switch (ev.key.toLowerCase()) {
    case "f": return { kind: "dock-feed" };
    case "t": return { kind: "dock-chat" };
    case "e": return { kind: "edit-workflow" };
    case "r": return { kind: "restart-focused" };
    case "enter": return { kind: "toggle-maximize" };
    default: return null;
  }
}

export function isAppChord(ev: KeyboardEvent): boolean {
  return resolveKey(ev, isMac()) !== null;
}
