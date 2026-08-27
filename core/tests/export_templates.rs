#![expect(
    clippy::expect_used,
    reason = "fixture parsing asserts a known-good TOML literal"
)]

use coretempo_core::export::{ExportTarget, dockerfile, export_target, systemd_unit};
use coretempo_core::types::FlowName;

#[test]
fn systemd_unit_is_a_user_unit_running_coretempod() {
    let unit = systemd_unit(
        "core-tempo-dev",
        "/srv/tempo/tempo.toml",
        &ExportTarget::WarmRun,
    );
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
    let df = dockerfile(&ExportTarget::WarmRun);
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
        &ExportTarget::Serve,
    );
    assert!(unit.contains("ExecStart=%h/.local/bin/coretempod serve /srv/tempo/tempo.toml"));

    let df = dockerfile(&ExportTarget::Serve);
    assert!(df.contains(
        "ENTRYPOINT [\"/usr/local/bin/coretempod\", \"serve\", \"/workflow/tempo.toml\"]"
    ));
}

#[test]
fn on_start_trigger_exports_an_always_restarting_batch_unit() {
    let target = ExportTarget::Batch {
        flow: FlowName("batch".into()),
    };
    let unit = systemd_unit("core-tempo-dev", "/srv/tempo/tempo.toml", &target);
    assert!(
        unit.contains("ExecStart=%h/.local/bin/coretempod run /srv/tempo/tempo.toml --flow batch")
    );
    assert!(unit.contains("Restart=always"));
    assert!(unit.contains("a successful batch run exits 0"));
}

const BASE: &str = "[workflow]\nname = \"x\"\n[agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n";
const ON_START: &str = "[flows.batch]\nagents = [\"a\"]\n\
    trigger = { type = \"on_start\", edge = { to = \"a\", kind = \"ask\" }, message = \"go\" }\n";
const WEBHOOK: &str = "[flows.hook]\nagents = [\"a\"]\n\
    trigger = { type = \"webhook\", edge = { to = \"a\", kind = \"ask\" } }\n";

fn parse(flows: &str) -> coretempo_core::types::WorkflowFile {
    toml::from_str(&format!("{BASE}{flows}")).expect("fixture parses")
}

#[test]
fn export_target_resolves_the_flow_matrix() {
    assert_eq!(export_target(&parse(""), None), Ok(ExportTarget::WarmRun));
    assert_eq!(
        export_target(&parse(WEBHOOK), None),
        Ok(ExportTarget::Serve)
    );
    let both = format!("{ON_START}{WEBHOOK}");
    assert_eq!(export_target(&parse(&both), None), Ok(ExportTarget::Serve));
    assert_eq!(
        export_target(&parse(ON_START), Some("batch")),
        Ok(ExportTarget::Batch {
            flow: FlowName("batch".into())
        })
    );
}

#[test]
fn export_target_errors_name_the_flows_and_the_fix() {
    let err = export_target(&parse(ON_START), None).expect_err("on_start-only needs --flow");
    assert!(err.contains("batch") && err.contains("--flow"), "{err}");

    let err = export_target(&parse(ON_START), Some("nope")).expect_err("unknown flow");
    assert!(err.contains("'nope'") && err.contains("batch"), "{err}");

    let err = export_target(&parse(WEBHOOK), Some("hook")).expect_err("webhook via --flow");
    assert!(err.contains("serve"), "points at the plain export: {err}");

    let err = export_target(&parse(""), Some("any")).expect_err("no flows at all");
    assert!(err.contains("no [flows"), "{err}");
}

#[test]
fn batch_units_run_the_named_flow() {
    let target = ExportTarget::Batch {
        flow: FlowName("batch".into()),
    };
    let unit = systemd_unit("x", "/w/tempo.toml", &target);
    assert!(
        unit.contains("ExecStart=%h/.local/bin/coretempod run /w/tempo.toml --flow batch"),
        "{unit}"
    );
    assert!(
        unit.contains("Restart=always"),
        "batch keeps the re-running worker note: {unit}"
    );
    let docker = dockerfile(&target);
    assert!(
        docker.contains(r#""run", "/workflow/tempo.toml", "--flow", "batch""#),
        "{docker}"
    );
    // Serve and WarmRun shapes are unchanged from today.
    assert!(systemd_unit("x", "/w/t.toml", &ExportTarget::Serve).contains("coretempod serve"));
    assert!(dockerfile(&ExportTarget::WarmRun).contains(r#""run", "/workflow/tempo.toml"]"#));
}
