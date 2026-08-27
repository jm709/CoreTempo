import { describe, expect, it } from "vitest";
import { toOutcome } from "./outcome.js";
import type { StandardSchemaV1 } from "./standard-schema.js";
import type { TerminalView } from "./wire.js";

interface Translations {
  translations: string[];
}

/** Minimal hand-rolled Standard Schema — proves the contract without a validator dep. */
const translationsSchema: StandardSchemaV1<unknown, Translations> = {
  "~standard": {
    version: 1,
    vendor: "coretempo-test",
    validate(value) {
      const candidate = value as { translations?: unknown } | null;
      const ok =
        typeof candidate === "object" &&
        candidate !== null &&
        Array.isArray(candidate.translations) &&
        candidate.translations.every((t) => typeof t === "string");
      return ok
        ? { value: value as Translations }
        : { issues: [{ message: "expected { translations: string[] }" }] };
    },
  },
};

const completed = (extra: Record<string, unknown>): TerminalView =>
  ({ trigger_id: "t-a3f91c2e", status: "completed", result: "replied", ...extra }) as TerminalView;

describe("toOutcome", () => {
  it("maps a schema-validated completion to a typed output", async () => {
    const view = completed({
      code: 0,
      reply: '{"translations":["bonjour"]}',
      output: { translations: ["bonjour"] },
    });
    const outcome = await toOutcome(view, translationsSchema);
    expect(outcome).toEqual({
      status: "completed",
      triggerId: "t-a3f91c2e",
      result: "replied",
      reply: '{"translations":["bonjour"]}',
      output: { translations: ["bonjour"] },
    });
  });

  it("maps code 1 to declined, ignoring the schema", async () => {
    const view = completed({ code: 1, reply: "source text is not translatable" });
    const outcome = await toOutcome(view, translationsSchema);
    expect(outcome).toEqual({
      status: "declined",
      triggerId: "t-a3f91c2e",
      code: 1,
      reply: "source text is not translatable",
    });
  });

  it("maps failed with its reason_code verbatim", async () => {
    const outcome = await toOutcome({
      trigger_id: "t-a3f91c2e",
      status: "failed",
      reason: "reply never conformed after 2 repairs",
      reason_code: "schema_validation_failed",
    });
    expect(outcome).toEqual({
      status: "failed",
      triggerId: "t-a3f91c2e",
      reason: "reply never conformed after 2 repairs",
      reasonCode: "schema_validation_failed",
    });
  });

  it("reports output_mismatch when the caller schema rejects the output", async () => {
    const view = completed({
      code: 0,
      reply: '{"translations":[1]}',
      output: { translations: [1] },
    });
    const outcome = await toOutcome(view, translationsSchema);
    expect(outcome.status).toBe("output_mismatch");
    if (outcome.status !== "output_mismatch") return;
    expect(outcome.raw).toEqual({ translations: [1] });
    expect(outcome.issues[0]?.message).toContain("translations");
  });

  it("reports output_mismatch when a schema is supplied but the server sent no output", async () => {
    const view = completed({ code: 0, reply: "plain prose" });
    const outcome = await toOutcome(view, translationsSchema);
    expect(outcome.status).toBe("output_mismatch");
    if (outcome.status !== "output_mismatch") return;
    expect(outcome.issues[0]?.message).toContain("[flows.<name>.output]");
  });

  it("passes output through untyped when no schema is supplied", async () => {
    const view = completed({ code: 0, reply: "ok", output: { anything: true } });
    const outcome = await toOutcome(view);
    expect(outcome).toEqual({
      status: "completed",
      triggerId: "t-a3f91c2e",
      result: "replied",
      reply: "ok",
      output: { anything: true },
    });
  });

  it("maps a quiesced send-kickoff completion (no code, no reply, no output)", async () => {
    const outcome = await toOutcome({
      trigger_id: "t-a3f91c2e",
      status: "completed",
      result: "quiesced",
    });
    expect(outcome).toEqual({
      status: "completed",
      triggerId: "t-a3f91c2e",
      result: "quiesced",
      output: undefined,
    });
  });
});
