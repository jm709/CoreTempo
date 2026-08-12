import { isTerminalStatus, type MessageRecord } from "../types";

export const messagesState = $state({
  list: [] as MessageRecord[],          // ascending created_at (event arrival order)
  index: {} as Record<string, number>,  // message id → list position
});

export function setMessages(desc: MessageRecord[]): void {
  // oxlint-disable-next-line no-array-reverse -- ES2022 lib; spread copy is safe
  const asc = [...desc].reverse();
  const index: Record<string, number> = {};
  asc.forEach((m, i) => {
    index[m.id] = i;
  });
  messagesState.list = asc;
  messagesState.index = index;
}

export function upsertMessage(m: MessageRecord): void {
  const i = messagesState.index[m.id];
  if (i !== undefined) {
    messagesState.list[i] = m;
    return;
  }
  messagesState.index[m.id] = messagesState.list.length;
  messagesState.list.push(m);
}

export function pendingAsksFor(agentId: string): number {
  const from = `agent:${agentId}`;
  let n = 0;
  for (const m of messagesState.list) {
    if (m.kind === "ask" && m.from === from && !isTerminalStatus(m.status)) n += 1;
  }
  return n;
}

export function resetMessages(): void {
  messagesState.list = [];
  messagesState.index = {};
}
