#!/usr/bin/env bash
# Copies the built coretempod binary into app/src-tauri/binaries, named with
# the target triple Tauri's externalBin contract requires
# (coretempod-<target-triple>), so `tauri.conf.json`'s
# `bundle.externalBin: ["binaries/coretempod"]` can find it.
#
# Run this before `pnpm tauri build`: the bundler does not build coretempod
# itself, so a build without a prior run of this script fails looking for the
# sidecar.
#
#   ./scripts/prepare-sidecar.sh [debug|release]   (default: release)

set -euo pipefail

PROFILE="${1:-release}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
BINARIES_DIR="$ROOT/app/src-tauri/binaries"

case "$PROFILE" in
debug | release) ;;
*)
  printf 'error: unknown profile %s (expected: debug, release)\n' "$PROFILE" >&2
  exit 1
  ;;
esac

SRC="$ROOT/target/$PROFILE/coretempod"
[ -f "$SRC" ] || {
  printf 'error: no coretempod binary at %s\n' "$SRC" >&2
  printf 'build it first: cargo build -p coretempo-daemon %s--manifest-path %s/Cargo.toml\n' \
    "$([ "$PROFILE" = release ] && echo '--release ' || echo '')" "$ROOT" >&2
  exit 1
}

TARGET_TRIPLE="$(rustc -vV | grep '^host:' | cut -d' ' -f2)"
DEST="$BINARIES_DIR/coretempod-$TARGET_TRIPLE"

mkdir -p "$BINARIES_DIR"
cp "$SRC" "$DEST"
printf '==> sidecar ready: %s\n' "$DEST"
