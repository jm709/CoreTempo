mod client;
mod connect;
mod export;
mod session;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use coretempo_core::types::message::MessageStatus;
use coretempo_core::types::{AgentListResponse, AgentState, MessageRecord, ReportedState};
use serde_json::json;

use crate::client::Client;

#[derive(Parser)]
#[command(name = "tempo", version, about = "CoreTempo agent messaging CLI")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Ask an agent a question (a reply is expected)
    Ask {
        agent: String,
        message: String,
        /// Block until the reply arrives even when running as an agent
        #[arg(long, conflicts_with = "no_wait")]
        wait: bool,
        /// Print the message id immediately instead of blocking
        #[arg(long)]
        no_wait: bool,
    },
    /// Fire-and-forget message to an agent
    Send { agent: String, message: String },
    /// Reply to an ask you received
    Reply {
        id: String,
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=1))]
        code: u8,
        /// Inline reply body (or use --json-file for JSON payloads)
        #[arg(required_unless_present = "json_file", conflicts_with = "json_file")]
        message: Option<String>,
        /// Read the reply body from a file — avoids shell-quoting JSON, which
        /// burns schema-repair attempts on quoting mistakes
        #[arg(long)]
        json_file: Option<PathBuf>,
    },
    /// List agents: id, state, outgoing pending asks — asks the agent SENT and
    /// has not been answered, not asks waiting on it (tab-separated)
    Agents,
    /// Print a message record as JSON; --wait long-polls once (max 300 s)
    Status {
        id: String,
        #[arg(long)]
        wait: Option<u64>,
    },
    /// Report this agent's state (called by the agent's own Claude Code hooks)
    State { state: StateArg },
    /// End your loop with an agent (edge-semantics: loop edges only)
    Done { agent: String },
    /// Write tempo.toml, a systemd user unit, and a Dockerfile into <DIR>
    Export {
        dir: PathBuf,
        /// Export a batch unit for one `on_start` flow (multi-flow spec §6).
        #[arg(long)]
        flow: Option<String>,
    },
    /// Manage Claude Code sessions in the sessions daemon (spec 2026-08-27 §7)
    Session(session::SessionArgs),
}

/// `tempo state <working|idle|blocked|unblocked|refused>`; clap accepts the
/// lowercase words and the wire value comes from the core type, so CLI and
/// server never drift.
#[derive(Clone, Copy, ValueEnum)]
enum StateArg {
    Working,
    Idle,
    Blocked,
    Unblocked,
    Refused,
}

impl From<StateArg> for ReportedState {
    fn from(arg: StateArg) -> ReportedState {
        match arg {
            StateArg::Working => ReportedState::Working,
            StateArg::Idle => ReportedState::Idle,
            StateArg::Blocked => ReportedState::Blocked,
            StateArg::Unblocked => ReportedState::Unblocked,
            StateArg::Refused => ReportedState::Refused,
        }
    }
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            // Frozen exit codes: usage errors are 3; --help/--version are success.
            return if error.use_stderr() {
                ExitCode::from(3)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    match run(cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::from(3)
        }
    }
}

/// The active run's API client. Resolved per command, not once up front:
/// `tempo session` reaches the sessions daemon through its own `api.json` and
/// must work with no run at all.
fn run_client() -> anyhow::Result<Client> {
    Ok(Client::new(&connect::resolve()?))
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.cmd {
        Cmd::Session(args) => session::run(args),
        Cmd::Agents => cmd_agents(&run_client()?),
        Cmd::Send { agent, message } => cmd_send(&run_client()?, &agent, &message),
        Cmd::Reply {
            id,
            code,
            message,
            json_file,
        } => {
            let body = match (message, json_file) {
                (Some(m), None) => m,
                (None, Some(path)) => std::fs::read_to_string(&path).with_context(|| {
                    format!(
                        "could not read --json-file '{}'; write the JSON reply \
                         to a file first, then pass its path",
                        path.display()
                    )
                })?,
                (Some(_), Some(_)) | (None, None) => {
                    anyhow::bail!(
                        "give the reply body exactly one way: a positional \
                         MESSAGE or --json-file <path>"
                    )
                }
            };
            cmd_reply(&run_client()?, &id, code, &body)
        }
        Cmd::Status { id, wait } => cmd_status(&run_client()?, &id, wait),
        Cmd::State { state } => cmd_state(&run_client()?, state),
        Cmd::Done { agent } => cmd_done(&run_client()?, &agent),
        Cmd::Export { dir, flow } => export::cmd_export(&run_client()?, &dir, flow.as_deref()),
        Cmd::Ask {
            agent,
            message,
            wait,
            no_wait,
        } => cmd_ask(&run_client()?, &agent, &message, wait, no_wait),
    }
}

fn state_str(state: AgentState) -> &'static str {
    match state {
        AgentState::Starting => "starting",
        AgentState::Idle => "idle",
        AgentState::Working => "working",
        AgentState::Exited => "exited",
        AgentState::Restarting => "restarting",
    }
}

