# Security Policy

## Reporting a vulnerability

Report privately via GitHub: the [Security tab → "Report a
vulnerability"](https://github.com/jm709/CoreTempo/security/advisories/new).
Please do not open a public issue for security reports. You can expect an
initial response within a week.

## Supported versions

Pre-1.0: only the latest commit on `main` is supported.

## Threat model

- The `/v1` API binds `127.0.0.1` by default. Non-loopback binds require a
  provisioned token and validate the `Host` header.
- CoreTempo spawns Claude Code agents with real shell access in the
  directories the workflow file configures. A workflow file is trusted
  input: running an untrusted `tempo.toml` is running untrusted code, and
  that is by design, not a vulnerability.
- Reports we care about include: `Host`/origin validation bypasses on the
  API, triggering message injection into a PTY without going through the
  authenticated API surface, and the webhook trigger path accepting input
  it should reject.
