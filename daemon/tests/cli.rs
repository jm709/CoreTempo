use std::process::Command;

fn coretempod() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_coretempod"));
    // isolate from the developer's environment
    for var in [
        "CORETEMPO_BIND",
        "CORETEMPO_PORT",
        "CORETEMPO_DB",
        "CORETEMPO_TOKEN",
        "CORETEMPO_TOKEN_FILE",
        "CORETEMPO_LOG",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

#[test]
fn help_exits_zero_and_names_run() {
    let out = coretempod().arg("--help").output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("run"));
}

#[test]
fn missing_config_fails_with_clear_error() {
    let out = coretempod()
        .args(["run", "/nonexistent/tempo.toml"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("failed to load workflow"), "stderr was: {err}");
    assert!(err.contains("/nonexistent/tempo.toml"));
}

#[test]
fn invalid_config_reports_validation_paths() {
    let dir = std::env::temp_dir().join(format!("coretempod-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tempo.toml");
    std::fs::write(&path, "[workflow]\nname = \"\"\n[agents]\n").unwrap();
    let out = coretempod()
        .args(["run", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("workflow.name"), "stderr was: {err}");
}

#[test]
fn non_loopback_bind_without_token_refuses_to_start() {
    let dir = std::env::temp_dir().join(format!("coretempod-bind-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tempo.toml");
    std::fs::write(
        &path,
        "[workflow]\nname = \"x\"\n[agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n",
    )
    .unwrap();
    let out = coretempod()
        .args(["run", path.to_str().unwrap(), "--bind", "0.0.0.0"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("provisioned token"), "stderr was: {err}");
}
