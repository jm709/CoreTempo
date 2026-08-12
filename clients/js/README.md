# @coretempo/client

Typed client for CoreTempo webhook triggers. Fires `POST /v1/trigger`, long-polls
to a terminal status, and returns either a schema-typed `output` object or a
machine-branchable failure — no prose-sniffing.

Works against a warm run's API and against `coretempod serve` (which cold-starts
a run per trigger and may queue).

## Usage

```ts
import { CoreTempoClient } from "@coretempo/client";
import { z } from "zod";

// The same shape your workflow's [trigger.output] schema_file declares.
const Output = z.object({ translations: z.array(z.string()) });

const client = new CoreTempoClient({
  baseUrl: "http://127.0.0.1:4820",
  token: process.env.CORETEMPO_TOKEN ?? "",
});

const outcome = await client.trigger("translate to French: hello", { schema: Output });

switch (outcome.status) {
  case "completed":
    return outcome.output.translations; // typed string[]
  case "declined":
    // The agent answered `tempo reply --code 1`: it cannot produce the shape. Not retryable.
    throw new Error(outcome.reply);
  case "output_mismatch":
    // Your zod schema and the workflow's schema_file have drifted. Fix the schema, not the request.
    throw new Error(outcome.issues.map((i) => i.message).join("; "));
  case "failed":
    // Branch on outcome.reasonCode:
    //   timeout, agent_exited          → retryable
    //   agent_failed                   → maybe retryable
    //   schema_validation_failed       → fix the workflow prompt/schema
    //   kickoff_rejected, internal     → inspect outcome.reason
    //   workflow_changed               → not retryable until the daemon is
    //                                     restarted with the updated workflow
    throw new Error(`${outcome.reasonCode}: ${outcome.reason}`);
}
```

Any Standard Schema v1 validator works (`schema:` accepts zod 4, valibot,
arktype, or a hand-rolled implementation). Omit `schema` to get `output` as
`unknown` (or a send-kickoff's quiesced completion with no output at all).

## Lower-level pieces

- `client.fire(body)` — POST without waiting; returns `{ triggerId, position }`.
- `client.status(triggerId)` — one raw `TriggerView` GET.
- `client.waitForOutcome(triggerId, options)` — poll a fired trigger to its outcome.
- `trigger()` options: `waitSecs` (server long-poll per POST, default 30, cap 300),
  `pollIntervalMs` (GET cadence after a 202, default 1000), `signal` (AbortSignal).

## Errors (thrown, not returned)

The outcome union describes the *workflow's* result. Failing to talk to the
daemon at all throws instead:

| Error | When | Fields |
|---|---|---|
| `CoreTempoApiError` | any non-2xx (`unauthorized`, `invalid_host`, `trigger_in_flight`, `unknown_trigger`, `payload_too_large`, `queue_full`…) | `status`, `code`, `message` |
| `CoreTempoRequestError` | no HTTP answer at all (connection refused, DNS, abort) | `url`, `cause` |

A 409 `trigger_in_flight` (warm run) and a 429 `queue_full` (serve mode) are
both retryable after the current trigger completes.
