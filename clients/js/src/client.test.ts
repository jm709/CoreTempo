import { createServer } from "node:http";
import type { AddressInfo } from "node:net";
import { afterEach, describe, expect, it } from "vitest";
import { CoreTempoClient } from "./client.js";
import { CoreTempoApiError, CoreTempoRequestError } from "./errors.js";
import type { StandardSchemaV1 } from "./standard-schema.js";
import { startScriptedServer, type ScriptedServer } from "./test-server.js";

let server: ScriptedServer | undefined;
afterEach(async () => {
  await server?.close();
  server = undefined;
});

const client = (url: string) =>
  new CoreTempoClient({ baseUrl: url, token: "tok-123", flow: "post" });

describe("fire", () => {
  it("POSTs the body verbatim with the bearer token and returns the accepted id", async () => {
    server = await startScriptedServer([
      { status: 202, body: { trigger_id: "t-aa11bb22", position: 3 } },
    ]);
    const accepted = await client(server.url).fire("translate to French: hello");
    expect(accepted).toEqual({ triggerId: "t-aa11bb22", position: 3 });
    expect(server.requests[0]?.method).toBe("POST");
    expect(server.requests[0]?.url).toBe("/v1/flows/post/trigger");
    expect(server.requests[0]?.authorization).toBe("Bearer tok-123");
    expect(server.requests[0]?.body).toBe("translate to French: hello");
  });

  it("throws CoreTempoApiError with the server code on a 401", async () => {
    server = await startScriptedServer([
      {
        status: 401,
        body: { error: { code: "unauthorized", message: "missing or invalid bearer token" } },
      },
    ]);
    const error = await client(server.url)
      .fire("x")
      .catch((e: unknown) => e);
    expect(error).toBeInstanceOf(CoreTempoApiError);
    expect((error as CoreTempoApiError).status).toBe(401);
    expect((error as CoreTempoApiError).code).toBe("unauthorized");
    expect((error as CoreTempoApiError).message).toContain("bearer");
  });

  it("throws CoreTempoApiError on 409 trigger_in_flight so callers can branch and retry", async () => {
    server = await startScriptedServer([
      {
        status: 409,
        body: { error: { code: "trigger_in_flight", message: "trigger 't-x' is still running" } },
      },
    ]);
    const error = await client(server.url)
      .fire("x")
      .catch((e: unknown) => e);
    expect((error as CoreTempoApiError).code).toBe("trigger_in_flight");
  });

  it("strips a trailing slash from baseUrl before joining the path", async () => {
    server = await startScriptedServer([
      { status: 202, body: { trigger_id: "t-aa11bb22", position: 0 } },
    ]);
    await new CoreTempoClient({ baseUrl: `${server.url}/`, token: "tok-123", flow: "post" }).fire(
      "x",
    );
    expect(server.requests[0]?.url).toBe("/v1/flows/post/trigger");
  });

  it("wraps a connection failure in CoreTempoRequestError naming the URL", async () => {
    const unreachable = new CoreTempoClient({
      baseUrl: "http://127.0.0.1:9",
      token: "t",
      flow: "post",
    });
    const error = await unreachable.fire("x").catch((e: unknown) => e);
    expect(error).toBeInstanceOf(CoreTempoRequestError);
    expect((error as CoreTempoRequestError).url).toContain(
      "http://127.0.0.1:9/v1/flows/post/trigger",
    );
    expect((error as CoreTempoRequestError).cause).toBeDefined();
  });
});

describe("status", () => {
  it("GETs the trigger view unchanged", async () => {
    server = await startScriptedServer([
      { status: 200, body: { trigger_id: "t-aa11bb22", status: "running" } },
    ]);
    const view = await client(server.url).status("t-aa11bb22");
    expect(view).toEqual({ trigger_id: "t-aa11bb22", status: "running" });
    expect(server.requests[0]?.url).toBe("/v1/trigger/t-aa11bb22");
  });

  it("throws CoreTempoApiError with code unknown_trigger on a 404", async () => {
    server = await startScriptedServer([
      {
        status: 404,
        body: { error: { code: "unknown_trigger", message: "no trigger with id 't-gone'" } },
      },
    ]);
    const error = await client(server.url)
      .status("t-gone")
      .catch((e: unknown) => e);
    expect((error as CoreTempoApiError).code).toBe("unknown_trigger");
  });
});

interface Translations {
  translations: string[];
}

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

