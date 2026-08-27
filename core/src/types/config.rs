//! tempo.toml wire structs and frozen-run config (contracts §2.5/§2.6).
//! Load/validate/freeze behavior lives in `crate::workflow` (workflow-run plan).

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::types::id::{AgentId, FlowName, Token};
use crate::types::message::MessageKind;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowFile {
    pub workflow: WorkflowSection,
    #[serde(default)]
    pub server: ServerSection,
    /// ≥1 required (enforced by `crate::workflow`); roster order = lexicographic.
    pub agents: BTreeMap<AgentId, AgentConfig>,
    /// Named sub-workflows (multi-flow spec §1). Zero flows is valid: a plain
    /// warm workflow with nothing self-starting.
    #[serde(default)]
    pub flows: BTreeMap<FlowName, FlowConfig>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    /// Default 127.0.0.1.
    pub bind: Option<IpAddr>,
    pub token_file: Option<PathBuf>,
    /// tracing `EnvFilter` string.
    pub log: Option<String>,
    /// Serve mode's ceiling on simultaneously live runs (multi-flow spec §1).
    /// Default 2, validated 1..=16. File-only: it does not join the
    /// flags > env > file resolution — it is workflow-shaped, not
    /// deployment-shaped.
    #[serde(default = "d_max_runs")]
    pub max_concurrent_runs: usize,
    /// May `CoreTempo` mark each agent's git root trusted in `~/.claude.json`
    /// (spec 2026-08-17 §1)? Default false: an untrusted root refuses the
    /// run naming it. Either this or `trust_agent_dirs` in
    /// `~/.coretempo/config.toml` grants. File-only, like `max_concurrent_runs`.
    #[serde(default)]
    pub trust_agent_dirs: bool,
}

impl Default for ServerSection {
    fn default() -> ServerSection {
        ServerSection {
            bind: None,
            token_file: None,
            log: None,
            max_concurrent_runs: d_max_runs(),
            trust_agent_dirs: false,
        }
    }
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
    /// Claude Code permission rules appended verbatim to the generated
    /// settings' `permissions.allow`, after the `Bash(...)` entries — for
    /// tools `tools` cannot express (`WebSearch`, `Read(//data/**)`, …).
    /// `CoreTempo` does not validate rule syntax; Claude Code ignores a bad
    /// rule, so the dialog still fires and the `PermissionRequest` hook
    /// signal shows it.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Names of MCP servers this agent may use (spec 2026-08-17 §2). Resolved
    /// at load against the user's own declarations — `~/.claude.json`
    /// top-level `mcpServers`, then its `projects["<dir>"].mcpServers`, then
    /// `~/.mcp.json`, then `<dir>/.mcp.json`; first match wins — and handed
    /// to the agent as `--mcp-config`. Every agent spawns with
    /// `--strict-mcp-config`, so an empty list means no MCP servers at all
    /// and no discovery prompt.
    #[serde(default)]
    pub mcp: Vec<String>,
    /// May this agent's sessions overlap across runs? `exclusive` (default)
    /// means one live session anywhere: every flow that includes this agent
    /// serializes against it. `shared` asserts the agent does not mutate its
    /// working directory, so duplicates are safe (multi-flow spec §9).
    #[serde(default)]
    pub concurrency: AgentConcurrency,
    /// Spawn with `CLAUDE_CONFIG_DIR` pointed at a CoreTempo-managed directory
    /// (spec 2026-08-24 §2): the agent sees nothing of the operator's
    /// `~/.claude` beyond its login — no global `CLAUDE.md`, skills, plugins,
    /// hooks, shared auto-memory or default model. Default false: inherit
    /// all of it, as every agent did before.
    #[serde(default)]
    pub isolated_config: bool,
    /// Skill directories (each holding `SKILL.md`) linked into the managed
    /// config dir's `skills/` (spec 2026-08-24 §1). Relative paths resolve
    /// against the directory holding `tempo.toml`, `~` expands; the frozen
    /// value is absolute. The directory's basename is the skill name.
    /// Requires `isolated_config`.
    #[serde(default)]
    pub skills: Vec<PathBuf>,
    /// What the agent's `PermissionRequest` hook does with a tool call no allow
    /// rule covers. `deny` (default): the hook answers the dialog itself with a
    /// refusal and a message naming the fix, so the call fails inside the turn
    /// and the agent carries on — nothing attended can answer a headless
    /// agent's prompt. `wait`: leave the dialog up for a human, as before;
    /// `CoreTempo` reports it (`agent.blocked`) and fails owed asks after 90 s.
    #[serde(default)]
    pub on_permission_prompt: PermissionPrompt,
}

