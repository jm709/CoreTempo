# Session manager — Spec A: core, daemon, API, CLI

Date: 2026-08-27
Status: draft for review (revision 3, after two review passes on 2026-08-27)
Amends: the design spec (2026-08-01) and the contracts doc (amendments 46–48,
assigned up front, renumbered on rebase if PRs cross).
Follow-up: Spec B (desktop Sessions mode) is designed after this spec ships.

## 1. Purpose

CoreTempo runs workflows: frozen prompts, routed messages, flows. This spec
adds a second, separate use — a **session manager** in the style of Spotify's
Xirp: many independent Claude Code sessions across many projects, each
optionally in its own git worktree, living in a background daemon that
outlives the desktop window, visible at a glance (working / idle / needs
input), resumable, and driveable from the CLI so an agent can do everything
an operator can.

Sessions are not workflow agents. They have no frozen prompt, no protocol
primer, no edges, no `tempo ask|send`, no auto-`/clear`. They are the
operator's ordinary Claude Code, owned by CoreTempo's PTY layer so state,
injection timing, trust and isolation come for free.

Decided during brainstorming (2026-08-27):

| Question | Decision |
|---|---|
| Core job | Sessions across projects **and** per-session worktrees, first-class from day one |
| Persistence | Sessions outlive the window: a background daemon |
| Harnesses | Claude Code only; Codex/Gemini are next steps — the schema carries `harness` now, nothing reads it |
| Messaging | Independent terminals; no `tempo ask|send` between sessions in v1 |
| Worktrees | Create, show (branch, changed files, ahead), remove — no in-app diff/merge |
| Split | Spec A = core + daemon + API + CLI (this document); Spec B = desktop UI |

## 2. Session model

A **project** is a registered local git repository root: `id` (`p-<8 hex>`),
`path` (canonical), `name` (display; defaults to the directory name),
`created_at`.

A **session** is one Claude Code process the daemon owns in a PTY:

| Field | Meaning |
|---|---|
| `id` | `s-<8 hex>` |
| `project` | project id |
| `cwd` | where Claude Code runs: any directory under the project root (monorepo packages), or the worktree path (or a directory under it — the same relative path applied to the worktree) |
| `worktree` | `{ path, branch, base }` when the daemon created one; `null` otherwise |
| `title` | operator-given, else the first line of `prompt`, else the branch or directory name |
| `claude_session_id` | the `session_id` of the most recent `SessionStart` hook (fires on start, after a user `/clear`, and on `--resume` — latest wins); `null` until one arrives |
| `model`, `permission_mode`, `isolated_config` | launch options, fixed at creation. `permission_mode` defaults to the operator's own default (no bypass); `isolated_config` defaults to `false` — sessions are personal and should see the operator's `~/.claude` |
| `prompt` | optional first turn, injected once after creation through the queue's existing submit verification (it waits for the debounced idle after `SessionStart`, so ≥ `idle_debounce` after the prompt appears). Never injected again. If the session exits first the injection fails `AgentExited`; that is logged at warn and nothing else happens |
| `created_at`, `stopped_at` | timestamps; `stopped_at` is set whenever the row leaves live (stop, exit, daemon shutdown) |
| runtime | `state` = `starting | idle | working` while live; for a non-live row it is `last_state` ∈ `stopped | exited`. `blocked` side flag (`{ tool, since }`) exactly as agents today; `exit` (`{ code, signal }`); `pty_cursor`; `changed_files`; `ahead`; `worktree_status` = `present | missing | none` |

The `harness` column (`"claude"`) is stored and not exposed by the API or
the CLI until a second value exists.

### Lifecycle

```
create ──▶ live ──stop──▶ stopped ──resume──▶ live
              │
              └──exit (child gone)──▶ exited ──resume──▶ live
stopped | exited ──delete──▶ gone   (remove_worktree also removes the worktree)
```

Every transition on one session runs under that session's `tokio::Mutex`
(stop takes up to `EXIT_GRACE`; a concurrent resume/delete waits, then sees
the new state and answers accordingly).

