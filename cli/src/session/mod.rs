//! `tempo session …` (spec 2026-08-27 §7). Talks to the sessions daemon found
//! through `<root>/api.json` — never through the `CORETEMPO_*` environment or a
//! run's `api.json`, so it works unchanged from inside a workflow agent or a
//! session.

pub mod attach;
mod b64;
mod sse;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use clap::{Args, Subcommand};
use coretempo_core::pid::pid_alive;
use coretempo_core::types::{
    DeleteSessionResponse, ProjectView, ResumeResponse, SessionState, SessionView, SessionsApiFile,
};
use serde_json::json;

use crate::client::{ApiCallError, Client};
use crate::connect::Connection;

#[derive(Args)]
pub struct SessionArgs {
    /// Sessions root (default ~/.coretempo/sessions).
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    cmd: SessionCmd,
}

#[derive(Subcommand)]
enum SessionCmd {
    /// Create a session in a project (registered on first use)
    New {
        project_path: PathBuf,
        #[arg(long)]
        worktree: bool,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        permission_mode: Option<String>,
        #[arg(long)]
        isolated_config: bool,
    },
    /// One line per session: id, project, branch, state, changed, ahead, title
    List,
    /// The session's JSON row
    Show { id: String },
    /// Stop the session's process, keeping its row and worktree
    Stop { id: String },
    /// Start the session again, resuming its Claude conversation when there is one
    Resume { id: String },
    /// Delete a stopped or exited session
    Rm {
        id: String,
        #[arg(long)]
        remove_worktree: bool,
        #[arg(long)]
        force: bool,
    },
    /// Raw terminal passthrough; Ctrl-] detaches
    Attach { id: String },
    /// List projects, or `projects rm <id>` to forget one
    Projects {
        #[command(subcommand)]
        cmd: Option<ProjectsCmd>,
    },
}

#[derive(Subcommand)]
enum ProjectsCmd {
    /// Forget a project (it must have no sessions left)
    Rm { id: String },
}

pub fn run(args: SessionArgs) -> anyhow::Result<ExitCode> {
    let api_file = api_file(args.root.as_deref())?;
    let conn = discover(&api_file)?;
    let outcome = match args.cmd {
        SessionCmd::Attach { id } => attach::run(&Client::new_untimed(&conn), &id),
        cmd => run_command(&Client::new(&conn), cmd),
    };
    outcome.map_err(|error| name_the_daemon(error, &api_file))
}

/// [`ApiCallError`]'s transport text is a run's ("is a run active?"); these
/// commands never talk to a run, so say which daemon went away and where the
/// address came from. The daemon was alive when `discover` read the pid, so
/// this is a stop mid-command, not a missing one.
fn name_the_daemon(error: anyhow::Error, api_file: &Path) -> anyhow::Error {
    let Some(ApiCallError::Transport(message)) = error.downcast_ref::<ApiCallError>() else {
        return error;
    };
    anyhow::anyhow!(
        "cannot reach the sessions daemon named by {}: {message}; it has stopped \
         since this command started — start it again with 'coretempod sessions'",
        api_file.display()
    )
}

fn api_file(root: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(root) = root {
        return Ok(root.join("api.json"));
    }
    let home =
        std::env::home_dir().context("HOME is not set; cannot locate ~/.coretempo/sessions")?;
    Ok(home.join(".coretempo/sessions/api.json"))
}

/// The daemon behind `api.json`, if the file exists and its pid is alive.
///
/// Only a missing file means "no daemon"; anything else that stops the read
/// (a permission, a directory in its place) is reported as itself, or the
/// operator would go start a daemon that is already running.
fn discover(api_file: &Path) -> anyhow::Result<Connection> {
    const NONE: &str = "no session daemon running; start it with 'coretempod sessions'";
    let text = std::fs::read_to_string(api_file).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => anyhow::anyhow!(NONE),
        _ => anyhow::Error::new(error).context(format!("cannot read {}", api_file.display())),
    })?;
    let file: SessionsApiFile =
        serde_json::from_str(&text).with_context(|| format!("malformed {}", api_file.display()))?;
    if !pid_alive(file.pid) {
        anyhow::bail!("{NONE} (stale api.json: pid {} is gone)", file.pid);
    }
    Ok(Connection {
        port: file.port,
        token: file.token.0,
        agent_id: None,
    })
}

