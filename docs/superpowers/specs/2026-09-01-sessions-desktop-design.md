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

Freshness, two sources: the daemon's `/v1/events` stream (forwarded by the
shell, §7) applies `session.*`, `project.*`, `agent.state`, `agent.blocked`
and `agent.lifecycle` instantly; a `GET /v1/sessions` poll every 5 s,
running while sessions mode is active, refreshes the git-status fields
(`branch`, `changed_files`, `ahead`, `worktree_status`), which Spec A
computes on GET and no event ever carries.

## 4. Creating a session

`+ session` / `+ new session` opens a modal form (the `dialogs.ts`
patterns):

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

Only the selected session holds an open PTY stream. The term manager keeps
xterm instances and cursors per session id (as it does per agent), so
switching away detaches the stream and switching back resubscribes at the
stored cursor — the SSE replay delivers exactly the missed bytes, then
live. Resize on attach and on container resize.

A stopped/exited session still renders its ring tail (the stream never
ends on exit and spans respawns) under a dim banner naming the state —
`stopped · resume` / `exited (code N) · resume` — with the banner's
`resume` wired to the same action as the card's.

## 6. Daemon lifecycle in the shell

The shell owns the daemon relationship, lazily — first entry into
sessions mode, never at app boot, so workflow-only usage spawns nothing:

1. Read `~/.coretempo/sessions/api.json`. Present with a live pid →
   connect.
2. Otherwise spawn `coretempod sessions` detached (own process group;
   stdio released — the daemon logs to its own `daemon.log`), then poll
   `/v1/health` until it answers.
3. Open `/v1/events` (no filter) and forward every event to the webview
   (§7).

The sessions topbar shows the state: `starting…`, `connected`, or `daemon
unreachable — retrying` on a backoff. On stream drop the shell re-reads
`api.json` (a restarted daemon binds a new port), reconnects, and the
webview refetches `GET /v1/sessions` to resync anything missed. The
operator token is read from `api.json` in the shell and never crosses into
the webview.

**Packaging.** The desktop must be able to spawn `coretempod`. In dev,
`./dev` points the shell at the workspace target binary; a release bundle
ships `coretempod` as a Tauri sidecar (`externalBin`) — the first second
binary in the app bundle.

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
| `session_subscribe_pty` | `GET /v1/sessions/{id}/pty?since=` → Channel of decoded bytes |
| `session_write_pty` | `POST /v1/sessions/{id}/pty` |
| `session_resize_pty` | `POST /v1/sessions/{id}/pty/resize` |
| `session_pause_pty` | `POST /v1/sessions/{id}/pty/pause` |

Daemon events forward on a new Tauri event `coretempo:session-event` —
not `coretempo:event`, whose payload type is the run bus envelope and
whose consumers assume run semantics. The shell gains `reqwest`
(exact-pinned, HTTP + SSE streaming); `ureq` stays where it is — the CLI
and shell clients are separate by design (blocking vs async), and no
shared client crate is extracted until a third consumer exists.

## 8. Webview state

`state/sessions.svelte.ts`: session rows by id, project list, connection
state, selected session id, blocked-count derivation for the badge. Fed by
`coretempo:session-event` and the §3 poll. `types.ts` gains `SessionRow`,
`ProjectRow`, `SessionsStatus` and the create-request shape, mirroring the
contracts doc. Selection and mode live in `uiState`.

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
  Channel pump against a scripted stream, including base64 decode and
  resubscribe-at-cursor; `api.json` discovery (missing, stale pid, live)
  and the spawn-then-health-poll path with a fake daemon binary.
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

- **49** — the Tauri sessions command surface (§7 table, request/response
  types mirroring the daemon API), the `coretempo:session-event` Tauri
  event, `uiState.mode`, and the `coretempod` sidecar packaging
  requirement.
