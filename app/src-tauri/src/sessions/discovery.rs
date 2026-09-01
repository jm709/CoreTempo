//! Finding the sessions daemon, and starting one when there isn't one
//! (spec 2026-08-27 §6 steps 1–3).
//!
//! A health answer is the only proof a daemon is up. The pid in `api.json` is
//! never consulted: it is there for a human reading the file, and trusting it
//! would mean believing a stale file over a live socket.

use std::path::{Path, PathBuf};
use std::time::Duration;

use coretempo_core::types::session::SessionsApiFile;

use crate::commands::CmdError;
use crate::sessions::client::DaemonClient;

/// How long one health probe may take before the daemon counts as not there.
const PROBE_TIMEOUT: Duration = Duration::from_millis(800);

/// How long to wait between polls for a freshly written `api.json`.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Everything [`Discovery::connect`] needs, injectable so tests can point it at
/// a scratch directory and a fake daemon.
pub struct Discovery {
    /// Directory holding `api.json` (production: `~/.coretempo/sessions`).
    pub sessions_dir: PathBuf,
    /// The `coretempod` binary (production: `$CORETEMPOD_BIN`, else next to the
    /// running app binary).
    pub bin: PathBuf,
    /// Total budget for spawn + fresh `api.json` + health (production: 10 s).
    pub deadline: Duration,
}

/// The daemon's `api.json`, or `None` when it is missing, unreadable, or
/// half-written — all of which mean the same thing here: no daemon to talk to.
#[must_use]
pub fn read_api_file(dir: &Path) -> Option<SessionsApiFile> {
    let text = std::fs::read_to_string(dir.join("api.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// `coretempod` beside the running app binary, which is how a bundled install
/// lays the two out.
fn bin_beside_app() -> Result<PathBuf, CmdError> {
    let exe = std::env::current_exe()
        .map_err(|err| CmdError::new("no_exe", format!("cannot locate the app binary: {err}")))?;
    Ok(exe.with_file_name("coretempod"))
}

impl Discovery {
    /// The real paths: `~/.coretempo/sessions`, and `coretempod` beside the app
    /// binary unless `CORETEMPOD_BIN` names one.
    ///
    /// # Errors
    /// `no_home` when `HOME` is unset, `no_exe` when the app cannot locate its
    /// own binary to look beside.
    pub fn production() -> Result<Discovery, CmdError> {
        let home = std::env::var_os("HOME").ok_or_else(|| {
            CmdError::new(
                "no_home",
                "HOME is not set; cannot find ~/.coretempo/sessions",
            )
        })?;
        let bin = match std::env::var_os("CORETEMPOD_BIN") {
            Some(path) => PathBuf::from(path),
            None => bin_beside_app()?,
        };
        Ok(Discovery {
            sessions_dir: PathBuf::from(home).join(".coretempo").join("sessions"),
            bin,
            deadline: Duration::from_secs(10),
        })
    }

    /// A client for whichever daemon answers, starting one if none does.
    ///
    /// # Errors
    /// `spawn_failed` when the binary could not be run at all, or
    /// `daemon_unreachable` when nothing answered health within `deadline`.
    pub async fn connect(&self) -> Result<DaemonClient, CmdError> {
        if let Some(client) = self.probe_current().await {
            return Ok(client);
        }
        self.spawn_detached()?;
        let start = tokio::time::Instant::now();
        while start.elapsed() < self.deadline {
            tokio::time::sleep(POLL_INTERVAL).await;
            if let Some(client) = self.probe_current().await {
                return Ok(client);
            }
        }
        Err(CmdError::new(
            "daemon_unreachable",
            format!(
                "the sessions daemon did not come up within {:?}; start it by hand with \
                 'coretempod sessions' and check {}/daemon.log",
                self.deadline,
                self.sessions_dir.display()
            ),
        ))
    }

    /// The daemon `api.json` currently names, if it answers health.
    async fn probe_current(&self) -> Option<DaemonClient> {
        let file = read_api_file(&self.sessions_dir)?;
        let client = DaemonClient::new(file.port, file.token.0);
        match tokio::time::timeout(PROBE_TIMEOUT, client.health()).await {
            Ok(Ok(_)) => Some(client),
            Ok(Err(_)) | Err(_) => None,
        }
    }

    /// Starts `coretempod sessions` in its own process group, so it survives the
    /// desktop exiting, with stdio released (it logs to `daemon.log`).
    ///
    /// Deliberately not `tauri-plugin-shell`'s sidecar API, which kills its
    /// children when the app exits (spec §6).
    ///
    /// A non-zero exit is *not* checked for: losing the race to a daemon that
    /// booted first is indistinguishable from winning it, and both end with a
    /// live daemon named by `api.json`.
    fn spawn_detached(&self) -> Result<(), CmdError> {
        use std::os::unix::process::CommandExt;

        std::process::Command::new(&self.bin)
            .arg("sessions")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|err| {
                CmdError::new(
                    "spawn_failed",
                    format!("could not start {} sessions: {err}", self.bin.display()),
                )
            })?;
        Ok(())
    }
}