impl AgentConfig {
    /// An agent with only the required fields set; every optional field takes
    /// the same default serde applies when the key is absent from `tempo.toml`.
    #[must_use]
    pub fn new(dir: PathBuf, prompt: impl Into<String>) -> AgentConfig {
        AgentConfig {
            dir,
            prompt: prompt.into(),
            model: None,
            permission_mode: None,
            auto_clear: true,
            edges: Vec::new(),
            tools: Vec::new(),
            allow: Vec::new(),
            mcp: Vec::new(),
            concurrency: AgentConcurrency::Exclusive,
            isolated_config: false,
            skills: Vec::new(),
            on_permission_prompt: PermissionPrompt::Deny,
        }
    }
}

/// How an agent's `PermissionRequest` hook answers a permission dialog
/// (contracts amendment 44). Lives on the agent: whether a human is expected
/// to be watching is a property of the workflow, not of the run mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionPrompt {
    #[default]
    Deny,
    Wait,
}

/// Whether an agent's sessions may overlap across runs (multi-flow spec §1).
/// Lives on the agent — next to the prompt that determines whether it mutates
/// anything — so it cannot be declared inconsistently across flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentConcurrency {
    #[default]
    Exclusive,
    Shared,
}

impl AgentConcurrency {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AgentConcurrency::Exclusive => "exclusive",
            AgentConcurrency::Shared => "shared",
        }
    }
}

/// One named sub-workflow (multi-flow spec §1): an agent subset, its trigger
/// edge, and an optional output contract for the webhook reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlowConfig {
    /// Non-empty subset of the `[agents.*]` pool; must be edge-closed
    /// (enforced by `crate::workflow::validate_workflow`).
    pub agents: Vec<AgentId>,
    /// The flow's trigger. Its `output` lives on the flow, not in here.
    pub trigger: TriggerConfig,
    /// The webhook reply's structured-output contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputConfig>,
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
}

