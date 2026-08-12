//! `tempo export <dir>`: freeze the running workflow into a deployable directory
//! (contract §7.1, spec §10) — `tempo.toml`, a systemd *user* unit, and a Dockerfile.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use coretempo_core::export::{dockerfile, systemd_unit};
use coretempo_core::types::WorkflowResponse;

use crate::client::Client;

/// Fetches `GET /v1/workflow` and writes the export directory, creating it if needed.
pub fn cmd_export(client: &Client, dir: &Path) -> anyhow::Result<ExitCode> {
    let value = client.get("/workflow")?;
    let response: WorkflowResponse = serde_json::from_value(value)?;
    let config = toml::to_string_pretty(&response.workflow)
        .context("cannot serialize the running workflow back to TOML")?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("cannot create export directory {}", dir.display()))?;

    let config_path = write(dir, "tempo.toml", &config)?;
    // The unit runs on the host, so it needs the absolute path of the file just written.
    let absolute = std::path::absolute(&config_path).unwrap_or(config_path);
    let trigger = response.workflow.trigger.as_ref().map(|t| t.trigger_type);
    let unit = systemd_unit(
        &response.workflow.workflow.name,
        &absolute.to_string_lossy(),
        trigger,
    );
    write(dir, "coretempo.service", &unit)?;
    write(dir, "Dockerfile", &dockerfile(trigger))?;
    Ok(ExitCode::SUCCESS)
}

fn write(dir: &Path, name: &str, content: &str) -> anyhow::Result<PathBuf> {
    let path = dir.join(name);
    std::fs::write(&path, content).with_context(|| format!("cannot write {}", path.display()))?;
    println!("{}", path.display());
    Ok(path)
}
