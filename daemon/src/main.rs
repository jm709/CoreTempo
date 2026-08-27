//! `coretempod`: the headless `CoreTempo` workflow daemon (contracts §7.3).

mod serve;
mod signal;

use std::net::IpAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use coretempo_core::router::FlowKickoff;
use coretempo_core::run::{Run, RunOptions};
use coretempo_core::trigger::{Completion, startup_id, watch_completion, watcher_deadline};
use coretempo_core::trust::TrustPolicy;
use coretempo_core::types::config::{FrozenWorkflow, ResolvedServer, ServerOverrides, TriggerType};
use coretempo_core::types::message::Origin;
use coretempo_core::types::{AgentId, FlowName, MessageKind};
use coretempo_core::user_config::UserConfig;
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
    /// With --flow, spawn only that flow's agents; an `on_start` flow fires its
    /// kickoff at launch and the process exits 0/1 on completion.
    Run {
        /// Path to tempo.toml.
        config: PathBuf,
        /// A [flows.<name>] to run by itself (multi-flow spec §6).
        #[arg(long)]
        flow: Option<String>,
        #[command(flatten)]
        flags: ServerFlags,
    },
    /// Listen for webhook triggers, cold-starting the workflow for each one.
    /// Requires a `[flows.<name>]` section with a `type = "webhook"` trigger.
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
    // Before anything a supervisor can observe (#66): once the API answers, a
    // signal must be caught, not take the process down on the default disposition.
    let interrupt = signal::Interrupt::install()?;
    let (mode, config, flow, flags) = match cmd {
        Cmd::Run {
            config,
            flow,
            flags,
        } => (Mode::Run, config, flow, flags),
        Cmd::Serve { config, flags } => (Mode::Serve, config, None, flags),
    };
    let env = ServerOverrides::from_env().context("invalid CORETEMPO_* environment variable")?;
    let (file, frozen) = load_workflow(&config)
        .with_context(|| format!("failed to load workflow '{}'", config.display()))?;
    let server =
        resolve_server(flags.into(), env, &file).context("cannot resolve server settings")?;
    // After resolve_server, not before: a bad server setting is the more
    // useful error, and cli.rs's bind test runs with the developer's own HOME.
    let user = UserConfig::load_default().context("invalid ~/.coretempo/config.toml")?;
    let trust = TrustPolicy::resolve(user.trust_agent_dirs, file.server.trust_agent_dirs);
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(&server.log).unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    match mode {
        Mode::Run => {
            let (frozen, kickoff) = match &flow {
                None => (frozen, None),
                Some(name) => select_flow(&frozen, name)?,
            };
            run_until_interrupt(frozen, server, kickoff, trust, interrupt).await
        }
        Mode::Serve => {
            serve::serve(serve::ServeInputs {
                config,
                file,
                frozen,
                server,
                trust,
                interrupt,
            })
            .await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// A launch kickoff for an `on_start` flow: its name, target, message kind, and
/// the static message the trigger config carries.
type Kickoff = (FlowName, AgentId, MessageKind, String);

/// Derives the subset run for `--flow <name>` and, for an `on_start` flow, its
/// launch kickoff. A webhook flow yields no kickoff: the run is warm with that
/// flow's trigger route armed.
fn select_flow(
    frozen: &FrozenWorkflow,
    name: &str,
) -> anyhow::Result<(FrozenWorkflow, Option<Kickoff>)> {
    let flow_name = FlowName(name.to_string());
    let Some(derived) = frozen.for_flow(&flow_name) else {
        let declared = if frozen.flows.is_empty() {
            "(none — this workflow declares no [flows.<name>] sections)".to_string()
        } else {
            frozen
                .flows
                .keys()
                .map(|f| f.0.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        anyhow::bail!(
            "no flow named '{name}' in '{}'; declared flows: {declared} — pick one \
             of them, or drop --flow for a warm whole-pool run",
            frozen.source_path.display()
        );
    };
    // The derived subset holds exactly one flow.
    let kickoff = derived.flows.get(&flow_name).and_then(|flow| {
        (flow.trigger_type == TriggerType::OnStart).then(|| {
            (
                flow_name.clone(),
                flow.edge.to.clone(),
                flow.edge.kind.message_kind(),
                // Validation guarantees an on_start trigger carries a message.
                flow.message.clone().unwrap_or_default(),
            )
        })
    });
    Ok((derived, kickoff))
}

/// Run mode: agents up until ctrl-c, or — for an `on_start` flow's kickoff —
/// until it completes. Exit code mirrors the outcome: `0` on a successful reply
/// or quiescence, `1` on any other completion, `130` on ctrl-c.
async fn run_until_interrupt(
    frozen: FrozenWorkflow,
    server: ResolvedServer,
    kickoff_plan: Option<Kickoff>,
    trust: TrustPolicy,
    mut interrupt: signal::Interrupt,
) -> anyhow::Result<ExitCode> {
    tracing::info!(workflow = %frozen.name, port = server.port, bind = %server.bind, "starting");
    let run = Run::start_with(
        frozen,
        server,
        RunOptions {
            trust,
            ..RunOptions::default()
        },
    )
    .await
    .context("failed to start run")?;

    let Some((flow_name, to, kind, message)) = kickoff_plan else {
        interrupt.wait().await?;
        return stop_after_interrupt(&run, 0).await;
    };

    // Multi-flow spec §5: the batch holds its members' locks for the kickoff's
    // whole life, so a warm webhook trigger sharing an `exclusive` agent
    // serializes behind it rather than interleaving prompts in that agent's one
    // live session. Raced against the signal — acquisition waits as long as the
    // contending flow takes, and ctrl-c must still land.
    let _guards = tokio::select! {
        guards = run.lock_flow(&flow_name) => guards,
        result = interrupt.wait() => {
            result?;
            return stop_after_interrupt(&run, 130).await;
        }
    };

    // The header names the flow (amendment 31) for the same reason a webhook
    // kickoff's does: one rule for the agent — a kickoff always says which flow
    // it belongs to, so an unlabelled ask is never a flow kickoff and never owes
    // an output contract.
    let kickoff = run
        .router()
        .create_kickoff(FlowKickoff {
            flow: flow_name.clone(),
            from: Origin::Trigger(startup_id()),
            to,
            kind,
            body: message,
        })
        .await
        .context("failed to fire the on_start kickoff")?;
    // Built immediately after creation: the watcher's deadline runs from here.
    // `coretempod run` has no trigger hub to register an id with. The watcher is
    // scoped to the on_start flow, so a webhook flow's output contract (and its
    // agents) stay out of this batch's completion. The fallback arm is
    // unreachable — the frozen workflow carries the flow the file declared —
    // but keeps the code total without an `unwrap`.
    let inputs = run
        .watch_inputs_for_flow(
            &flow_name,
            watcher_deadline(run.workflow().ask_timeout),
            None,
        )
        .unwrap_or_else(|| run.watch_inputs(watcher_deadline(run.workflow().ask_timeout), None));
    tokio::select! {
        completion = watch_completion(inputs, kickoff) => {
            run.stop().await.context("failed to stop run cleanly")?;
            let code = match completion {
                Completion::Replied { code: 0, .. } | Completion::Quiesced => 0u8,
                Completion::Replied { .. } | Completion::Failed { .. } | Completion::Timeout => 1u8,
            };
            Ok(ExitCode::from(code))
        }
        result = interrupt.wait() => {
            result?;
            stop_after_interrupt(&run, 130).await
        }
    }
}

/// Tears the run down after a signal and reports `code` as the process exit.
async fn stop_after_interrupt(run: &Run, code: u8) -> anyhow::Result<ExitCode> {
    tracing::info!("interrupt received; stopping run");
    run.stop().await.context("failed to stop run cleanly")?;
    Ok(ExitCode::from(code))
}
