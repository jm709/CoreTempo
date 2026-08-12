use coretempo_core::export::{dockerfile, systemd_unit};
use coretempo_core::types::config::TriggerType;

#[test]
fn systemd_unit_is_a_user_unit_running_coretempod() {
    let unit = systemd_unit("core-tempo-dev", "/srv/tempo/tempo.toml", None);
    assert!(unit.contains("[Unit]"));
    assert!(unit.contains("Description=CoreTempo workflow 'core-tempo-dev'"));
    assert!(unit.contains("ExecStart=%h/.local/bin/coretempod run /srv/tempo/tempo.toml"));
    assert!(unit.contains("Restart=on-failure"));
    // user unit: installs under default.target, not multi-user.target
    assert!(unit.contains("WantedBy=default.target"));
    assert!(unit.contains("systemctl --user"));
}

#[test]
fn dockerfile_ships_the_agent_runtime() {
    let df = dockerfile(None);
    assert!(df.contains("FROM node:22-bookworm-slim"));
    assert!(df.contains("@anthropic-ai/claude-code"));
    assert!(df.contains("git"));
    assert!(df.contains("ANTHROPIC_API_KEY"));
    assert!(df.contains("ENV CORETEMPO_BIND=0.0.0.0"));
    assert!(df.contains("CORETEMPO_TOKEN"));
    assert!(
        df.contains(
            "ENTRYPOINT [\"/usr/local/bin/coretempod\", \"run\", \"/workflow/tempo.toml\"]"
        )
    );
}

#[test]
fn webhook_trigger_exports_a_serve_unit_and_entrypoint() {
    let unit = systemd_unit(
        "core-tempo-dev",
        "/srv/tempo/tempo.toml",
        Some(TriggerType::Webhook),
    );
    assert!(unit.contains("ExecStart=%h/.local/bin/coretempod serve /srv/tempo/tempo.toml"));

    let df = dockerfile(Some(TriggerType::Webhook));
    assert!(df.contains(
        "ENTRYPOINT [\"/usr/local/bin/coretempod\", \"serve\", \"/workflow/tempo.toml\"]"
    ));
}

#[test]
fn on_start_trigger_exports_an_always_restarting_batch_unit() {
    let unit = systemd_unit(
        "core-tempo-dev",
        "/srv/tempo/tempo.toml",
        Some(TriggerType::OnStart),
    );
    assert!(unit.contains("ExecStart=%h/.local/bin/coretempod run /srv/tempo/tempo.toml"));
    assert!(unit.contains("Restart=always"));
    assert!(unit.contains("a successful batch run exits 0"));
}