- **create**: register the project if new, create the worktree if asked
  (§5), run the trust gate (§3), write the session's files (§4), spawn,
  inject `prompt` if given. **Create is atomic**: a failure anywhere up to
  and including the spawn (untrusted root, git failure, `claude` missing,
  PTY open) rolls back the row, the session files and the fresh worktree
  (nothing has run in it) and returns that error — a session exists only
  once its process is running.
- **stop**: `PtyManager::stop(session)` (§4): SIGHUP via portable-pty's
  killer, reap with the existing `EXIT_GRACE` (5 s) then SIGKILL; the
  `blocked` flag is cleared with the usual `blocked: false` event; the row
  keeps everything, `last_state = stopped`, `stopped_at` set, worktree and
  files kept, ring tail readable.
- **exit**: the child left on its own; the reaper records `exit`,
  `last_state = exited`, otherwise as stop.
- **resume**: 409 if live or if `worktree_status = missing` (the hint names
  `delete`). Respawn in the same `cwd`. With a `claude_session_id`, the
  respawn passes `--resume <id>`; without one, a fresh spawn — the response
  carries `resumed: false`. If Claude Code rejects the id (transcript gone)
  it exits within seconds and the session is reported `exited` through
  `agent.lifecycle` (code/signal; the output stays on the PTY stream);
  there is no silent fallback to a fresh start (to verify live — §10). The PTY reopens at the handle's last
  size within one daemon life and at the 120×40 default after a daemon
  restart; `attach` and the desktop resize on connect, so this self-corrects.
- **delete**: 409 if live. The row and the session's files go; with
  `remove_worktree`, §5 applies.
- Daemon shutdown stops every live session (`last_state = exited`,
  `stopped_at` set). Nothing auto-resumes at boot.

### Permissions and hooks

A human is present, so sessions use the **wait** semantics: the
`PermissionRequest` hook reports `tempo state blocked`, the dialog stays up,
the `blocked` flag is visible. The deny hook (`on_permission_prompt =
"deny"`) is a workflow-only default. Generated per-session settings allow
`Bash(tempo:*)` only (so `tempo state` works) and nothing else — the
operator's own Claude Code allow rules apply as usual, because
`isolated_config` is off by default and `--settings` merges over them.

## 3. The daemon

`coretempod sessions` — a subcommand of the existing daemon binary, one
process per user, no new crate. Its root is `~/.coretempo/sessions/`,
resolved from `HOME` exactly as `~/.coretempo/runs/` is:

| Path | Purpose |
|---|---|
| `api.json` | `SessionsApiFile { port, token, pid }` (a new type; `ApiFile` requires a `run_id`), mode 0600, written after the listener binds, deleted on clean exit. A stale file (dead pid) is overwritten by the next start. |
| `sessions.db` | `SessionStore`: `projects`, `sessions` (§9). Its own type and file — the message `Store` has its own schema and orphan sweep and is not reused; the WAL, `user_version` and `spawn_blocking` conventions are. |
| `sessions.lock` | `flock`ed (`libc`, already a core dependency) for the daemon's lifetime; a second start finds it held, prints the running pid and port from `api.json`, exits 1. |
| `daemon.log` | `tracing` to file; stderr in the foreground. |
| `<session-id>/` | that session's files (§4); removed on delete. |

