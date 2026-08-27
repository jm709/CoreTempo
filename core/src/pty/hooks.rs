//! Turn-boundary and in-turn dialog hooks handed to every spawned agent via
//! `claude --settings`.
//!
//! Claude Code fires `SessionStart` when the session is ready, `UserPromptSubmit`
//! when a turn starts, and `Stop` (or `StopFailure`, when the turn ended on an API
//! error) when it ends; those run `tempo state <working|idle>`. It also fires
//! `PermissionRequest` when a tool call is waiting on a permission dialog and
//! `PostToolBatch` once that dialog is answered; those run
//! `tempo state <blocked|unblocked>` (spec 2026-08-17 §3). Each hook reports the
//! agent's state over the API; the hook inherits the agent's `CORETEMPO_*` env, so
//! it authenticates as that agent without any extra wiring.
//!
//! Entries merge with the user's own settings rather than replacing them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::api::auth::write_private_file;
use crate::types::config::{AgentConfig, PermissionPrompt};
use crate::types::id::{AgentId, RunId};

/// Renders the settings JSON that maps Claude Code's turn boundaries onto
/// `tempo state` invocations of the binary at `tempo_bin`, allows the agent to
/// call `tempo` plus each of `tools` via Bash without a permission prompt, and
/// appends `allow` verbatim after those `Bash(...)` entries. Under
/// [`PermissionPrompt::Deny`] the `PermissionRequest` hook runs
/// `tempo state refused`, which answers the dialog with a deny decision.
pub(crate) fn settings_json(
    tempo_bin: &Path,
    tools: &[String],
    allow: &[String],
    on_permission_prompt: PermissionPrompt,
) -> String {
    let event = |state: &str| {
        json!([{
            "hooks": [{
                "type": "command",
                "command": format!("{} state {state}", shell_word(tempo_bin)),
            }],
        }])
    };
    let mut allow_rules = vec!["Bash(tempo:*)".to_string()];
    for tool in tools {
        let entry = format!("Bash({tool}:*)");
        if !allow_rules.contains(&entry) {
            allow_rules.push(entry);
        }
    }
    for rule in allow {
        if !allow_rules.contains(rule) {
            allow_rules.push(rule.clone());
        }
    }
    json!({
        "hooks": {
            "SessionStart": event("idle"),
            "UserPromptSubmit": event("working"),
            "Stop": event("idle"),
            "StopFailure": event("idle"),
            // `PermissionRequest`/`PostToolBatch` carry the in-turn dialog signal
            // (spec 2026-08-17 §3); under `deny` the request is answered instead.
            "PermissionRequest": event(match on_permission_prompt {
                PermissionPrompt::Deny => "refused",
                PermissionPrompt::Wait => "blocked",
            }),
            "PostToolBatch": event("unblocked"),
        },
        "permissions": { "allow": allow_rules },
    })
    .to_string()
}

/// Quotes a path for the shell that runs the hook command. Ordinary paths pass
/// through untouched; anything else is single-quoted (`'` closed and re-opened).
fn shell_word(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let safe = |c: char| c.is_ascii_alphanumeric() || "._-+/=:,@".contains(c);
    if !raw.is_empty() && raw.chars().all(safe) {
        return raw.into_owned();
    }
    format!("'{}'", raw.replace('\'', r"'\''"))
}

