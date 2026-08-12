//! `coretempod`: the headless `CoreTempo` workflow daemon (contracts §7.3).

mod serve;
mod signal;

use std::net::IpAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use coretempo_core::run::Run;
use coretempo_core::trigger::{
    Completion, startup_id, startup_kickoff, watch_completion, watcher_deadline,
};
use coretempo_core::types::config::{
    FrozenWorkflow, ResolvedServer, ServerOverrides, WorkflowFile,
};
use coretempo_core::types::message::Origin;
use coretempo_core::workflow::{load_workflow, resolve_server};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "coretempod",
    version,
    about = "CoreTempo headless workflow daemon"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// The server overrides both subcommands accept; flags beat env beats tempo.toml.
#[derive(Args)]
struct ServerFlags {
    #[arg(long)]
    bind: Option<IpAddr>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    db: Option<PathBuf>,
    #[arg(long)]
    token_file: Option<PathBuf>,
}

impl From<ServerFlags> for ServerOverrides {
    fn from(flags: ServerFlags) -> ServerOverrides {
        ServerOverrides {
            bind: flags.bind,
            port: flags.port,
            db: flags.db,
            token: None,
            token_file: flags.token_file,
            log: None,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a workflow from a tempo.toml until interrupted (ctrl-c / SIGINT).
    Run {
        /// Path to tempo.toml.
        config: PathBuf,
        #[command(flatten)]
        flags: ServerFlags,
    },
    /// Listen for webhook triggers, cold-starting the workflow for each one.
    /// Requires a `[trigger] type = "webhook"` section.
    Serve {
        /// Path to tempo.toml.
        config: PathBuf,
        #[command(flatten)]
        flags: ServerFlags,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Run,
    Serve,
}

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    let Cli { cmd } = Cli::parse();
    let (mode, config, flags) = match cmd {
        Cmd::Run { config, flags } => (Mode::Run, config, flags),
        Cmd::Serve { config, flags } => (Mode::Serve, config, flags),
    };
    let env = ServerOverrides::from_env().context("invalid CORETEMPO_* environment variable")?;
    let (file, frozen) = load_workflow(&config)
        .with_context(|| format!("failed to load workflow '{}'", config.display()))?;
    let server =
        resolve_server(flags.into(), env, &file).context("cannot resolve server settings")?;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(&server.log).unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    match mode {
        Mode::Run => run_until_interrupt(file, frozen, server).await,
        Mode::Serve => {
            serve::serve(serve::ServeInputs {
                config,
                file,
                frozen,
                server,
            })
            .await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Run mode: agents up until ctrl-c, or — for an `on_start` workflow — until the
/// kickoff completes. Exit code mirrors the outcome: `0` on a successful reply
/// or quiescence, `1` on any other completion, `130` on ctrl-c.
async fn run_until_interrupt(
    file: WorkflowFile,
    frozen: FrozenWorkflow,
    server: ResolvedServer,
) -> anyhow::Result<ExitCode> {
    tracing::info!(workflow = %frozen.name, port = server.port, bind = %server.bind, "starting");
    let run = Run::start(frozen, server)
        .await
        .context("failed to start run")?;

    let Some((to, kind, message)) = startup_kickoff(&file) else {
        signal::interrupted().await?;
        tracing::info!("interrupt received; stopping run");
        run.stop().await.context("failed to stop run cleanly")?;
        return Ok(ExitCode::SUCCESS);
    };

    let kickoff = run
        .router()
        .create_message(Origin::Http(startup_id()), to, kind, message)
        .await
        .context("failed to fire the on_start kickoff")?;
    // Built immediately after creation: the watcher's deadline runs from here.
    // `coretempod run` has no trigger hub to register an id with.
    let inputs = run.watch_inputs(watcher_deadline(run.workflow().ask_timeout), None);
    tokio::select! {
        completion = watch_completion(inputs, kickoff) => {
            run.stop().await.context("failed to stop run cleanly")?;
            let code = match completion {
                Completion::Replied { code: 0, .. } | Completion::Quiesced => 0u8,
                Completion::Replied { .. } | Completion::Failed { .. } | Completion::Timeout => 1u8,
            };
            Ok(ExitCode::from(code))
        }
        result = signal::interrupted() => {
            result?;
            tracing::info!("interrupt received; stopping run");
            run.stop().await.context("failed to stop run cleanly")?;
            Ok(ExitCode::from(130u8))
        }
    }
}
