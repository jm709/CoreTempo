import { describe, expect, it, vi } from "vitest";
import { Backpressure, HIGH_WATER, LOW_WATER } from "./backpressure";

describe("term.write backpressure gauge (contracts §10: ~1 MB high water)", () => {
  it("constants match the frozen threshold", () => {
    expect(HIGH_WATER).toBe(1_048_576);
    expect(LOW_WATER).toBe(262_144);
  });
  it("fires pause exactly once when crossing the high-water mark", () => {
    const onPause = vi.fn();
    const g = new Backpressure(onPause);
    g.wrote(HIGH_WATER); // at the mark: not past it
    expect(onPause).not.toHaveBeenCalled();
    g.wrote(1); // past it
    expect(onPause).toHaveBeenCalledExactlyOnceWith(true);
    g.wrote(500_000); // already paused: no re-fire
    expect(onPause).toHaveBeenCalledTimes(1);
  });
  it("resumes exactly once when draining below the low-water mark", () => {
    const onPause = vi.fn();
    const g = new Backpressure(onPause);
    g.wrote(HIGH_WATER + 1);
    g.parsed(HIGH_WATER + 1 - LOW_WATER); // exactly at low water: still paused
    expect(onPause).toHaveBeenCalledTimes(1);
    g.parsed(1); // below it
    expect(onPause).toHaveBeenLastCalledWith(false);
    expect(onPause).toHaveBeenCalledTimes(2);
    expect(g.bytesOutstanding).toBe(LOW_WATER - 1);
  });
  it("never goes negative on spurious parsed callbacks", () => {
    const g = new Backpressure(() => {});
    g.parsed(999);
    expect(g.bytesOutstanding).toBe(0);
  });
});
