# Trigger output schema — design

**Date:** 2026-08-06
**Status:** Approved
**Feature:** `[trigger.output]` — structured output contracts for webhook triggers

## Motivation

Website backends fire CoreTempo webhook triggers ("translate X to French", "create a
DealCloud draft from this email") and need a structured, validated result back — not
free text. Today the trigger reply is the agent's `tempo reply` body verbatim; every
consumer would have to prompt for JSON, parse, validate, and retry by hand. This
feature makes the output shape a first-class part of the workflow contract: declared
in `tempo.toml`, enforced by the daemon, delivered to the HTTP caller as a parsed,
guaranteed-conforming object.

Prior art converges on exactly this: MCP tool `outputSchema`/`structuredContent`,
OpenAI Agents SDK `output_type`, Pydantic AI, Google ADK `output_schema`, and Claude
Code's own `--json-schema` (print mode only, so unusable through CoreTempo's
interactive PTYs — but it validates the validate-and-re-ask loop we build here).

## Scope

In scope:

- `[trigger.output]` config: JSON Schema, inline or from a file, with `max_repairs`.
- Validation + bounded self-repair loop at `Router::reply`.
- Trigger-boundary re-validation; `output` and `reason_code` on the HTTP wire.
- Schema composed into the target agent's system prompt.
- `tempo reply --json-file <path>`.
- Companion fix: the clear gate learns about owed replies.

Out of scope (deliberately):

- Multiple named task types per workflow. Forward-compatible path if wanted later:
  `[[trigger]]` array with a `name` field selected by `?task=<name>`; the in-flight
  claim stays global, so this buys no concurrency and is deferred until needed.
- A reusable npm client and a drop-in frontend widget (filed as follow-up issues).
- Schema enforcement on agent-to-agent asks — inter-agent replies are prose by design.

## Config

```toml
[trigger]
type = "webhook"
edge = { to = "translator", kind = "ask" }

# New. Absent = today's behaviour exactly: free-text reply, no validation.
[trigger.output]
schema_file = "schemas/translate-result.json"   # relative to the tempo.toml — OR —
# schema = { type = "object", ... }             # inline; exactly one of the two
max_repairs = 2                                 # 0..=5, default 2
```

