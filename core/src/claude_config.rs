//! The managed Claude config dir an `isolated_config` agent spawns against
//! (spec 2026-08-24 §2). `CLAUDE_CONFIG_DIR` relocates everything Claude Code
//! keeps under `~/.claude` *and* `~/.claude.json`, so an empty dir is logged
//! out, opens on the theme picker, and would raise the trust dialog. This
//! module seeds exactly what prevents each of those and nothing else:
//!
//! - **no** `.credentials.json` — login comes from the operator's own store
//!   through `CLAUDE_SECURESTORAGE_CONFIG_DIR` ([`operator_credential_store`]),
//!   which relocates only the credentials file and its refresh lock. A copy
//!   goes stale on OAuth refresh, and a symlink is *replaced* by it: Claude
//!   Code writes `<path>.tmp.<hex>` and renames it over the path (its
//!   fallback opens with `O_NOFOLLOW`), so the first refresh in a linked dir
//!   leaves that dir a private token pair, the operator's file the old one,
//!   and every other holder logged out at its next refresh (rotated token);
//! - `.claude.json` = `{"hasCompletedOnboarding":true}` — the trust key is
//!   mirrored into this same file by `TrustGate` before every spawn;
//! - `settings.json` = `{"autoMemoryEnabled":false,
//!   "skipDangerousModePermissionPrompt":true}` — no memory instructions in
//!   context, no memory writes into a per-run dir, and no "Bypass Permissions
//!   mode" acknowledgment for `permission_mode = "bypassPermissions"` agents
//!   (the operator's `permission_mode` line is the consent; the `.claude.json`
//!   `bypassPermissionsModeAccepted` key does not suppress it);
//! - `skills/<name>` → symlink to each declared skill dir.
//!
//! All verified live on Claude Code 2.1.241. The dir itself is 0700; its
//! parent `<runs_dir>/<run_id>` is created by `write_agent_settings_files`
//! with the umask default, as before — the secrets inside are 0600 files.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::api::auth::write_private_file;
use crate::types::config::FrozenWorkflow;
use crate::types::id::{AgentId, RunId};

pub const CLAUDE_JSON: &str = r#"{"hasCompletedOnboarding":true}"#;
pub const SETTINGS_JSON: &str =
    r#"{"autoMemoryEnabled":false,"skipDangerousModePermissionPrompt":true}"#;

#[derive(Debug, thiserror::Error)]
pub enum ClaudeConfigError {
    #[error(
        "cannot build the managed Claude config dir for agent {agent}: '{path}': {source}; \
         check that the path and its parents are writable and not already occupied"
    )]
    Io {
        agent: AgentId,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Where the operator's Claude Code keeps `.credentials.json`:
/// `$CLAUDE_SECURESTORAGE_CONFIG_DIR` when the daemon has one, else
/// [`operator_config_dir`]. Exported to every `isolated_config` agent so all
/// sessions read and refresh one credential store under one refresh lock.
#[must_use]
pub fn operator_credential_store() -> Option<PathBuf> {
    credential_store_path(
        std::env::var_os("CLAUDE_SECURESTORAGE_CONFIG_DIR"),
        std::env::var_os("CLAUDE_CONFIG_DIR"),
        std::env::home_dir(),
    )
}

/// [`operator_credential_store`] with its inputs explicit. An empty variable
/// counts as unset.
#[must_use]
pub fn credential_store_path(
    secure_storage_dir: Option<OsString>,
    config_dir: Option<OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    match secure_storage_dir {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => operator_config_dir_path(config_dir, home),
    }
}

/// Where the operator's own Claude Code keeps its state: `$CLAUDE_CONFIG_DIR`
/// when the daemon has one, else `~/.claude`. `None` only when neither is
/// known.
#[must_use]
pub fn operator_config_dir() -> Option<PathBuf> {
    operator_config_dir_path(std::env::var_os("CLAUDE_CONFIG_DIR"), std::env::home_dir())
}

/// [`operator_config_dir`] with its inputs explicit, so the rule is tested
/// without mutating the process environment. An empty variable counts as
/// unset.
#[must_use]
pub fn operator_config_dir_path(
    config_dir: Option<OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    match config_dir.filter(|v| !v.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => home.map(|home| home.join(".claude")),
    }
}

/// The operator's `.claude.json` (trust keys, MCP declarations, onboarding
/// state). Claude Code keeps it inside `CLAUDE_CONFIG_DIR` when that is set
/// and at `~/.claude.json` — beside `~/.claude`, not inside it — otherwise.
/// Verified live on Claude Code 2.1.241.
#[must_use]
pub fn operator_claude_json() -> Option<PathBuf> {
    claude_json_path(std::env::var_os("CLAUDE_CONFIG_DIR"), std::env::home_dir())
}

/// [`operator_claude_json`] with its inputs explicit, so the rule is tested
/// without mutating the process environment. An empty variable counts as
/// unset.
#[must_use]
pub fn claude_json_path(config_dir: Option<OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    match config_dir.filter(|v| !v.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir).join(".claude.json")),
        None => home.map(|home| home.join(".claude.json")),
    }
}

