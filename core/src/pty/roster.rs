//! What `PtyManager` knows about each agent it owns (spec 2026-08-27 §4,
//! contracts amendment 46). A workflow run builds one entry per frozen agent;
//! the session daemon adds entries at runtime. Nothing here reads a workflow.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::types::config::AgentConfig;
use crate::types::id::{AgentId, Token};

/// How the spawn recipe handles MCP servers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpPolicy {
    /// `--strict-mcp-config` always, `--mcp-config <file>` when there is one:
    /// the agent sees only the servers its workflow declared (workflow runs).
    Strict(Option<PathBuf>),
    /// Neither flag: the agent inherits the operator's own MCP setup (sessions).
    /// Hazard: without `--strict-mcp-config`, a project whose `.mcp.json`
    /// servers the operator has not approved raises Claude Code's "New MCP
    /// server found" dialog, which fires no hook — the agent then sits in
    /// `starting` forever (CLAUDE.md gotchas). An `isolated_config` session's
    /// managed `.claude.json` has no approvals at all. Callers must seed
    /// approvals (`claude_config.rs`) or use `Strict`.
    Inherit,
}

/// One agent's spawn inputs. Everything the recipe used to look up in the
/// frozen workflow or in per-agent maps lives here, per entry.
#[derive(Debug, Clone, PartialEq)]
pub struct RosterEntry {
    pub cfg: AgentConfig,
    /// `Some` = `--append-system-prompt` with this text; `None` = flag omitted.
    pub system_prompt: Option<String>,
    pub mcp: McpPolicy,
    /// `--settings` file (turn-boundary hooks, allowlist), if any.
    pub settings_path: Option<PathBuf>,
    /// `CLAUDE_CONFIG_DIR` for `isolated_config` agents, if any.
    pub config_dir: Option<PathBuf>,
    /// Overrides `AgentEnv::token` as `CORETEMPO_TOKEN` (per-session hook tokens).
    pub token: Option<Token>,
    /// `--resume <claude_session_id>` for the next spawn only; consumed by the
    /// next spawn that succeeds. A spawn refused by a [`crate::pty::SpawnGate`]
    /// or that fails to open a pty leaves it armed for the next attempt.
    pub resume: Option<String>,
}

impl RosterEntry {
    /// An entry with nothing optional set: strict MCP with no file, no system
    /// prompt, no settings, no config dir, the env token, no resume.
    #[must_use]
    pub fn new(cfg: AgentConfig) -> Self {
        RosterEntry {
            cfg,
            system_prompt: None,
            mcp: McpPolicy::Strict(None),
            settings_path: None,
            config_dir: None,
            token: None,
            resume: None,
        }
    }
}

/// The agents a `PtyManager` starts with, plus the idle debounce every
/// agent's state channel uses. May be empty (the session daemon adds agents
/// through `PtyManager::add_agent`).
#[derive(Debug, Clone)]
pub struct PtyRoster {
    pub agents: BTreeMap<AgentId, RosterEntry>,
    pub idle_debounce: Duration,
}

impl PtyRoster {
    /// No agents yet.
    #[must_use]
    pub fn empty(idle_debounce: Duration) -> Self {
        PtyRoster {
            agents: BTreeMap::new(),
            idle_debounce,
        }
    }
}
