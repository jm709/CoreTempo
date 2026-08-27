# @coretempo/client

Typed client for CoreTempo webhook flows. Fires `POST /v1/flows/{name}/trigger`,
long-polls to a terminal status, and returns either a schema-typed `output`
object or a machine-branchable failure — no prose-sniffing.

Works against a warm run's API and against `coretempod serve` (which cold-starts
a run per trigger and may queue).

> **Server requirement:** 2.x targets the per-flow trigger routes of the
> multi-flow CoreTempo release. Against an older daemon that only exposes bare
> `POST /v1/trigger`, stay on `@coretempo/client` 1.x.

## Usage

```ts
import { CoreTempoClient } from "@coretempo/client";
import { z } from "zod";

// The same shape your workflow's [flows.classify.output] schema_file declares.
const Output = z.object({ translations: z.array(z.string()) });

const client = new CoreTempoClient({
  baseUrl: "http://127.0.0.1:4820",
  token: process.env.CORETEMPO_TOKEN ?? "",
  flow: "classify", // the [flows.<name>] section this client fires
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
    //   blocked_on_permission          → add the named tool to tools/allow in tempo.toml, then retry
    //   agent_restarted                → retryable
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
| `CoreTempoApiError` | any non-2xx (`unauthorized`, `invalid_host`, `trigger_in_flight`, `unknown_trigger`, `payload_too_large`, `queue_full`, `shutting_down`…) | `status`, `code`, `message` |
| `CoreTempoRequestError` | no HTTP answer at all (connection refused, DNS, abort) | `url`, `cause` |

A 409 `trigger_in_flight` (one in-flight trigger *per flow* on a warm run) and
a 429 `queue_full` (per-flow queue, serve mode) are both retryable after the
current trigger completes. A 503 `shutting_down` is not: the daemon was
interrupted and is draining its queues, so retrying only fails again — and
sooner or later stops connecting at all. It has to be restarted first.

These request-time 4xx codes are not the full set of ways a trigger can be
turned away. The server answers synchronously only for what it can check before
it takes the flow's agent locks — unknown flow, an on_start flow, payload size,
the per-flow 409/429. Anything that goes wrong after that point is reported on
the trigger, not on the POST: a 202 with a trigger id can still settle as
`failed` with `reasonCode: "kickoff_rejected"` (the run started but the kickoff
message was refused). Treat a 202 as "accepted", not "ran", and take the verdict
from `trigger()` — or from `waitForOutcome()` / `status()` when you fired with
`fire()`.
