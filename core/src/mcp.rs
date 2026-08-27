//! Per-agent MCP server opt-in (spec 2026-08-17 §2). Every agent spawns with
//! `--strict-mcp-config`, so it sees exactly the servers `CoreTempo` hands it
//! via `--mcp-config` — resolved here, at workflow load, from the user's own
//! declarations. Nothing else on the machine leaks in, and no "new MCP server
//! found" approval dialog can park a spawn.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::api::auth::write_private_file;
use crate::types::config::{FrozenWorkflow, McpServers};
use crate::types::id::{AgentId, RunId};

/// Where MCP declarations are read from. Built from the home directory by
/// [`McpSources::from_home`]; tests point it at a temp dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSources {
    /// The operator's `.claude.json` — `$CLAUDE_CONFIG_DIR/.claude.json` when
    /// that is set, else `~/.claude.json` — top-level `mcpServers` (user
    /// scope) and `projects["<dir>"].mcpServers` (project-local scope).
    pub claude_json: PathBuf,
    /// `~/.mcp.json`: user-wide project-style file.
    pub user_mcp_json: PathBuf,
}

impl McpSources {
    #[must_use]
    pub fn from_home(home: &Path) -> McpSources {
        McpSources {
            claude_json: home.join(".claude.json"),
            user_mcp_json: home.join(".mcp.json"),
        }
    }

    /// `.claude.json` from `CLAUDE_CONFIG_DIR` or the home directory (see
    /// [`crate::claude_config::operator_claude_json`]) and `~/.mcp.json` from
    /// the home directory; `None` when the home directory is unknown.
    #[must_use]
    pub fn from_env() -> Option<McpSources> {
        let claude_json = crate::claude_config::operator_claude_json()?;
        let home = std::env::home_dir()?;
        Some(McpSources {
            claude_json,
            user_mcp_json: home.join(".mcp.json"),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error(
        "cannot determine where to read .claude.json and ~/.mcp.json from; set HOME \
         (CLAUDE_CONFIG_DIR relocates .claude.json only)"
    )]
    NoHome,
    #[error("cannot read MCP declarations from '{path}': {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("MCP declarations in '{path}' are not valid JSON: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "MCP server {name:?} is not declared anywhere CoreTempo looks for agent dir \
         '{dir}'; available: {available}. Declare it (`claude mcp add -s user {name} ...`, \
         or in {dir}/.mcp.json) or remove it from mcp = [...]"
    )]
    Unknown {
        name: String,
        dir: PathBuf,
        available: String,
    },
}

/// One declaration layer: where it came from (for error text) and its servers.
struct Layer {
    source: String,
    servers: McpServers,
}

/// Resolves `names` for an agent working in `dir`, first match wins across the
/// operator's `.claude.json` (see [`McpSources`]) `mcpServers`, that same file's
/// `projects[dir].mcpServers`, `~/.mcp.json`, `<dir>/.mcp.json`. Empty `names`
/// reads nothing.
///
/// # Errors
/// [`McpError::Unknown`] for a name no layer declares (naming every declared
/// server and its source); [`McpError::Io`]/[`McpError::Json`] for a present
/// but unreadable or malformed source — skipping it silently would turn a
/// typo in `~/.mcp.json` into a misleading "unknown server".
pub fn resolve_mcp_servers(
    sources: &McpSources,
    dir: &Path,
    names: &[String],
) -> Result<McpServers, McpError> {
    if names.is_empty() {
        return Ok(McpServers::new());
    }
    let layers = load_layers(sources, dir)?;
    let mut selected = McpServers::new();
    for name in names {
        let found = layers
            .iter()
            .find_map(|layer| layer.servers.get(name).cloned());
        let Some(definition) = found else {
            return Err(McpError::Unknown {
                name: name.clone(),
                dir: dir.to_path_buf(),
                available: describe(&layers),
            });
        };
        selected.insert(name.clone(), definition);
    }
    Ok(selected)
}

