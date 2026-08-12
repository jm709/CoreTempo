# CoreTempo

A desktop app (and headless daemon) that runs multi-agent Claude Code
workflows. CoreTempo spawns `claude` sessions in PTYs it owns, routes messages
between them by typing into their prompts, and shows the traffic in a
terminal-centric UI.

You describe a workflow in one TOML file — agents, their working directories,
prompts, and the edges between them — and CoreTempo runs it: spawning each
agent, delegating work along the edges, and watching agent state to know when
work is done.

## How it works

- Each agent is a real `claude` session in a PTY that CoreTempo owns. The
  server is the only writer to any PTY, and per-agent injections are
  serialized through one queue.
- Agents talk to each other with the bundled `tempo` CLI: `tempo ask` expects
  a reply, `tempo send` does not. The server injects each message into the
  target agent's prompt as typed text and logs everything to SQLite.
- Agent state (idle/working) comes from Claude Code hooks, not from parsing
  the terminal screen.
- Edges declared in the workflow file become numbered delegation steps in each
  agent's prompt and are enforced at runtime: an agent that idles with unmet
  steps gets one nudge, then is flagged as stalled.
- An optional `[trigger]` section makes a workflow self-starting: inject a
  kickoff message at launch, or accept webhook kickoffs over HTTP with a
  JSON-Schema-validated reply (`coretempod serve`).

## Requirements

- Rust (stable, installed via `rustup` — the workspace pins its toolchain)
- Node 22+ with `pnpm` (`corepack enable`)
- [Claude Code](https://claude.com/claude-code) (`claude` on your PATH)
- For the Linux desktop app: webkit2gtk and GTK development libraries.
  `./dev` checks for them and prints the `apt-get` install line if missing.

## Quickstart

```bash
cp tempo.example.toml tempo.toml    # then edit the dirs and prompts
./dev                               # desktop app (vite + Rust core in one process)
./dev headless                      # coretempod against ./tempo.toml
./dev check                         # every gate: cargo test/clippy/fmt, svelte-check, oxlint, vitest
```

`tempo.example.toml` documents every workflow option. The desktop app embeds
the backend — there is no separate server to start.

## Layout

| Crate | What it owns |
|---|---|
| `core` | PTY manager, message router, SQLite store, axum `/v1` API, event bus, workflow load/freeze. Zero UI dependencies. |
| `app` | Tauri 2 shell (`app/src-tauri`) + Svelte 5 webview (`app/src`). |
| `cli` | The `tempo` binary agents call from their Bash tool. |
| `daemon` | `coretempod`, the headless runner. |
| `clients/js` | `@coretempo/client`, the typed npm client for webhook triggers. |

Design docs live in `docs/superpowers/specs/`, starting with
[the CoreTempo design spec](docs/superpowers/specs/2026-08-01-coretempo-design.md).
`CLAUDE.md` carries the working conventions and hard-won gotchas for anyone
developing the repo with Claude Code.

## License

[MIT](LICENSE)
