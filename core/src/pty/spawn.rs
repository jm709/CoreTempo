//! Spawn recipe (spec §4.1): `claude` at the agent's frozen `dir` with
//! `--append-system-prompt` (the role prompt plus the protocol primer, built by
//! [`crate::types::config::FrozenWorkflow::system_prompt`]), optional
//! `--model`/`--permission-mode`, `--settings` pointing at the run's turn-boundary
//! hooks, and the `CORETEMPO_*` env with `tempo` on PATH.

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

    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![env.tempo_bin_dir.clone()];
    paths.extend(std::env::split_paths(&inherited));
    let path_value = std::env::join_paths(paths).map_or_else(
        |_| inherited.to_string_lossy().into_owned(),
        |p| p.to_string_lossy().into_owned(),
    );

    SpawnSpec {
        program: program.to_string(),
        args,
        env: vec![
            ("CORETEMPO_AGENT_ID".to_string(), id.0.clone()),
            ("CORETEMPO_PORT".to_string(), env.port.to_string()),
            ("CORETEMPO_TOKEN".to_string(), env.token.0.clone()),
            ("PATH".to_string(), path_value),
        ],
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
            dir: PathBuf::from("/home/u/projects/CoreTempo"),
            prompt: "You are the planning agent".into(),
            model: model.map(str::to_string),
            permission_mode: mode.map(str::to_string),
            auto_clear: true,
            edges: Vec::new(),
            tools: Vec::new(),
        }
    }

    fn env() -> AgentEnv {
        AgentEnv {
            port: 4820,
            token: Token("ab".repeat(32)),
            tempo_bin_dir: PathBuf::from("/opt/coretempo/bin"),
            settings_paths: std::collections::BTreeMap::new(),
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
            vec!["--append-system-prompt", "You are the planning agent"]
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
            ]
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

    #[test]
    fn spawn_spec_marks_leaked_vars_for_removal() {
        unsafe { std::env::set_var("CLAUDE_CODE_TEST_LEAK", "1") };
        let id = AgentId("a".to_string());
        let cfg = cfg(None, None);
        let env = env();
        let spec = spawn_spec(&inputs(&id, &cfg, &env));
        unsafe { std::env::remove_var("CLAUDE_CODE_TEST_LEAK") };
        assert!(spec.unset_env.iter().any(|k| k == "CLAUDE_CODE_TEST_LEAK"));
    }
}