/// Writes `<runs_dir>/<run_id>/agent-settings-<agent_id>.json` (mode 0600, next to
/// `api.json`) for every agent and returns each path. Called once per run, before
/// the agents are spawned. Each agent's file allows only its own configured tools.
///
/// # Errors
/// Propagates filesystem errors from creating the run directory or writing a file.
pub(crate) fn write_agent_settings_files(
    runs_dir: &Path,
    run_id: &RunId,
    tempo_bin: &Path,
    agents: &BTreeMap<AgentId, AgentConfig>,
) -> std::io::Result<BTreeMap<AgentId, PathBuf>> {
    let dir = runs_dir.join(&run_id.0);
    std::fs::create_dir_all(&dir)?;
    let mut paths = BTreeMap::new();
    for (id, cfg) in agents {
        let path = dir.join(format!("agent-settings-{}.json", id.0));
        write_private_file(
            &path,
            &settings_json(tempo_bin, &cfg.tools, &cfg.allow, cfg.on_permission_prompt),
        )?;
        paths.insert(id.clone(), path);
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::pty::hooks::{settings_json, shell_word, write_agent_settings_files};
    use crate::types::config::{AgentConfig, PermissionPrompt};
    use crate::types::id::{AgentId, RunId};

    fn command_for(parsed: &serde_json::Value, event: &str) -> String {
        parsed["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap_or_else(|| panic!("no command for {event} in {parsed}"))
            .to_string()
    }

    #[test]
    fn renders_the_six_hooks() {
        let json = settings_json(
            Path::new("/opt/coretempo/bin/tempo"),
            &[],
            &[],
            PermissionPrompt::Deny,
        );
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(
            command_for(&parsed, "SessionStart"),
            "/opt/coretempo/bin/tempo state idle"
        );
        assert_eq!(
            command_for(&parsed, "UserPromptSubmit"),
            "/opt/coretempo/bin/tempo state working"
        );
        assert_eq!(
            command_for(&parsed, "Stop"),
            "/opt/coretempo/bin/tempo state idle"
        );
        assert_eq!(
            command_for(&parsed, "StopFailure"),
            "/opt/coretempo/bin/tempo state idle"
        );
        assert_eq!(
            command_for(&parsed, "PermissionRequest"),
            "/opt/coretempo/bin/tempo state refused",
            "deny is the default: the hook answers the dialog"
        );
        assert_eq!(
            command_for(&parsed, "PostToolBatch"),
            "/opt/coretempo/bin/tempo state unblocked"
        );
        let hooks = parsed["hooks"].as_object().expect("hooks object");
        assert_eq!(hooks.len(), 6, "no extra events: {json}");
        assert_eq!(parsed["hooks"]["Stop"][0]["hooks"][0]["type"], "command");
    }

    #[test]
    fn wait_policy_keeps_the_blocked_hook() {
        let json = settings_json(
            Path::new("/opt/coretempo/bin/tempo"),
            &[],
            &[],
            PermissionPrompt::Wait,
        );
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(
            command_for(&parsed, "PermissionRequest"),
            "/opt/coretempo/bin/tempo state blocked"
        );
        assert_eq!(
            command_for(&parsed, "PostToolBatch"),
            "/opt/coretempo/bin/tempo state unblocked"
        );
    }

    #[test]
    fn awkward_paths_survive_json_and_the_shell() {
        let json = settings_json(
            Path::new("/home/o'brien/my tools/tempo"),
            &[],
            &[],
            PermissionPrompt::Deny,
        );
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(
            command_for(&parsed, "Stop"),
            r"'/home/o'\''brien/my tools/tempo' state idle"
        );
    }

    #[test]
    fn shell_word_quotes_only_when_needed() {
        assert_eq!(shell_word(Path::new("/usr/bin/tempo")), "/usr/bin/tempo");
        assert_eq!(shell_word(Path::new("/a b/tempo")), "'/a b/tempo'");
        assert_eq!(shell_word(Path::new("")), "''");
    }

    #[test]
    fn permissions_always_allow_tempo_and_config_tools() {
        let json = settings_json(
            Path::new("/opt/bin/tempo"),
            &["pat".into(), "pat".into()],
            &[],
            PermissionPrompt::Deny,
        );
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(
            parsed["permissions"]["allow"],
            serde_json::json!(["Bash(tempo:*)", "Bash(pat:*)"]),
            "tempo first, tools deduped: {json}"
        );
    }

    #[test]
    fn permissions_with_no_tools_still_allow_tempo() {
        let json = settings_json(
            Path::new("/opt/bin/tempo"),
            &[],
            &[],
            PermissionPrompt::Deny,
        );
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(
            parsed["permissions"]["allow"],
            serde_json::json!(["Bash(tempo:*)"])
        );
    }

    #[test]
    fn allow_rules_follow_the_bash_entries_verbatim() {
        let json = settings_json(
            Path::new("/opt/bin/tempo"),
            &["pat".into()],
            &[
                "WebSearch".into(),
                "Read(//data/**)".into(),
                "WebSearch".into(),
            ],
            PermissionPrompt::Deny,
        );
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(
            parsed["permissions"]["allow"],
            serde_json::json!([
                "Bash(tempo:*)",
                "Bash(pat:*)",
                "WebSearch",
                "Read(//data/**)"
            ]),
            "bash first, then allow rules deduped: {json}"
        );
    }

    #[test]
    fn per_agent_files_differ_by_tools() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut agents = std::collections::BTreeMap::new();
        let base = AgentConfig::new(PathBuf::from("/w"), "p");
        agents.insert(
            AgentId("pa".into()),
            AgentConfig {
                tools: vec!["pat".into()],
                ..base.clone()
            },
        );
        agents.insert(AgentId("helper".into()), base);
        let run_id = RunId("r1".into());
        let paths =
            write_agent_settings_files(dir.path(), &run_id, Path::new("/opt/bin/tempo"), &agents)
                .expect("write");
        assert!(paths[&AgentId("pa".into())].ends_with("r1/agent-settings-pa.json"));
        let pa: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&paths[&AgentId("pa".into())]).expect("read"),
        )
        .expect("json");
        let helper: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&paths[&AgentId("helper".into())]).expect("read"),
        )
        .expect("json");
        assert_eq!(
            pa["permissions"]["allow"],
            serde_json::json!(["Bash(tempo:*)", "Bash(pat:*)"])
        );
        assert_eq!(
            helper["permissions"]["allow"],
            serde_json::json!(["Bash(tempo:*)"])
        );
        assert_eq!(
            pa["hooks"], helper["hooks"],
            "hooks identical across agents"
        );
    }
}
