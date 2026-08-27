# Session manager — Spec A: core, daemon, API, CLI

Date: 2026-08-27
Status: draft for review
Amends: the design spec (2026-08-01) and the contracts doc (amendment 46 and
following, assigned as the PRs land).
Follow-up: Spec B (desktop Sessions mode) is designed after this spec ships.

## 1. Purpose

CoreTempo runs workflows: frozen prompts, routed messages, flows. This spec
adds a second, separate use — a **session manager** in the style of Spotify's
Xirp: many independent Claude Code sessions across many projects, each
optionally in its own git worktree, living in a background daemon that
outlives the desktop window, visible at a glance (working / idle / needs
input), resumable, and driveable from the CLI so an agent can do everything
an operator can.

Sessions are not workflow agents. They have no frozen prompt, no edges, no
`tempo ask|send`, no auto-`/clear`. They are the operator's ordinary Claude
Code, owned by CoreTempo's PTY layer so state, injection timing, trust and
isolation come for free.

Decided during brainstorming (2026-08-27):

| Question | Decision |
|---|---|
| Core job | Sessions across projects **and** per-session worktrees, first-class from day one |
| Persistence | Sessions outlive the window: a background daemon |
| Harnesses | Claude Code only; Codex/Gemini are next steps — the schema carries `harness` now |
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
| `cwd` | the project root, or the worktree path |
| `worktree` | `{ path, branch }` when the daemon created one; `null` otherwise |
| `title` | operator-given, else the first line of `prompt`, else the branch or directory name |
| `harness` | `"claude"` (the only value in v1; present so other harnesses are an additive change) |
| `claude_session_id` | captured from the `SessionStart` hook; `null` until it arrives |
| `model`, `permission_mode`, `isolated_config` | launch options, fixed at creation. `permission_mode` defaults to the operator's own default (no bypass); `isolated_config` defaults to `false` — sessions are personal and should see the operator's `~/.claude` |
| `prompt` | optional first turn, injected once the prompt is ready using the queue's existing submit verification |
| `created_at`, `stopped_at` | timestamps; `stopped_at` is `null` while live |
| runtime (not stored) | `state` = `starting | idle | working | exited`; `blocked` side flag (`{ tool, since }`) exactly as agents today; `exit` (`{ code, signal }`); `pty_cursor`; `changed_files`; `ahead`; `worktree_status` = `present | missing | none` |

### Lifecycle

```
create ──▶ live ──stop──▶ stopped ──resume──▶ live
              │                        
              └──exit (child gone)──▶ exited ──resume──▶ live
stopped | exited ──delete──▶ gone   (--remove-worktree also removes the worktree)
```

- **create**: register the project if new, create the worktree if asked
  (§5), run the trust gate, spawn, inject `prompt` if given. Any failure
  before spawn leaves no session row and no worktree.
- **stop**: SIGTERM, reap with the existing `EXIT_GRACE` (5 s) then
  SIGKILL, row keeps everything, `stopped_at` set, worktree kept.
- **exit**: the child left on its own. Same row treatment as stop; the last
  ring tail stays readable through the PTY stream until `delete`.
- **resume**: respawn in the same `cwd`. With a `claude_session_id`, pass
  `--resume <id>`; without one, a fresh spawn — the response carries
  `resumed: false`. The creation `prompt` is never injected again. If Claude Code rejects the id (transcript gone) the
  session exits within seconds and is reported `exited` with the PTY tail in
  the event; there is no silent fallback to a fresh start.
- **delete**: the row goes. With `remove_worktree`, §5 applies. A live
  session cannot be deleted; stop it first (the error says so).
- Daemon shutdown stops every live session and marks the rows `exited`.
  Nothing auto-resumes at boot; sessions come back on demand.

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

| File | Purpose |
|---|---|
| `api.json` | the `ApiFile` shape (`port`, `token`) plus `pid`, mode 0600, written after the listener binds, deleted on clean exit. A stale file (dead pid) is overwritten by the next start. |
| `sessions.db` | SQLite: `projects`, `sessions`. Its own file, never shared with a workflow store, so the two never contend for one lock. |
| `sessions.lock` | `flock`ed for the daemon's lifetime; a second start finds it held, prints the running pid and port from `api.json`, exits 1. |
| `daemon.log` | `tracing` to file; stderr in the foreground. |