describe("trigger", () => {
  it("returns the typed outcome straight from a 200 long-poll win", async () => {
    server = await startScriptedServer([
      {
        status: 200,
        body: {
          trigger_id: "t-aa11bb22",
          status: "completed",
          result: "replied",
          code: 0,
          reply: '{"translations":["bonjour"]}',
          output: { translations: ["bonjour"] },
        },
      },
    ]);
    const outcome = await client(server.url).trigger("hello", { schema: translationsSchema });
    expect(outcome.status).toBe("completed");
    if (outcome.status !== "completed") return;
    expect(outcome.output.translations).toEqual(["bonjour"]);
    expect(server.requests[0]?.url).toBe("/v1/flows/post/trigger?wait=30");
  });

  it("falls back to GET polling on a 202, riding out queued and running", async () => {
    server = await startScriptedServer([
      { status: 202, body: { trigger_id: "t-aa11bb22", position: 1 } },
      { status: 200, body: { trigger_id: "t-aa11bb22", status: "queued", position: 1 } },
      { status: 200, body: { trigger_id: "t-aa11bb22", status: "running" } },
      {
        status: 200,
        body: {
          trigger_id: "t-aa11bb22",
          status: "failed",
          reason: "agent exited",
          reason_code: "agent_exited",
        },
      },
    ]);
    const outcome = await client(server.url).trigger("hello", { pollIntervalMs: 5 });
    expect(outcome).toEqual({
      status: "failed",
      triggerId: "t-aa11bb22",
      reason: "agent exited",
      reasonCode: "agent_exited",
    });
    expect(server.requests.map((r) => r.url)).toEqual([
      "/v1/flows/post/trigger?wait=30",
      "/v1/trigger/t-aa11bb22",
      "/v1/trigger/t-aa11bb22",
      "/v1/trigger/t-aa11bb22",
    ]);
  });

  it("passes waitSecs through to the wait query parameter", async () => {
    server = await startScriptedServer([
      { status: 200, body: { trigger_id: "t-aa11bb22", status: "completed", result: "quiesced" } },
    ]);
    await client(server.url).trigger("go", { waitSecs: 120 });
    expect(server.requests[0]?.url).toBe("/v1/flows/post/trigger?wait=120");
  });

  it("aborts polling when the signal fires", async () => {
    server = await startScriptedServer([
      { status: 202, body: { trigger_id: "t-aa11bb22", position: 0 } },
      { status: 200, body: { trigger_id: "t-aa11bb22", status: "running" } },
    ]);
    const controller = new AbortController();
    const pending = client(server.url).trigger("hello", {
      pollIntervalMs: 60_000,
      signal: controller.signal,
    });
    const settled = pending.catch((e: unknown) => e);
    // Give the client time to reach the poll sleep, then abort it.
    await new Promise((resolve) => setTimeout(resolve, 50));
    controller.abort();
    const error = await settled;
    expect(error).toBeInstanceOf(Error);
    expect((error as Error).name).toBe("AbortError");
  });

  it("rejects with a bare AbortError when the POST itself is aborted", async () => {
    const arrived: string[] = [];
    const rawServer = createServer((req) => {
      arrived.push(req.url ?? "");
      // Never respond: the abort must land while the POST is still in flight.
    });
    await new Promise<void>((resolve) => rawServer.listen(0, "127.0.0.1", resolve));
    const { port } = rawServer.address() as AddressInfo;
    try {
      const controller = new AbortController();
      const pending = client(`http://127.0.0.1:${String(port)}`).trigger("hello", {
        signal: controller.signal,
      });
      const settled = pending.catch((e: unknown) => e);
      await new Promise((resolve) => setTimeout(resolve, 50));
      controller.abort();
      const error = await settled;
      expect(arrived).toHaveLength(1);
      expect(error).toBeInstanceOf(Error);
      expect((error as Error).name).toBe("AbortError");
      expect(error).not.toBeInstanceOf(CoreTempoRequestError);
    } finally {
      rawServer.closeAllConnections();
      await new Promise<void>((resolve, reject) => {
        rawServer.close((closeError) =>
          closeError === undefined ? resolve() : reject(closeError),
        );
      });
    }
  });
});

describe("waitForOutcome", () => {
  it("polls an already-fired trigger to its outcome", async () => {
    server = await startScriptedServer([
      { status: 200, body: { trigger_id: "t-aa11bb22", status: "running" } },
      {
        status: 200,
        body: {
          trigger_id: "t-aa11bb22",
          status: "completed",
          result: "replied",
          code: 1,
          reply: "no",
        },
      },
    ]);
    const outcome = await client(server.url).waitForOutcome("t-aa11bb22", { pollIntervalMs: 5 });
    expect(outcome).toEqual({ status: "declined", triggerId: "t-aa11bb22", code: 1, reply: "no" });
  });
});

describe("flow option", () => {
  it("is required and non-empty", () => {
    expect(() => new CoreTempoClient({ baseUrl: "http://x", token: "t", flow: "" })).toThrow(
      /flow/,
    );
  });

  it("is URI-encoded into the trigger path", async () => {
    server = await startScriptedServer([
      { status: 202, body: { trigger_id: "t-aa11bb22", position: 0 } },
    ]);
    await new CoreTempoClient({ baseUrl: server.url, token: "t", flow: "flow-1" }).fire("x");
    expect(server.requests[0]?.url).toBe("/v1/flows/flow-1/trigger");
  });
});
