# Contributing

## How this repo works

The public repo is a snapshot mirror of a private working repo: each commit
on `main` is one sync. Issues and pull requests are welcome here — a merged
PR is applied upstream and lands in the next sync commit rather than as your
original commit.

## Getting set up

The [README](README.md) covers requirements and the quickstart. The short
version:

```bash
cp tempo.example.toml tempo.toml
./dev              # desktop app
./dev headless     # headless daemon
./dev check        # every quality gate
```

## The bar for changes

- `./dev check` must pass: cargo test, clippy (pedantic, `-D warnings`),
  rustfmt, svelte-check, oxlint, vitest, and the JS client's tsc/oxlint/vitest.
  CI runs the same gates.
- Zero warnings, no exceptions. No `unwrap`/`panic` in `src`.
- Tests first: write the failing test, watch it fail, then implement.
- Frontend dependencies are pinned exactly — no `^` or `~`.
- [CLAUDE.md](CLAUDE.md) carries the full working conventions and the
  gotchas that cost real debugging time; read it before touching the PTY,
  injection, or state-reporting paths.

## Commits

Imperative mood, ≤72-character subject, one logical change per commit.
