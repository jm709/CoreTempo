# Static musl builds (`coretempod` + `tempo`)

The headless export ships fully static Linux binaries. Nothing in either binary needs
TLS (the daemon is loopback-or-reverse-proxy; the CLI is loopback-only), rusqlite uses
the `bundled` feature, and portable-pty works on musl — so static linking is painless.

## One-time setup

    rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
    cargo install cargo-zigbuild   # requires zig on PATH (https://ziglang.org/download)

cargo-zigbuild uses `zig cc` as the C cross-compiler, which handles rusqlite's bundled
sqlite3.c for both architectures without a musl-gcc toolchain.

## Build

    cargo zigbuild --release --target x86_64-unknown-linux-musl \
        -p coretempo-daemon -p coretempo-cli
    cargo zigbuild --release --target aarch64-unknown-linux-musl \
        -p coretempo-daemon -p coretempo-cli

Artifacts land in `target/<triple>/release/{coretempod,tempo}` (~5–10 MB each after
the release profile's `strip = "symbols"`).

## Verify

    file target/x86_64-unknown-linux-musl/release/coretempod
    # → ELF 64-bit LSB executable, x86-64, statically linked
    ldd target/x86_64-unknown-linux-musl/release/coretempod
    # → "statically linked" (or "not a dynamic executable")

## Use with the export

`tempo export <dir>` writes a Dockerfile whose build context expects `coretempod` and
`tempo` next to the exported `tempo.toml` — copy the musl artifacts in before
`docker build`. The systemd user unit expects `coretempod` at `~/.local/bin/`.
