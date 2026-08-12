# CoreTempo

A desktop app (and headless daemon) that runs multi-agent Claude Code workflows.
CoreTempo spawns `claude` sessions in PTYs it owns, routes messages between them
by typing into their prompts, and shows the traffic in a terminal-centric UI.

Design spec: `docs/superpowers/specs/2026-08-01-coretempo-design.md`.
Frozen type/API contracts: `docs/superpowers/plans/2026-08-01-contracts.md` —
**read its "Reconciliation amendments" section; those 24 amendments are the
authoritative type/API shapes wherever another doc disagrees.**

## Running it

```bash
./dev              # desktop app (vite :1420 + Rust core in one process)
./dev headless     # coretempod against ./tempo.toml
./dev check        # every gate: cargo test/clippy/fmt, svelte-check, oxlint, vitest, client tsc/oxlint/vitest
```

Copy `tempo.example.toml` to `tempo.toml` to get a workflow to run. The desktop
app embeds the backend — there is no separate server to start. In dev mode Tauri
points its native webview at vite; a release build bundles the assets instead.

## Layout

| Crate | What it owns |
|---|---|
| `core` | Everything. PTY manager, message router, SQLite store, axum `/v1` API, event bus, workflow load/freeze. Zero UI dependencies. |
| `app` | Tauri 2 shell (`app/src-tauri`) + Svelte 5 webview (`app/src`). |
| `cli` | The `tempo` binary agents call from their Bash tool. |
| `daemon` | `coretempod`, the headless runner. Thin `main` over `core`. |
| `clients/js` | `@coretempo/client`, the typed npm client for webhook triggers (issue #17). Standalone pnpm package; zero runtime deps. |

`core` never depends on anything UI-related; that boundary is what makes the
headless daemon possible, and it is load-bearing — keep it.

Key modules: `core/src/run.rs` (the orchestrator that wires everything),
`core/src/pty/queue.rs` (the only writer of text into a PTY),
`core/src/router/` (message lifecycle + reply sinks), `core/src/api/`.

## How messaging works

- `ask` expects a reply; `send` does not. Agents call `tempo ask|send|reply`.
- The server injects the message into the target's PTY as typed text. It is the
  **only** writer to any PTY, and per-agent injections are serialized through one
  queue — that serialization is the correctness story.
- For an agent-origin `ask`, the reply is injected back into the asker's PTY. For
  a UI or HTTP origin it resolves that caller instead. Everything is logged to
  SQLite regardless.
- `send` completion is inferred from the target's observed state transition, not
  from any acknowledgement.
- Status lifecycle: `queued → injected → working → replied | done | failed`.
- Edges (`[agents.<id>] edges = [{ to, kind }]`, kind `ask|send|loop`) are
  deterministic delegation steps: composed into the frozen prompt as numbered
  `tempo` commands and enforced by per-turn obligation tracking. An agent that
  idles with unmet steps gets one nudge instead of `/clear`; idle again →
  `agent.stalled` and it is left un-cleared. Messages from an agent the
  receiver has an edge to never arm its turn (downstream feedback is exempt),
  and replies never open a turn — except a loop target's reply, which re-arms
  the owner until `tempo done <target>` or the edge's `max_rounds` soft cap
  (default 10). Restart disarms and zeroes round counters. The decision point
  is `ClearGate::on_stable_idle`, evaluated inside the queue worker after the
  drain.
- A `[trigger]` section makes the workflow self-starting: `on_start` injects a
  configured message at launch (`coretempod run` exits 0/1 on completion);
  `webhook` makes `coretempod serve` cold-start a run per API call. Completion
  is inferred: ask kickoff → its reply; send kickoff → global quiescence
  (armed only after the kickoff reaches `working` — never weaken that guard).
- `[trigger.output]` declares a JSON Schema (inline `schema` or `schema_file`,
  exactly one) for the webhook reply. `tempo reply` rejects non-conforming
  bodies with the validation errors (422) so the agent repairs in-turn, up to
  `max_repairs`; the watcher re-validates what it returns, so callers get a
  parsed `output` object or a `reason_code`d failure. `--code 1` always
  bypasses validation. The schema file's bytes join the freeze hash.

## Agent state comes from hooks, not the screen

CoreTempo writes one `agent-settings-<agent_id>.json` per agent and passes
`--settings` for the matching file when spawning each agent. Those hooks call
`tempo state`:

| Hook | Reported state |
|---|---|
| `SessionStart` | idle |
| `UserPromptSubmit` | working |
| `Stop`, `StopFailure` | idle |

This replaced screen-scraping the TUI, which broke: Claude Code 2.1.220 emits
neither `esc to interrupt` nor `? for shortcuts`, and its spinner verbs are
randomised. **Do not reintroduce marker matching.** Reported state feeds the same
raw-state channel the old detector drove, so the debouncer, injection gating and
auto-`/clear` are unchanged downstream.

Auto-`/clear`: on a debounced working→idle transition with zero pending asks, an
empty queue, and no open obligation turn, the server types `/clear`. Ordering is
strict drain-then-clear.

## Gotchas that cost real debugging time

- **Enter must be a separate write.** Injecting `text + "\r"` in one write leaves
  the prompt typed but unsubmitted whenever Claude Code is rebuilding its input
  box — right after spawn, and after the session restart `/clear` triggers. The
  queue sends the text, waits `SUBMIT_DELAY`, then sends `\r`.
- **The state detector's stripper passes printable ASCII only.** Any marker or
  parsing you add against PTY output cannot rely on `❯`, box drawing, or emoji.
- **Spawned agents must not inherit `CLAUDE_CODE_*`.** A daemon launched from
  inside a Claude Code session leaks its own session markers into every agent and
  silently changes their behaviour. `spawn.rs` strips them.
- **A late hook must not revive an exited agent**, or the queue injects into a
  dead PTY and the write vanishes. `report_state` guards this.
- **Claude Code blocks on a trust dialog** in any directory it has not seen, and
  `--dangerously-skip-permissions` does not skip it. See the open issues.
- **Generated per-agent settings always allow `Bash(tempo:*)`.** Each agent's
  `agent-settings-<agent_id>.json` (written by `write_agent_settings_files`)
  allows it unconditionally, plus `Bash(<bin>:*)` for every entry in that
  agent's `tools = [...]` in `tempo.toml`. Manually editing the agent dir's
  `.claude` settings is only needed for tools not declared this way. Without
  an allowlist entry for a tool an agent actually calls, that call parks on
  Claude Code's approval dialog: the agent reads idle with unmet steps, gets
  nudged, and stalls.
- **WSL:** the webkitgtk window never maps under Wayland, and MESA has no device
  without `/dev/dri`. `./dev` sets `GDK_BACKEND=x11` and `LIBGL_ALWAYS_SOFTWARE=1`
  for you. Without a GPU, expect xterm's DOM renderer rather than WebGL.
- **`coretempod serve` bound off-loopback still validates `Host`.** A public
  `0.0.0.0` bind 403s any caller whose `Host` header isn't `localhost`,
  loopback, or the bind IP literal — the same rule the run API enforces. Put a
  public deployment behind a reverse proxy that rewrites `Host`, or bind
  loopback and tunnel in.

## Conventions

- TDD: failing test first, run it, confirm the expected failure, then implement.
  Where implementation and tests land together, prove the test bites by mutating
  the code and watching it fail.
- Zero warnings. `cargo clippy --workspace --all-targets --all-features --
  -D warnings` must be clean, pedantic included. No `unwrap`/`panic` in `src`,
  `tracing` not `println`, 100-char lines, max 5 positional params (group extras
  into a struct — see `SpawnInputs`).
- Integration test files need
  `#![expect(clippy::panic_in_result_fn, reason = "assertions are the vocabulary of tests")]`.
- Remove an `#[expect(dead_code, …)]` as soon as your change makes the item live;
  an unfulfilled expectation is itself a warning.
- Frontend: exact pinned versions, no `^`. TypeScript stays on 5.9.x — TS 7
  breaks `svelte-check`.
- Errors are read by LLMs. Include the roster, the valid values, the fix.

## Testing against real agents

Unit and integration tests use a scripted fake agent, which cannot catch PTY
timing or TUI behaviour — the four bugs above all escaped it. When touching the
spawn recipe, injection, or state reporting, drive a real `claude`: write a
workflow, `coretempod run` it, and check a round trip with
`tempo ask <agent> "..."`. Real agents cost tokens; keep prompts trivial.