- Binds loopback only. The token is generated per start; there is no
  provisioning rule — nothing remote calls this API. Bearer auth and `Host`
  validation are the run API's.
- Runs in the foreground. Supervision (systemd, tmux, `setsid`) is the
  operator's; the desktop (Spec B) spawns it detached when `api.json` is
  absent or its pid is dead, then polls `/v1/health`.
- SIGTERM/SIGINT: stop every session (§2), mark rows, remove `api.json`,
  exit 0. `coretempod sessions stop` sends SIGTERM to the pid in `api.json`
  and is the only management subcommand.
- **Trust**: the same `TrustGate` runs before every spawn and resume. An
  untrusted root fails `create` with the roots and both fixes
  (`trust_agent_dirs` applies from `~/.coretempo/config.toml`). Trust is
  never granted silently. A worktree resolves to the project root under
  `trust_root` (git toplevel of the physical path), so one entry covers every
  session on a project; a test pins this.

## 4. `PtyManager` decoupling

`PtyManager` reads three things from the `FrozenWorkflow` it is built with:
`workflow.agents` (at construction and in `spawn` via `open_pty(agent,
&cfg, size)`), `workflow.idle_debounce`, and `cfg.auto_clear`. Everything
else — queue worker, ring, hooks, debouncer, gates, reaping — is per-agent
and workflow-agnostic. Two mechanical changes, landed as their own PR before
any session code, with the existing suites as the proof of behaviour
preservation:

1. **Roster, not workflow.**
   ```rust
   pub struct PtyRoster {
       pub agents: BTreeMap<AgentId, AgentConfig>,
       pub idle_debounce: Duration,
   }
   impl From<&FrozenWorkflow> for PtyRoster { … }
   pub fn new(roster: PtyRoster, bus: EventBus, env: AgentEnv) -> Arc<PtyManager>
   ```
   The `workflow` field is gone; `Run` builds the roster from its frozen
   workflow. `spawn_all` iterates the roster it was given.
2. **Dynamic roster.** The per-agent handle construction becomes
   `fn new_handle(&self, id: &AgentId, cfg: &AgentConfig) -> AgentHandle`.
   `add_agent(id, cfg) -> Result<(), PtyError>` (error `AgentExists` if the
   id is taken) and `remove_agent(id) -> Result<(), PtyError>` (shuts that
   session down through the existing reaping path, drops the handle, closes
   its channels so output/state subscribers end). Workflow runs never call
   either; their roster just happens to be fixed.

The session daemon owns one `PtyManager` with an empty roster and an
`AgentEnv` whose per-agent maps (`settings_paths`, `config_dirs`) it fills
per session. Each session's `AgentConfig` is synthesised: `dir = cwd`,
`model`, `permission_mode`, `auto_clear = false`, `prompt = ""`, no edges,
no tools, no allow, no mcp. The settings file comes from the same
`write_agent_settings_files` path with `on_permission_prompt = wait`.

**Spawn recipe knob.** Workflow agents always get `--strict-mcp-config` and
an `--mcp-config` file; sessions must inherit the operator's normal MCP
setup. The spec builder takes an MCP policy:

```rust
pub enum McpPolicy { Strict(PathBuf), Inherit }
```

`Strict` is today's behaviour, `Inherit` passes neither flag.

**`tempo state` addition.** The `SessionStart` hook payload carries
`session_id`; `tempo state idle` forwards it as `claude_session_id` in the
report body. Inside a session `tempo` resolves the daemon through the
`CORETEMPO_PORT`/`CORETEMPO_TOKEN`/`CORETEMPO_AGENT_ID` the spawn exports
(session id as agent id), so the hook reports reach the sessions daemon on
the same `POST /v1/agents/{id}/state` route the run API mounts; the run
API ignores the new field, the sessions daemon stores it. No new hook, no
new route for hooks.

## 5. Worktrees

**Create** (when `create` asks): in the project root,

```
git worktree add -b session/<slug> <project>/.coretempo-worktrees/<slug> HEAD
```