The consuming project owns the schema's source of truth. A TypeScript project emits
the file from its zod schema during its own build (zod 4 native
`z.toJSONSchema(Schema, { target: "draft-2020-12" })`; the vendor-neutral route is
`@standard-schema/spec`'s `StandardJSONSchemaV1`). The exported JSON Schema is a
strictly weaker validator than the source schema (`.refine()` predicates vanish;
`z.date()`/transforms are unrepresentable) — it is the contract CoreTempo enforces,
not an equivalence. CoreTempo never reads the project's code; the `.json` artifact is
the boundary. (Live scanning of tagged symbols in a foreign repo was evaluated and
rejected: no industry precedent, requires executing user TypeScript inside the Rust
daemon, and sits outside the freeze hash.)

Load-time validation, reported as `ValidationIssue`s in the same pass as existing
checks (`core/src/workflow.rs` `validate_trigger`):

- `trigger.output` with `edge.kind = "send"` → error (a send kickoff completes on
  quiescence and carries no body; point at `kind = "ask"`).
- `trigger.output` with `type = "on_start"` → error (only an HTTP-origin kickoff
  gets the repair loop and has a caller to receive the structured result).
- Both or neither of `schema`/`schema_file` → error naming both keys.
- `schema_file` unreadable, non-UTF-8, or invalid JSON → error with the resolved
  absolute path.
- Schema fails to compile → error carrying the compiler diagnostic. External `$ref`
  gets its own message (resolution is disabled; see Validator).
- `max_repairs` outside `0..=5` → error. Zero is legal: validate once, never re-ask.

**Freeze:** the schema file's bytes are hashed together with the tempo.toml bytes
into the workflow sha256, so serve mode's reload guard rejects a changed contract
("a queued trigger cannot be answered by a different workflow" already states the
invariant). The compiled artifact lives on `FrozenWorkflow` as
`output: Option<Arc<OutputContract>>` — compiled validator, raw schema `Value` (for
the prompt), target `AgentId`, `max_repairs`. `FrozenWorkflow` drops its `PartialEq`
derive (verified unused on whole structs).

## Validator

`jsonschema` crate, `default-features = false` (removes reqwest/rustls/tokio from
the tree; no network, no filesystem `$ref` escape from a user-supplied schema).
Draft 2020-12 pinned via `jsonschema::draft202012` so a stray `$schema` string
cannot change semantics. Compile once at load; `iter_errors` yields instance and
schema JSON Pointers, which render into directly actionable error text.

## Enforcement loop

Validation lives in `Router::reply` (`core/src/router/mod.rs`), the single choke
point both `tempo reply` and the UI already pass through. Rejecting there means the
agent is still `working` — blocked inside the very Bash call that submitted the
reply — so the validation errors arrive as that command's output and the agent
retries within the same turn. No PTY injection, no idle transition, no `/clear`
risk, and the feedback provably reaches the model. (The alternative — validating in
the trigger watcher only — was rejected: by then the reply has settled, the agent
has gone idle, and its context may already be cleared.)

Gate: the check applies only to the trigger's kickoff ask —
`kind == Ask && matches!(from, Origin::Http(_)) && to == contract.target`.

1. **`--code 1` bypasses validation entirely.** The escape hatch: an agent that
   cannot produce the shape reports failure in prose instead of burning retries.
   (MCP shipped the opposite bug — `outputSchema` made error reporting impossible —
   and had to fix it; this rule is load-bearing.)
2. **Repair before validate** (deterministic, zero tokens): trim; strip a
   leading/trailing markdown fence with or without a `json` tag; if the remainder
   does not start with `{`/`[`, take the first balanced JSON span; unwrap a
   single-key `{"output": {...}}` when the schema root is an object and `output`
   is not a declared property.
3. **Validate** the parsed `Value` against the compiled validator.
4. **On failure under budget:** return a new `RouterError::OutputSchema { errors,
   attempts_left }`, mapped to **422 `schema_validation_failed`**, before any store
   update — the record stays non-terminal and re-repliable. Per-message attempt
   counts live in a map on the `Router` under the existing `transition` guard and
   are cleaned up in `settle`. Each rejection publishes a `reply.rejected` bus
   event carrying the message id and errors, so a failing workflow is debuggable.
5. **On budget exhaustion:** accept the reply normally (`status = replied`; the
   frozen message state machine is unchanged, and observers see what the agent
   actually said). The trigger produces the caller-facing verdict:
6. **The trigger validates what it returns.** In `terminal_completion`
   (`core/src/trigger.rs`), the `Replied` arm re-runs repair + validate on the
   final body. Valid → completion carries the parsed `Value`. Invalid →
   `Failed` with the validation errors and attempt count. Defence in depth: the
   router enforces what it accepts; the trigger guarantees what it returns. Both
   call one shared function in a new `core/src/schema.rs`. Both warm runs
   (`core/src/api/trigger.rs`) and serve-mode cold starts (`daemon/src/serve.rs`)
   build their watcher through `watch_completion`, so one hook covers both.

Rejection error text (errors are read by LLMs — include the fix): at most ten
errors, one line each with instance pointer, message, schema pointer; then the
remaining budget; then "reply with ONLY the JSON object — no prose, no fences";
then an explicit pointer to `--code 1` for the can't-produce case. Without that
last line an agent facing an impossible schema burns the whole budget instead of
taking the escape hatch.

No separate token cap for the loop: each attempt is one Bash call inside an
existing turn (not a fresh model turn), `max_repairs` bounds attempts, and
`ask_timeout` remains the backstop. Decided consciously, not by omission.

## Prompt composition

The schema enters the target agent's `--append-system-prompt` in
`FrozenWorkflow::system_prompt` (`core/src/workflow.rs`), as a block after the
"Required workflow steps" section: the pretty-printed schema plus a short
instruction — reply via `tempo reply` with the JSON object only, no fences, no
commentary; use `--code 1` with a plain-text explanation if the shape cannot be
produced. A process argument costs nothing at injection time and survives `/clear`.
Not in `render_ask`: injected text is typed keystrokes through the timing-sensitive
queue, and a multi-kilobyte schema there is asking for the class of bug CLAUDE.md
already documents.

## HTTP wire

Success — `output` is the parsed object, populated only when a schema is declared
and validation passed (dual emission: callers that don't care read `reply` as
before):

```json
{
  "trigger_id": "t-a3f91c2e",
  "status": "completed",
  "result": "replied",
  "code": 0,
  "reply": "{\"translations\":[...]}",
  "output": { "translations": ["..."] }
}
```

Agent declined: `code: 1`, `reply` = the explanation, `output` absent; the caller
branches on `code`.

Failure — `TriggerStatus::Failed` gains a machine-readable
`reason_code: &'static str` beside the prose `reason`. Initial set, one per
failure kind the watcher already distinguishes: `schema_validation_failed`,
`agent_failed`, `timeout`, `agent_exited`. Contracts amendment 16
folds failure kinds into one prose reason and points machine callers at the bus —
but a webhook caller cannot reach the bus, and schema-mismatch vs timeout vs
agent-declined imply different retry decisions. This is a small additive amendment
to the wire contract, made for the same reason the feature exists: machine
consumption.

## CLI

- `tempo reply <id> --json-file <path>`, mutually exclusive with the positional
  body: the agent writes `result.json` and points at it, removing shell-quoting of
  nested JSON as an error class (every quoting mistake would otherwise burn a
  repair attempt on a non-schema problem).
- `cmd_reply` (`cli/src/main.rs`) prints the server's error body on non-2xx instead
  of collapsing it into a generic message. This is the entire feedback channel; a
  test asserts the validation error text reaches the agent-visible output.

## Companion fix: owed replies and the clear gate

Pre-existing hole, made likelier by rejection: an agent that ends its turn still
owing the kickoff reply goes idle with `pending_asks == 0` (the count tracks asks
*sent by* an agent), so `ClearGate::on_stable_idle` allows `/clear` and the trigger
waits out the full `ask_timeout` for a reply that can never come. The gate learns
about owed replies (asks addressed to the agent, not yet replied): idle with an
owed reply → nudge once, then stall semantics — mirroring the existing obligation
machinery rather than inventing new behaviour.

## Testing

TDD throughout; the scripted fake agent covers everything here (router + trigger
paths, no PTY timing or TUI behaviour).

- Load: each `ValidationIssue` (send-kind, both/neither schema keys, bad file, bad
  compile, external `$ref`, `max_repairs` range); freeze hash changes when only the
  schema file changes; serve-mode reload rejects it.
- Repair: fences, prose prefix/suffix, `{"output": ...}` unwrap, balanced-span
  extraction; property-based tests are a good fit here.
- Loop: reject → error body content → re-reply succeeds; budget exhaustion →
  reply accepted → trigger `Failed` with `schema_validation_failed`; `--code 1`
  bypass; `max_repairs = 0`; counter cleanup in `settle`; `reply.rejected` events.
- Wire: `output` present/absent; `reason_code` per failure kind; declined shape.
- Editor: `workflow_parse`/`workflow_merge` round-trips an inline `schema` table
  (same bug class as the `tools`-key fix, commit 47da0c7).
- Clear gate: idle-with-owed-reply → nudge, not `/clear`.
- One real-`claude` smoke test (trivial schema, trivial prompt): does an agent
  given the schema in its system prompt reply with bare JSON or fenced prose —
  calibrates how much the repair pass must do.

## Follow-ups (tracked as GitHub issues, not in this slice)

- `@coretempo/client`: typed npm client — fire trigger, long-poll, parse `output`,
  typed errors keyed on `reason_code`.
- Drop-in frontend widget: input box + progress + result/error states over the
  client.
