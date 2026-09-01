# Session manager — Spec B: desktop Sessions mode

Date: 2026-09-01
Status: draft for review
Amends: the design spec (2026-08-01) and the contracts doc (amendment 49,
assigned up front, renumbered on rebase if PRs cross). Companion to Spec A
(2026-08-27), which built the core, daemon, API and CLI this UI drives.

## 1. Purpose

The desktop face of the session manager: many independent Claude Code
sessions across many projects, visible at a glance in the window the
operator already has open, with a full terminal for the one they are
looking at. Everything the UI does, `tempo session` already does — this
spec adds no daemon capability, only surface.

Decided during brainstorming (2026-09-01):

| Question | Decision |
|---|---|
| Navigation | One window, a top-level `workflows \| sessions` mode switch; both modes stay mounted |
| Layout | Rail of session cards grouped by project + one full-size terminal for the selected session |
| Scope | Full session lifecycle (create/stop/resume/delete) and project register/forget; no daemon controls beyond auto-spawn, no worktree inspection beyond the row's counts |
| Transport | Shell proxy: Tauri commands + Channels in front of the daemon's HTTP/SSE API. No CORS layer (amendment 41 stays), the operator token never enters the webview |

## 2. Mode switch and layout

The topbar gains a `workflows | sessions` switcher (styled like the
graph/terminals toggle) to the left of the workflow-path label, driven by
`uiState.mode`. Both modes stay mounted, hidden with `display: none`
exactly as graph/terminals keep the editor alive: a running workflow keeps
streaming while the operator is in Sessions, and vice versa.

Per mode:

- **workflows** — everything as today: path label, ▶ Run, view toggle,
  agents meta, Roster rail, editor/terminals center, dock, statusbar hints.
- **sessions** — topbar shows the daemon connection state (§6) and a
  `+ new session` button; the rail is the session list (§3); the center is
  the selected session's terminal (§5); the dock column collapses to zero
  width (feed/chat are workflow concepts); the statusbar shows sessions
  hints.

The `sessions` switcher label carries a count badge of sessions whose
`blocked` flag is set, so a permission dialog raised while the operator
edits a workflow is visible without switching. The badge exists only once
the shell has connected to the daemon (§6 — connection is lazy); before
that there is nothing to count and nothing is shown. No other cross-mode
notification surface in v1.

