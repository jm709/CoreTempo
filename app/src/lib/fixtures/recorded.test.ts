import { describe, expect, it } from "vitest";
import { recordedRun, snapshotMidRun, snapshotRunning } from "./recorded";

describe("recorded fixtures", () => {
  it("event seqs are strictly 1..N in order (contracts §9: seq starts at 1)", () => {
    recordedRun.forEach((ev, i) => {
      expect(ev.seq).toBe(i + 1);
    });
  });
  it("snapshot fixtures carry the dedup floor", () => {
    expect(snapshotRunning.last_seq).toBe(0);
    expect(snapshotMidRun.last_seq).toBe(8);
  });
  it("snapshot messages are created_at DESC (contracts §8.1)", () => {
    const ts = snapshotMidRun.messages.map((m) => m.created_at);
    // oxlint-disable-next-line no-array-sort, no-array-reverse -- ES2022 lib; spread copy is safe
    expect(ts).toEqual([...ts].sort().reverse());
  });
});
