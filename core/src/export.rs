//! Templates emitted by `tempo export` (spec §10). Pure string generation —
//! compiled without the `server` feature so the CLI's types-only build can use it.

use crate::types::config::TriggerType;

/// The service's `ExecStart` line and `Restart=` line, chosen by trigger.
///
/// `webhook` serves triggers over HTTP and should come back up after any
/// failure. `on_start` and the no-trigger default run once per invocation, so
/// `on-failure` is right for them — but a successful `on_start` batch run
/// exits 0, which `on-failure` would leave stopped; `always` makes this unit
/// a re-running batch worker instead.
fn service_lines(config_path: &str, trigger: Option<TriggerType>) -> (String, &'static str) {
    match trigger {
        Some(TriggerType::Webhook) => (
            format!("ExecStart=%h/.local/bin/coretempod serve {config_path}"),
            "Restart=on-failure",
        ),
        Some(TriggerType::OnStart) => (
            format!("ExecStart=%h/.local/bin/coretempod run {config_path}"),
            "# a successful batch run exits 0; on-failure would leave it stopped — always\n\
             # makes this a re-running batch worker. Cost: each restart spawns the full\n\
             # agent roster against a paid API, so an enabled unit runs a complete\n\
             # workflow every completion + RestartSec — raise RestartSec, or drive this\n\
             # from a systemd .timer instead, for scheduled batches.\n\
             Restart=always",
        ),
        None => (
            format!("ExecStart=%h/.local/bin/coretempod run {config_path}"),
            "Restart=on-failure",
        ),
    }
}

/// systemd *user* unit (agents need the user's credentials and home directory).
/// `config_path` must be the absolute path of the exported `tempo.toml`.
#[must_use]
pub fn systemd_unit(
    workflow_name: &str,
    config_path: &str,
    trigger: Option<TriggerType>,
) -> String {
    let (exec_start, restart) = service_lines(config_path, trigger);
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
pub fn dockerfile(trigger: Option<TriggerType>) -> String {
    let subcommand = if trigger == Some(TriggerType::Webhook) {
        "serve"
    } else {
        "run"
    };
    // A public serve bind 403s any caller whose Host header isn't localhost, loopback,
    // or the bind IP literal — only relevant once this image actually serves webhooks.
    let host_note = if trigger == Some(TriggerType::Webhook) {
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
         ENTRYPOINT [\"/usr/local/bin/coretempod\", \"{subcommand}\", \"/workflow/tempo.toml\"]\n"
    )
}
