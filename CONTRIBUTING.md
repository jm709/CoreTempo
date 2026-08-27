# Contributing

## How this repo works

This is the development repo: features and fixes land here as pull requests
against `main`, and your merged commits are the history. Commits before
2026-08-27 are snapshots of an earlier private repo; issue and PR numbers
cited in older specs and in `CLAUDE.md` refer to that repo's tracker, not
this one's.

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
