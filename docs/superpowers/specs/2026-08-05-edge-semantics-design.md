# Edge semantics: feedback exemption, quiet protocol, loop edges

2026-08-05. Approved design for three changes to how workflow edges drive
agent behavior. Motivated by observed traffic: a two-agent planner/builder
workflow produced 42 sends across 7 runs, of which roughly a third were
acknowledgement ceremony ("sign-off received" → "confirmed closed"), because
every inbound send re-arms the receiver's edge obligations — including sends
from the receiver's own delegate reporting back.

## 1. Downstream-feedback exemption

**Rule:** an agent's edge obligations arm when a turn is opened by any
origin *except an agent the receiving agent has an edge to* (its
"downstream"). A message from a downstream agent is feedback on delegated
work; it must not re-obligate the delegation.

- Trigger kickoff, user chat, HTTP callers, and upstream/peer agents still
  arm obligations exactly as today. Forward chains (`A → B → C`) are
  unaffected: B's obligation to C arms when A's message lands, because A is
  not B's downstream.
- The exemption applies at the arming site in `Router::create_message`
  (core/src/router/mod.rs:209-228): when the sender is `Origin::Agent(s)`
  and `s` appears among the target's edge targets, skip opening/merging the
  turn entirely (no nudge-budget reset either).
- Consequence, stated plainly: an intentional send-cycle (A has an edge to
  B and B has an edge to A) no longer self-perpetuates. That pattern is what
  `ask` and the new `loop` kind are for; the primer and docs say so.

## 2. Quiet protocol primer line

Append to the generated protocol primer (`FrozenWorkflow::system_prompt`):

> Never send acknowledgements, sign-offs, or thanks. If a message requires
> no new work from you, do nothing and end your turn.

Applies to every agent in every workflow; costs nothing; kills the etiquette
half of the ping-pong even on paths the exemption does not reach.

## 3. `loop` edge kind

A third edge kind for supervised iteration: `edges = [{ to = "builder",
kind = "loop" }]` means the owner repeatedly delegates to the target and
decides when the work is complete.

**Type change.** `Edge.kind` becomes a new `EdgeKind` enum (`ask | send |
loop`, serde lowercase) instead of reusing `MessageKind`. Wire values for
existing workflows are unchanged (`ask`/`send` parse as before).