/// `<runs_dir>/<run_id>/claude-config-<agent_id>`.
#[must_use]
pub fn config_dir_path(runs_dir: &Path, run_id: &RunId, agent: &AgentId) -> PathBuf {
    runs_dir
        .join(&run_id.0)
        .join(format!("claude-config-{}", agent.0))
}

/// Builds one managed dir per `isolated_config` agent and returns their
/// paths. Called once per run, before any spawn; a restart never rebuilds
/// the dir.
///
/// # Errors
/// [`ClaudeConfigError::Io`] naming the agent and the entry that failed.
pub(crate) fn write_agent_config_dirs(
    runs_dir: &Path,
    run_id: &RunId,
    workflow: &FrozenWorkflow,
) -> Result<BTreeMap<AgentId, PathBuf>, ClaudeConfigError> {
    let mut dirs = BTreeMap::new();
    for (id, cfg) in &workflow.agents {
        if !cfg.isolated_config {
            continue;
        }
        let dir = config_dir_path(runs_dir, run_id, id);
        write_config_dir(&dir, id, &cfg.skills)?;
        dirs.insert(id.clone(), dir);
    }
    Ok(dirs)
}

/// Seeds one managed dir for `agent` (0700; `.claude.json`, `settings.json`,
/// `skills/` links). Idempotent except for the skill links, which must not
/// already exist. Used per run agent and per `isolated_config` session.
///
/// # Errors
/// [`ClaudeConfigError::Io`] naming the agent and the entry that failed.
pub(crate) fn write_config_dir(
    dir: &Path,
    agent: &AgentId,
    skills: &[PathBuf],
) -> Result<(), ClaudeConfigError> {
    write_one(dir, skills).map_err(|IoAt { path, source }| ClaudeConfigError::Io {
        agent: agent.clone(),
        path,
        source,
    })
}

/// An IO failure with the entry it happened on, so the error names the
/// file or link rather than the whole dir.
struct IoAt {
    path: PathBuf,
    source: std::io::Error,
}

fn at(path: &Path) -> impl FnOnce(std::io::Error) -> IoAt {
    let path = path.to_path_buf();
    move |source| IoAt { path, source }
}

