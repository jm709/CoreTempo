//! Spawn recipe (spec §4.1): `claude` at the agent's frozen `dir` with
//! `--append-system-prompt` (the role prompt plus the protocol primer, built by
//! [`crate::types::config::FrozenWorkflow::system_prompt`]), optional
//! `--model`/`--permission-mode`, `--settings` pointing at the run's turn-boundary
//! hooks, `--strict-mcp-config` always plus `--mcp-config` for agents that opted
//! into MCP servers (spec 2026-08-17 §2), and the `CORETEMPO_*` env with `tempo`
//! on PATH plus `CLAUDE_CONFIG_DIR` and `CLAUDE_SECURESTORAGE_CONFIG_DIR` for
//! `isolated_config` agents (spec 2026-08-24 §2, §4).

use std::path::PathBuf;

use crate::pty::AgentEnv;
use crate::types::config::AgentConfig;
use crate::types::id::AgentId;

pub(crate) struct SpawnSpec {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    pub(crate) env: Vec<(String, String)>,
    /// Parent vars to drop before exec (see `leaked_claude_vars`).
    pub(crate) unset_env: Vec<String>,
    pub(crate) cwd: PathBuf,
}

/// `CLAUDE_CODE_*` vars inherited from a parent Claude Code session silently
/// change a spawned agent's behavior (e.g. `CLAUDE_CODE_CHILD_SESSION` turns
/// off transcript saving). Agents must start from a clean slate.
pub(crate) fn leaked_claude_vars<I>(vars: I) -> Vec<String>
where
    I: Iterator<Item = String>,
{
    let mut found: Vec<String> = vars.filter(|k| k.starts_with("CLAUDE_CODE_")).collect();
    found.sort();
    found
}

/// Everything the recipe needs about one agent. Grouped so the recipe stays a
/// single-argument call as it grows (precedent: `PipelineCtx`).
pub(crate) struct SpawnInputs<'a> {
    pub(crate) id: &'a AgentId,
    pub(crate) cfg: &'a AgentConfig,
    pub(crate) env: &'a AgentEnv,
    pub(crate) program: &'a str,
    pub(crate) system_prompt: &'a str,
}

pub(crate) fn spawn_spec(inputs: &SpawnInputs<'_>) -> SpawnSpec {
    let SpawnInputs {
        id,
        cfg,
        env,
        program,
        system_prompt,
    } = *inputs;

    let mut args = vec![
        "--append-system-prompt".to_string(),
        system_prompt.to_string(),
    ];
    if let Some(model) = &cfg.model {
        args.push("--model".to_string());
        args.push(model.clone());
    }
    if let Some(mode) = &cfg.permission_mode {
        args.push("--permission-mode".to_string());
        args.push(mode.clone());
    }
    if let Some(settings) = env.settings_paths.get(id) {
        args.push("--settings".to_string());
        args.push(settings.to_string_lossy().into_owned());
    }
    args.push("--strict-mcp-config".to_string());
    if let Some(mcp) = env.mcp_paths.get(id) {
        args.push("--mcp-config".to_string());
        args.push(mcp.to_string_lossy().into_owned());
    }

    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![env.tempo_bin_dir.clone()];
    paths.extend(std::env::split_paths(&inherited));
    let path_value = std::env::join_paths(paths).map_or_else(
        |_| inherited.to_string_lossy().into_owned(),
        |p| p.to_string_lossy().into_owned(),
    );

    let mut env_vars = vec![
        ("CORETEMPO_AGENT_ID".to_string(), id.0.clone()),
        ("CORETEMPO_PORT".to_string(), env.port.to_string()),
        ("CORETEMPO_TOKEN".to_string(), env.token.0.clone()),
        ("PATH".to_string(), path_value),
    ];
    if let Some(dir) = env.config_dirs.get(id) {
        env_vars.push((
            "CLAUDE_CONFIG_DIR".to_string(),
            dir.to_string_lossy().into_owned(),
        ));
        if let Some(store) = &env.credential_store {
            env_vars.push((
                "CLAUDE_SECURESTORAGE_CONFIG_DIR".to_string(),
                store.to_string_lossy().into_owned(),
            ));
        }
    }

    SpawnSpec {
        program: program.to_string(),
        args,
        env: env_vars,
        unset_env: leaked_claude_vars(std::env::vars().map(|(k, _)| k)),
        cwd: cfg.dir.clone(),
    }
}

