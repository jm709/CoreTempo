use std::collections::BTreeMap;
use std::sync::Arc;

use coretempo_core::run::Run;
use coretempo_core::trigger::Completion;
use coretempo_core::types::id::FlowName;
use tokio::sync::Mutex;

/// One started run plus everything the commands need that `Run` does not expose:
/// the resolved port and the workflow file path (both surfaced in `RunInfo`), the
/// bus→webview bridge task handle so `run_stop` can abort it, and one kickoff
/// watcher task handle per fired flow so `run_stop` can abort every one of them.
/// A watcher does end by itself eventually — at its deadline, or when a member's
/// exit event or a closed state channel settles it — but it holds an `Arc<Run>`,
/// so `stop()` never closes the bus underneath it. An ask kickoff in particular
/// keeps waiting against a torn-down `Router`/`PtyManager` for as long as
/// `ask_timeout` before publishing a `workflow.completed` nobody hears; aborting
/// is what makes teardown immediate.
pub struct ActiveRun {
    pub run: Arc<Run>,
    pub port: u16,
    pub workflow_path: String,
    pub bridge: tauri::async_runtime::JoinHandle<()>,
    pub kickoffs: BTreeMap<FlowName, tauri::async_runtime::JoinHandle<Completion>>,
}

/// Managed Tauri state. `active` uses a tokio Mutex because commands hold it across awaits
/// (`Run::start` / `Run::stop`). Exit facts are NOT cached here: `PtyManager::exit`
/// (contracts amendment 10) is the single source of truth, read via `run.pty()`.
#[derive(Default)]
pub struct AppState {
    pub active: Mutex<Option<ActiveRun>>,
    /// `~/.coretempo/config.toml` `trust_agent_dirs`, read once at launch.
    pub trust_grant: bool,
}

impl AppState {
    #[must_use]
    pub fn with_trust(trust_grant: bool) -> AppState {
        AppState {
            active: Mutex::new(None),
            trust_grant,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn app_state_starts_with_no_run() {
        let state = AppState::default();
        assert!(state.active.lock().await.is_none());
    }
}
