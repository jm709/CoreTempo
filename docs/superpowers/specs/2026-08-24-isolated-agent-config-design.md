# Isolated agent config (#67)

Date: 2026-08-24
Status: approved design, pre-implementation
Closes: #67 (agents inherit the operator's personal Claude Code setup)

A spawned `claude` runs in the operator's user profile, so every agent
inherits `~/.claude`: the global `CLAUDE.md`, `settings.json` (hooks,
plugins, model), every installed skill and plugin hook, and — because the
agent's `dir` is usually the operator's working repo — the operator's
auto-memory directory, read *and* written. `spawn.rs` scrubs `CLAUDE_CODE_*`
so agents start from a clean slate, but the config dir is the larger slate
and there is no per-agent way to drop it. Observed in the fund-data resolver
flow (issue #67): ~300 lines of unrelated dev standards, a plugin's
"you MUST invoke a skill" preamble, the operator's `MEMORY.md`, and
subagents spending turns on `SendMessage` machinery the workflow never asked
for — paid on every cold session.

Claude Code's only lever for this is `CLAUDE_CONFIG_DIR`, which only the
spawner can set. This spec adds an opt-in per-agent managed config dir.

## Live findings that fix the design (Claude Code 2.1.241, 2026-08-24)

Verified with `claude -p --model haiku` and PTY captures; each is load-bearing
below.

- `CLAUDE_CONFIG_DIR` relocates everything under `~/.claude` **and**
  `~/.claude.json`: the file is created inside the dir, so the trust store
  (`projects[<root>].hasTrustDialogAccepted`) and onboarding state move with
  it. Project-scoped files (`<dir>/CLAUDE.md`, `<dir>/.claude/settings.json`,
  `<dir>/.claude/skills/`) still load. `--settings` hooks still fire.
- An empty dir is **not logged in**. `CLAUDE_SECURESTORAGE_CONFIG_DIR`
  (undocumented; what Claude Code's own teammate spawns use) relocates only
  `.credentials.json` and its refresh lock, independently of
  `CLAUDE_CONFIG_DIR`, so an isolated session pointed at the operator's
  dir shares one login. Placing the file in the managed dir instead does
  not work: a copy goes stale on OAuth refresh, and a symlink is
  **replaced** by it — Claude Code writes `<path>.tmp.<hex>` and renames it
  over the path (fallback opens `O_NOFOLLOW`), so the first refresh leaves
  the managed dir a private token pair, the operator's file the old one,
  and every other holder (the operator's own sessions included) logged out
  at its next refresh because refresh tokens rotate. Verified live 2.1.241
  with an expired copy: symlink → regular file, target untouched;
  `CLAUDE_SECURESTORAGE_CONFIG_DIR` → the rename lands in the store.
- An empty dir opens on the **theme picker** ("Let's get started") before
  anything else — a hookless startup dialog, exactly like the trust dialog.
  `.claude.json` `{"hasCompletedOnboarding": true}` suppresses it. With that
  key, the credential store variable and the trust key, the session lands
  on the prompt with no dialog.
- `settings.json` `{"autoMemoryEnabled": false}` removes the "save memories"
  instructions from the agent's context.
- A fresh dir also raises the **"Bypass Permissions mode" acknowledgment**
  for `permission_mode = "bypassPermissions"` agents. `settings.json`
  `{"skipDangerousModePermissionPrompt": true}` suppresses it; the
  `.claude.json` key `bypassPermissionsModeAccepted` does not.
- A skill directory symlinked into `<config>/skills/<name>/` loads as a
  user-level skill; one committed in `<dir>/.claude/skills/` loads without
  any help. Bundled skills (`dataviz`, `simplify`, …) are always present.
- Alternatives rejected: `--setting-sources project,local` drops the global
  `CLAUDE.md`, skills and plugins but **still loads and can write the
  operator's auto-memory**; `--bare` skips OAuth ("Not logged in" with the
  operator's credentials on disk) and is API-key only.

## Goals

- An agent can be declared to see nothing of the operator's `~/.claude`
  beyond its login: no global `CLAUDE.md`, no operator skills, plugins or
  hooks, no shared auto-memory, no operator `model` default.
- Such an agent still gets everything CoreTempo gives it today: the
  `tempo state` hooks and `allow` rules via `--settings`, its `mcp` servers
  via `--mcp-config`, the repo's `CLAUDE.md` and `.claude/`.
- A workflow can hand an isolated agent a chosen set of skills that live
  next to `tempo.toml`.
- No new startup dialog, no new consent surface, no new machine-local state
  the freeze hash cannot see.

## Non-goals

- Changing the default. Inheritance stays the behaviour for agents that do
  not opt in; it becomes documented.
- An operator-curated `config_dir = "path"`. The managed dir plus `skills`
  covers the stated need; a curated dir would be invisible to the freeze
  hash and a second Claude setup to maintain.
- Persistent per-agent config dirs (agent-owned memory across runs). The
  managed dir is per run. If a workflow needs cross-run memory later, a
  `persist_config` knob relocating the dir to
  `~/.coretempo/agents/<workflow>/<agent>/` slots in without changing
  anything else here.
- Plugins. `--plugin-dir <path>` exists and would be the mechanism; add it
  when a workflow needs a plugin's hooks or MCP servers, not before.
- Making the operator-side readers (`trust.rs`, `mcp.rs`) honour an
  operator-exported `CLAUDE_CONFIG_DIR`. They read `$HOME/.claude.json`
  today and so look at the wrong file for operators who relocate their
  config — a pre-existing bug, filed separately.

## Section 1 — Config surface

Two new `[agents.<id>]` keys, both default-off. `AgentConfig` is
`deny_unknown_fields`, so each is added in `core/src/types/config.rs`,
`app/src/lib/types.ts` (`AgentModel`) and `app/src-tauri/src/merge.rs`
(round-trip; optionals only when non-default, as for `mcp`).

```toml
[agents.resolver]
dir = "~/projects/fund-data"
prompt = "..."
isolated_config = true          # default false: inherit the operator's ~/.claude
skills = ["./skills/handoff"]   # paths relative to tempo.toml; requires isolated_config
```

- `isolated_config: bool` — spawn with `CLAUDE_CONFIG_DIR` set to a
  CoreTempo-managed directory (Section 2).
- `skills: Vec<PathBuf>` — skill directories, resolved relative to the
  directory containing `tempo.toml` (`~` expanded, as `dir` is). Each must
  exist, be a directory, and contain `SKILL.md`; the directory's basename is
  the skill name and must be unique within the agent.

Validation — the first and last rules in `validate_workflow` (structural), the
two path rules at load (`load_workflow`, which knows the `tempo.toml`
directory, exactly as `schema_file` is checked there); error paths
`agents.<id>.skills[<n>]`. The desktop's parse/merge path therefore accepts a
missing skill dir and the run start refuses it:

| condition | error |
|---|---|
| `skills` non-empty, `isolated_config` false | `agents.<id>.skills: declared skills reach the agent only through an isolated config dir; set isolated_config = true or drop skills` |
| path missing / not a directory | `agents.<id>.skills[<n>]: '<resolved>' is not a directory (relative to '<tempo.toml dir>')` |
| no `SKILL.md` | `agents.<id>.skills[<n>]: '<resolved>' has no SKILL.md; a skill is a directory whose SKILL.md carries the frontmatter` |
| duplicate basename | `agents.<id>.skills: two entries are both named '<name>' ('<a>', '<b>'); Claude Code keys skills by directory name` |

A knob that does nothing is a phantom feature, hence the first rule.

## Section 2 — The managed config dir

Path: `~/.coretempo/runs/<run_id>/claude-config-<agent_id>/`, mode 0700,
created by `write_agent_config_dirs` in a new `core/src/claude_config.rs`,
called from `Run::start_with` next to `write_agent_settings_files` and
`write_agent_mcp_files`, for every agent with `isolated_config = true`.

Contents, and nothing else:

| entry | content | why |
|---|---|---|
| `.claude.json` | `{"hasCompletedOnboarding": true}` | the theme-picker dialog an empty dir raises; the trust key — the other hookless dialog — is written into this same file by the gate before the first spawn (Section 3) |
| `settings.json` | `{"autoMemoryEnabled": false, "skipDangerousModePermissionPrompt": true}` | no memory instructions in context, no memory writes into a dir that dies with the run; no bypass-mode acknowledgment — the operator's `permission_mode` line is the consent |
| `skills/<name>` | symlink → each declared skill dir | Section 1 |

Files are written with `write_private_file` (0600, fsync). Symlinks are
plain `std::os::unix::fs::symlink`. The dir is created before any agent
spawns; a spawn or restart never rebuilds it — only the trust key is
re-seeded (Section 3).

No `.credentials.json` is ever placed in the dir. Login comes from the
operator's store: the spawn exports `CLAUDE_SECURESTORAGE_CONFIG_DIR` =
`$CLAUDE_SECURESTORAGE_CONFIG_DIR` if the daemon has it, else
`$CLAUDE_CONFIG_DIR`, else `~/.claude` (`operator_credential_store`), so
every isolated session reads and refreshes the same file under the same
refresh lock as the operator. Unknown home → the variable is not set and
the run logs a warning (API-key and `apiKeyHelper` users need nothing).

Everything else in the spawn recipe is unchanged: `--append-system-prompt`,
`--model`, `--permission-mode`, `--settings <agent-settings-<id>.json>`,
`--strict-mcp-config`, `--mcp-config <agent-mcp-<id>.json>`. With no
operator `settings.json`, `model` in `tempo.toml` is the only model lever
for an isolated agent.

## Section 3 — Trust mirrors the operator's store

The trust dialog fires against the file Claude Code reads, which for an
isolated agent is the managed `.claude.json`. CoreTempo must therefore
write the key there — but writing it unconditionally would let an agent run
with full tool access in a directory the operator never trusted in Claude
Code, circumventing the dialog spec 2026-08-17 §1 exists to respect.

Rule: **the managed key is a mirror, never a second consent source.**

- Preflight (`Run::start_with`, `coretempod serve` boot) runs exactly as
  today against the operator's store, with the same `trust_agent_dirs`
  policy and the same refusal text. Isolated agents' dirs are in the roster
  like every other.
- `TrustGate::before_spawn` (every spawn and restart) runs today's check
  against the operator's store and then, for an isolated agent, writes
  `projects[<trust_root(dir)>].hasTrustDialogAccepted = true` into the
  managed `.claude.json` via `TrustStore::at(<managed>/.claude.json).grant`
  — the same read-modify-write, so whatever Claude Code has since written
  to that file is kept. Re-seeding on every spawn is what stops a live
  session's edits drifting it.
- If the operator's store does not trust the root and policy does not
  grant, the spawn is refused exactly as today; the managed key is never
  written.

`TrustGate` gains the per-agent managed-store map to do this; `TrustStore`,
`preflight` and `TrustPolicy` are unchanged.

## Section 4 — Spawn

`AgentEnv` gains `config_dirs: BTreeMap<AgentId, PathBuf>` and
`credential_store: Option<PathBuf>` beside `settings_paths` and
`mcp_paths`. `spawn_spec`:

- isolated agent: `env` gains `("CLAUDE_CONFIG_DIR", <managed dir>)` and,
  when the store is known, `("CLAUDE_SECURESTORAGE_CONFIG_DIR", <store>)`
  (Section 2);
- otherwise: untouched. An operator-exported `CLAUDE_CONFIG_DIR` keeps
  reaching non-isolated agents, as it does today — that *is* inheriting the
  operator's setup — and `tempo.example.toml` now says so.

`leaked_claude_vars` is unchanged (`CLAUDE_CODE_*` only).

## Section 5 — Freeze hash

`isolated_config` and `skills` already join the hash through the raw
`tempo.toml` bytes. Skill *content* joins after the MCP frames, in agent-id
order, then per agent in declaration order: `push_framed(agent_id)`,
`push_framed(skill name)`, `push_framed(file count)`, then for every
regular file under the skill dir in sorted relative-path order,
`push_framed(relative path)` + `push_framed(bytes)` — the count keeps one
skill's trailing files from reading as the next skill's leading frames. The declared path is `~`-expanded and joined onto the
tempo.toml directory (not canonicalized — the declared basename is the
skill name even when the dir is a symlink); inside it only regular files
and directories are walked — a symlink or any other entry fails the load
naming the path (no cycles, nothing hashed that lives elsewhere). Editing a
skill therefore produces the same `hash mismatch` on a warm run or serve as
editing a schema file, and the `RunError::SourceChanged` text gains "or a
declared skill" to its list.

## Section 6 — Lifecycle

- Built in `Run::start_with` before `spawn_all`, after the settings and MCP
  files. Survives every restart within the run.
- Removed with the run dir: `RunOptions.cleanup_run_dir` (serve mode sets it)
  takes the config dirs with everything else; interactive runs keep the run
  dir as they do today. `Run::stop` waits for every agent process to exit
  (SIGHUP, SIGKILL after `EXIT_GRACE`) before the removal, so Claude Code's
  session-end write cannot resurrect the dir (#94). `remove_dir_all`
  removes symlinks, not their targets, so the skill sources are never
  touched; the operator's credentials were never inside the dir.
- Concurrent runs never share a managed dir (the path carries `run_id`).

## Section 7 — Surfaces and docs

- `AgentDetail` gains `isolated_config: bool` and `skills: Vec<String>`
  (the frozen absolute paths, as `dir` is frozen); the Inspector shows both
  read-only after `auto_clear`.
- `tempo.example.toml`: a paragraph stating that agents inherit the
  operator's `~/.claude` (global `CLAUDE.md`, settings, skills, plugins,
  auto-memory) unless `isolated_config = true`, and documenting both keys.
- `CLAUDE.md`: the theme-picker onboarding dialog joins the list of
  hookless startup dialogs (prevented by the managed `.claude.json`), and
  the "trust mirrors" rule joins the trust gotcha.
- Contracts amendment 37 records the type changes above.

## Section 8 — Tests

Unit (`core`):
- `spawn_spec`: isolated agent gets `CLAUDE_CONFIG_DIR=<managed>` and
  `CLAUDE_SECURESTORAGE_CONFIG_DIR=<store>`, neither when the store is
  unknown beyond the config dir; non-isolated gets no such entries.
- `claude_config` in a tempdir: dir mode 0700; exact `.claude.json` and
  `settings.json` bytes; no `.credentials.json`; `skills/<name>` targets;
  a run with no isolated agent creates nothing; `credential_store_path`
  prefers a non-empty `CLAUDE_SECURESTORAGE_CONFIG_DIR`, then the operator
  config dir.
- `TrustGate`: mirror written when the operator store trusts the root;
  refused and not written when it does not; Claude Code's own edits to the
  managed file are kept across a re-seed.
- Validation: each error in Section 1, exact text.
- Freeze: hash changes when a skill file's bytes change, when a file is
  added, and when the skill is renamed; not when an untracked sibling
  directory changes; two agents declaring the same skill hash it under
  each id.

Integration (`core/tests/run_smoke.rs` style, fake `claude`): a run with
one isolated agent creates the dir before the agent spawns, the fake
`claude` sees `CLAUDE_CONFIG_DIR` and `CLAUDE_SECURESTORAGE_CONFIG_DIR`
(the operator's `~/.claude`), the dir holds no credentials, and serve-mode
cleanup removes the dir while the skill targets remain.

Live (dogfood, haiku): a two-agent workflow with one isolated agent
declaring one skill — no dialog on spawn (`idle` reported), a
`tempo ask` round trip works, and the agent answers "no" to having the
global `CLAUDE.md` and "yes" to having the declared skill.

## Delivery

One PR on `feat/isolated-agent-config`, tasks in this order: config +
validation → freeze hash → `claude_config` writer → trust mirror → spawn
env → lifecycle/cleanup → surfaces + docs → live verification. The
pre-existing `CLAUDE_CONFIG_DIR`-blind reader bug is filed as its own
issue and not fixed here.