fn write_one(dir: &Path, skills: &[PathBuf]) -> Result<(), IoAt> {
    use std::os::unix::fs::{PermissionsExt, symlink};
    std::fs::create_dir_all(dir).map_err(at(dir))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(at(dir))?;
    for (name, contents) in [
        (".claude.json", CLAUDE_JSON),
        ("settings.json", SETTINGS_JSON),
    ] {
        let path = dir.join(name);
        write_private_file(&path, contents).map_err(at(&path))?;
    }
    if !skills.is_empty() {
        let skills_dir = dir.join("skills");
        std::fs::create_dir_all(&skills_dir).map_err(at(&skills_dir))?;
        for skill in skills {
            let Some(name) = skill.file_name() else {
                continue;
            };
            let link = skills_dir.join(name);
            if link.exists() {
                continue;
            }
            symlink(skill, &link).map_err(at(&link))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::Duration;

    use crate::claude_config::write_agent_config_dirs;
    use crate::types::config::{AgentConfig, FrozenWorkflow};
    use crate::types::id::{AgentId, RunId};

    fn workflow(agents: Vec<(&str, AgentConfig)>) -> FrozenWorkflow {
        FrozenWorkflow {
            name: "t".into(),
            hash: "h".into(),
            source_path: PathBuf::from("/tmp/tempo.toml"),
            ask_timeout: Duration::from_mins(1),
            idle_debounce: Duration::from_secs(2),
            scrollback: 1000,
            agents: agents
                .into_iter()
                .map(|(id, cfg)| (AgentId(id.into()), cfg))
                .collect(),
            mcp_servers: BTreeMap::new(),
            flows: BTreeMap::new(),
        }
    }

    fn isolated(skills: Vec<PathBuf>) -> AgentConfig {
        AgentConfig {
            isolated_config: true,
            skills,
            ..AgentConfig::new(PathBuf::from("/tmp"), "p")
        }
    }

    #[test]
    fn nothing_is_written_for_a_workflow_without_isolated_agents() {
        let t = tempfile::tempdir().expect("tmp");
        let wf = workflow(vec![(
            "plain",
            AgentConfig::new(PathBuf::from("/tmp"), "p"),
        )]);
        let dirs = write_agent_config_dirs(t.path(), &RunId("r1".into()), &wf).expect("writes");
        assert!(dirs.is_empty());
        assert!(!t.path().join("r1").exists(), "no run dir either");
    }

    #[test]
    fn managed_dir_is_private_and_seeded_exactly() {
        let t = tempfile::tempdir().expect("tmp");
        let skill = t.path().join("wf").join("skills").join("handoff");
        std::fs::create_dir_all(&skill).expect("skill");
        std::fs::write(skill.join("SKILL.md"), "---\nname: handoff\n---\n").expect("md");

        let wf = workflow(vec![("iso", isolated(vec![skill.clone()]))]);
        let dirs = write_agent_config_dirs(t.path(), &RunId("r1".into()), &wf).expect("writes");
        write_agent_config_dirs(t.path(), &RunId("r1".into()), &wf)
            .expect("a rewrite is idempotent — the skill link already exists");

        let dir = t.path().join("r1").join("claude-config-iso");
        assert_eq!(dirs[&AgentId("iso".into())], dir);
        let mode = std::fs::metadata(&dir).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "dir mode {mode:o}");
        assert_eq!(
            std::fs::read_to_string(dir.join(".claude.json")).expect("read"),
            r#"{"hasCompletedOnboarding":true}"#
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("settings.json")).expect("read"),
            r#"{"autoMemoryEnabled":false,"skipDangerousModePermissionPrompt":true}"#
        );
        assert_eq!(
            std::fs::read_link(dir.join("skills").join("handoff")).expect("link"),
            skill
        );
        let mut entries: Vec<String> = std::fs::read_dir(&dir)
            .expect("ls")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            [".claude.json", "settings.json", "skills"],
            "nothing else is seeded — credentials stay in the operator's store \
             (CLAUDE_SECURESTORAGE_CONFIG_DIR), never in this dir"
        );
    }

    #[test]
    fn operator_config_dir_path_prefers_config_dir_when_set_and_non_empty() {
        let home = PathBuf::from("/home/op");
        assert_eq!(
            super::operator_config_dir_path(Some("/srv/cfg".into()), Some(home)),
            Some(PathBuf::from("/srv/cfg"))
        );
    }

    #[test]
    fn operator_config_dir_path_falls_back_to_home_when_config_dir_is_empty() {
        let home = PathBuf::from("/home/op");
        assert_eq!(
            super::operator_config_dir_path(Some("".into()), Some(home.clone())),
            Some(home.join(".claude")),
            "an empty value counts as unset"
        );
    }

    #[test]
    fn operator_config_dir_path_falls_back_to_home_when_config_dir_is_unset() {
        let home = PathBuf::from("/home/op");
        assert_eq!(
            super::operator_config_dir_path(None, Some(home.clone())),
            Some(home.join(".claude"))
        );
    }

    #[test]
    fn operator_config_dir_path_is_none_when_neither_is_known() {
        assert_eq!(super::operator_config_dir_path(None, None), None);
    }

    #[test]
    fn operator_config_dir_path_ignores_a_missing_home_when_config_dir_is_set() {
        assert_eq!(
            super::operator_config_dir_path(Some("/srv/cfg".into()), None),
            Some(PathBuf::from("/srv/cfg"))
        );
    }

    #[test]
    fn claude_json_path_prefers_config_dir_when_set_and_non_empty() {
        let home = PathBuf::from("/home/op");
        assert_eq!(
            super::claude_json_path(Some("/srv/cfg".into()), Some(home)),
            Some(PathBuf::from("/srv/cfg/.claude.json"))
        );
    }

    #[test]
    fn claude_json_path_falls_back_to_home_when_config_dir_is_empty() {
        let home = PathBuf::from("/home/op");
        assert_eq!(
            super::claude_json_path(Some("".into()), Some(home.clone())),
            Some(home.join(".claude.json")),
            "an empty value counts as unset"
        );
    }

    #[test]
    fn claude_json_path_falls_back_to_home_when_config_dir_is_unset() {
        let home = PathBuf::from("/home/op");
        assert_eq!(
            super::claude_json_path(None, Some(home.clone())),
            Some(home.join(".claude.json"))
        );
    }

    #[test]
    fn claude_json_path_is_none_when_neither_is_known() {
        assert_eq!(super::claude_json_path(None, None), None);
    }

    #[test]
    fn claude_json_path_ignores_a_missing_home_when_config_dir_is_set() {
        assert_eq!(
            super::claude_json_path(Some("/srv/cfg".into()), None),
            Some(PathBuf::from("/srv/cfg/.claude.json"))
        );
    }

    #[test]
    fn seeded_files_are_private() {
        let t = tempfile::tempdir().expect("tmp");
        let wf = workflow(vec![("iso", isolated(Vec::new()))]);
        write_agent_config_dirs(t.path(), &RunId("r1".into()), &wf).expect("writes");
        let dir = t.path().join("r1").join("claude-config-iso");
        for name in [".claude.json", "settings.json"] {
            let mode = std::fs::metadata(dir.join(name))
                .expect("meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{name} mode {mode:o}");
        }
    }

    #[test]
    fn a_mixed_workflow_gets_exactly_one_dir_for_its_isolated_agent() {
        let t = tempfile::tempdir().expect("tmp");
        let wf = workflow(vec![
            ("plain", AgentConfig::new(PathBuf::from("/tmp"), "p")),
            ("iso", isolated(Vec::new())),
        ]);
        let dirs = write_agent_config_dirs(t.path(), &RunId("r1".into()), &wf).expect("writes");
        assert_eq!(dirs.keys().collect::<Vec<_>>(), [&AgentId("iso".into())]);
        let mut entries: Vec<String> = std::fs::read_dir(t.path().join("r1"))
            .expect("ls")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(entries, ["claude-config-iso"]);
    }

    #[test]
    fn an_unwritable_runs_dir_is_an_io_error_naming_the_agent_and_path() {
        let t = tempfile::tempdir().expect("tmp");
        let runs_dir = t.path().join("runs");
        std::fs::write(&runs_dir, "not a directory").expect("file");
        let wf = workflow(vec![("iso", isolated(Vec::new()))]);
        let err = write_agent_config_dirs(&runs_dir, &RunId("r1".into()), &wf).expect_err("fails");
        let super::ClaudeConfigError::Io { agent, path, .. } = &err;
        assert_eq!(agent, &AgentId("iso".into()));
        assert!(path.starts_with(&runs_dir), "{}", path.display());
        let text = err.to_string();
        assert!(text.contains("agent iso"), "{text}");
        assert!(text.contains(&runs_dir.display().to_string()), "{text}");
    }

    #[test]
    fn a_failing_seed_file_is_named_itself_not_its_dir() {
        let t = tempfile::tempdir().expect("tmp");
        let dir = t.path().join("r1").join("claude-config-iso");
        std::fs::create_dir_all(dir.join(".claude.json")).expect("a dir in the file's place");
        let wf = workflow(vec![("iso", isolated(Vec::new()))]);
        let err = write_agent_config_dirs(t.path(), &RunId("r1".into()), &wf).expect_err("fails");
        let super::ClaudeConfigError::Io { path, .. } = &err;
        assert_eq!(path, &dir.join(".claude.json"));
        let text = err.to_string();
        assert!(
            !text.contains("~/.coretempo/runs"),
            "no hardcoded fix path: {text}"
        );
        assert!(
            text.contains(&dir.join(".claude.json").display().to_string()),
            "{text}"
        );
    }

    #[test]
    fn no_skills_means_no_skills_dir() {
        let t = tempfile::tempdir().expect("tmp");
        let wf = workflow(vec![("iso", isolated(Vec::new()))]);
        write_agent_config_dirs(t.path(), &RunId("r1".into()), &wf).expect("writes");
        let dir = t.path().join("r1").join("claude-config-iso");
        assert!(dir.join(".claude.json").is_file());
        assert!(!dir.join("skills").exists(), "no skills dir without skills");
    }

    #[test]
    fn credential_store_path_prefers_an_operator_exported_secure_storage_dir() {
        assert_eq!(
            super::credential_store_path(
                Some("/srv/store".into()),
                Some("/srv/cfg".into()),
                Some(PathBuf::from("/home/op"))
            ),
            Some(PathBuf::from("/srv/store"))
        );
    }

    #[test]
    fn credential_store_path_falls_back_to_the_operator_config_dir() {
        assert_eq!(
            super::credential_store_path(None, Some("/srv/cfg".into()), None),
            Some(PathBuf::from("/srv/cfg"))
        );
        assert_eq!(
            super::credential_store_path(Some("".into()), None, Some(PathBuf::from("/home/op"))),
            Some(PathBuf::from("/home/op/.claude")),
            "an empty value counts as unset"
        );
        assert_eq!(super::credential_store_path(None, None, None), None);
    }
}
