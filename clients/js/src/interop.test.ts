import { afterEach, describe, expect, it } from "vitest";
import { z } from "zod";
import { CoreTempoClient } from "./index.js";
import { startScriptedServer, type ScriptedServer } from "./test-server.js";

let server: ScriptedServer | undefined;
afterEach(async () => {
  await server?.close();
  server = undefined;
});

const Output = z.object({ translations: z.array(z.string()) });

describe("zod interop", () => {
  it("a zod 4 schema types the trigger output end to end", async () => {
    server = await startScriptedServer([
      {
        status: 200,
        body: {
          trigger_id: "t-aa11bb22",
          status: "completed",
          result: "replied",
          code: 0,
          reply: '{"translations":["hola"]}',
          output: { translations: ["hola"] },
        },
      },
    ]);
    const client = new CoreTempoClient({ baseUrl: server.url, token: "t", flow: "post" });
    const outcome = await client.trigger("hello", { schema: Output });
    expect(outcome.status).toBe("completed");
    if (outcome.status !== "completed") return;
    // The next line is the point: `output` is inferred as { translations: string[] }.
    expect(outcome.output.translations[0]).toBe("hola");
  });

  it("a zod rejection surfaces as output_mismatch with zod issues", async () => {
    server = await startScriptedServer([
      {
        status: 200,
        body: {
          trigger_id: "t-aa11bb22",
          status: "completed",
          result: "replied",
          code: 0,
          reply: '{"translations":"hola"}',
          output: { translations: "hola" },
        },
      },
    ]);
    const client = new CoreTempoClient({ baseUrl: server.url, token: "t", flow: "post" });
    const outcome = await client.trigger("hello", { schema: Output });
    expect(outcome.status).toBe("output_mismatch");
    if (outcome.status !== "output_mismatch") return;
    expect(outcome.issues.length).toBeGreaterThan(0);
  });
});
