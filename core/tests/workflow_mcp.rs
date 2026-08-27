//! `agents.<id>.mcp` resolution and hashing at load (spec 2026-08-17 §2). One
//! process-wide fake HOME; each test varies only its own agent dir.
#![expect(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::OnceLock;

use coretempo_core::types::id::AgentId;
use coretempo_core::workflow::{ConfigError, load_workflow};
use serde_json::json;

/// Sets HOME to a fake home holding `~/.claude.json` (mailbox) and
/// `~/.mcp.json` (context7) exactly once for this test binary.
fn home() -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let name = format!("coretempo-mcp-home-{}", std::process::id());
        let home = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&home).unwrap();
        let claude = json!({
            "mcpServers": {"mailbox": {"command": "mailbox-mcp"}},
            "projects": {}
        });
        std::fs::write(home.join(".claude.json"), claude.to_string()).unwrap();
        let user = json!({"mcpServers": {
            "context7": {"command": "npx", "args": ["-y", "@upstash/context7-mcp"]}
        }});
        std::fs::write(home.join(".mcp.json"), user.to_string()).unwrap();
        // SAFETY: set once, before any test in this binary reads HOME; every
        // test goes through `home()` first.
        unsafe { std::env::set_var("HOME", &home) };
        home
    })
    .clone()
}

/// A fresh agent dir under the fake home plus a tempo.toml beside it.
fn workflow(name: &str, agents: &str) -> (PathBuf, PathBuf) {
    let root = home().join(name);
    let dir = root.join("agent");
    std::fs::create_dir_all(&dir).unwrap();
    let text = format!(
        "[workflow]\nname = \"dev\"\n{}",
        agents.replace("$DIR", &dir.display().to_string())
    );
    let path = root.join("tempo.toml");
    std::fs::write(&path, text).unwrap();
    (path, dir)
}

#[test]
fn resolved_servers_freeze_per_agent_and_only_for_opted_in_agents() {
    let (path, dir) = workflow(
        "freeze",
        "[agents.a]\ndir = \"$DIR\"\nprompt = \"p\"\nmcp = [\"context7\", \"local\"]\n\
         [agents.b]\ndir = \"$DIR\"\nprompt = \"p\"\n",
    );
    std::fs::write(
        dir.join(".mcp.json"),
        json!({"mcpServers": {"local": {"command": "l"}}}).to_string(),
    )
    .unwrap();
    let (_, frozen) = load_workflow(&path).unwrap();
    let a = &frozen.mcp_servers[&AgentId("a".into())];
    assert_eq!(a.len(), 2);
    assert_eq!(a["context7"]["command"], "npx");
    assert_eq!(a["local"], json!({"command": "l"}));
    assert!(
        !frozen.mcp_servers.contains_key(&AgentId("b".into())),
        "b declared none"
    );
}

#[test]
fn unknown_server_is_a_load_error_naming_the_agent_and_the_available_servers() {
    let (path, _) = workflow(
        "unknown",
        "[agents.a]\ndir = \"$DIR\"\nprompt = \"p\"\nmcp = [\"nope\"]\n",
    );
    let err = load_workflow(&path).unwrap_err();
    let text = err.to_string();
    let ConfigError::Mcp { agent, .. } = &err else {
        panic!("expected ConfigError::Mcp, got {err:?}");
    };
    assert_eq!(agent.0, "a");
    assert!(text.starts_with("agents.a.mcp:"), "{text}");
    for expected in ["\"nope\"", "context7", "mailbox", "mcp = [...]"] {
        assert!(text.contains(expected), "{expected} missing from: {text}");
    }
}

#[test]
fn mcp_selection_joins_the_hash_in_canonical_form() {
    let (path, dir) = workflow(
        "hash",
        "[agents.a]\ndir = \"$DIR\"\nprompt = \"p\"\nmcp = [\"local\"]\n",
    );
    let write = |body: &str| std::fs::write(dir.join(".mcp.json"), body).unwrap();
    write(r#"{"mcpServers": {"local": {"command": "npx", "args": ["-y", "x"]}}}"#);
    let (_, first) = load_workflow(&path).unwrap();
    // Reformatted and reordered, same servers: same hash.
    write(concat!(
        "{\n  \"mcpServers\": {\n",
        "    \"local\": {\"args\": [\"-y\", \"x\"], \"command\": \"npx\"}\n",
        "  }\n}\n",
    ));
    let (_, reformatted) = load_workflow(&path).unwrap();
    assert_eq!(
        first.hash, reformatted.hash,
        "formatting must not move the hash"
    );
    // A definition edit: different hash.
    write(r#"{"mcpServers": {"local": {"command": "npx", "args": ["-y", "y"]}}}"#);
    let (_, edited) = load_workflow(&path).unwrap();
    assert_ne!(first.hash, edited.hash, "an args edit must move the hash");
    // An agent that opts out of the same server: different hash again.
    let (plain, _) = workflow("hash-plain", "[agents.a]\ndir = \"$DIR\"\nprompt = \"p\"\n");
    let (_, no_mcp) = load_workflow(&plain).unwrap();
    assert_ne!(edited.hash, no_mcp.hash);
}

#[test]
fn agents_without_mcp_never_touch_the_sources() {
    // Poison a source: an agent with no `mcp` must still load.
    let (path, dir) = workflow("untouched", "[agents.a]\ndir = \"$DIR\"\nprompt = \"p\"\n");
    std::fs::write(dir.join(".mcp.json"), "{ nope").unwrap();
    let (_, frozen) = load_workflow(&path).unwrap();
    assert!(frozen.mcp_servers.is_empty());
}