/// The four layers, highest precedence first. A missing file is an empty layer.
fn load_layers(sources: &McpSources, dir: &Path) -> Result<Vec<Layer>, McpError> {
    let claude = read_json(&sources.claude_json)?;
    // Claude Code keys `projects` by the physical cwd (symlinks resolved, no
    // trailing slash); a `~`-expanded `dir` may differ in either respect.
    let physical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let dir_key = physical.to_string_lossy().into_owned();
    let user_file = read_json(&sources.user_mcp_json)?;
    let dir_file_path = dir.join(".mcp.json");
    let dir_file = read_json(&dir_file_path)?;
    Ok(vec![
        Layer {
            source: format!("{} mcpServers", sources.claude_json.display()),
            servers: servers_at(claude.as_ref(), &["mcpServers"]),
        },
        Layer {
            source: format!("{} projects[{dir_key}]", sources.claude_json.display()),
            servers: servers_at(claude.as_ref(), &["projects", &dir_key, "mcpServers"]),
        },
        Layer {
            source: sources.user_mcp_json.display().to_string(),
            servers: servers_at(user_file.as_ref(), &["mcpServers"]),
        },
        Layer {
            source: dir_file_path.display().to_string(),
            servers: servers_at(dir_file.as_ref(), &["mcpServers"]),
        },
    ])
}

fn read_json(path: &Path) -> Result<Option<Value>, McpError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(McpError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| McpError::Json {
            path: path.to_path_buf(),
            source,
        })
}

/// The `mcpServers` object found by walking `keys` from `root`, or empty.
fn servers_at(root: Option<&Value>, keys: &[&str]) -> McpServers {
    let mut node = root;
    for key in keys {
        node = node.and_then(|value| value.get(key));
    }
    match node.and_then(Value::as_object) {
        Some(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        None => McpServers::new(),
    }
}

/// `name (source), name (source), …` over every layer, or `(none)`.
fn describe(layers: &[Layer]) -> String {
    let mut parts = Vec::new();
    for layer in layers {
        for name in layer.servers.keys() {
            parts.push(format!("{name} ({})", layer.source));
        }
    }
    if parts.is_empty() {
        return "(none)".to_string();
    }
    parts.join(", ")
}

/// Compact JSON with every object's keys sorted at every depth — the form the
/// freeze hash covers. Two declarations that differ only in whitespace or key
/// order hash alike: a live Claude session reformats `~/.claude.json` on every
/// flush, so hashing raw bytes would refuse runs for a workflow nobody edited.
#[must_use]
pub fn canonical_bytes(servers: &McpServers) -> Vec<u8> {
    let object = servers
        .iter()
        .map(|(name, definition)| (name.clone(), canonicalize(definition)))
        .collect();
    Value::Object(object).to_string().into_bytes()
}

/// Sorts every nested object's keys without relying on how `serde_json::Map` is
/// backed: it is a `BTreeMap` (already sorted) unless something in the build
/// graph turns on `serde_json/preserve_order`, which would make it an `IndexMap`
/// that keeps parse order. Recursing here keeps the freeze hash the same either
/// way, so a transitive feature flip cannot invalidate stored hashes.
fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k.clone(), canonicalize(v)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.clone(),
    }
}

/// The `--mcp-config` file body: `{"mcpServers": {...}}`.
#[must_use]
pub fn mcp_config_json(servers: &McpServers) -> String {
    serde_json::json!({ "mcpServers": servers }).to_string()
}

