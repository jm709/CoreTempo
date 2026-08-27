//! Templates emitted by `tempo export` (spec §10). Pure string generation —
//! compiled without the `server` feature so the CLI's types-only build can use it.

use crate::types::FlowName;
use crate::types::config::{TriggerType, WorkflowFile};

/// What `tempo export` deploys, resolved from the flows map and the optional
/// `--flow` pick (multi-flow spec §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportTarget {
    /// ≥1 webhook flow, no `--flow`: a standing `coretempod serve` unit.
    Serve,
    /// `--flow <name>` naming an `on_start` flow: a batch unit running
    /// `coretempod run <config> --flow <name>`.
    Batch { flow: FlowName },
    /// No flows: the plain warm-run unit.
    WarmRun,
}

fn flow_roster(file: &WorkflowFile) -> String {
    file.flows
        .keys()
        .map(|f| f.0.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resolves the deployment shape. Errors are written for the CLI to print
/// verbatim: they name the flows and the fix.
///
/// # Errors
/// Unknown `--flow`; `--flow` naming a webhook flow (its deployment is the
/// serve unit); on_start-only files exported without `--flow`.
pub fn export_target(file: &WorkflowFile, flow: Option<&str>) -> Result<ExportTarget, String> {
    let Some(name) = flow else {
        return export_target_default(file);
    };
    let flow_name = FlowName(name.to_string());
    let Some(config) = file.flows.get(&flow_name) else {
        let declared = if file.flows.is_empty() {
            "this workflow declares no [flows.<name>] sections".to_string()
        } else {
            format!("declared flows: {}", flow_roster(file))
        };
        return Err(format!(
            "no flow named '{name}'; {declared} — pick one of them, or drop \
             --flow to export the workflow's default unit"
        ));
    };
    match config.trigger.trigger_type {
        TriggerType::OnStart => Ok(ExportTarget::Batch { flow: flow_name }),
        TriggerType::Webhook => Err(format!(
            "flow '{name}' is a webhook flow; webhook flows deploy as the \
             standing serve unit a plain `tempo export` already emits — \
             --flow picks an on_start flow for a batch unit"
        )),
    }
}

/// `export_target`'s no-`--flow` arm: any webhook flow wins the serve unit; otherwise
/// a declared `on_start` flow needs `--flow` to pick one; no flows keeps the warm-run unit.
fn export_target_default(file: &WorkflowFile) -> Result<ExportTarget, String> {
    let mut saw_on_start = false;
    for config in file.flows.values() {
        match config.trigger.trigger_type {
            TriggerType::Webhook => return Ok(ExportTarget::Serve),
            TriggerType::OnStart => saw_on_start = true,
        }
    }
    if !saw_on_start {
        return Ok(ExportTarget::WarmRun);
    }
    let on_start = file
        .flows
        .iter()
        .filter(|(_, f)| f.trigger.trigger_type == TriggerType::OnStart)
        .map(|(n, _)| n.0.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "every flow in this workflow is on_start ({on_start}); a batch \
         unit needs to know which one to run — re-run with --flow <name>"
    ))
}

/// The service's `ExecStart` line and `Restart=` line, chosen by target.
///
/// `Serve` serves triggers over HTTP and should come back up after any
/// failure. `Batch` and `WarmRun` run once per invocation, so `on-failure`
/// is right for `WarmRun` — but a successful `Batch` run exits 0, which
/// `on-failure` would leave stopped; `always` makes this unit a re-running
/// batch worker instead.
fn service_lines(config_path: &str, target: &ExportTarget) -> (String, &'static str) {
    match target {
        ExportTarget::Serve => (
            format!("ExecStart=%h/.local/bin/coretempod serve {config_path}"),
            "Restart=on-failure",
        ),
        ExportTarget::Batch { flow } => (
            format!("ExecStart=%h/.local/bin/coretempod run {config_path} --flow {flow}"),
            "# a successful batch run exits 0; on-failure would leave it stopped — always\n\
             # makes this a re-running batch worker. Cost: each restart spawns the full\n\
             # agent roster against a paid API, so an enabled unit runs a complete\n\
             # workflow every completion + RestartSec — raise RestartSec, or drive this\n\
             # from a systemd .timer instead, for scheduled batches.\n\
             Restart=always",
        ),
        ExportTarget::WarmRun => (
            format!("ExecStart=%h/.local/bin/coretempod run {config_path}"),
            "Restart=on-failure",
        ),
    }
}

/// systemd *user* unit (agents need the user's credentials and home directory).
/// `config_path` must be the absolute path of the exported `tempo.toml`.
#[must_use]
pub fn systemd_unit(workflow_name: &str, config_path: &str, target: &ExportTarget) -> String {
    let (exec_start, restart) = service_lines(config_path, target);
    format!(
        "# CoreTempo systemd user unit for workflow '{workflow_name}'.\n\
         # Install: cp this file to ~/.config/systemd/user/coretempo-{workflow_name}.service\n\
         # Then:    systemctl --user daemon-reload && \
         systemctl --user enable --now coretempo-{workflow_name}\n\
         # Note: a USER unit — agents need your credentials and home directory.\n\
         \n\
         [Unit]\n\
         Description=CoreTempo workflow '{workflow_name}'\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         # Adjust if coretempod is installed elsewhere.\n\
         {exec_start}\n\
         {restart}\n\
         RestartSec=5\n\
         Environment=CORETEMPO_LOG=info\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// Dockerfile whose real job is the agent runtime: Node 22 + claude-code + git.
/// Build context: this export directory containing `tempo.toml` plus static-musl
/// `coretempod` and `tempo` binaries (see `docs/build-musl.md`).
#[must_use]
pub fn dockerfile(target: &ExportTarget) -> String {
    let entrypoint = match target {
        ExportTarget::Serve => {
            "ENTRYPOINT [\"/usr/local/bin/coretempod\", \"serve\", \"/workflow/tempo.toml\"]\n"
                .to_string()
        }
        ExportTarget::Batch { flow } => format!(
            "ENTRYPOINT [\"/usr/local/bin/coretempod\", \"run\", \"/workflow/tempo.toml\", \
             \"--flow\", \"{flow}\"]\n"
        ),
        ExportTarget::WarmRun => {
            "ENTRYPOINT [\"/usr/local/bin/coretempod\", \"run\", \"/workflow/tempo.toml\"]\n"
                .to_string()
        }
    };
    // A public serve bind 403s any caller whose Host header isn't localhost, loopback,
    // or the bind IP literal — only relevant once this image actually serves webhooks.
    let host_note = if *target == ExportTarget::Serve {
        "# A public serve bind also 403s any caller whose Host header isn't localhost,\n\
         # loopback, or this container's bind IP literal — put it behind a reverse proxy\n\
         # that rewrites Host, or tunnel to a loopback bind instead.\n"
    } else {
        ""
    };
    format!(
        "# CoreTempo headless runtime.\n\
         # Build context must contain: tempo.toml, coretempod, tempo (static musl builds).\n\
         # Run: docker run -e ANTHROPIC_API_KEY=... -e CORETEMPO_TOKEN=<64 hex> \
         -p 4820:4820 <img>\n\
         FROM node:22-bookworm-slim\n\
         RUN apt-get update \\\n \
          && apt-get install -y --no-install-recommends git ca-certificates \\\n \
          && rm -rf /var/lib/apt/lists/*\n\
         RUN npm install -g @anthropic-ai/claude-code\n\
         COPY coretempod /usr/local/bin/coretempod\n\
         COPY tempo /usr/local/bin/tempo\n\
         WORKDIR /workflow\n\
         COPY tempo.toml ./tempo.toml\n\
         # 0.0.0.0 is correct inside a container; the server refuses to start without a\n\
         # provisioned token when bound off-loopback, so CORETEMPO_TOKEN is required.\n\
         {host_note}\
         ENV CORETEMPO_BIND=0.0.0.0\n\
         EXPOSE 4820\n\
         {entrypoint}"
    )
}
