//! tempo.toml wire structs and frozen-run config (contracts §2.5/§2.6).
//! Load/validate/freeze behavior lives in `crate::workflow` (workflow-run plan).

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::types::id::{AgentId, Token};
use crate::types::message::MessageKind;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowFile {
    pub workflow: WorkflowSection,
    #[serde(default)]
    pub server: ServerSection,
    /// ≥1 required (enforced by `crate::workflow`); roster order = lexicographic.
    pub agents: BTreeMap<AgentId, AgentConfig>,
    #[serde(default)]
    pub trigger: Option<TriggerConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSection {
    pub name: String,
    #[serde(default = "d_db")]
    pub db: PathBuf,
    #[serde(default = "d_port")]
    pub port: u16,
    #[serde(default = "d_ttl")]
    pub ask_timeout_minutes: u64,
    #[serde(default = "d_deb")]
    pub idle_debounce_seconds: f64,
    /// xterm scrollback lines per terminal pane (desktop app only; headless
    /// runs ignore it). Memory cost is roughly 1 KiB x scrollback x agents.
    #[serde(default = "d_scrollback")]
    pub scrollback: u32,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    /// Default 127.0.0.1.
    pub bind: Option<IpAddr>,
    pub token_file: Option<PathBuf>,
    /// Future remote UI; empty = no CORS.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// tracing `EnvFilter` string.
    pub log: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Required; `~` expanded at freeze.
    pub dir: PathBuf,
    /// Required; becomes `--append-system-prompt` (+ protocol primer).
    pub prompt: String,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    #[serde(default = "d_true")]
    pub auto_clear: bool,
    /// Ordered delegation steps composed into the frozen prompt (spec §1.2)
    /// and enforced by obligation tracking (spec §2). Default: none.
    #[serde(default)]
    pub edges: Vec<Edge>,
    /// Bare binary names composed into the generated settings' Bash allowlist
    /// as `Bash(<name>:*)` (PA spec 2026-08-05). `tempo` is always included.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// Rounds a loop may run before the soft cap (edge-semantics spec): the loop
/// stops re-arming and the owner is nudged toward `tempo done`.
pub const DEFAULT_LOOP_ROUNDS: u32 = 10;

/// One deterministic delegation step (spec §1.1): the UI's node-graph edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    pub to: AgentId,
    pub kind: EdgeKind,
    /// `loop` edges only: soft round cap (default [`DEFAULT_LOOP_ROUNDS`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
}

impl Edge {
    #[must_use]
    pub fn effective_max_rounds(&self) -> u32 {
        self.max_rounds.unwrap_or(DEFAULT_LOOP_ROUNDS)
    }
}

/// How an edge delegates (edge-semantics spec, 2026-08-05): `ask` is one
/// round-trip, `send` a one-way handoff, `loop` supervised iteration ended
/// by `tempo done`. Distinct from `MessageKind` — a loop round travels as
/// an `ask` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeKind {
    Ask,
    Send,
    Loop,
}

