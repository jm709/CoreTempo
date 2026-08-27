//! Waits for whichever signal a supervisor uses to ask the daemon to stop.
//!
//! A terminal sends SIGINT (ctrl-c). `systemctl stop` and `docker stop` send
//! SIGTERM instead — and inside a container `coretempod` runs as PID 1, where
//! the kernel's default disposition for an unhandled SIGTERM is to ignore it,
//! so `docker stop` would block for its full grace period and then SIGKILL,
//! orphaning PTY children instead of letting the daemon stop them cleanly.
//! Both signals must drive the same drain-then-exit path.
//!
//! Registration is a synchronous side effect of [`Interrupt::install`], not of
//! the first `.await`: tokio installs a handler when the `Signal` is created,
//! so an `async fn` that created one lazily left the process on the default
//! disposition until it was first polled. `/v1/health` answers before
//! `Run::start_with` returns, so a supervisor keyed on health could kill the
//! process outright in that window (#66). Install before anything observable.

use anyhow::Context;

/// Armed SIGINT/SIGTERM handlers. A signal that arrives between
/// [`Interrupt::install`] and the first [`Interrupt::wait`] is buffered by
/// tokio and reported by that first wait.
pub(crate) struct Interrupt {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl Interrupt {
    /// Registers both handlers now. Must run inside a tokio runtime.
    ///
    /// # Errors
    /// If either handler cannot be installed.
    pub(crate) fn install() -> anyhow::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                interrupt: signal(SignalKind::interrupt())
                    .context("failed to install a SIGINT handler")?,
                terminate: signal(SignalKind::terminate())
                    .context("failed to install a SIGTERM handler")?,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    /// Resolves on SIGINT or SIGTERM, whichever arrives first.
    ///
    /// # Errors
    /// If the runtime's signal driver is gone (non-unix ctrl-c only).
    pub(crate) async fn wait(&mut self) -> anyhow::Result<()> {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.interrupt.recv() => Ok(()),
                _ = self.terminate.recv() => Ok(()),
            }
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c()
                .await
                .context("failed waiting for ctrl-c")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Interrupt;
    use std::process::Command;
    use std::time::Duration;

    /// The bug behind #66: a signal that lands between `install` and the first
    /// `wait` — during `Run::start_with`, after `/v1/health` already answers —
    /// must be caught and reported, not kill the process. With a lazy handler
    /// this test does not fail; the test binary dies of SIGTERM.
    #[tokio::test]
    async fn a_signal_before_the_first_wait_is_caught() {
        let mut interrupt = Interrupt::install().expect("install handlers");
        let sent = Command::new("kill")
            .arg("-TERM")
            .arg(std::process::id().to_string())
            .status()
            .expect("run kill");
        assert!(sent.success(), "could not signal the test process");
        tokio::time::timeout(Duration::from_secs(5), interrupt.wait())
            .await
            .expect("the early SIGTERM was never observed")
            .expect("wait reports an error");
    }
}
