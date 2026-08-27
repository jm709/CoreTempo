import { describe, expect, it } from "vitest";
import { KNOWN_REASON_CODES } from "./wire.js";

describe("KNOWN_REASON_CODES", () => {
  it("lists the codes core can now surface on a failed trigger", () => {
    // The two the owed-ask watchdog added; a typo in either is a wire lie.
    expect(KNOWN_REASON_CODES).toContain("blocked_on_permission");
    expect(KNOWN_REASON_CODES).toContain("agent_restarted");
  });

  it("names each code once", () => {
    expect(new Set(KNOWN_REASON_CODES).size).toBe(KNOWN_REASON_CODES.length);
  });
});
