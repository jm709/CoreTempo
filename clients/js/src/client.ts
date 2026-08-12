import { setTimeout as sleep } from "node:timers/promises";
import { CoreTempoApiError, CoreTempoRequestError } from "./errors.js";
import { toOutcome } from "./outcome.js";
import type { TriggerOutcome } from "./outcome.js";
import type { StandardSchemaV1 } from "./standard-schema.js";
import type { ApiErrorBody, TerminalView, TriggerAccepted, TriggerView } from "./wire.js";

export interface ClientOptions {
  /** e.g. "http://127.0.0.1:4820" — port and token come from the run's api.json or your deployment config. */
  baseUrl: string;
  token: string;
  /** Injection point for tests or fetch polyfills; defaults to globalThis.fetch. */
  fetch?: typeof globalThis.fetch;
}

export interface RequestOptions {
  signal?: AbortSignal;
}

export interface TriggerOptions<T = unknown> extends RequestOptions {
  /** Standard Schema the parsed `output` must satisfy — pass the same schema the workflow's schema_file was generated from. */
  schema?: StandardSchemaV1<unknown, T>;
  /** Server-side long-poll per POST, in seconds (server cap 300). Default 30. */
  waitSecs?: number;
  /** GET polling cadence once the POST answers 202. Default 1000. */
  pollIntervalMs?: number;
}

export class CoreTempoClient {
  private readonly baseUrl: string;
  private readonly token: string;
  private readonly fetchImpl: typeof globalThis.fetch;

  constructor(options: ClientOptions) {
    this.baseUrl = options.baseUrl.replace(/\/+$/, "");
    this.token = options.token;
    this.fetchImpl = options.fetch ?? globalThis.fetch;
  }

  /** Fires the trigger without waiting; the result arrives via status()/waitForOutcome(). */
  async fire(
    body: string,
    options: RequestOptions = {},
  ): Promise<{ triggerId: string; position: number }> {
    const response = await this.request("/v1/trigger", {
      method: "POST",
      body,
      ...(options.signal === undefined ? {} : { signal: options.signal }),
    });
    const accepted = (await response.json()) as TriggerAccepted;
    return { triggerId: accepted.trigger_id, position: accepted.position };
  }

  /** One instant GET of the trigger's current wire view. */
  async status(triggerId: string, options: RequestOptions = {}): Promise<TriggerView> {
    const response = await this.request(`/v1/trigger/${encodeURIComponent(triggerId)}`, {
      method: "GET",
      ...(options.signal === undefined ? {} : { signal: options.signal }),
    });
    return (await response.json()) as TriggerView;
  }

  /** Fires the trigger and stays with it to a terminal outcome: long-polls the POST, then GET-polls. */
  async trigger<T = unknown>(
    body: string,
    options: TriggerOptions<T> = {},
  ): Promise<TriggerOutcome<T>> {
    const waitSecs = options.waitSecs ?? 30;
    const response = await this.request(`/v1/trigger?wait=${String(waitSecs)}`, {
      method: "POST",
      body,
      ...(options.signal === undefined ? {} : { signal: options.signal }),
    });
    if (response.status === 202) {
      const accepted = (await response.json()) as TriggerAccepted;
      return this.waitForOutcome(accepted.trigger_id, options);
    }
    const view = (await response.json()) as TerminalView;
    return toOutcome(view, options.schema);
  }

  /** Polls a fired trigger (yours or one recovered from fire()) until it is terminal. */
  async waitForOutcome<T = unknown>(
    triggerId: string,
    options: TriggerOptions<T> = {},
  ): Promise<TriggerOutcome<T>> {
    const interval = options.pollIntervalMs ?? 1000;
    for (;;) {
      const view = await this.status(triggerId, options);
      if (view.status === "completed" || view.status === "failed") {
        return toOutcome(view, options.schema);
      }
      await sleep(
        interval,
        undefined,
        options.signal === undefined ? {} : { signal: options.signal },
      );
    }
  }

  private async request(path: string, init: RequestInit): Promise<Response> {
    const url = `${this.baseUrl}${path}`;
    let response: Response;
    try {
      response = await this.fetchImpl(url, {
        ...init,
        headers: { authorization: `Bearer ${this.token}` },
      });
    } catch (cause) {
      if (cause instanceof Error && cause.name === "AbortError") throw cause;
      throw new CoreTempoRequestError(url, cause);
    }
    if (!response.ok) {
      let code = "unknown";
      let message = `HTTP ${String(response.status)}`;
      try {
        const parsed = (await response.json()) as ApiErrorBody;
        code = parsed.error.code;
        message = parsed.error.message;
      } catch {
        // Non-JSON error body (a proxy in the path, not the daemon): keep the status fallback.
      }
      throw new CoreTempoApiError(response.status, code, message);
    }
    return response;
  }
}