/// Structured-output contract declaration (design 2026-08-06). Exactly one of
/// `schema`/`schema_file` — enforced by `crate::workflow::validate_output_shape`.
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
fn d_max_runs() -> usize {
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

/// One flow, resolved at freeze (multi-flow spec §2).
#[cfg(feature = "server")]
#[derive(Debug, Clone)]
pub struct FrozenFlow {
    /// The resolved member set. `BTreeSet` iterates in sorted order — the
    /// lock-acquisition order the serve scheduler needs.
    pub members: std::collections::BTreeSet<AgentId>,
    pub trigger_type: TriggerType,
    pub edge: Edge,
    /// `on_start` only: the static kickoff message.
    pub message: Option<String>,
    /// Compiled `[flows.<name>.output]` contract, if declared.
    pub output: Option<std::sync::Arc<crate::schema::OutputContract>>,
}

/// One agent's resolved MCP servers: name → the definition exactly as the user
/// declared it (`command`/`args`/`env`, or `type`/`url`, …). Passed through to
/// Claude Code verbatim; hashed in canonical form (`crate::mcp::canonical_bytes`).
pub type McpServers = BTreeMap<String, serde_json::Value>;

/// Immutable for the life of a run.
#[derive(Debug, Clone)]
pub struct FrozenWorkflow {
    pub name: String,
    /// Lowercase hex sha256 of the tempo.toml bytes, then every flow's
    /// `schema_file` bytes in flow-name order, then every agent's resolved MCP
    /// servers (canonical JSON, agent-id order) — each blob length-framed.
    pub hash: String,
    pub source_path: PathBuf,
    pub ask_timeout: Duration,
    pub idle_debounce: Duration,
    pub scrollback: u32,
    pub agents: BTreeMap<AgentId, AgentConfig>,
    /// Resolved MCP servers for each agent that declares `mcp = [...]`
    /// (spec 2026-08-17 §2); agents with none are absent. Written per run
    /// as `agent-mcp-<id>.json` and passed as `--mcp-config`.
    pub mcp_servers: BTreeMap<AgentId, McpServers>,
    /// Frozen flows by name (multi-flow spec §2); empty when the file
    /// declares none.
    #[cfg(feature = "server")]
    pub flows: BTreeMap<FlowName, FrozenFlow>,
}

#[cfg(feature = "server")]
impl FrozenWorkflow {
    /// Derived subset for one flow's run (multi-flow spec §2): `agents`
    /// narrowed to the flow's member set and `flows` to just this flow, so
    /// everything that iterates the roster — prompt composition, settings
    /// files, `spawn_all`, message-target validation, the quiescence roster —
    /// sees only the members. `hash` and `source_path` are unchanged: the
    /// subset is the same frozen workflow. `None` for an undeclared name.
    #[must_use]
    pub fn for_flow(&self, name: &FlowName) -> Option<FrozenWorkflow> {
        let flow = self.flows.get(name)?;
        let agents = self
            .agents
            .iter()
            .filter(|(id, _)| flow.members.contains(id))
            .map(|(id, cfg)| (id.clone(), cfg.clone()))
            .collect();
        Some(FrozenWorkflow {
            agents,
            flows: BTreeMap::from([(name.clone(), flow.clone())]),
            ..self.clone()
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidationIssue {
    /// e.g. `"agents.builder.dir"`; `""` for whole-file parse errors.
    pub path: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::types::config::{AgentConcurrency, AgentConfig, TriggerType, WorkflowFile};
    use crate::types::id::{AgentId, FlowName};

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
    fn flow_output_schema_file_parses_with_default_repairs() {
        let toml = r#"
            [workflow]
            name = "x"
            [agents.a]
            dir = "~/p"
            prompt = "p"
            [flows.hook]
            agents = ["a"]
            trigger = { type = "webhook", edge = { to = "a", kind = "ask" } }
            [flows.hook.output]
            schema_file = "schemas/out.json"
        "#;
        let file: WorkflowFile = toml::from_str(toml).unwrap();
        let output = file.flows[&FlowName("hook".into())].output.clone().unwrap();
        assert_eq!(output.max_repairs, 2, "default");
        assert_eq!(
            output.schema_file.unwrap(),
            std::path::PathBuf::from("schemas/out.json")
        );
    }

    #[test]
    fn flows_and_concurrency_parse_from_toml() {
        let toml = r#"
            [workflow]
            name = "x"
            [server]
            max_concurrent_runs = 4
            [agents.smm]
            dir = "~/p"
            prompt = "p"
            [agents.kb]
            dir = "~/p"
            prompt = "p"
            concurrency = "shared"
            [flows.post]
            agents = ["smm"]
            trigger = { type = "webhook", edge = { to = "smm", kind = "ask" } }
            [flows.classify]
            agents = ["kb"]
            trigger = { type = "webhook", edge = { to = "kb", kind = "ask" } }
            [flows.classify.output]
            schema = { type = "object" }
        "#;
        let file: WorkflowFile = toml::from_str(toml).unwrap();
        assert_eq!(file.server.max_concurrent_runs, 4);
        assert_eq!(
            file.agents[&AgentId("smm".into())].concurrency,
            AgentConcurrency::Exclusive,
            "concurrency defaults to exclusive"
        );
        assert_eq!(
            file.agents[&AgentId("kb".into())].concurrency,
            AgentConcurrency::Shared
        );
        let post = &file.flows[&FlowName("post".into())];
        assert_eq!(post.agents, vec![AgentId("smm".into())]);
        assert_eq!(post.trigger.trigger_type, TriggerType::Webhook);
        assert!(post.output.is_none());
        let classify = &file.flows[&FlowName("classify".into())];
        assert_eq!(classify.output.as_ref().unwrap().max_repairs, 2, "default");
    }

    #[test]
    fn bad_concurrency_value_names_both_variants() {
        let toml = r#"
            [workflow]
            name = "x"
            [agents.a]
            dir = "~/p"
            prompt = "p"
            concurrency = "parallel"
        "#;
        let err = toml::from_str::<WorkflowFile>(toml)
            .unwrap_err()
            .to_string();
        assert!(err.contains("exclusive") && err.contains("shared"), "{err}");
    }

    #[test]
    fn flows_and_max_concurrent_runs_default() {
        let file: WorkflowFile = serde_json::from_value(spec_example_json()).unwrap();
        assert!(file.flows.is_empty(), "no [flows] is a valid, empty map");
        assert_eq!(file.server.max_concurrent_runs, 2);
    }

    #[test]
    fn new_matches_serde_defaults() {
        let parsed: AgentConfig =
            toml::from_str("dir = \"/w\"\nprompt = \"p\"\n").expect("minimal agent parses");
        assert_eq!(AgentConfig::new(PathBuf::from("/w"), "p"), parsed);
    }

    #[test]
    fn mcp_names_parse_and_default_empty() {
        let parsed: AgentConfig =
            toml::from_str("dir = \"/w\"\nprompt = \"p\"\nmcp = [\"context7\"]\n").expect("parses");
        assert_eq!(parsed.mcp, vec!["context7".to_string()]);
        assert!(AgentConfig::new(PathBuf::from("/w"), "p").mcp.is_empty());
    }
}