impl EdgeKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Ask => "ask",
            EdgeKind::Send => "send",
            EdgeKind::Loop => "loop",
        }
    }

    /// The message kind that travels for this edge: loop rounds are asks.
    #[must_use]
    pub fn message_kind(self) -> MessageKind {
        match self {
            EdgeKind::Ask | EdgeKind::Loop => MessageKind::Ask,
            EdgeKind::Send => MessageKind::Send,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerType {
    OnStart,
    Webhook,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerConfig {
    #[serde(rename = "type")]
    pub trigger_type: TriggerType,
    /// Kickoff target + kind — reuses the agent-edge shape; `kind` picks the
    /// completion rule (ask → terminal status, send → global quiescence).
    pub edge: Edge,
    /// `on_start` only: the static kickoff message. Webhook uses the HTTP body.
    pub message: Option<String>,
    /// Webhook only: the reply's structured-output contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputConfig>,
}

/// Structured-output contract declaration (design 2026-08-06). Exactly one of
/// `schema`/`schema_file` — enforced by `crate::workflow::validate_trigger`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputConfig {
    /// Inline JSON Schema (draft 2020-12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
    /// Path to a JSON Schema file, relative to the tempo.toml.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_file: Option<PathBuf>,
    /// Rejections allowed before the reply is accepted as-is and the trigger
    /// fails (0..=5; 0 = validate once, never re-ask).
    #[serde(default = "d_repairs")]
    pub max_repairs: u32,
}

fn d_db() -> PathBuf {
    PathBuf::from("./tempo.db")
}
fn d_port() -> u16 {
    4820
}
fn d_ttl() -> u64 {
    30
}
fn d_deb() -> f64 {
    2.0
}
fn d_true() -> bool {
    true
}
fn d_scrollback() -> u32 {
    5_000
}
fn d_repairs() -> u32 {
    2
}

/// One instance per layer (flags, env). Precedence: flags > env > tempo.toml > defaults.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ServerOverrides {
    pub bind: Option<IpAddr>,
    pub port: Option<u16>,
    pub db: Option<PathBuf>,
    /// env `CORETEMPO_TOKEN` only.
    pub token: Option<Token>,
    pub token_file: Option<PathBuf>,
    pub log: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedServer {
    pub bind: IpAddr,
    pub port: u16,
    pub db: PathBuf,
    /// Provisioned or generated; generated forbidden off-loopback.
    pub token: Token,
    /// true iff the token came from flags / `CORETEMPO_TOKEN` / `token_file`, not generated.
    pub token_provisioned: bool,
    /// Default "info".
    pub log: String,
}

/// Immutable for the life of a run.
#[derive(Debug, Clone)]
pub struct FrozenWorkflow {
    pub name: String,
    /// Lowercase hex sha256 of the tempo.toml bytes, followed by the schema
    /// file bytes when `[trigger.output]` uses `schema_file`.
    pub hash: String,
    pub source_path: PathBuf,
    pub ask_timeout: Duration,
    pub idle_debounce: Duration,
    pub scrollback: u32,
    pub agents: BTreeMap<AgentId, AgentConfig>,
    /// Compiled `[trigger.output]` contract, if declared.
    #[cfg(feature = "server")]
    pub output: Option<std::sync::Arc<crate::schema::OutputContract>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidationIssue {
    /// e.g. `"agents.builder.dir"`; `""` for whole-file parse errors.
    pub path: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use crate::types::config::{AgentConfig, WorkflowFile};
    use crate::types::id::AgentId;

    fn spec_example_json() -> serde_json::Value {
        serde_json::json!({
            "workflow": { "name": "core-tempo-dev" },
            "agents": {
                "planner": { "dir": "~/projects/CoreTempo", "prompt": "You are the planning agent",
                             "model": "opus", "auto_clear": true },
                "builder": { "dir": "~/projects/CoreTempo", "prompt": "You implement tasks",
                             "permission_mode": "acceptEdits" }
            }
        })
    }

    #[test]
    fn spec_example_parses_with_defaults() {
        let file: WorkflowFile = serde_json::from_value(spec_example_json()).unwrap();
        assert_eq!(file.workflow.name, "core-tempo-dev");
        assert_eq!(file.workflow.port, 4820);
        assert_eq!(file.workflow.ask_timeout_minutes, 30);
        assert!((file.workflow.idle_debounce_seconds - 2.0).abs() < f64::EPSILON);
        assert_eq!(file.workflow.db, std::path::PathBuf::from("./tempo.db"));
        let first = file.agents.keys().next().unwrap();
        assert_eq!(
            first,
            &AgentId("builder".into()),
            "roster order is lexicographic"
        );
        let builder: &AgentConfig = &file.agents[&AgentId("builder".into())];
        assert!(builder.auto_clear, "auto_clear defaults to true");
        assert_eq!(builder.permission_mode.as_deref(), Some("acceptEdits"));
    }

    #[test]
    fn unknown_fields_rejected() {
        let mut v = spec_example_json();
        v["workflow"]["bogus"] = serde_json::json!(1);
        assert!(serde_json::from_value::<WorkflowFile>(v).is_err());
    }

    #[test]
    fn tools_parse_and_default_empty() {
        let mut v = spec_example_json();
        v["agents"]["planner"]["tools"] = serde_json::json!(["pat", "jq"]);
        let file: WorkflowFile = serde_json::from_value(v).unwrap();
        assert_eq!(
            file.agents[&AgentId("planner".into())].tools,
            vec!["pat".to_string(), "jq".to_string()]
        );
        assert!(file.agents[&AgentId("builder".into())].tools.is_empty());
    }

    #[test]
    fn trigger_output_parses_from_toml() {
        let toml = r#"
            [workflow]
            name = "x"
            [agents.a]
            dir = "~/p"
            prompt = "p"
            [trigger]
            type = "webhook"
            edge = { to = "a", kind = "ask" }
            [trigger.output]
            schema = { type = "object", required = ["name"] }
            max_repairs = 3
        "#;
        let file: WorkflowFile = toml::from_str(toml).unwrap();
        let output = file.trigger.unwrap().output.unwrap();
        assert_eq!(output.max_repairs, 3);
        assert_eq!(output.schema.unwrap()["required"][0], "name");
        assert!(output.schema_file.is_none());
    }

    #[test]
    fn trigger_output_defaults_and_schema_file() {
        let toml = r#"
            [workflow]
            name = "x"
            [agents.a]
            dir = "~/p"
            prompt = "p"
            [trigger]
            type = "webhook"
            edge = { to = "a", kind = "ask" }
            [trigger.output]
            schema_file = "schemas/out.json"
        "#;
        let file: WorkflowFile = toml::from_str(toml).unwrap();
        let output = file.trigger.unwrap().output.unwrap();
        assert_eq!(output.max_repairs, 2, "default");
        assert_eq!(
            output.schema_file.unwrap(),
            std::path::PathBuf::from("schemas/out.json")
        );
    }

    #[test]
    fn trigger_without_output_still_parses() {
        let file: WorkflowFile = serde_json::from_value(spec_example_json()).unwrap();
        assert!(file.trigger.is_none());
    }
}