fn cmd_agents(client: &Client) -> anyhow::Result<ExitCode> {
    let value = client.get("/agents")?;
    let list: AgentListResponse = serde_json::from_value(value)?;
    for agent in list.agents {
        println!(
            "{}\t{}\t{}",
            agent.id.0,
            state_str(agent.state),
            agent.pending_asks
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_send(client: &Client, agent: &str, message: &str) -> anyhow::Result<ExitCode> {
    let value = client.post(
        "/messages",
        &json!({"to": agent, "kind": "send", "body": message}),
    )?;
    let record: MessageRecord = serde_json::from_value(value)?;
    println!("{}", record.id.0);
    Ok(ExitCode::SUCCESS)
}

fn cmd_reply(client: &Client, id: &str, code: u8, message: &str) -> anyhow::Result<ExitCode> {
    client.post(
        &format!("/messages/{id}/reply"),
        &json!({"code": code, "body": message}),
    )?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_status(client: &Client, id: &str, wait: Option<u64>) -> anyhow::Result<ExitCode> {
    let path = match wait {
        Some(secs) => format!("/messages/{id}?wait={}", secs.min(300)),
        None => format!("/messages/{id}"),
    };
    let value = client.get(&path)?;
    println!("{}", serde_json::to_string(&value)?);
    Ok(ExitCode::SUCCESS)
}

fn cmd_state(client: &Client, state: StateArg) -> anyhow::Result<ExitCode> {
    let agent = client.agent_id.as_deref().context(
        "CORETEMPO_AGENT_ID is not set; only an agent may report its own state — run \
         'tempo state' from inside an agent session (its hooks inherit the variable)",
    )?;
    let hook_stdin = drain_hook_stdin();
    let state = ReportedState::from(state);
    let payload = hook_stdin.as_deref();
    if state == ReportedState::Refused {
        return Ok(refuse_permission(client, agent, payload));
    }
    let mut body = match state {
        // A subagent's dialog fires the parent's PermissionRequest hook, and a
        // sibling helper agent fires PostToolBatch for tools it did not run, so
        // both reports carry the hook payload's agent_id to scope the clear.
        ReportedState::Blocked => json!({
            "state": state,
            "tool": hook_field(payload, "tool_name"),
            "agent_id": hook_field(payload, "agent_id"),
        }),
        ReportedState::Unblocked => json!({
            "state": state,
            "agent_id": hook_field(payload, "agent_id"),
        }),
        ReportedState::Working | ReportedState::Idle => json!({"state": state}),
        ReportedState::Refused => unreachable!("handled above"),
    };
    // Every hook payload carries the Claude Code session id; the sessions
    // daemon stores the latest one for `--resume` (spec 2026-08-27 §4).
    body["claude_session_id"] = json!(hook_field(payload, "session_id"));
    client.post(&format!("/agents/{agent}/state"), &body)?;
    Ok(ExitCode::SUCCESS)
}

/// `tempo state refused`, the `PermissionRequest` hook of an agent whose
/// `on_permission_prompt` is `deny`: answers the dialog with a deny decision on
/// stdout (Claude Code reads it only from an exit-0 hook, so this never fails)
/// and reports the refused tool to the server best-effort — the decision must
/// not depend on the API being reachable.
fn refuse_permission(client: &Client, agent: &str, payload: Option<&str>) -> ExitCode {
    let tool = hook_field(payload, "tool_name");
    let message = format!(
        "CoreTempo runs this agent unattended, so nobody can answer a permission prompt: \
         the {} call was refused. Only tools on this agent's allow list run — use one of \
         them in a single plain invocation, or reply and say what you could not do.",
        tool.as_deref().unwrap_or("tool")
    );
    println!(
        "{}",
        json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": { "behavior": "deny", "message": message },
            }
        })
    );
    let body = json!({
        "state": ReportedState::Refused,
        "tool": tool,
        "input": hook_input_summary(payload),
        "agent_id": hook_field(payload, "agent_id"),
        "claude_session_id": hook_field(payload, "session_id"),
    });
    if let Err(error) = client.post(&format!("/agents/{agent}/state"), &body) {
        eprintln!("tempo: could not report the refusal to CoreTempo: {error}");
    }
    ExitCode::SUCCESS
}

/// Claude Code pipes a JSON payload to every hook; read it to EOF so a large
/// `tool_input` never blocks the hook, and return it only when stdin is a pipe.
fn drain_hook_stdin() -> Option<String> {
    use std::io::{IsTerminal, Read};
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return None;
    }
    let mut buf = String::new();
    stdin.take(1 << 20).read_to_string(&mut buf).ok()?;
    Some(buf)
}