pub(crate) fn to_command(spec: &SpawnSpec) -> portable_pty::CommandBuilder {
    let mut cmd = portable_pty::CommandBuilder::new(&spec.program);
    cmd.args(&spec.args);
    cmd.cwd(&spec.cwd);
    for key in &spec.unset_env {
        cmd.env_remove(key);
    }
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }
    cmd
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::pty::AgentEnv;
    use crate::pty::spawn::{SpawnInputs, spawn_spec};
    use crate::types::config::AgentConfig;
    use crate::types::id::{AgentId, Token};

    fn cfg(model: Option<&str>, mode: Option<&str>) -> AgentConfig {
        AgentConfig {
            model: model.map(str::to_string),
            permission_mode: mode.map(str::to_string),
            ..AgentConfig::new(
                PathBuf::from("/home/u/projects/CoreTempo"),
                "You are the planning agent",
            )
        }
    }

    fn env() -> AgentEnv {
        AgentEnv {
            port: 4820,
            token: Token("ab".repeat(32)),
            tempo_bin_dir: PathBuf::from("/opt/coretempo/bin"),
            settings_paths: std::collections::BTreeMap::new(),
            mcp_paths: std::collections::BTreeMap::new(),
            config_dirs: std::collections::BTreeMap::new(),
            credential_store: Some(PathBuf::from("/home/u/.claude")),
        }
    }

    fn inputs<'a>(id: &'a AgentId, cfg: &'a AgentConfig, env: &'a AgentEnv) -> SpawnInputs<'a> {
        SpawnInputs {
            id,
            cfg,
            env,
            program: "claude",
            system_prompt: "You are the planning agent",
        }
    }

    #[test]
    fn minimal_spec_has_prompt_env_and_cwd() {
        let id = AgentId("planner".into());
        let cfg = cfg(None, None);
        let env = env();
        let spec = spawn_spec(&inputs(&id, &cfg, &env));
        assert_eq!(spec.program, "claude");
        assert_eq!(
            spec.args,
            vec![
                "--append-system-prompt",
                "You are the planning agent",
                "--strict-mcp-config"
            ],
            "strict MCP is unconditional: no --mcp-config means zero servers"
        );
        assert_eq!(spec.cwd, PathBuf::from("/home/u/projects/CoreTempo"));
        let get = |k: &str| {
            spec.env
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("CORETEMPO_AGENT_ID").as_deref(), Some("planner"));
        assert_eq!(get("CORETEMPO_PORT").as_deref(), Some("4820"));
        assert_eq!(
            get("CORETEMPO_TOKEN").as_deref(),
            Some("ab".repeat(32).as_str())
        );
        let path = get("PATH").unwrap();
        assert!(
            path.starts_with("/opt/coretempo/bin"),
            "tempo dir prepended, got {path}"
        );
    }

    #[test]
    fn optional_flags_pass_through_in_order() {
        let id = AgentId("builder".into());
        let cfg = cfg(Some("opus"), Some("acceptEdits"));
        let env = env();
        let spec = spawn_spec(&inputs(&id, &cfg, &env));
        assert_eq!(
            spec.args,
            vec![
                "--append-system-prompt",
                "You are the planning agent",
                "--model",
                "opus",
                "--permission-mode",
                "acceptEdits",
                "--strict-mcp-config",
            ]
        );
    }

    #[test]
    fn agents_with_mcp_get_their_own_config_file_last() {
        let id = AgentId("resolver".into());
        let other = AgentId("helper".into());
        let cfg = cfg(None, None);
        let mut env = env();
        env.settings_paths.insert(
            id.clone(),
            PathBuf::from("/home/u/.coretempo/runs/r1/agent-settings-resolver.json"),
        );
        env.mcp_paths.insert(
            id.clone(),
            PathBuf::from("/home/u/.coretempo/runs/r1/agent-mcp-resolver.json"),
        );
        let spec = spawn_spec(&inputs(&id, &cfg, &env));
        let n = spec.args.len();
        assert_eq!(
            &spec.args[n - 3..],
            [
                "--strict-mcp-config",
                "--mcp-config",
                "/home/u/.coretempo/runs/r1/agent-mcp-resolver.json"
            ],
            "--mcp-config is variadic in claude, so it must be the final flag: {:?}",
            spec.args
        );
        let spec = spawn_spec(&inputs(&other, &cfg, &env));
        assert!(spec.args.contains(&"--strict-mcp-config".to_string()));
        assert!(
            !spec.args.contains(&"--mcp-config".to_string()),
            "no file for helper: {:?}",
            spec.args
        );
    }

    #[test]
    fn agents_get_their_own_settings_file() {
        let id = AgentId("pa".into());
        let other = AgentId("helper".into());
        let cfg = cfg(None, None);
        let mut env = env();
        env.settings_paths.insert(
            id.clone(),
            PathBuf::from("/home/u/.coretempo/runs/r1/agent-settings-pa.json"),
        );
        let spec = spawn_spec(&inputs(&id, &cfg, &env));
        let pos = spec
            .args
            .iter()
            .position(|a| a == "--settings")
            .expect("--settings");
        assert_eq!(
            spec.args[pos + 1],
            "/home/u/.coretempo/runs/r1/agent-settings-pa.json"
        );
        let spec = spawn_spec(&inputs(&other, &cfg, &env));
        assert!(
            !spec.args.contains(&"--settings".to_string()),
            "no file for helper: {:?}",
            spec.args
        );
    }

    #[test]
    fn isolated_agents_get_config_dir_and_credential_store_and_others_do_not() {
        let iso = AgentId("iso".into());
        let plain = AgentId("plain".into());
        let cfg = cfg(None, None);
        let mut env = env();
        env.config_dirs.insert(
            iso.clone(),
            PathBuf::from("/home/u/.coretempo/runs/r1/claude-config-iso"),
        );
        let get = |spec: &super::SpawnSpec, k: &str| {
            spec.env
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        let spec = spawn_spec(&inputs(&iso, &cfg, &env));
        assert_eq!(
            get(&spec, "CLAUDE_CONFIG_DIR").as_deref(),
            Some("/home/u/.coretempo/runs/r1/claude-config-iso")
        );
        assert_eq!(
            get(&spec, "CLAUDE_SECURESTORAGE_CONFIG_DIR").as_deref(),
            Some("/home/u/.claude"),
            "an isolated agent reads and refreshes the operator's credentials in place"
        );
        let spec = spawn_spec(&inputs(&plain, &cfg, &env));
        assert_eq!(
            get(&spec, "CLAUDE_CONFIG_DIR"),
            None,
            "a non-isolated agent's env is untouched (inheritance): {:?}",
            spec.env
        );
        assert_eq!(get(&spec, "CLAUDE_SECURESTORAGE_CONFIG_DIR"), None);
    }

    #[test]
    fn an_unknown_credential_store_sets_only_the_config_dir() {
        let iso = AgentId("iso".into());
        let cfg = cfg(None, None);
        let mut env = env();
        env.credential_store = None;
        env.config_dirs
            .insert(iso.clone(), PathBuf::from("/runs/r1/claude-config-iso"));
        let spec = spawn_spec(&inputs(&iso, &cfg, &env));
        assert!(spec.env.iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"));
        assert!(
            !spec
                .env
                .iter()
                .any(|(k, _)| k == "CLAUDE_SECURESTORAGE_CONFIG_DIR"),
            "nothing to point at, so the var is left alone: {:?}",
            spec.env
        );
    }

    #[test]
    fn inherited_claude_code_vars_are_dropped() {
        let vars = [
            "PATH".to_string(),
            "CLAUDE_CODE_CHILD_SESSION".to_string(),
            "CLAUDE_CODE_ENTRYPOINT".to_string(),
            "HOME".to_string(),
        ];
        let leaked = super::leaked_claude_vars(vars.into_iter());
        assert_eq!(
            leaked,
            vec![
                "CLAUDE_CODE_CHILD_SESSION".to_string(),
                "CLAUDE_CODE_ENTRYPOINT".to_string()
            ],
            "only CLAUDE_CODE_* vars are unset; PATH/HOME must survive"
        );
    }

    /// `leaked_claude_vars` is pure and tested above; this proves `spawn_spec`
    /// feeds it the live process environment.
    #[test]
    fn spawn_spec_marks_leaked_vars_for_removal() {
        // SAFETY: tests share one process environment, so a concurrent reader can
        // observe this variable — it is unique to this test, harmless to every
        // other reader (no code path consumes it), and removed right after the
        // one call under test.
        unsafe { std::env::set_var("CLAUDE_CODE_TEST_LEAK", "1") };
        let id = AgentId("a".to_string());
        let cfg = cfg(None, None);
        let env = env();
        let spec = spawn_spec(&inputs(&id, &cfg, &env));
        // SAFETY: as above.
        unsafe { std::env::remove_var("CLAUDE_CODE_TEST_LEAK") };
        assert!(spec.unset_env.iter().any(|k| k == "CLAUDE_CODE_TEST_LEAK"));
    }
}