**Round mechanics.** A loop round is an `ask` (the reply channel is the
loop's return path). The composed prompt step reads:

> N. Loop with <target>: `tempo ask <target> "<instruction>"` — each reply
> arrives as a new prompt. Issue the next round with another `tempo ask`,
> or, when the task is fully complete, run `tempo done <target>` to end the
> loop. Never leave a loop open.

**Context strategy (research-informed).** The loop target keeps its normal
per-round auto-`/clear` — fresh context per iteration with state
externalized to files is the pattern every surveyed system converged on
(ralph loops, evaluator-optimizer, research subagents), and context-rot
evidence says accumulated history degrades attention well before window
limits. Continuity is carried by *instruction*, not mechanism: the composed
loop step tells the owner that each round message must be self-contained
(file paths, deltas, acceptance criteria — the target remembers nothing
between rounds), and the primer tells agents to write durable state to
disk and reply with a short summary. `auto_clear = false` on the target
remains the escape hatch for continuity-heavy loops. The owner receives
summaries, never transcripts; its growth is bounded by the round cap.

**Obligation mechanics.**
- An arming turn leaves the loop step unmet until the owner either asks the
  target (one round) or has already completed the loop (`loops_done`).
- While the round's ask is pending, the existing `pending_asks > 0` guard
  holds quiet — no nudge mid-round.
- **A reply from the loop target re-arms the loop step** (new `TurnState`
  for the owner, nudge budget reset). This is the deliberate exception to
  "replies never open a turn," scoped to loop edges only, and it is what
  makes the loop self-sustaining: after processing a reply the owner must
  either fire the next round or declare completion, or it gets one nudge and
  then stalls — same enforcement as any other unmet step.
- `tempo done <target>` marks `(owner, target)` complete in a new
  `Router::loops_done` set: the loop step counts as met for the current and
  subsequent turns, and replies from the target stop re-arming. The set
  entry clears when a new *arming* (non-exempt) turn opens for the owner —
  a fresh kickoff restarts the loop.

**Round cap.** Signal-only termination fails everywhere it has been tried;
every mature framework pairs the done-signal with a hard cap (LangGraph
`recursion_limit`, Agents SDK `max_turns`, CrewAI `max_iter`, AutoGen
`MaxMessageTermination`). Loop edges accept `max_rounds` (default 10),
counted on the owner's round asks and reset when `loops_done` clears. The
existing nudge→stall machinery only catches an owner that *idles* without
acting — the cap is what bounds an actively non-converging loop. Hitting
the cap is soft (never force-`done`; premature termination is its own
top-tier failure mode): the loop step stops re-arming, and the owner's next
stable idle gets one nudge whose text says the cap was reached — run
`tempo done <target>` or report to your upstream. No new bus event this
iteration; the existing `agent.nudged`/`agent.stalled` events plus a
`tracing::warn!` carry the signal. The round counter is in-memory and
resets on daemon restart — accepted and documented, matching restart's
existing disarm semantics.

**Cycle rejection.** `validate_workflow` rejects cycles in the loop-edge
subgraph at freeze (`A loop B, B loop A`): a loop cycle is always-active,
never quiescent, and invisible to stall detection (the circular-delegation
lesson). The error names the cycle.

**`tempo done` plumbing.** New CLI subcommand (`tempo done <agent>`) →
`POST /v1/agents/{target}/loop-done`, caller identity from the same
agent-origin auth `tempo reply` uses. The server validates that the caller
has a `loop` edge to the target; the error message names the caller's actual
edges and kinds (errors are read by LLMs).

**Nudge text.** `render_nudge` for an unmet loop step says: continue the
loop with `tempo ask <target> "..."` or end it with `tempo done <target>`.

## Frontend

- `EdgeKind` union gains `"loop"` (types.ts, graphModel, merge round-trip).
- Edge click-cycle in `GraphCanvas` becomes ask → send → loop → ask; the
  edge label renders the kind name as it does today.
- No loop-state indicator (running/done) in this iteration.

## Contracts

Amendments required: `Edge.kind` type change (`EdgeKind` supersedes
`MessageKind` in the config shape), the arming-exemption rule, the
loop-reply exception to "replies never open a turn," the new CLI subcommand
and endpoint, and the primer additions. No `EventPayload` changes: loop
completion emits no event in this iteration.

## Testing

- `core/tests/obligations.rs` gains: exemption (downstream send does not
  arm; upstream send still arms; chain A→B→C still propagates), loop round
  cycle (arm → ask → reply re-arms → done → reply no longer re-arms →
  new kickoff clears `loops_done`), round-cap behavior (cap stops re-arming,
  capped nudge text, counter resets with `loops_done`), a reply racing
  `tempo done`, and nudge-text coverage.
- `core/tests/workflow_validate.rs`: loop-cycle rejection with the cycle
  named in the error.
- `core/tests/system_prompt.rs`: primer line + loop step text.
- CLI/API: integration test for `tempo done` auth + validation errors.
- Frontend: graphModel/merge round-trip tests for `"loop"`.
- Real-agent check: a small loop workflow (owner loops a worker two rounds,
  then `tempo done`), confirming nudge fires if the owner idles mid-loop.

## Out of scope

- Loop-state UI, loop events on the bus (cap signal rides the existing
  nudge/stall events), owner-side `/compact` or context compaction, any
  mechanical guard against a premature `tempo done`.
- Changing `send` delivery semantics (still fire-and-forget; only the
  obligation arming changes).
- Migrating `MessageKind` anywhere outside the config `Edge` shape.
