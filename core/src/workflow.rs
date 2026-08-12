//! tempo.toml load/validate/freeze + server-setting precedence (contracts §2.6).

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sha2::Digest;

use crate::types::config::{
    AgentConfig, EdgeKind, FrozenWorkflow, ResolvedServer, ServerOverrides, TriggerConfig,
    TriggerType, ValidationIssue, WorkflowFile,
};
use crate::types::id::{AgentId, Token};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read workflow file '{path}': {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid workflow: {summary}")]
    Invalid {
        issues: Vec<ValidationIssue>,
        summary: String,
    },
    #[error("environment variable {var} is invalid: {message}")]
    BadEnv { var: String, message: String },
    #[error("token in '{path}' must be exactly 64 lowercase hex characters")]
    BadTokenFile { path: PathBuf },
    #[error(
        "refusing to bind non-loopback address {bind} without a provisioned token; \
         set CORETEMPO_TOKEN, CORETEMPO_TOKEN_FILE, or [server] token_file"
    )]
    NonLoopbackWithoutToken { bind: std::net::IpAddr },
    #[error("cannot determine home directory to expand '~' in agents.{agent}.dir")]
    NoHome { agent: AgentId },
}

/// Parse + validate tempo.toml text. All problems are reported at once so an agent
/// (or the workflow editor) can fix the file in one pass.
///
/// # Errors
///
/// Returns every [`ValidationIssue`] found — a single empty-path issue for TOML
/// parse failures, or one issue per bad field otherwise.
pub fn validate_workflow(text: &str) -> Result<WorkflowFile, Vec<ValidationIssue>> {
    let file: WorkflowFile = match toml::from_str(text) {
        Ok(f) => f,
        Err(e) => {
            return Err(vec![ValidationIssue {
                path: String::new(),
                message: e.to_string(),
            }]);
        }
    };
    let mut issues: Vec<ValidationIssue> = Vec::new();
    if file.workflow.name.trim().is_empty() {
        push(&mut issues, "workflow.name", "name must be non-empty");
    }
    if file.workflow.port == 0 {
        push(
            &mut issues,
            "workflow.port",
            "port 0 is not allowed; pick a fixed port like 4820",
        );
    }
    if file.workflow.ask_timeout_minutes == 0 {
        push(
            &mut issues,
            "workflow.ask_timeout_minutes",
            "must be >= 1 (minutes); the default is 30",
        );
    }
    let deb = file.workflow.idle_debounce_seconds;
    if !deb.is_finite() || deb <= 0.0 || deb > 60.0 {
        push(
            &mut issues,
            "workflow.idle_debounce_seconds",
            "must be a finite number in (0, 60]; the default is 2.0",
        );
    }
    if file.agents.is_empty() {
        push(
            &mut issues,
            "agents",
            "at least one [agents.<id>] section is required",
        );
    }
    for (id, agent) in &file.agents {
        if !AgentId::is_valid(&id.0) {
            issues.push(ValidationIssue {
                path: format!("agents.{}", id.0),
                message: format!(
                    "agent id '{}' must match ^[a-z0-9][a-z0-9_-]{{0,31}}$ \
                     (lowercase ascii, digits, '_', '-'; max 32 chars)",
                    id.0
                ),
            });
        }
        if agent.prompt.trim().is_empty() {
            push2(&mut issues, &id.0, "prompt", "prompt must be non-empty");
        }
        if agent.dir.as_os_str().is_empty() {
            push2(&mut issues, &id.0, "dir", "dir must be non-empty");
        }
        validate_edges(&file.agents, id, agent, &mut issues);
        validate_tools(id, agent, &mut issues);
    }
    validate_loop_cycles(&file.agents, &mut issues);
    if let Some(trigger) = &file.trigger {
        validate_trigger(&file.agents, trigger, &mut issues);
    }
    if issues.is_empty() {
        Ok(file)
    } else {
        Err(issues)
    }
}

fn push(issues: &mut Vec<ValidationIssue>, path: &str, message: &str) {
    issues.push(ValidationIssue {
        path: path.to_string(),
        message: message.to_string(),
    });
}

fn push2(issues: &mut Vec<ValidationIssue>, agent: &str, field: &str, message: &str) {
    issues.push(ValidationIssue {
        path: format!("agents.{agent}.{field}"),
        message: message.to_string(),
    });
}

