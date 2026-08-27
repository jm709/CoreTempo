//! Trust preflight in coretempod (spec 2026-08-17 §1).
#![expect(clippy::unwrap_used, reason = "assertions are the vocabulary of tests")]
#![expect(clippy::expect_used, reason = "assertions are the vocabulary of tests")]
#![expect(clippy::panic, reason = "assertions are the vocabulary of tests")]

mod support;

use coretempo_core::trust::{TrustStore, trust_root};

const ONE_AGENT: &str = "[agents.a]\ndir = \"{dir}\"\nprompt = \"You reply.\"\n";
const ONE_WEBHOOK: &str = "[agents.a]\ndir = \"{dir}\"\nprompt = \"You reply.\"\n\
    [flows.hook]\nagents = [\"a\"]\n\
    trigger = { type = \"webhook\", edge = { to = \"a\", kind = \"ask\" } }\n";

#[test]
fn run_refuses_an_untrusted_agent_dir_naming_it_and_the_fixes() {
    let scratch = support::scratch_without_trust("run-untrusted", ONE_AGENT);
    let out = support::daemon_command(&scratch, "run", 0, "0")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    let root = trust_root(&scratch.root).display().to_string();
    assert!(err.contains(&root), "names the root; stderr was: {err}");
    assert!(
        err.contains("trust_agent_dirs = true"),
        "names the fix; stderr was: {err}"
    );
    assert!(
        !scratch.home.join(".claude.json").exists(),
        "granted nothing"
    );
}

#[test]
fn serve_refuses_to_boot_on_an_untrusted_agent_dir() {
    let scratch = support::scratch_without_trust("serve-untrusted", ONE_WEBHOOK);
    let out = support::daemon_command(&scratch, "serve", 0, "0")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "serve must not listen with an untrusted root"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("trust_agent_dirs = true"), "stderr was: {err}");
}

#[test]
fn serve_grants_via_the_workflow_key() {
    let tail = format!("[server]\ntrust_agent_dirs = true\n{ONE_WEBHOOK}");
    let serve = support::serving_flows_without_user_config("serve-grant-file", &tail, "0");
    let store = TrustStore::at(serve.scratch.home.join(".claude.json"));
    assert!(
        store
            .untrusted_roots([serve.scratch.root.as_path()])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn serve_grants_via_the_user_config() {
    let scratch_cfg = std::env::temp_dir().join(format!("ct-trust-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&scratch_cfg).unwrap();
    let cfg = scratch_cfg.join("config.toml");
    std::fs::write(&cfg, "trust_agent_dirs = true\n").unwrap();
    let serve = support::serving_flows_env_without_user_config(
        "serve-grant-user",
        ONE_WEBHOOK,
        "0",
        &[("CORETEMPO_CONFIG", cfg.to_str().unwrap())],
    );
    let store = TrustStore::at(serve.scratch.home.join(".claude.json"));
    assert!(
        store
            .untrusted_roots([serve.scratch.root.as_path()])
            .unwrap()
            .is_empty()
    );
}