fn run_command(client: &Client, cmd: SessionCmd) -> anyhow::Result<ExitCode> {
    match cmd {
        SessionCmd::New {
            project_path,
            worktree,
            cwd,
            title,
            prompt,
            model,
            permission_mode,
            isolated_config,
        } => {
            let project = ensure_project(client, &project_path)?;
            let value = client.post(
                "/sessions",
                &json!({
                    "project": project.id, "worktree": worktree, "cwd": cwd, "title": title,
                    "prompt": prompt, "model": model, "permission_mode": permission_mode,
                    "isolated_config": isolated_config,
                }),
            )?;
            let view: SessionView = serde_json::from_value(value)?;
            println!("{}", view.id.0);
            if let Some(worktree) = view.worktree {
                println!("{}", worktree.branch);
            }
        }
        SessionCmd::List => {
            let projects: Vec<ProjectView> = serde_json::from_value(client.get("/projects")?)?;
            let sessions: Vec<SessionView> = serde_json::from_value(client.get("/sessions")?)?;
            for session in sessions {
                let project = projects
                    .iter()
                    .find(|p| p.id == session.project)
                    .map_or("-", |p| p.name.as_str());
                println!("{}", list_line(&session, project));
            }
        }
        SessionCmd::Show { id } => {
            println!(
                "{}",
                serde_json::to_string(&client.get(&session_path(&id))?)?
            );
        }
        SessionCmd::Stop { id } => {
            client.post(&format!("{}/stop", session_path(&id)), &json!({}))?;
        }
        SessionCmd::Resume { id } => {
            let value = client.post(&format!("{}/resume", session_path(&id)), &json!({}))?;
            let resumed: ResumeResponse = serde_json::from_value(value)?;
            match (resumed.resumed, resumed.session.claude_session_id) {
                (true, Some(conversation)) => println!("resumed conversation {conversation}"),
                _ => println!("started fresh"),
            }
        }
        SessionCmd::Rm {
            id,
            remove_worktree,
            force,
        } => {
            let value = client.delete(&format!(
                "{}?remove_worktree={remove_worktree}&force={force}",
                session_path(&id)
            ))?;
            let deleted: DeleteSessionResponse = serde_json::from_value(value)?;
            if deleted.branch_kept {
                println!("branch kept");
            }
        }
        SessionCmd::Projects { cmd: None } => {
            let projects: Vec<ProjectView> = serde_json::from_value(client.get("/projects")?)?;
            for project in projects {
                println!("{}\t{}\t{}", project.id.0, project.name, project.path);
            }
        }
        SessionCmd::Projects {
            cmd: Some(ProjectsCmd::Rm { id }),
        } => {
            client.delete(&format!("/projects/{id}"))?;
        }
        SessionCmd::Attach { .. } => unreachable!("dispatched in run()"),
    }
    Ok(ExitCode::SUCCESS)
}

/// One session's route. Ids are `s-` + hex, so nothing here needs escaping.
fn session_path(id: &str) -> String {
    format!("/sessions/{id}")
}

/// The registered project for `path`'s repository, registering it if new.
/// The path is canonicalised here so `.` and symlinks match the daemon's
/// stored root (a subdirectory registers its repository root server-side).
fn ensure_project(client: &Client, path: &Path) -> anyhow::Result<ProjectView> {
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("cannot resolve project path {}", path.display()))?;
    let projects: Vec<ProjectView> = serde_json::from_value(client.get("/projects")?)?;
    // Longest prefix, not the first: a repository nested inside another
    // registered one (`/w/outer` and `/w/outer/inner`) must resolve to the
    // inner project, whatever order the daemon lists them in.
    if let Some(existing) = projects
        .into_iter()
        .filter(|p| canonical.starts_with(&p.path))
        .max_by_key(|p| p.path.len())
    {
        return Ok(existing);
    }
    let value = client.post(
        "/projects",
        &json!({"path": canonical.display().to_string()}),
    )?;
    Ok(serde_json::from_value(value)?)
}

/// `blocked` replaces `working` while the permission flag is set — the one
/// state an operator has to act on.
fn state_word(session: &SessionView) -> &'static str {
    match (session.state, session.blocked.is_some()) {
        (SessionState::Working, true) => "blocked",
        (SessionState::Working, false) => "working",
        (SessionState::Starting, _) => "starting",
        (SessionState::Idle, _) => "idle",
        (SessionState::Stopped, _) => "stopped",
        (SessionState::Exited, _) => "exited",
    }
}

fn list_line(session: &SessionView, project: &str) -> String {
    let dash = |value: Option<String>| value.unwrap_or_else(|| "-".to_string());
    format!(
        "{}\t{project}\t{}\t{}\t{}\t{}\t{}",
        session.id.0,
        dash(session.branch.clone()),
        state_word(session),
        dash(session.changed_files.map(|n| n.to_string())),
        dash(session.ahead.map(|n| n.to_string())),
        session.title
    )
}
