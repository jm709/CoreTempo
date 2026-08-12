use std::sync::Arc;

use coretempo_core::run::Run;
use coretempo_core::trigger::Completion;
use tokio::sync::Mutex;

/// One started run plus everything the commands need that `Run` does not expose:
/// the resolved port and the workflow file path (both surfaced in `RunInfo`), the
/// bus→webview bridge task handle so `run_stop` can abort it, and — for an `on_start`
/// workflow — the kickoff watcher's task handle so `run_stop` can abort that too.
/// Without an abort, stopping a run before its kickoff settles would leave the
/// watcher running detached against a torn-down `Router`/`PtyManager` until its
/// deadline (up to `ask_timeout`), then publish a `workflow.completed` nobody hears.
pub struct ActiveRun {
    pub run: Arc<Run>,
    pub port: u16,
    pub workflow_path: String,
    pub bridge: tauri::async_runtime::JoinHandle<()>,
    pub kickoff: Option<tauri::async_runtime::JoinHandle<Completion>>,
}

/// Managed Tauri state. `active` uses a tokio Mutex because commands hold it across awaits
/// (`Run::start` / `Run::stop`). Exit codes are NOT cached here: `PtyManager::exit_code`
/// (contracts amendment 10) is the single source of truth, read via `run.pty()`.
#[derive(Default)]
pub struct AppState {
    pub active: Mutex<Option<ActiveRun>>,
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
