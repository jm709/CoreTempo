//! Waits for whichever signal a supervisor uses to ask the daemon to stop.
//!
//! A terminal sends SIGINT (ctrl-c). `systemctl stop` and `docker stop` send
//! SIGTERM instead — and inside a container `coretempod` runs as PID 1, where
//! the kernel's default disposition for an unhandled SIGTERM is to ignore it,
//! so `docker stop` would block for its full grace period and then SIGKILL,
//! orphaning PTY children instead of letting the daemon stop them cleanly.
//! Both signals must drive the same drain-then-exit path.

use anyhow::Context;

/// Resolves on SIGINT or SIGTERM, whichever arrives first.
pub(crate) async fn interrupted() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .context("failed to install a SIGTERM handler")?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.context("failed waiting for ctrl-c"),
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .context("failed waiting for ctrl-c")
    }
}
