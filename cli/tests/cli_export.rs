#![expect(
    clippy::panic_in_result_fn,
    reason = "tests assert inside Result-returning fns"
)]

mod support;

use std::path::PathBuf;

use coretempo_core::types::{WorkflowFile, WorkflowResponse};
use support::{exit_code, serve, stderr, tempo};

fn workflow_json() -> String {
    concat!(
        r#"{"run_id":"r-1f2e3d4c","started_at":"2026-08-01T17:03:11Z","workflow":{"#,
        r#""workflow":{"name":"core-tempo-dev","db":"./tempo.db","port":4820,"#,
        r#""ask_timeout_minutes":30,"idle_debounce_seconds":2.0},"#,
        r#""server":{"bind":"127.0.0.1","token_file":null,"log":"info"},"#,
        r#""agents":{"#,
        r#""builder":{"dir":"/home/u/projects/CoreTempo","prompt":"You implement tasks","#,
        r#""model":null,"permission_mode":"acceptEdits","auto_clear":true},"#,
        r#""planner":{"dir":"/home/u/projects/CoreTempo","prompt":"You plan","#,
        r#""model":"opus","permission_mode":null,"auto_clear":false}}}}"#
    )
    .to_string()
}

fn workflow_json_with_on_start_flow() -> String {
    // The landed fixture ends the inner workflow object with the agents map:
    // splice "flows" in before the closing braces. Two braces close the
    // agent object and the agents map before "flows" can start as their
    // sibling key; five close trigger/flow/flows-map/workflow-file/response.
    workflow_json().replace(
        r#""permission_mode":null,"auto_clear":false}}}}"#,
        concat!(
            r#""permission_mode":null,"auto_clear":false}},"#,
            r#""flows":{"batch":{"agents":["builder"],"trigger":{"type":"on_start","#,
            r#""edge":{"to":"builder","kind":"ask"},"message":"go"}}}}}"#
        ),
    )
}

fn out_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tempo-export-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join("out")
}

fn closed_port() -> anyhow::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

#[test]
fn export_writes_config_unit_and_dockerfile() -> anyhow::Result<()> {
    let srv = serve(vec![(200, workflow_json())])?;
    let dir = out_dir("ok");
    let out = tempo(&["export", &dir.to_string_lossy()], srv.port, None)?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));

    let reqs = srv.requests();
    assert_eq!(
        (reqs[0].method.as_str(), reqs[0].path.as_str()),
        ("GET", "/v1/workflow")
    );

    let expected: WorkflowResponse = serde_json::from_str(&workflow_json())?;
    let written: WorkflowFile = toml::from_str(&std::fs::read_to_string(dir.join("tempo.toml"))?)?;
    assert_eq!(written, expected.workflow, "tempo.toml must round-trip");

    let unit = std::fs::read_to_string(dir.join("coretempo.service"))?;
    assert!(unit.contains("core-tempo-dev"), "unit: {unit}");
    assert!(unit.contains("tempo.toml"), "unit: {unit}");

    let docker = std::fs::read_to_string(dir.join("Dockerfile"))?;
    assert!(
        docker.contains("@anthropic-ai/claude-code"),
        "dockerfile: {docker}"
    );
    Ok(())
}

#[test]
fn export_without_a_running_server_exits_3() -> anyhow::Result<()> {
    let dir = out_dir("unreachable");
    let out = tempo(&["export", &dir.to_string_lossy()], closed_port()?, None)?;
    assert_eq!(exit_code(&out), 3);
    assert!(
        stderr(&out).contains("cannot reach the CoreTempo server"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(
        !dir.join("tempo.toml").exists(),
        "nothing written on failure"
    );
    Ok(())
}

#[test]
fn export_flow_writes_a_batch_unit_for_the_named_flow() -> anyhow::Result<()> {
    let srv = serve(vec![(200, workflow_json_with_on_start_flow())])?;
    let dir = out_dir("flow-batch");
    let out = tempo(
        &["export", &dir.to_string_lossy(), "--flow", "batch"],
        srv.port,
        None,
    )?;
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr(&out));
    let unit = std::fs::read_to_string(dir.join("coretempo.service"))?;
    assert!(unit.contains("--flow batch"), "unit: {unit}");
    Ok(())
}

#[test]
fn export_of_an_on_start_only_workflow_without_flow_names_the_flows() -> anyhow::Result<()> {
    let srv = serve(vec![(200, workflow_json_with_on_start_flow())])?;
    let dir = out_dir("flow-missing");
    let out = tempo(&["export", &dir.to_string_lossy()], srv.port, None)?;
    assert_eq!(exit_code(&out), 3, "user errors exit 3: {}", stderr(&out));
    assert!(stderr(&out).contains("batch"), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("--flow"), "stderr: {}", stderr(&out));
    Ok(())
}