/// A top-level string field of a Claude Code hook payload; `None` when the
/// payload is absent, unparseable, or carries no such field. `agent_id` is
/// absent for the main session and names the subagent otherwise.
fn hook_field(payload: Option<&str>, field: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(payload?).ok()?;
    value.get(field)?.as_str().map(str::to_owned)
}

/// Longest input summary forwarded with a refusal, in bytes; the server caps
/// at the same length.
const INPUT_SUMMARY_MAX: usize = 200;

/// The one thing an operator needs from a refused call's `tool_input` to write
/// the allow rule: the Bash `command`, a file tool's `file_path`, otherwise the
/// whole input as compact JSON — capped at [`INPUT_SUMMARY_MAX`] with an
/// ellipsis, so a pasted document never lands in a log line.
fn hook_input_summary(payload: Option<&str>) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(payload?).ok()?;
    let input = value.get("tool_input")?;
    let summary = match input.get("command").or_else(|| input.get("file_path")) {
        Some(serde_json::Value::String(text)) => text.clone(),
        _ => input.to_string(),
    };
    if summary.len() <= INPUT_SUMMARY_MAX {
        return Some(summary);
    }
    let mut cut = INPUT_SUMMARY_MAX - '…'.len_utf8();
    while !summary.is_char_boundary(cut) {
        cut -= 1;
    }
    Some(format!("{}…", &summary[..cut]))
}

fn cmd_done(client: &Client, agent: &str) -> anyhow::Result<ExitCode> {
    client.agent_id.as_deref().context(
        "CORETEMPO_AGENT_ID is not set; only an agent may end its loop — run \
         'tempo done' from inside an agent session",
    )?;
    client.post(&format!("/agents/{agent}/loop-done"), &json!({}))?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_ask(
    client: &Client,
    agent: &str,
    message: &str,
    force_wait: bool,
    no_wait: bool,
) -> anyhow::Result<ExitCode> {
    let blocking = if force_wait {
        true
    } else if no_wait {
        false
    } else {
        client.agent_id.is_none() // agents get async asks; humans/scripts block
    };
    let value = client.post(
        "/messages",
        &json!({"to": agent, "kind": "ask", "body": message}),
    )?;
    let record: MessageRecord = serde_json::from_value(value)?;
    if !blocking {
        println!("{}", record.id.0);
        return Ok(ExitCode::SUCCESS);
    }
    loop {
        let value = client.get(&format!("/messages/{}?wait=30", record.id.0))?;
        let record: MessageRecord = serde_json::from_value(value)?;
        match record.status {
            MessageStatus::Replied => {
                println!("{}", record.reply.unwrap_or_default());
                let failed = record.code == Some(1);
                return Ok(ExitCode::from(u8::from(failed)));
            }
            MessageStatus::Done => return Ok(ExitCode::SUCCESS),
            MessageStatus::Failed => {
                let why = record.reason.unwrap_or_else(|| {
                    "the target exited, was restarted, or the ask timed out before a reply"
                        .to_string()
                });
                eprintln!("ask {} failed: {why}", record.id.0);
                return Ok(ExitCode::from(2));
            }
            MessageStatus::Queued | MessageStatus::Injected | MessageStatus::Working => {}
        }
    }
}

#[cfg(test)]
mod hook_field_tests {
    use super::hook_field;

    #[test]
    fn valid_payload_yields_the_tool_name() {
        let payload = r#"{"hook_event_name":"PermissionRequest","tool_name":"Read"}"#;
        assert_eq!(
            hook_field(Some(payload), "tool_name"),
            Some("Read".to_string())
        );
    }

    /// A subagent's dialog fires the parent's hook with the subagent's id; the
    /// main session's payload has no `agent_id` key at all.
    #[test]
    fn agent_id_is_read_when_present_and_none_for_the_main_session() {
        let sub = r#"{"hook_event_name":"PermissionRequest","agent_id":"a9c8","tool_name":"Bash"}"#;
        assert_eq!(
            hook_field(Some(sub), "agent_id"),
            Some("a9c8".to_string()),
            "the subagent's id"
        );
        let main = r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash"}"#;
        assert_eq!(hook_field(Some(main), "agent_id"), None);
    }

    #[test]
    fn missing_tool_name_key_yields_none() {
        let payload = r#"{"hook_event_name":"PermissionRequest"}"#;
        assert_eq!(hook_field(Some(payload), "tool_name"), None);
    }

    /// A non-string value is not a name; taking it as one would forward garbage.
    #[test]
    fn non_string_value_yields_none() {
        assert_eq!(hook_field(Some(r#"{"agent_id":17}"#), "agent_id"), None);
    }

    #[test]
    fn invalid_json_yields_none() {
        assert_eq!(hook_field(Some("not json"), "tool_name"), None);
        assert_eq!(hook_field(Some("not json"), "agent_id"), None);
    }

    #[test]
    fn no_payload_yields_none() {
        assert_eq!(hook_field(None, "tool_name"), None);
        assert_eq!(hook_field(None, "agent_id"), None);
    }
}