Keybindings: mod+1–9 focus-terminal and mod+E stay workflows-only (guarded
by mode); mod+` releases terminal capture in both modes; sessions mode adds
no bindings in v1.

## 3. The session rail

`SessionRail.svelte`, in the grid area Roster occupies in workflows mode:
a scrollable column, one heading per registered project (display name),
that project's sessions as cards beneath, newest first, and a trailing
`+ project` row that opens the native folder picker and registers the
choice (`POST /v1/projects`; the API's 409/422 shown inline). Each project
heading carries a `+ session` action opening the create form (§4)
pre-filled with that project.

Each card:

- **Line 1** — `StatusGlyph` + the session title, with ⏸ while the
  `blocked` flag is set (tooltip names the tool, as agents today). The
  glyph maps `starting | idle | working | exited` onto its existing
  states and gains a `stopped` variant (distinct from `exited`: the
  operator chose it).
- **Line 2** (mono, dim) — the worktree branch, or the cwd directory name
  for sessions without one; then `±N` changed files and `↑N` ahead, each
  only when non-zero.

Clicking a card selects it (highlight like Roster's `.row.hl`); the center
shows its terminal. Inline actions mirror Roster's `restart` idiom:
`stop` on live cards; `resume` and `rm` on stopped/exited ones. `rm`
confirms first; when the session has a worktree the confirm offers
remove-worktree, and a 422-dirty response re-prompts showing the API's
porcelain summary verbatim with a force option. `branch_kept: true` in the
response is reported in the confirm's closing state.

A card whose `worktree_status` is `missing` disables `resume` with a
tooltip naming `delete` — the client already knows the 409 the API would
return, so it does not round-trip for it.

Freshness, two sources with a precedence rule: the daemon's `/v1/events`
stream (forwarded by the shell, §7) applies `session.*`, `project.*`,
`agent.state`, `agent.blocked` and `agent.lifecycle` instantly and owns
`state` and `blocked`; a `GET /v1/sessions` poll every 5 s, running while
sessions mode is active, writes only the git-status fields (`branch`,
`changed_files`, `ahead`, `worktree_status` — computed on GET, carried by
no event) and reconciles membership (rows the poll adds or no longer
returns). The poll never writes `state`/`blocked`, so a response computed
just before an event arrived cannot regress a card.

If the selected session is deleted — from the UI or externally
(`session.deleted` from a `tempo session rm`) — the selection clears and
the center shows the empty state. An external stop is already covered by
the §5 banner.

## 4. Creating a session

`+ session` / `+ new session` opens a modal form. The webview has no
modal-form precedent — `dialogs.ts` is two native two-button confirms —
so this is a new in-app modal component, and the §3 delete flow (a
remove-worktree option, a dirty re-prompt carrying the porcelain summary
and a force option) uses the same component family, since native `ask()`
cannot express three-way choices with rich content.

| Field | Behaviour |
|---|---|
| Project | dropdown of registered projects, pre-selected from the opening context |
| Worktree | checkbox, default on; branch/slug naming stays the daemon's |
| cwd | optional, relative to the project root; the API's 422 shown inline |
| Title | optional; placeholder states the fallback (first prompt line, else branch/dir) |
| First prompt | optional multiline; injected once by the daemon with submit verification |
| Advanced (collapsed) | model (free text, empty = default); permission mode (`default` = the operator's own, or `bypassPermissions`); isolated config (checkbox, default off) |

Submit calls `POST /v1/sessions` through the shell. Success selects the
new session and closes the form — the operator watches it boot in the
terminal. Failure keeps the form open and shows the API error verbatim
(untrusted root with both fixes, git's stderr, cwd outside the root with
both paths). There is no trust-confirmed retry: Spec A's API has no such
knob and this spec does not add one; the error's two fixes
(`trust_agent_dirs`, or trust the repo in Claude Code) are the v1 path.

## 5. The center terminal

`SessionTerminal.svelte` hosts one xterm for the selected session, reusing
the term manager, renderer and backpressure modules through a second
transport with the same interface as the workflow one: subscribe opens the
daemon's SSE PTY stream in the shell (base64 decoded there; bytes reach
the webview over a `Channel<ArrayBuffer>`, as `subscribePty` today), and
write/resize/pause call the session PTY routes. Keystrokes go raw —
xterm `onData` → shell → `POST /v1/sessions/{id}/pty` — so the operator
types prompts, answers permission dialogs (sessions run `wait` semantics
for exactly this) and uses Claude Code's own bindings; the shell
interprets nothing. Raw writes have no submit verification (Spec A): an
Enter typed into a mid-redraw prompt can be swallowed, and the operator
sees and retypes, as in any terminal. A concurrent `tempo session attach`
types into the same PTY; last writer wins.

Only the selected session holds an open PTY stream, and that takes an
explicit close: switching away calls `session_unsubscribe_pty`, which
ends the shell's SSE connection and pump for that session. The workflow
detach idiom — clearing the Channel's JS handler — is not enough here:
Tauri's callback map keeps the channel alive, so the shell pump would
hold one SSE connection per session ever viewed.

**Cursor accounting is the shell's.** Per session, the shell records the
stream cursor after the last chunk it forwarded.
`session_subscribe_pty(session, resume)` with `resume: true` resubscribes
at that stored cursor — the SSE replay delivers exactly the bytes the
webview missed, then live; `resume: false` (first view of a session, a
disposed xterm instance, or after a reconnect reset — §6) replays from
the ring start into a fresh terminal. Resize on attach and on container
resize.

**Term-manager refactor.** Today's manager is a module-level singleton
with the workflow IPC hardcoded (its transport functions are direct
imports, including the backpressure → `pausePty` closure) and a global
`disposeAllTerminals()` that `stopRun` calls. It becomes a factory taking
a transport (subscribe/write/resize/pause), instantiated once for
workflow agents — behaviour unchanged — and once for sessions, each with
its own entries map, so stopping a workflow run cannot touch session
terminals. It tracks no cursors today and gains none: that stays in the
shell, above.

A stopped/exited session still renders its ring tail (the stream never
ends on exit and spans respawns) under a dim banner naming the state —
`stopped · resume`, or the exit rendered by the existing `exitLabel`
(code or signal, amendment 42) — with the banner's `resume` wired to the
same action as the card's. Rings are in-memory: after a **daemon**
restart a stopped session's terminal is blank by design (the new daemon
re-attaches rows to fresh rings), not a replay bug.

## 6. Daemon lifecycle in the shell

The shell owns the daemon relationship, lazily — first entry into
sessions mode, never at app boot, so workflow-only usage spawns nothing:

1. Read `~/.coretempo/sessions/api.json`. If present, probe
   `GET /v1/health` with its token (short timeout). An answer → connect.
   The pid is never consulted: amendment 47 makes the daemon's `flock`
   the liveness authority and the kernel recycles pids — a health answer
   is the only proof the shell needs.
2. No file or no answer → spawn `coretempod sessions` detached (own
   process group; stdio released — the daemon logs to its own
   `daemon.log`). A spawn that exits 1 with the second-start refusal is
   **success** — it lost a race to a live peer.
3. Poll for a fresh `api.json` — the port is ephemeral (`--port` defaults
   to 0) and the file is written only after the listener binds — then
   `/v1/health`, up to ~10 s before giving up as unreachable. Then open
   `/v1/events` (no filter) and forward every event to the webview (§7).

Connection state reaches the webview as shell-emitted transitions on a
dedicated Tauri event, `coretempo:sessions-status`, carrying
`{ state: "starting" | "connected" | "unreachable" }`; the sessions
topbar renders it (`starting…`, `connected`, `daemon unreachable —
retrying`). On stream drop the shell emits `unreachable` and re-runs the
sequence from step 1 on a backoff. Every `connected` transition —
initial or reconnect — is the webview's trigger to refetch
`GET /v1/sessions` and `GET /v1/projects`; on a **re**connect the shell
has also reset its stored PTY cursors (a restarted daemon numbers from a
fresh ring, and the shell does not try to distinguish a blip from a
restart), so the webview disposes session xterm instances and the
selected session resubscribes with `resume: false`. The operator token is
read from `api.json` in the shell and never crosses into the webview.

**Packaging.** The desktop must be able to spawn `coretempod`. In dev,
`./dev` points the shell at the workspace target binary; a release bundle
ships `coretempod` as a Tauri sidecar (`externalBin`) — the first second
binary in the app bundle, and the change that flips `bundle.active` on
(it is `false` today). Two constraints: `externalBin` entries carry
target-triple-suffixed names; and the shell must **not** spawn through
`tauri-plugin-shell`'s sidecar API, which tracks children and kills them
on app exit — exactly wrong for a daemon that outlives the window. It
resolves the sidecar path and spawns via `std::process::Command` in its
own process group.

## 7. Shell proxy surface

New Tauri commands, each a thin translation (bearer header, route, JSON
through, `CmdError` out) to the daemon API:

| Command | Route |
|---|---|
| `sessions_status` | connection state + `/v1/health` passthrough |
| `session_list` | `GET /v1/sessions` |
| `session_create` | `POST /v1/sessions` |
| `session_stop` | `POST /v1/sessions/{id}/stop` |
| `session_resume` | `POST /v1/sessions/{id}/resume` |
| `session_delete` | `DELETE /v1/sessions/{id}?remove_worktree=&force=` |
| `project_list` | `GET /v1/projects` |
| `project_register` | `POST /v1/projects` |
| `project_forget` | `DELETE /v1/projects/{id}` |
| `session_subscribe_pty` | `(session, resume)` — `GET /v1/sessions/{id}/pty?since=` → Channel of decoded bytes; `resume` per §5 |
| `session_unsubscribe_pty` | closes the shell's SSE connection and pump for that session (§5 — the Channel-handler detach idiom cannot) |
| `session_write_pty` | `POST /v1/sessions/{id}/pty` |
| `session_resize_pty` | `POST /v1/sessions/{id}/pty/resize` |
| `session_pause_pty` | `POST /v1/sessions/{id}/pty/pause` |

Daemon events forward on a new Tauri event `coretempo:session-event` —
not `coretempo:event`, whose payload type is the run bus envelope and
whose consumers assume run semantics. Shell-originated connection
transitions travel on `coretempo:sessions-status` (§6), separate because
they exist precisely when there is no daemon to emit anything. The shell gains `reqwest`
(exact-pinned, HTTP + SSE streaming); `ureq` stays where it is — the CLI
and shell clients are separate by design (blocking vs async), and no
shared client crate is extracted until a third consumer exists.

## 8. Webview state

`state/sessions.svelte.ts`: session rows by id, project list, connection
state, selected session id, blocked-count derivation for the badge. Fed by
`coretempo:session-event`, `coretempo:sessions-status` and the §3 poll.
`types.ts` gains the contracts doc's wire types under their contracts
names — `SessionView`, `ProjectView`, `CreateSessionRequest` (amendment
47; `SessionRow` is core's private store type, and renaming wire shapes
invites drift) — plus the `coretempo:sessions-status` payload. Selection
and mode live in `uiState`.

## 9. Errors

Every daemon error message is written for display (Spec A §8) and the UI
shows it verbatim — inline in the form, in the delete confirm, or as the
topbar connection state. Nothing is rephrased or swallowed. Shell-side
failures (daemon spawn failure, `api.json` unreadable, request timeout)
produce `CmdError`s naming the operation and the fix
(`start it with 'coretempod sessions'` mirrors the CLI's message).

## 10. Testing

TDD throughout.

- **Webview (vitest):** `sessions.svelte.ts` store transitions from a
  scripted event/poll sequence (fixtures for rows, wireEvents-style
  mapping tests); rail grouping/ordering and card action visibility per
  state; create-form request shaping; blocked-badge derivation.
- **Shell (Rust):** proxy commands against a stub axum server asserting
  route, bearer header, query encoding and error passthrough; the SSE →
  Channel pump against a scripted stream, including base64 decode,
  resume-at-stored-cursor, `resume: false` from ring start, and
  unsubscribe actually closing the connection; discovery and spawn — a
  live daemon answers health and no spawn happens, a stale `api.json`
  with a dead port spawns, a spawn exiting 1 (second-start refusal) is
  treated as success, the ephemeral port is learned from the rewritten
  `api.json`; reconnect resets stored cursors and emits the status
  transitions in order.
- **Live (manual, on the PR's checklist):** desktop against a real
  daemon — create with worktree from the UI, watch it boot, type a turn,
  see ⏸ on a permission dialog and answer it in the terminal, stop,
  resume, delete with remove-worktree. The badge appears from workflows
  mode.

## 11. Out of scope

Unchanged from Spec A: in-app diff/merge/PR, base-branch selection,
`tempo ask|send` between sessions, other harnesses. Deferred from this
spec: daemon stop control and any daemon management UI beyond the
connection state; multi-terminal splits or pinning; worktree file lists;
OS notifications (the badge is v1's whole notification surface); a
trust-confirmed retry on create.

## 12. Contracts amendment

- **49** — the Tauri sessions command surface (§7 table including
  `session_subscribe_pty(session, resume)` and `session_unsubscribe_pty`,
  request/response types under the contracts wire names), the
  `coretempo:session-event` and `coretempo:sessions-status` Tauri events,
  `uiState.mode`, the term-manager factory (§5), and the `coretempod`
  sidecar packaging requirement (§6).
