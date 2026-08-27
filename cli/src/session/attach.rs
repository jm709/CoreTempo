//! `tempo session attach <id>`: raw terminal passthrough over the session's
//! PTY routes (spec 2026-08-27 §7).

use std::process::ExitCode;

use crate::client::Client;

pub(crate) fn run(_client: &Client, id: &str) -> anyhow::Result<ExitCode> {
    anyhow::bail!("attaching to session '{id}' is not implemented yet")
}