/// Writes `<runs_dir>/<run_id>/agent-mcp-<agent_id>.json` (mode 0600, beside
/// the settings file) for every agent in `workflow.agents` that has resolved
/// servers, and returns each path. Called once per run, before spawning.
///
/// # Errors
/// Propagates filesystem errors from creating the run directory or writing a file.
pub(crate) fn write_agent_mcp_files(
    runs_dir: &Path,
    run_id: &RunId,
    workflow: &FrozenWorkflow,
) -> std::io::Result<BTreeMap<AgentId, PathBuf>> {
    let dir = runs_dir.join(&run_id.0);
    std::fs::create_dir_all(&dir)?;
    let mut paths = BTreeMap::new();
    for id in workflow.agents.keys() {
        let Some(servers) = workflow.mcp_servers.get(id) else {
            continue;
        };
        let path = dir.join(format!("agent-mcp-{}.json", id.0));
        write_private_file(&path, &mcp_config_json(servers))?;
        paths.insert(id.clone(), path);
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use serde_json::json;

    use crate::mcp::{
        McpError, McpSources, canonical_bytes, mcp_config_json, resolve_mcp_servers,
        write_agent_mcp_files,
    };
    use crate::types::config::McpServers;
    use crate::types::id::AgentId;

    /// A fake home with all four layers declaring `shared` differently, plus one
    /// name unique to each layer, and an agent dir with its own `.mcp.json`.
    struct Fixture {
        /// Held so the temp dir outlives the fixture; also the place tests put
        /// sibling paths like the symlink below.
        home: tempfile::TempDir,
        sources: McpSources,
        dir: PathBuf,
    }

    fn fixture() -> Fixture {
        let home = tempfile::tempdir().expect("tmpdir");
        let dir = home.path().join("work");
        std::fs::create_dir_all(&dir).expect("dir");
        let dir_key = std::fs::canonicalize(&dir)
            .expect("canonical")
            .to_string_lossy()
            .into_owned();
        let claude = json!({
            "hasCompletedOnboarding": true,
            "mcpServers": {
                "mailbox": {"command": "mailbox-mcp"},
                "shared": {"command": "user-scope"}
            },
            "projects": {
                dir_key: {
                    "allowedTools": [],
                    "mcpServers": {
                        "proj-only": {"command": "p"},
                        "shared": {"command": "project-scope"}
                    }
                }
            }
        });
        std::fs::write(home.path().join(".claude.json"), claude.to_string()).expect("write");
        let user_file = json!({"mcpServers": {
            "context7": {"command": "npx", "args": ["-y", "@upstash/context7-mcp"]},
            "shared": {"command": "user-file"}
        }});
        std::fs::write(home.path().join(".mcp.json"), user_file.to_string()).expect("write");
        let dir_file = json!({"mcpServers": {
            "local": {"command": "l"},
            "shared": {"command": "dir-file"}
        }});
        std::fs::write(dir.join(".mcp.json"), dir_file.to_string()).expect("write");
        Fixture {
            sources: McpSources::from_home(home.path()),
            dir,
            home,
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn from_home_points_at_the_two_user_files() {
        let s = McpSources::from_home(Path::new("/home/u"));
        assert_eq!(s.claude_json, PathBuf::from("/home/u/.claude.json"));
        assert_eq!(s.user_mcp_json, PathBuf::from("/home/u/.mcp.json"));
    }

    #[test]
    fn each_layer_is_reachable_by_its_unique_name() {
        let f = fixture();
        let got = resolve_mcp_servers(
            &f.sources,
            &f.dir,
            &names(&["mailbox", "proj-only", "context7", "local"]),
        )
        .expect("all four resolve");
        assert_eq!(got.len(), 4);
        assert_eq!(got["mailbox"], json!({"command": "mailbox-mcp"}));
        assert_eq!(got["proj-only"], json!({"command": "p"}));
        assert_eq!(got["context7"]["args"][1], "@upstash/context7-mcp");
        assert_eq!(got["local"], json!({"command": "l"}));
    }

    #[test]
    fn first_match_wins_in_declared_precedence() {
        let f = fixture();
        let pick = |fx: &Fixture| {
            resolve_mcp_servers(&fx.sources, &fx.dir, &names(&["shared"])).expect("resolves")
                ["shared"]["command"]
                .as_str()
                .expect("command")
                .to_string()
        };
        assert_eq!(pick(&f), "user-scope", "~/.claude.json mcpServers wins");
        // Drop the user-scope layer: project scope is next.
        let mut claude: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&f.sources.claude_json).expect("read"))
                .expect("json");
        claude["mcpServers"] = json!({});
        std::fs::write(&f.sources.claude_json, claude.to_string()).expect("write");
        assert_eq!(pick(&f), "project-scope");
        // Drop the project layer: ~/.mcp.json is next.
        claude["projects"] = json!({});
        std::fs::write(&f.sources.claude_json, claude.to_string()).expect("write");
        assert_eq!(pick(&f), "user-file");
        // Drop ~/.mcp.json: the agent dir's .mcp.json is last.
        std::fs::remove_file(&f.sources.user_mcp_json).expect("rm");
        assert_eq!(pick(&f), "dir-file");
    }

    #[test]
    fn project_scope_is_keyed_by_the_physical_dir() {
        let f = fixture();
        // A symlink to the agent dir and a trailing slash both reach the same
        // `projects[...]` entry that Claude Code (cwd-keyed) would write.
        let link = f.home.path().join("work-link");
        std::os::unix::fs::symlink(&f.dir, &link).expect("symlink");
        let via_link = resolve_mcp_servers(&f.sources, &link, &names(&["proj-only"]))
            .expect("resolves through the symlink");
        assert_eq!(via_link["proj-only"], json!({"command": "p"}));
        let slashed = PathBuf::from(format!("{}/", f.dir.display()));
        let via_slash = resolve_mcp_servers(&f.sources, &slashed, &names(&["proj-only"]))
            .expect("resolves with a trailing slash");
        assert_eq!(via_slash["proj-only"], json!({"command": "p"}));
    }

    #[test]
    fn unknown_name_lists_every_available_server_with_its_source() {
        let f = fixture();
        let err = resolve_mcp_servers(&f.sources, &f.dir, &names(&["context7", "nope"]))
            .expect_err("nope is undeclared");
        let text = err.to_string();
        let McpError::Unknown {
            name,
            dir,
            available,
        } = &err
        else {
            panic!("expected Unknown, got {err:?}");
        };
        assert_eq!(name, "nope");
        assert_eq!(*dir, f.dir);
        for expected in ["mailbox", "proj-only", "context7", "local", "shared"] {
            assert!(
                available.contains(expected),
                "{expected} missing from: {available}"
            );
        }
        assert!(
            available.contains(".mcp.json"),
            "sources named: {available}"
        );
        assert!(
            text.contains("\"nope\"") && text.contains("mcp = [...]"),
            "{text}"
        );
    }

    #[test]
    fn nothing_declared_anywhere_says_so() {
        let home = tempfile::tempdir().expect("tmpdir");
        let dir = home.path().join("empty");
        std::fs::create_dir_all(&dir).expect("dir");
        let err = resolve_mcp_servers(&McpSources::from_home(home.path()), &dir, &names(&["x"]))
            .expect_err("nothing declared");
        let McpError::Unknown { available, .. } = err else {
            panic!("expected Unknown, got {err:?}");
        };
        assert_eq!(available, "(none)");
    }

    #[test]
    fn empty_names_resolve_to_nothing_without_reading_any_file() {
        let home = tempfile::tempdir().expect("tmpdir");
        std::fs::write(home.path().join(".claude.json"), "not json").expect("write");
        let got = resolve_mcp_servers(&McpSources::from_home(home.path()), home.path(), &[])
            .expect("no names, no reads");
        assert!(got.is_empty());
    }

    #[test]
    fn a_malformed_source_is_an_error_not_a_miss() {
        let f = fixture();
        std::fs::write(&f.sources.user_mcp_json, "{ nope").expect("write");
        let err = resolve_mcp_servers(&f.sources, &f.dir, &names(&["context7"]))
            .expect_err("malformed ~/.mcp.json");
        let McpError::Json { path, .. } = err else {
            panic!("expected Json, got {err:?}");
        };
        assert_eq!(path, f.sources.user_mcp_json);
    }

    #[test]
    fn canonical_bytes_ignore_key_order_and_whitespace_but_not_values() {
        let a: McpServers = serde_json::from_str(
            r#"{"z": {"command": "npx", "args": ["-y", "x"], "env": {"B": "2", "A": "1"}},
                "a": {"type": "http", "url": "https://x"}}"#,
        )
        .expect("json");
        let b: McpServers = serde_json::from_str(concat!(
            r#"{"a":{"url":"https://x","type":"http"},"#,
            r#""z":{"env":{"A":"1","B":"2"},"args":["-y","x"],"command":"npx"}}"#,
        ))
        .expect("json");
        assert_eq!(canonical_bytes(&a), canonical_bytes(&b));
        let mut c = a.clone();
        c.insert(
            "z".into(),
            json!({"command": "npx", "args": ["-y", "y"], "env": {"A": "1", "B": "2"}}),
        );
        assert_ne!(
            canonical_bytes(&a),
            canonical_bytes(&c),
            "an args edit changes the bytes"
        );
        let expected = concat!(
            r#"{"a":{"type":"http","url":"https://x"},"#,
            r#""z":{"args":["-y","x"],"command":"npx","env":{"A":"1","B":"2"}}}"#,
        );
        assert_eq!(
            String::from_utf8(canonical_bytes(&a)).expect("utf8"),
            expected
        );
    }

    #[test]
    fn mcp_config_json_wraps_the_selection() {
        let mut s = McpServers::new();
        s.insert("context7".into(), json!({"command": "npx"}));
        let parsed: serde_json::Value =
            serde_json::from_str(&mcp_config_json(&s)).expect("valid json");
        assert_eq!(
            parsed,
            json!({"mcpServers": {"context7": {"command": "npx"}}})
        );
    }

    #[test]
    fn writes_one_private_file_per_opted_in_agent() {
        use std::os::unix::fs::PermissionsExt;

        use crate::types::config::{AgentConfig, FrozenWorkflow};
        use crate::types::id::RunId;

        let dir = tempfile::tempdir().expect("tmpdir");
        let mut agents = std::collections::BTreeMap::new();
        agents.insert(
            AgentId("resolver".into()),
            AgentConfig::new(PathBuf::from("/w"), "p"),
        );
        agents.insert(
            AgentId("helper".into()),
            AgentConfig::new(PathBuf::from("/w"), "p"),
        );
        let mut mcp_servers = std::collections::BTreeMap::new();
        let mut selection = McpServers::new();
        selection.insert("context7".into(), json!({"command": "npx"}));
        mcp_servers.insert(AgentId("resolver".into()), selection.clone());
        // A `for_flow` subset keeps every agent's servers but narrows `agents`;
        // a non-member must get no file.
        mcp_servers.insert(AgentId("absent".into()), selection);
        let workflow = FrozenWorkflow {
            name: "t".into(),
            hash: "0".repeat(64),
            source_path: PathBuf::from("tempo.toml"),
            ask_timeout: std::time::Duration::from_mins(1),
            idle_debounce: std::time::Duration::from_secs(2),
            scrollback: 100,
            agents,
            mcp_servers,
            flows: std::collections::BTreeMap::new(),
        };
        let paths =
            write_agent_mcp_files(dir.path(), &RunId("r1".into()), &workflow).expect("write");
        assert_eq!(
            paths.len(),
            1,
            "helper opted into nothing and absent is not in agents: {paths:?}"
        );
        let path = &paths[&AgentId("resolver".into())];
        assert!(
            path.ends_with("r1/agent-mcp-resolver.json"),
            "{}",
            path.display()
        );
        let mode = std::fs::metadata(path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let body: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("json");
        assert_eq!(
            body,
            json!({"mcpServers": {"context7": {"command": "npx"}}})
        );
    }
}
