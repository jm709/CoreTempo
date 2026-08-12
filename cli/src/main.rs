mod client;
mod connect;
mod export;

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
    /// List agents: id, state, pending asks (tab-separated)
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
    Export { dir: PathBuf },
}

/// `tempo state <working|idle>`; clap accepts the lowercase words and the wire value
/// comes from the core type, so CLI and server never drift.
#[derive(Clone, Copy, ValueEnum)]
enum StateArg {
    Working,
    Idle,
}

impl From<StateArg> for ReportedState {
    fn from(arg: StateArg) -> ReportedState {
        match arg {
            StateArg::Working => ReportedState::Working,
            StateArg::Idle => ReportedState::Idle,
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

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let conn = connect::resolve()?;
    let client = Client::new(&conn);
    match cli.cmd {
        Cmd::Agents => cmd_agents(&client),
        Cmd::Send { agent, message } => cmd_send(&client, &agent, &message),
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
            cmd_reply(&client, &id, code, &body)
        }
        Cmd::Status { id, wait } => cmd_status(&client, &id, wait),
        Cmd::State { state } => cmd_state(&client, state),
        Cmd::Done { agent } => cmd_done(&client, &agent),
        Cmd::Export { dir } => export::cmd_export(&client, &dir),
        Cmd::Ask {
            agent,
            message,
            wait,
            no_wait,
        } => cmd_ask(&client, &agent, &message, wait, no_wait),
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
    let state = ReportedState::from(state);
    client.post(&format!("/agents/{agent}/state"), &json!({"state": state}))?;
    Ok(ExitCode::SUCCESS)
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
                eprintln!(
                    "ask {} failed: the target exited, was restarted, or the ask \
                           timed out before a reply",
                    record.id.0
                );
                return Ok(ExitCode::from(2));
            }
            MessageStatus::Queued | MessageStatus::Injected | MessageStatus::Working => {}
        }
    }
}