`<slug>` = two random words + 4 hex (`brisk-otter-3f1a`), regenerated on a
branch-name collision. The worktree lives inside the repo so relative
tooling works; `.coretempo-worktrees/` is appended once to
`.git/info/exclude`, never to the tracked `.gitignore`. The base is always
the project's current `HEAD` (no base-branch option in v1). A project that is
not a git repository, or any `git worktree add` failure, returns the command
and git's stderr; no row is written.

**Status** (computed on every `GET`, not cached — cheap at tens of
sessions): `branch`; `changed_files` = line count of `git status --porcelain`
in `cwd`; `ahead` = `git rev-list --count <base>..HEAD` where `<base>` is
the commit recorded at creation; `worktree_status` = `missing` when the path
is no longer in `git worktree list`. A session without a worktree reports
the `cwd`'s current branch, `ahead: null` and `worktree_status: none`.

**Remove** (`delete` with `remove_worktree`): `git worktree remove <path>`
(`--force` when asked; a dirty tree otherwise fails with the porcelain
summary and the hint), then `git branch -D session/<slug>` **only** when
`git merge-base --is-ancestor <branch> <base>` — i.e. the branch has no
commits of its own. Otherwise the branch stays and the response says so
(`branch_kept: true`). A plain `delete` keeps both.

## 6. HTTP API

All routes under the daemon's `/v1`, bearer-authed, JSON, errors in the
existing `CmdError` shape (`code`, `message`; the message carries the
roster, the valid values, the fix).

| Route | Behaviour |
|---|---|
| `GET /v1/health` | `{ ok: true, sessions: { live, total } }` |
| `GET /v1/projects` · `POST /v1/projects { path, name? }` · `DELETE /v1/projects/{id}` | list / register (409 if the path is registered) / forget — delete refuses while sessions reference it |
| `GET /v1/sessions` | every row with runtime fields |
| `POST /v1/sessions` | `{ project, worktree, title?, prompt?, model?, permission_mode?, isolated_config? }` → 201 with the session. 409 `untrusted` with roots and fixes; 422 on git failure with the command and stderr |
| `GET /v1/sessions/{id}` | one row |
| `POST /v1/sessions/{id}/stop` | 200 with the row; 409 if not live |
| `POST /v1/sessions/{id}/resume` | 200 `{ session, resumed: bool }`; 409 if live |
| `DELETE /v1/sessions/{id}?remove_worktree=&force=` | 200 `{ branch_kept }`; 409 if live; 422 dirty (unless force) |
| `GET /v1/sessions/{id}/pty?cursor=` | SSE ring replay + live, the existing contract (§8.2) keyed by session |
| `POST /v1/sessions/{id}/pty` (body: raw bytes) | write to the PTY |
| `POST /v1/sessions/{id}/pty/resize { cols, rows }` | resize |
| `POST /v1/sessions/{id}/pty/pause { paused }` | backpressure flag |
| `POST /v1/agents/{id}/state` | the hook target, mounted unchanged from the run API; the report body gains optional `claude_session_id` |
| `GET /v1/events` | SSE, the `Event` envelope with new payloads: `project.registered`, `project.forgotten`, `session.created`, `session.state`, `session.blocked`, `session.unblocked`, `session.permission_refused` (never expected under `wait`, kept for parity), `session.exited`, `session.stopped`, `session.resumed`, `session.deleted` |

The three PTY commands that today exist only as Tauri commands (`write`,
`resize`, `pause`) move into `core::api` as handlers both the run API and
the session API mount; the desktop keeps its Tauri commands for workflow
runs and (in Spec B) calls the HTTP routes for sessions.

**Discovery.** `tempo session …` reads `~/.coretempo/sessions/api.json`
only — never the `CORETEMPO_*` environment and never
`runs/current/api.json`, so running it from inside a workflow agent or a
session still addresses the sessions daemon. A missing file, or one whose
`pid` is dead, yields `no session daemon running; start it with
'coretempod sessions'`.

## 7. `tempo session`