fn validate_edges(
    agents: &BTreeMap<AgentId, AgentConfig>,
    id: &AgentId,
    agent: &AgentConfig,
    issues: &mut Vec<ValidationIssue>,
) {
    let mut seen_edges: std::collections::HashSet<(&AgentId, EdgeKind)> =
        std::collections::HashSet::new();
    for edge in &agent.edges {
        if edge.to == *id {
            push2(
                issues,
                &id.0,
                "edges",
                "an agent cannot have an edge to itself; delete the self-edge",
            );
        } else if !agents.contains_key(&edge.to) {
            let roster = agents
                .keys()
                .map(|a| a.0.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            issues.push(ValidationIssue {
                path: format!("agents.{}.edges", id.0),
                message: format!(
                    "edge target '{}' is not in the roster ({roster}); \
                     fix the name or add the agent",
                    edge.to.0
                ),
            });
        }
        if !seen_edges.insert((&edge.to, edge.kind)) {
            issues.push(ValidationIssue {
                path: format!("agents.{}.edges", id.0),
                message: format!(
                    "duplicate edge to '{}' with kind '{}'; each (target, kind) \
                     pair may appear once",
                    edge.to.0,
                    edge.kind.as_str()
                ),
            });
        }
        if edge.max_rounds.is_some() && edge.kind != EdgeKind::Loop {
            push2(
                issues,
                &id.0,
                "edges",
                "max_rounds only applies to kind 'loop' edges; delete it or \
                 change the edge kind",
            );
        }
        if edge.max_rounds == Some(0) {
            push2(
                issues,
                &id.0,
                "edges",
                "max_rounds must be >= 1 (rounds before the soft cap); the \
                 default is 10",
            );
        }
    }
}

fn validate_tools(id: &AgentId, agent: &AgentConfig, issues: &mut Vec<ValidationIssue>) {
    for tool in &agent.tools {
        let bare = !tool.is_empty()
            && tool
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !bare {
            issues.push(ValidationIssue {
                path: format!("agents.{}.tools", id.0),
                message: format!(
                    "tool '{tool}' must be a bare binary name (ascii letters, digits, \
                     '.', '_', '-'); it becomes the Bash({tool}:*) allowlist entry — \
                     no paths, spaces, or shell metacharacters"
                ),
            });
        }
    }
}

/// A cycle in the loop-edge subgraph is always-active and never quiescent —
/// invisible to stall detection (edge-semantics spec). Reject it at freeze.
fn validate_loop_cycles(
    agents: &BTreeMap<AgentId, AgentConfig>,
    issues: &mut Vec<ValidationIssue>,
) {
    let mut visiting: Vec<AgentId> = Vec::new();
    let mut done: std::collections::HashSet<AgentId> = std::collections::HashSet::new();
    for start in agents.keys() {
        if done.contains(start) {
            continue;
        }
        walk_loops(agents, start, &mut visiting, &mut done, issues);
    }
}

fn walk_loops(
    agents: &BTreeMap<AgentId, AgentConfig>,
    at: &AgentId,
    visiting: &mut Vec<AgentId>,
    done: &mut std::collections::HashSet<AgentId>,
    issues: &mut Vec<ValidationIssue>,
) {
    if let Some(pos) = visiting.iter().position(|v| v == at) {
        let cycle = visiting[pos..]
            .iter()
            .chain(std::iter::once(at))
            .map(|a| a.0.as_str())
            .collect::<Vec<_>>()
            .join(" → ");
        issues.push(ValidationIssue {
            path: format!("agents.{}.edges", visiting[pos].0),
            message: format!(
                "loop cycle: {cycle}; loops must be acyclic — use ask for \
                 request/response pairs"
            ),
        });
        return;
    }
    if done.contains(at) {
        return;
    }
    visiting.push(at.clone());
    if let Some(cfg) = agents.get(at) {
        for edge in &cfg.edges {
            if edge.kind == EdgeKind::Loop {
                walk_loops(agents, &edge.to, visiting, done, issues);
            }
        }
    }
    visiting.pop();
    done.insert(at.clone());
}

fn validate_trigger(
    agents: &BTreeMap<AgentId, AgentConfig>,
    trigger: &TriggerConfig,
    issues: &mut Vec<ValidationIssue>,
) {
    if !agents.contains_key(&trigger.edge.to) {
        let roster = agents
            .keys()
            .map(|a| a.0.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        issues.push(ValidationIssue {
            path: "trigger.edge".to_string(),
            message: format!(
                "trigger target '{}' is not in the roster ({roster}); \
                 fix the name or add the agent",
                trigger.edge.to.0
            ),
        });
    }
    if trigger.edge.kind == EdgeKind::Loop {
        push(
            issues,
            "trigger.edge",
            "a trigger edge cannot be kind 'loop' — loops belong to agents; \
             use 'ask' (completion = the reply) or 'send' (completion = quiescence)",
        );
    }
    match trigger.trigger_type {
        TriggerType::OnStart => {
            if trigger
                .message
                .as_deref()
                .is_none_or(|m| m.trim().is_empty())
            {
                push(
                    issues,
                    "trigger.message",
                    "an on_start trigger requires a non-empty message — it is \
                     the kickoff prompt sent when the run launches",
                );
            }
        }
        TriggerType::Webhook => {
            if trigger.message.is_some() {
                push(
                    issues,
                    "trigger.message",
                    "a webhook trigger takes no message — the HTTP request body \
                     is the kickoff message; delete this key",
                );
            }
        }
    }
    if let Some(output) = &trigger.output {
        if trigger.edge.kind == EdgeKind::Send {
            push(
                issues,
                "trigger.output",
                "an output schema requires edge kind 'ask' — a send kickoff \
                 completes on quiescence and carries no reply body to validate; \
                 change kind to 'ask' or delete [trigger.output]",
            );
        }
        if trigger.trigger_type == TriggerType::OnStart {
            push(
                issues,
                "trigger.output",
                "an output schema requires type 'webhook' — only an HTTP-origin \
                 kickoff gets the validate-and-repair loop and a caller to \
                 receive the structured result; change the type or delete \
                 [trigger.output]",
            );
        }
        match (&output.schema, &output.schema_file) {
            (Some(_), Some(_)) | (None, None) => push(
                issues,
                "trigger.output",
                "declare exactly one of 'schema' (inline JSON Schema) or \
                 'schema_file' (path relative to the tempo.toml)",
            ),
            (None, Some(path)) if path.as_os_str().is_empty() => push(
                issues,
                "trigger.output.schema_file",
                "schema_file is empty; point it at a JSON Schema file relative \
                 to the tempo.toml, or use an inline 'schema'",
            ),
            (Some(_), None) | (None, Some(_)) => {}
        }
        if output.max_repairs > 5 {
            push(
                issues,
                "trigger.output.max_repairs",
                "max_repairs must be 0..=5 (0 = validate once, never re-ask); \
                 each repair is one more agent attempt, and a larger budget \
                 hides an impossible schema instead of failing it",
            );
        }
    }
}

/// Read, validate, and freeze a tempo.toml. The returned pair is the API copy
/// (`WorkflowFile`, served by GET /v1/workflow) and the immutable run config.
///
/// # Errors
///
/// Returns [`ConfigError::Io`] when the file cannot be read or canonicalized,
/// [`ConfigError::Invalid`] when it is not UTF-8, fails validation, or holds an
/// unrepresentable duration, and [`ConfigError::NoHome`] when a `~` dir cannot
/// be expanded.
pub fn load_workflow(path: &Path) -> Result<(WorkflowFile, FrozenWorkflow), ConfigError> {
    let bytes = std::fs::read(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let text = std::str::from_utf8(&bytes).map_err(|e| ConfigError::Invalid {
        issues: vec![ValidationIssue {
            path: String::new(),
            message: e.to_string(),
        }],
        summary: "file is not valid UTF-8".to_string(),
    })?;
    let file = validate_workflow(text).map_err(|issues| {
        let summary = issues
            .iter()
            .map(|i| {
                if i.path.is_empty() {
                    i.message.clone()
                } else {
                    format!("{}: {}", i.path, i.message)
                }
            })
            .collect::<Vec<_>>()
            .join("; ");
        ConfigError::Invalid { issues, summary }
    })?;
    let source_path = std::fs::canonicalize(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let output = compile_output(file.trigger.as_ref(), &source_path)?;
    let hash = match output.as_ref().and_then(|o| o.schema_bytes.as_deref()) {
        Some(extra) => {
            let mut all = bytes.clone();
            all.extend_from_slice(extra);
            sha256_hex(&all)
        }
        None => sha256_hex(&bytes),
    };
    let mut agents = BTreeMap::new();
    for (id, cfg) in &file.agents {
        let mut cfg = cfg.clone();
        cfg.dir = expand_tilde(&cfg.dir, id)?;
        agents.insert(id.clone(), cfg);
    }
    let idle_debounce =
        Duration::try_from_secs_f64(file.workflow.idle_debounce_seconds).map_err(|e| {
            ConfigError::Invalid {
                issues: vec![ValidationIssue {
                    path: "workflow.idle_debounce_seconds".to_string(),
                    message: e.to_string(),
                }],
                summary: "idle_debounce_seconds is not a valid duration".to_string(),
            }
        })?;
    let frozen = FrozenWorkflow {
        name: file.workflow.name.clone(),
        hash,
        source_path,
        ask_timeout: Duration::from_secs(file.workflow.ask_timeout_minutes * 60),
        idle_debounce,
        scrollback: file.workflow.scrollback,
        agents,
        output: output.map(|o| o.contract),
    };
    Ok((file, frozen))
}

/// A compiled `[trigger.output]` alongside the bytes the freeze hash must cover.
struct FrozenOutput {
    contract: Arc<crate::schema::OutputContract>,
    /// `Some` when the schema came from a `schema_file`.
    schema_bytes: Option<Vec<u8>>,
}

/// Compiles `[trigger.output]` when declared. A `schema_file`'s bytes join the
/// tempo.toml bytes in the freeze hash, so editing an external schema is an
/// edit to the workflow.
fn compile_output(
    trigger: Option<&TriggerConfig>,
    source_path: &Path,
) -> Result<Option<FrozenOutput>, ConfigError> {
    let Some((trigger, output)) = trigger.and_then(|t| t.output.as_ref().map(|o| (t, o))) else {
        return Ok(None);
    };
    let mut schema_bytes: Option<Vec<u8>> = None;
    let schema = match (&output.schema, &output.schema_file) {
        (Some(inline), None) => inline.clone(),
        (None, Some(relative)) => {
            let base = source_path.parent().unwrap_or_else(|| Path::new("."));
            let absolute = base.join(relative);
            let bytes = std::fs::read(&absolute).map_err(|e| {
                invalid_output(format!(
                    "cannot read schema_file '{}': {e}; the path is resolved relative to \
                     the tempo.toml — create the file or fix the path",
                    absolute.display()
                ))
            })?;
            let value = serde_json::from_slice(&bytes).map_err(|e| {
                invalid_output(format!(
                    "schema_file '{}' is not valid JSON: {e}",
                    absolute.display()
                ))
            })?;
            schema_bytes = Some(bytes);
            value
        }
        // validate_workflow already rejected both sources and neither.
        (Some(_), Some(_)) | (None, None) => return Err(unresolved_schema_source()),
    };
    let target = trigger.edge.to.clone();
    let contract = crate::schema::OutputContract::compile(schema, target, output.max_repairs)
        .map_err(|e| invalid_output(format!("trigger.output schema: {e}")))?;
    Ok(Some(FrozenOutput {
        contract: Arc::new(contract),
        schema_bytes,
    }))
}

fn invalid_output(message: String) -> ConfigError {
    ConfigError::Invalid {
        issues: vec![ValidationIssue {
            path: "trigger.output".to_string(),
            message: message.clone(),
        }],
        summary: message,
    }
}

/// `validate_workflow` guarantees exactly one schema source, so this is a
/// `CoreTempo` bug — reported as one rather than panicking mid-load.
fn unresolved_schema_source() -> ConfigError {
    invalid_output(
        "internal: schema source not resolved despite validation; report this".to_string(),
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = sha2::Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

impl ServerOverrides {
    /// Reads the `CORETEMPO_*` env layer (contracts §7.2). Malformed values are
    /// hard errors — a typo'd port must never silently fall through to the file.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::BadEnv`] when a set variable holds a value that
    /// does not parse (IP address, port number, or 64-hex token).
    pub fn from_env() -> Result<ServerOverrides, ConfigError> {
        fn get(name: &str) -> Option<String> {
            std::env::var(name).ok().filter(|v| !v.is_empty())
        }
        let bind = get("CORETEMPO_BIND")
            .map(|v| {
                v.parse::<IpAddr>().map_err(|e| ConfigError::BadEnv {
                    var: "CORETEMPO_BIND".to_string(),
                    message: format!("'{v}' is not an IP address: {e}"),
                })
            })
            .transpose()?;
        let port = get("CORETEMPO_PORT")
            .map(|v| {
                v.parse::<u16>().map_err(|e| ConfigError::BadEnv {
                    var: "CORETEMPO_PORT".to_string(),
                    message: format!("'{v}' is not a port number (1-65535): {e}"),
                })
            })
            .transpose()?;
        let token = get("CORETEMPO_TOKEN")
            .map(|v| {
                parse_token(&v).ok_or_else(|| ConfigError::BadEnv {
                    var: "CORETEMPO_TOKEN".to_string(),
                    message: "token must be exactly 64 lowercase hex characters".to_string(),
                })
            })
            .transpose()?;
        Ok(ServerOverrides {
            bind,
            port,
            db: get("CORETEMPO_DB").map(PathBuf::from),
            token,
            token_file: get("CORETEMPO_TOKEN_FILE").map(PathBuf::from),
            log: get("CORETEMPO_LOG"),
        })
    }
}

/// Precedence: flags > env > tempo.toml > defaults — SERVER settings only; the agent
/// roster always comes from the file alone (run reproducibility). A provisioned token
/// (env `CORETEMPO_TOKEN`, else the highest-precedence `token_file`) beats generation;
/// generation is refused off-loopback.
///
/// # Errors
///
/// Returns [`ConfigError::Io`] / [`ConfigError::BadTokenFile`] when the resolved
/// token file cannot be read or is malformed, and
/// [`ConfigError::NonLoopbackWithoutToken`] when a token would have to be
/// generated for a non-loopback bind.
pub fn resolve_server(
    flags: ServerOverrides,
    env: ServerOverrides,
    file: &WorkflowFile,
) -> Result<ResolvedServer, ConfigError> {
    let bind = flags
        .bind
        .or(env.bind)
        .or(file.server.bind)
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let port = flags.port.or(env.port).unwrap_or(file.workflow.port);
    let db = flags
        .db
        .or(env.db)
        .unwrap_or_else(|| file.workflow.db.clone());
    let log = flags
        .log
        .or(env.log)
        .or_else(|| file.server.log.clone())
        .unwrap_or_else(|| "info".to_string());
    let token_file = flags
        .token_file
        .or(env.token_file)
        .or_else(|| file.server.token_file.clone());
    let provisioned = match (env.token, token_file) {
        (Some(token), _) => Some(token),
        (None, Some(path)) => Some(read_token_file(&path)?),
        (None, None) => None,
    };
    let (token, token_provisioned) = if let Some(token) = provisioned {
        (token, true)
    } else {
        if !bind.is_loopback() {
            return Err(ConfigError::NonLoopbackWithoutToken { bind });
        }
        (Token::generate(), false)
    };
    Ok(ResolvedServer {
        bind,
        port,
        db,
        token,
        token_provisioned,
        log,
    })
}

fn read_token_file(path: &Path) -> Result<Token, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_token(text.trim()).ok_or_else(|| ConfigError::BadTokenFile {
        path: path.to_path_buf(),
    })
}

fn parse_token(s: &str) -> Option<Token> {
    let ok = s.len() == 64
        && s.chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
    ok.then(|| Token(s.to_string()))
}

impl FrozenWorkflow {
    /// Full `--append-system-prompt` value for one agent: its role prompt followed by
    /// the `CoreTempo` protocol primer. Injected messages stay short because everything
    /// about tokens and replying lives here (spec §3.4). `None` if the id is unknown.
    #[must_use]
    pub fn system_prompt(&self, agent: &AgentId) -> Option<String> {
        use std::fmt::Write as _;

        let config = self.agents.get(agent)?;
        let roster = self
            .agents
            .keys()
            .filter(|id| *id != agent)
            .map(|id| id.0.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let others = if roster.is_empty() {
            "(none)".to_string()
        } else {
            roster
        };
        let mut prompt = format!(
            "{role}\n\n---\n\
             CoreTempo protocol (generated — follow exactly):\n\
             You are agent '{id}' in the CoreTempo workflow '{name}'. \
             Other agents: {others}.\n\
             A local message server routes messages between agents. \
             Use the `tempo` CLI (already on your PATH):\n\
             \n\
             - tempo ask <agent> \"<message>\" — ask a question. It prints a message id \
             (like m-a3f91c2e) and returns immediately. End your turn after asking; the \
             reply arrives later as a new prompt starting with: \
             [CoreTempo reply to <id> from <agent> — code 0|1] <reply body>\n\
             - tempo send <agent> \"<message>\" — fire-and-forget; no reply will come.\n\
             - tempo reply <id> --code 0 \"<answer>\" — answer an ask that was injected \
             into your prompt. Use --code 1 if you could not do what was asked.\n\
             \n\
             When a prompt starts with [CoreTempo <id> from <sender> — reply expected], \
             you MUST run `tempo reply <id> --code 0 '<answer>'` FIRST (or --code 1 on \
             failure), then continue with any follow-up work.\n\
             Prompts starting with [CoreTempo <id> from <sender>] without 'reply expected' \
             need no reply.\n\
             Never invent message ids; only reply to ids you actually received.\n\
             Never send acknowledgements, sign-offs, or thanks. If a message requires no \
             new work from you, do nothing and end your turn.\n\
             Agents' contexts reset between tasks: write durable state to files in your \
             working directory and keep messages short — a path plus a summary beats a \
             transcript.",
            role = config.prompt,
            id = agent.0,
            name = self.name,
        );
        if !config.edges.is_empty() {
            let mut steps = String::new();
            for (i, edge) in config.edges.iter().enumerate() {
                let n = i + 1;
                match edge.kind {
                    EdgeKind::Ask => {
                        let _ = write!(
                            steps,
                            "\n{n}. tempo ask {to} \"<your delegation>\" — then end your \
                             turn; the reply will arrive as a new prompt. Continue with \
                             the remaining steps after it does.",
                            to = edge.to.0
                        );
                    }
                    EdgeKind::Send => {
                        let _ = write!(
                            steps,
                            "\n{n}. tempo send {to} \"<your message>\"",
                            to = edge.to.0
                        );
                    }
                    EdgeKind::Loop => {
                        let _ = write!(
                            steps,
                            "\n{n}. Loop with {to}: tempo ask {to} \"<instruction>\" — each \
                             round message must be self-contained (file paths, deltas, \
                             acceptance criteria: the target's context resets between \
                             rounds). Each reply arrives as a new prompt; issue the next \
                             round with another tempo ask, or when the task is fully \
                             complete run `tempo done {to}` to end the loop. Never leave \
                             a loop open.",
                            to = edge.to.0
                        );
                    }
                }
            }
            let _ = write!(
                prompt,
                "\n\nRequired workflow steps — when your work for a prompt is complete \
                 you must perform these, in order:{steps}\nDo not skip these steps."
            );
        }
        if let Some(contract) = self.output.as_ref().filter(|c| c.target == *agent) {
            push_output_contract(&mut prompt, contract);
        }
        Some(prompt)
    }
}

/// Appends the schema block that tells the trigger's target agent how to shape
/// its HTTP-origin reply — the counterpart the router's validate-and-repair
/// loop (Task 6) enforces against.
fn push_output_contract(prompt: &mut String, contract: &crate::schema::OutputContract) {
    use std::fmt::Write as _;

    let schema = serde_json::to_string_pretty(&contract.schema).unwrap_or_default();
    let _ = write!(
        prompt,
        "\n\nOutput contract — when you reply to an ask whose sender is \
         an http caller, the reply body MUST be a single JSON value \
         matching this JSON Schema:\n{schema}\nReply with ONLY the JSON \
         — no prose, no markdown fences. The reliable way: write the \
         JSON to a file, then run `tempo reply <id> --code 0 \
         --json-file <path>`. A reply that does not match is rejected \
         with the validation errors; fix the JSON and reply again. If \
         you cannot produce this shape, reply with --code 1 and a \
         plain-text explanation instead."
    );
}

/// Expand a leading `~` / `~/` against the home directory. Paths without a
/// tilde pass through untouched. `None` only when expansion is needed but no
/// home directory can be determined.
#[must_use]
pub fn expand_home(path: &Path) -> Option<PathBuf> {
    let Some(s) = path.to_str() else {
        return Some(path.to_path_buf());
    };
    if s == "~" || s.starts_with("~/") {
        let home = std::env::home_dir()?;
        if s == "~" {
            return Some(home);
        }
        return Some(home.join(&s[2..]));
    }
    Some(path.to_path_buf())
}

fn expand_tilde(dir: &Path, agent: &AgentId) -> Result<PathBuf, ConfigError> {
    expand_home(dir).ok_or_else(|| ConfigError::NoHome {
        agent: agent.clone(),
    })
}
