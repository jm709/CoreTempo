//! Trust preflight at run start (spec 2026-08-17 §1).

mod support;

use coretempo_core::run::{RunError, RunOptions};
use coretempo_core::trust::{TrustStore, trust_root};
use coretempo_core::types::AgentId;
use support::run::{GRANT_TRUST, RunScaffold};

#[tokio::test(flavor = "multi_thread")]
async fn start_refuses_untrusted_dirs_without_grant_before_touching_the_store() {
    let scaffold = RunScaffold::new("trust-refuse").await;

    let err = scaffold.start(RunOptions::default()).await.unwrap_err();

    let RunError::Trust(inner) = &err else {
        panic!("expected Trust, got {err:?}");
    };
    let text = inner.to_string();
    let expected_root = trust_root(&scaffold.agent_dir).display().to_string();
    assert!(text.contains(&expected_root), "names the root: {text}");
    assert!(
        text.contains("trust_agent_dirs = true"),
        "names the fix: {text}"
    );
    assert!(
        !scaffold.root.join("tempo.db").exists(),
        "refused before opening the store"
    );
    assert!(
        !scaffold.home.join(".claude.json").exists(),
        "nothing was granted without the policy"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn start_with_grant_writes_the_key_and_spawns() {
    let scaffold = RunScaffold::new("trust-grant").await;

    let run = scaffold.start(GRANT_TRUST).await.unwrap();

    let store = TrustStore::at(scaffold.home.join(".claude.json"));
    let agent_dir = scaffold.agent_dir.as_path();
    assert!(
        store.untrusted_roots([agent_dir]).unwrap().is_empty(),
        "the preflight granted the agent's root"
    );
    // The run also installed the per-spawn gate: a key reverted by a live
    // Claude session comes back on the next spawn.
    std::fs::write(scaffold.home.join(".claude.json"), r#"{"projects": {}}"#).unwrap();
    assert!(
        !store.untrusted_roots([agent_dir]).unwrap().is_empty(),
        "the revert took effect"
    );
    run.pty().restart(&AgentId("echo".into())).await.unwrap();
    assert!(
        store.untrusted_roots([agent_dir]).unwrap().is_empty(),
        "the spawn gate re-granted on restart"
    );

    run.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relocated_claude_config_dir_is_where_trust_and_mcp_are_read() {
    let scaffold = RunScaffold::new("trust-relocated").await;
    let cfg = scaffold.root.join("cfg");
    std::fs::create_dir_all(&cfg).unwrap();
    // The relocated .claude.json declares the MCP server the agent names; the
    // HOME one does not exist at all.
    std::fs::write(
        cfg.join(".claude.json"),
        r#"{"mcpServers":{"m":{"command":"m-mcp"}},"projects":{}}"#,
    )
    .unwrap();
    let text = std::fs::read_to_string(&scaffold.config).unwrap();
    std::fs::write(&scaffold.config, format!("{text}mcp = [\"m\"]\n")).unwrap();
    // SAFETY: the scaffold holds the env lock; removed again below, and the
    // next scaffold removes it too.
    unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &cfg) };

    let loaded = scaffold.load();
    assert!(
        loaded.1.mcp_servers.contains_key(&AgentId("echo".into())),
        "mcp resolved from the relocated file"
    );
    let run = RunScaffold::start_loaded(loaded, GRANT_TRUST)
        .await
        .unwrap();
    run.stop().await.unwrap();
    // SAFETY: the run's threads (and the PTY they hold CLAUDE_CONFIG_DIR
    // open for) are gone after stop(), so the env mutation races nothing.
    unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };

    let relocated = TrustStore::at(cfg.join(".claude.json"));
    assert!(
        relocated
            .untrusted_roots([scaffold.agent_dir.as_path()])
            .unwrap()
            .is_empty(),
        "granted into the relocated file"
    );
    assert!(
        !scaffold.home.join(".claude.json").exists(),
        "nothing written beside HOME"
    );
    // The MCP declaration survived the grant's read-modify-write.
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cfg.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(doc["mcpServers"]["m"]["command"], "m-mcp");
}
