export const RECENTS_KEY = "coretempo.recents";
const CAP = 8;

type KV = Pick<Storage, "getItem" | "setItem">;

export function loadRecents(kv: KV): string[] {
  const raw = kv.getItem(RECENTS_KEY);
  if (raw === null) return [];
  try {
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed.filter((p): p is string => typeof p === "string");
  } catch {
    return [];
  }
}

export function pushRecent(kv: KV, path: string): string[] {
  const list = [path, ...loadRecents(kv).filter((p) => p !== path)].slice(0, CAP);
  kv.setItem(RECENTS_KEY, JSON.stringify(list));
  return list;
}