- Binds loopback only. Bearer auth and `Host` validation are the run API's.
- **Two token classes.** The operator token lives only in `api.json` and
  authorises everything. Each session gets its own hook token, generated at
  create and exported as `CORETEMPO_TOKEN` into that session's environment;
  it authorises exactly `POST /v1/agents/{own-id}/state` (and nothing else:
  403 elsewhere, 403 for another id). A tool a session's Claude runs can
  therefore report its own state and not type into, create, or delete other
  sessions. Workflow runs keep their single-token model; this scoping is
  sessions-only because sessions span every registered project.
  **Mechanism** (amendment 47): today `check_bearer` compares against the
  one `ctx.token` and identity comes from the `X-CoreTempo-Agent` header
  the CLI sends from `CORETEMPO_AGENT_ID` — with several tokens the token
  must decide identity, or a session could spoof another's `/state` by
  changing the header. The shared context gets
  `trait TokenAuth { fn classify(&self, bearer: &str) -> Operator | Hook(AgentId) | Unknown }`;
  runs implement it as the single operator token, the daemon compares the
  operator token then every live hook token in constant time. The guard
  403s `Hook(id)` on any route other than `POST /v1/agents/{id}/state`
  (message names the one route); `caller_origin` derives `Origin::Agent(id)`
  from `Hook(id)` and 403s a mismatching `X-CoreTempo-Agent`. A hook token
  lives as long as its row (resume exports the same one; a late `Stop` hook
  after stop is dropped by `report_state`'s exited guard as today).
  `sessions.db` and its `-wal`/`-shm` are created 0600 — it holds bearer
  tokens, which the message store never did.
- Runs in the foreground. Supervision (systemd, tmux, `setsid`) is the
  operator's; the desktop (Spec B) spawns it detached when `api.json` is
  absent or its pid is dead, then polls `/v1/health`.
- SIGTERM/SIGINT: stop every session (§2), mark rows, remove `api.json`,
  exit 0. `coretempod sessions stop` sends SIGTERM to the pid in `api.json`
  and is the only management subcommand.
- **Trust.** `trust_root` returns the git toplevel of the physical path, and
  a linked worktree is its own toplevel (`trust.rs`:
  `trust_root_of_a_worktree_is_the_worktree_dir`), so a fresh worktree is
  always an untrusted root. Rule: the `TrustGate` runs before every spawn
  and resume with one derived rule for worktree sessions, applied at
  **every** spawn (create and each resume): if the **project root** is
  trusted, the daemon (re)writes the trust key for the worktree path — the
  operator consented to that repository, CoreTempo made the directory, and
  a live Claude flush can revert the worktree key at any time; if the
  project root is not trusted, the spawn fails. In `create` the project
  root is checked **before** `git worktree add`, so an untrusted project
  never leaves a worktree behind. Any other untrusted root (a session
  without a worktree in an untrusted repo) fails with the roots and both
  fixes exactly as `Run::start_with` prints them; `trust_agent_dirs` from
  `~/.coretempo/config.toml` applies. `TrustGate`'s isolated-config
  mirrors are fixed at construction, so the daemon has its own `SpawnGate`
  (`SessionTrustGate`) holding a mirror registry keyed by session id.
  Trust is never granted silently beyond the derived worktree case, which
  the spec states here and the log records on every grant.

## 4. `PtyManager` decoupling

`PtyManager` reads four things from the `FrozenWorkflow` it is built with:
`workflow.agents` (at construction and in `spawn` via `open_pty(agent,
&cfg, size)`), `workflow.idle_debounce`, `cfg.auto_clear`, and
`workflow.system_prompt(agent)` — which `spawn_spec` always emits as
`--append-system-prompt`. `AgentEnv` (per-agent `settings_paths`,
`mcp_paths`, `config_dirs`) is an immutable field read at spawn. Everything
else — queue worker, ring, hooks, debouncer, gates, reaping — is per-agent
and workflow-agnostic. Changes, landed as their own PR before any session
code, with the existing suites as the proof of behaviour preservation
(amendment 46):

1. **Roster, not workflow.**
   ```rust
   pub struct RosterEntry {
       pub cfg: AgentConfig,
       pub system_prompt: Option<String>,  // None = no --append-system-prompt
       pub mcp: McpPolicy,
       pub settings_path: Option<PathBuf>,
       pub config_dir: Option<PathBuf>,
       pub token: Option<Token>,           // None = the AgentEnv token (runs)
       pub resume: Option<String>,         // --resume <claude_session_id>
   }
   // Strict = today's argv: --strict-mcp-config always, --mcp-config only
   // for agents that have a file. Inherit passes neither flag.
   pub enum McpPolicy { Strict(Option<PathBuf>), Inherit }
   pub struct PtyRoster {
       pub agents: BTreeMap<AgentId, RosterEntry>,
       pub idle_debounce: Duration,
   }
   pub fn new(roster: PtyRoster, bus: EventBus, env: AgentEnv) -> Arc<PtyManager>
   ```
   `Run` builds the roster from its frozen workflow (`system_prompt =
   Some(workflow.system_prompt(id))`, `mcp = Strict(mcp_paths.get(id))`,
   `token = None`, `resume = None`). The per-agent maps leave `AgentEnv`,
   which keeps `port`, `token`, `tempo_bin_dir`, `credential_store`;
   `SpawnInputs` gains the resolved token (`entry.token` else `env.token`)
   and exports it as `CORETEMPO_TOKEN`. `Strict(..)` and a `Some` system
   prompt reproduce today's argv byte for byte; `Inherit` passes neither
   `--strict-mcp-config` nor `--mcp-config`; `None` omits
   `--append-system-prompt`. `resume` adds `--resume <id>` to the next
   spawn only: `spawn` consumes it (clears the field), so a stale id can
   never ride along on a later respawn; the daemon sets it fresh before
   every resume.
2. **Dynamic roster.** Handle construction becomes `fn new_handle(&self, id,
   &RosterEntry)`. `add_agent(id, entry) -> Result<(), PtyError>` (error
   `AgentExists`), `set_resume(id, Option<String>)`, `remove_agent(id)`
   (stops it if live, drops the handle, closes its channels so subscribers
   end). Workflow runs call none of these.
3. **Per-agent stop.** `stop(agent)` — `shutdown` for one handle:
   `session.take()`, raw state `Exited`, `take_blocked` with its
   `blocked: false` event, await `reap`. No epoch bump: `restart` bumps the
   epoch to fail queued and in-flight injections with `AgentRestarted`, and
   suppressing the stale exit record is a side effect of that; under `stop`
   the queue fails its injections itself on the debounced `Exited`, and
   because `reap` awaits the `exited` oneshot the reaper sends only after
   `on_child_exit`, the exit is recorded before `stop` returns. The handle,
   ring and subscribers survive; `spawn` on it later is the resume path.
   Listed in amendment 46.

The session daemon owns one `PtyManager` with an empty roster; every
entry carries the session's hook token in `RosterEntry.token`. Each
session's `AgentConfig` is synthesised: `dir = cwd`, `model`,
`permission_mode`, `auto_clear = false`, `prompt = ""`, no edges, tools,
allow or mcp; `system_prompt = None`, `mcp = Inherit`. `idle_debounce` is
2 s, the workflow default.

**Session files** live under `~/.coretempo/sessions/<session-id>/` for the
row's lifetime (stop and resume keep them; delete removes them):
`settings.json` from the same generator as `write_agent_settings_files`
(`on_permission_prompt = wait`, `Bash(tempo:*)`), and for
`isolated_config`, `claude-config/` seeded exactly as `claude_config.rs`
seeds a run's dir. Stability matters: an isolated session's transcript
lives under that `CLAUDE_CONFIG_DIR`, so `--resume` only works because the
directory outlives stop.

**`tempo state` addition.** Every hook payload carries `session_id`;
`tempo state idle` forwards it as `claude_session_id` in the report body.
Inside a session `tempo` resolves the daemon through the `CORETEMPO_PORT`/
`CORETEMPO_TOKEN`/`CORETEMPO_AGENT_ID` the spawn exports (session id as
agent id, hook token as the token), so hook reports reach the sessions
daemon on the same `POST /v1/agents/{id}/state` route runs mount; the run
API ignores the new field, the sessions daemon stores it. No new hook.

**Roster abstraction for the API.** The agent-state, PTY and auth handlers
validate ids against `ctx.workflow.agents` and `ApiContext` carries
run-only fields (`router`, `workflow`, `workflow_file`, `run_id`,
`triggers`, `agent_locks`, `stopping`). Amendment 47 introduces
`trait Roster { fn contains(&AgentId) -> bool; fn ids(&self) -> Vec<AgentId> }`
on `PtySource`, backed by the frozen workflow in runs and by the live
handle set in the daemon, and splits `ApiContext` into a shared core
(`pty`, `bus`, `token`, `bind`, `port`, `started*`) plus a run extension,
so the state, PTY-stream, and the moved write/resize/pause handlers mount
on both without a workflow.

**Dispatch restructure.** `tempo` calls `connect::resolve()` before
matching any subcommand and `coretempod` loads the workflow before
dispatch; both move resolution under the run-scoped commands so `tempo
session …` and `coretempod sessions` never touch a run connection or a
`tempo.toml`.

## 5. Worktrees

**Location.** `~/.coretempo/worktrees/<project-id>/<slug>` — outside the
repository, so `git clean -fdx` in the main checkout cannot delete session
worktrees and the main checkout's own Claude session does not see them in
its tree. The trade-off (relative paths into the main checkout do not
resolve from a session) is accepted.

**Create** (when `create` asks): in the project root,

```
git worktree add -b session/<slug> ~/.coretempo/worktrees/<project-id>/<slug> HEAD
```

`<slug>` = two random words + 4 hex (`brisk-otter-3f1a`), regenerated on a
branch-name collision. `base` = the `HEAD` commit at creation, stored. A
project that is not a git repository, or any `git worktree add` failure,
returns the command and git's stderr; no row is written. Trust for the new
path is the derived grant of §3.

**Status** (computed on every `GET`, not cached — cheap at tens of
sessions): `branch` (the `cwd`'s current branch for every session);
`changed_files` = line count of `git status --porcelain` in `cwd`; `ahead`
= `git rev-list --count <base>..HEAD` for worktree sessions, `null`
otherwise; `worktree_status` = `missing` when the path is no longer in `git
worktree list`, `none` for sessions without one.

**Remove** (`delete` with `remove_worktree`): if the path exists, `git
worktree remove <path>` (`--force` when asked; a dirty tree otherwise fails
422 with the porcelain summary and the `force` hint); if it is `missing`,
`git worktree prune` instead. Then `git branch -D session/<slug>` **only**
when `git merge-base --is-ancestor session/<slug> <base>` — the branch has
no commits of its own. Otherwise the branch stays and the response says so
(`branch_kept: true`). A plain `delete` keeps both.

## 6. HTTP API

All routes under the daemon's `/v1`, bearer-authed, JSON, errors in the
run API's `{ "error": { "code", "message" } }` shape (`ApiErrorBody`,
contracts §5.2); the message carries the roster, the valid values, the fix.
Operator token unless noted.

| Route | Behaviour |
|---|---|
| `GET /v1/health` | `SessionsHealth { ok: true, sessions: { live, total } }` — a new type; the run `Health` requires `run_id` |
| `GET /v1/projects` · `POST /v1/projects { path, name? }` · `DELETE /v1/projects/{id}` | list / register (409 if the path is registered; 422 if not a git repository) / forget — delete refuses 409 while sessions reference it |
| `GET /v1/sessions` | every row with runtime fields |
| `POST /v1/sessions` | `{ project, worktree, cwd?, title?, prompt?, model?, permission_mode?, isolated_config? }` → 201 with the session. `cwd` must be under the project root (422 otherwise). 409 `untrusted` with roots and fixes; 422 on git failure with the command and stderr |
| `GET /v1/sessions/{id}` | one row |
| `POST /v1/sessions/{id}/stop` | 200 with the row after reaping (blocks up to `EXIT_GRACE`); 409 if not live |
| `POST /v1/sessions/{id}/resume` | 200 `{ session, resumed }`; 409 if live or worktree missing |
| `DELETE /v1/sessions/{id}?remove_worktree=&force=` | 200 `{ branch_kept }`; 409 if live; 422 dirty (unless force) |
| `GET /v1/sessions/{id}/pty?since=` (or `Last-Event-ID`) | SSE ring replay + live, the PTY stream contract (contracts §6.2, amendment 40) keyed by session; the cursor is monotonic across stop/resume, so one stream spans respawns |
| `POST /v1/sessions/{id}/pty` (body: raw bytes) | write |
| `POST /v1/sessions/{id}/pty/resize { cols, rows }` | resize |
| `POST /v1/sessions/{id}/pty/pause { paused }` | backpressure flag |
| `POST /v1/agents/{id}/state` | the hook target, mounted from the run API through the `Roster` trait; body gains optional `claude_session_id`. Hook token of that id, or the operator token |
| `GET /v1/events?agent=` | SSE, the `Event` envelope. The `PtyManager`'s own events are emitted as they are today with `agent` = session id: `agent.state`, `agent.lifecycle` (spawned / exited with code and signal — no tail; the ring holds it), `agent.blocked`, `agent.permission_refused`. Added: `session.created`, `session.stopped`, `session.resumed`, `session.deleted` (each carries `agent: <session id>`, so the `?agent=` filter passes them to `attach`) and `project.registered`, `project.forgotten` (always pass the filter, like `run.started`). No duplicated `session.*` copies of `agent.*` |

The three PTY commands that today exist only as Tauri commands (`write`,
`resize`, `pause`) move into `core::api` as handlers both the run API and
the session API mount; the desktop keeps its Tauri commands for workflow
runs and (in Spec B) calls the HTTP routes for sessions.

**Discovery.** `tempo session …` reads `~/.coretempo/sessions/api.json`
only — never the `CORETEMPO_*` environment and never
`runs/current/api.json` — so running it from inside a workflow agent or a
session still addresses the sessions daemon. A missing file, or one whose
`pid` is dead, yields `no session daemon running; start it with
'coretempod sessions'`.

## 7. `tempo session`

| Command | Behaviour |
|---|---|
| `new <project-path> [--worktree] [--cwd DIR] [--title T] [--prompt P] [--model M] [--permission-mode PM] [--isolated-config]` | registers the project if new, creates the session, prints its id and (if any) branch |
| `list` | table: id, project name, branch or `-`, state (`blocked` shown in place of `working` while the flag is set), changed, ahead, title |
| `show <id>` | the JSON row |
| `stop <id>` · `resume <id>` | as the API; `resume` says `resumed conversation <claude_session_id>` or `started fresh` |
| `rm <id> [--remove-worktree] [--force]` | as the API; reports `branch kept` when it was |
| `attach <id>` | raw PTY passthrough: SSE → stdout, stdin in raw mode (termios via `libc`, ~30 lines; no crossterm) → `POST …/pty`, terminal size → `/resize` on start and `SIGWINCH`. Consumes `/v1/events?agent=<id>` alongside the PTY stream because the PTY stream never ends on exit (subscribers outlive the child and span resumes). `Ctrl-]` detaches; no other key is interpreted, so Claude Code's own bindings work. Exit 0 on detach, 1 if the session exits while attached (message names the exit). Uses a client without the 330 s global timeout the other commands share |
| `projects [rm <id>]` | list / forget |

`attach` is what makes the feature usable headless before Spec B and is the
live-verification vehicle.

## 8. Errors

Every failure names the fix:

- untrusted root → the roots and both fixes, as `Run::start_with` prints;
- `git worktree add` failure → the command and git's stderr;
- dirty worktree on remove → the porcelain summary and `force`;
- `cwd` outside the project root → both paths;
- unknown session or project id → the current roster;
- second daemon → the running pid and port;
- hook token used beyond its scope → 403 naming the one route it allows;
- spawn failure (`claude` missing, PTY open) → the existing `PtyError::Spawn`
  text;
- stop/resume/delete in the wrong state → the state and the valid action.

Nothing is swallowed. An unexpected exit publishes `agent.lifecycle` with
code/signal; the ring keeps the last output readable until `delete`.

## 9. Storage

```sql
CREATE TABLE projects (
  id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE, name TEXT NOT NULL,
  created_at TEXT NOT NULL);
CREATE TABLE sessions (
  id TEXT PRIMARY KEY, project TEXT NOT NULL REFERENCES projects(id),
  cwd TEXT NOT NULL, worktree_path TEXT, worktree_branch TEXT, base_commit TEXT,
  title TEXT NOT NULL, harness TEXT NOT NULL DEFAULT 'claude',
  claude_session_id TEXT, model TEXT, permission_mode TEXT,
  isolated_config INTEGER NOT NULL DEFAULT 0, prompt TEXT,
  hook_token TEXT NOT NULL,
  last_state TEXT NOT NULL, exit_code INTEGER, exit_signal TEXT,
  created_at TEXT NOT NULL, stopped_at TEXT);
```

`last_state ∈ { stopped, exited }`; it is written on the first transition
out of live (create is atomic, so a row never exists before its spawn) and
the live `state` comes from the `PtyManager`. `SessionStore`
follows the `Store` conventions (`spawn_blocking` open, WAL, `user_version`).

## 10. Testing

TDD throughout; the scripted fake agent for everything but one live check.

- `core/tests/pty_roster.rs` (refactor PR): `add_agent`/`set_resume`/`stop`/
  `remove_agent` on a live manager; spawn, write, subscribe on an added
  agent; `stop` records `exit`, clears `blocked`, keeps the ring and the
  subscriber; a second `spawn` after `stop` works; `remove` ends
  subscribers; `AgentExists`; the fake agent (a bash script) dumps `$@` to
  a file so a test asserts the argv for `Strict`/`Inherit`, `Some`/`None`
  system prompt and `resume` (`spawn_spec` is `pub(crate)`, so argv is
  observed, not called); all existing `PtyManager` suites unchanged.
- `core/tests/sessions.rs`: `SessionManager` against a temp git repo —
  create with and without worktree, and with `cwd` a subdirectory; `cwd`
  outside the root rejected; branch naming, collision retry; worktree
  location outside the repo; stop/resume/delete; stop-then-resume racing
  (serialized by the mutex, second call sees the new state); dirty refuse
  and force; `branch_kept`; `changed_files`/`ahead`; `worktree_status:
  missing` → resume 409, delete prunes; `claude_session_id` capture (the
  fake agent execs `tempo state idle` with a `SessionStart`-shaped JSON on
  stdin — a harness change from today's direct API reports) and
  latest-wins after a second `SessionStart`; `--resume` in the respawn
  argv; session files created at create, kept through stop, removed on
  delete; daemon shutdown marks rows `exited` with `stopped_at` and a
  reopen shows them; non-git project fails create with git's stderr, no
  row, no files.
- `core/src/trust.rs`: `trust_root(worktree) == worktree` already exists
  and stays. In `core/tests/sessions.rs`: create-with-worktree under a
  trusted project root spawns and the derived grant is written (and
  re-written on resume after the key is reverted); under an untrusted
  project root create fails naming both fixes and leaves no worktree;
  spawn failure (fake agent missing) rolls back row, files and worktree.
- `daemon/tests/sessions_api.rs`: boot on a temp home; every route;
  `api.json` mode 0600, contents, removal on exit; stale-pid overwrite;
  second-instance refusal with pid/port; SSE event order for a full
  lifecycle; PTY write/resize/pause round trip; `Host`/auth rejections; a
  hook token accepted on its own `/state` and 403 on another id's `/state`
  and on every other route.
- `cli/tests/cli_session.rs`: every `tempo session` command against that
  daemon, `attach` over a pipe including `Ctrl-]` detach and exit-while-
  attached status 1; the no-daemon message; `tempo session` with
  `CORETEMPO_*` set still addresses the daemon.
- `./dev live` gains a sessions leg (`#[ignore]`, Haiku, isolated config):
  a real `claude` in a worktree session; the first prompt asks it to
  remember a word; `SessionStart` delivers a `claude_session_id`; `stop`;
  `resume`; a second turn asks for the word back and the answer is checked.
  Two turns. A third, separately ignored probe verifies the rejected
  `--resume` claim (bogus id → exit within seconds, no fallback).

## 11. Out of scope

Explicitly not in Spec A: the desktop Sessions mode (Spec B); Codex/Gemini
harnesses (next step; `harness` column already present); `tempo ask|send`
between sessions; fork and dependencies between sessions; in-app diff,
merge or PR; base-branch selection; auto-resume at daemon boot; moving
workflow runs behind the daemon; per-session token scoping for workflow
runs.

## 12. Contracts amendments

- **46** — `PtyRoster`, `RosterEntry` (incl. `token`), `McpPolicy`, `PtyManager::new(roster,
  …)`, `add_agent`, `set_resume`, `stop`, `remove_agent`,
  `PtyError::AgentExists`; `AgentEnv` loses its per-agent maps.
- **47** — session and project types, `SessionsApiFile`, `SessionsHealth`,
  the `/v1/sessions` and `/v1/projects` routes, the PTY write/resize/pause
  routes, the `Roster` and `TokenAuth` traits and the `ApiContext` split,
  the `session.*` / `project.*` events and their filter rule,
  `claude_session_id` on the state report, hook-token scope and lifetime,
  `SessionTrustGate`, the `tempo`/`coretempod` dispatch restructure.
- **48** — `tempo session` commands and exit statuses.