| Command | Behaviour |
|---|---|
| `new <project-path> [--worktree] [--title T] [--prompt P] [--model M] [--permission-mode PM] [--isolated-config]` | registers the project if new, creates the session, prints its id and (if any) branch |
| `list` | table: id, project name, branch or `-`, state (`blocked` shown in place of `working` while the flag is set), changed, ahead, title |
| `show <id>` | the JSON row |
| `stop <id>` · `resume <id>` | as the API; `resume` says `resumed conversation <claude_session_id>` or `started fresh` |
| `rm <id> [--remove-worktree] [--force]` | as the API; reports `branch kept` when it was |
| `attach <id>` | raw PTY passthrough: SSE → stdout, stdin (raw mode) → `POST …/pty`, terminal size → `/resize` on start and `SIGWINCH`. `Ctrl-]` detaches; no other key is interpreted, so Claude Code's own bindings work. Exit status 0 on detach, 1 if the session exits while attached (message names the exit) |
| `projects [rm <id>]` | list / forget |

`attach` is what makes the feature usable headless before Spec B and is the
live-verification vehicle.

## 8. Errors

Every failure names the fix:

- untrusted root → the roots and both fixes, as `Run::start_with` prints;
- `git worktree add` failure → the command and git's stderr;
- dirty worktree on remove → the porcelain summary and `--force`;
- unknown session or project id → the current roster;
- second daemon → the running pid and port;
- spawn failure (`claude` missing, PTY open) → the existing `PtyError::Spawn`
  text;
- delete/stop/resume in the wrong state → the state and the valid action.

Nothing is swallowed. An unexpected exit publishes `session.exited` with
code/signal and keeps the ring tail readable until `delete`.

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
  last_state TEXT NOT NULL, exit_code INTEGER, exit_signal TEXT,
  created_at TEXT NOT NULL, stopped_at TEXT);
```

`last_state` is what the daemon last knew (`exited` after shutdown); the
live `state` comes from the `PtyManager` while the daemon runs. The same
`Store` conventions apply (`spawn_blocking` open, WAL, version table).

## 10. Testing

TDD throughout; the scripted fake agent for everything but one live check.

- `core/tests/pty_roster.rs` (refactor PR): `add_agent`/`remove_agent` on a
  live manager; spawn, write, subscribe on an added agent; `remove` ends
  subscribers; `AgentExists`; all existing `PtyManager` suites unchanged.
- `core/tests/sessions.rs`: `SessionManager` against a temp git repo —
  create with and without worktree; branch naming, collision retry,
  `.git/info/exclude` append (once); stop/resume/delete; dirty refuse and
  force; `branch_kept`; `changed_files`/`ahead`; `worktree_status: missing`;
  `claude_session_id` capture from a scripted `SessionStart`; `--resume`
  appears in the respawn argv; daemon shutdown marks rows `exited` and a
  reopen shows them; non-git project fails create with git's stderr and no
  row.
- `daemon/tests/sessions_api.rs`: boot on a temp home; every route;
  `api.json` mode 0600, contents, removal on exit; stale-pid overwrite;
  second-instance refusal with pid/port; SSE event order for a full
  lifecycle; PTY write/resize/pause round trip; `Host`/auth rejections.
- `cli/tests/cli_session.rs`: every `tempo session` command against that
  daemon, `attach` over a pipe including `Ctrl-]` detach and exit-while-
  attached status 1; the no-daemon message.
- `core/src/trust.rs`: `trust_root(worktree) == project root`.
- `./dev live` gains a sessions leg (`#[ignore]`, Haiku, isolated config):
  a real `claude` in a worktree session; the first prompt lands;
  `SessionStart` delivers a `claude_session_id`; `stop`; `resume` continues
  the same conversation. Two turns.

## 11. Out of scope

Explicitly not in Spec A: the desktop Sessions mode (Spec B); Codex/Gemini
harnesses (next step; `harness` column already present); `tempo ask|send`
between sessions; fork and dependencies between sessions; in-app diff,
merge or PR; base-branch selection; auto-resume at daemon boot; moving
workflow runs behind the daemon.

## 12. Contracts amendments

Assigned up front, renumbered on rebase if PRs cross:

- **46** — `PtyRoster`, `PtyManager::new(roster, …)`, `add_agent`,
  `remove_agent`, `PtyError::AgentExists`; `McpPolicy` on the spawn spec.
- **47** — session and project types, the `/v1/sessions` and `/v1/projects`
  routes, the `session.*`/`project.*` events, `claude_session_id` on the
  state report, the `api.json` discovery file.
- **48** — `tempo session` commands and exit statuses.
