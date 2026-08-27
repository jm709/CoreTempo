//! The session manager (spec 2026-08-27): many independent Claude Code
//! sessions across registered projects, each optionally in its own git
//! worktree, owned by the `coretempod sessions` daemon. Nothing here is a
//! workflow: no frozen prompt, no router, no auto-`/clear`.

pub mod store;

use std::path::{Path, PathBuf};

use crate::types::id::{AgentId, random_hex};

pub use store::{LastState, ProjectRow, SessionRow, SessionStore, SessionStoreError, WorktreeRow};

/// Where the daemon keeps everything (spec §3, §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionsRoot {
    /// `api.json`, `sessions.db`, `sessions.lock`, `daemon.log`, `<id>/`.
    pub dir: PathBuf,
    /// `<project-id>/<slug>` worktrees live here — outside every repository.
    pub worktrees: PathBuf,
}

impl SessionsRoot {
    /// `~/.coretempo/sessions` and `~/.coretempo/worktrees`.
    #[must_use]
    pub fn from_home(home: &Path) -> SessionsRoot {
        let base = home.join(".coretempo");
        SessionsRoot {
            dir: base.join("sessions"),
            worktrees: base.join("worktrees"),
        }
    }

    /// An explicit root (`--root <dir>`): worktrees go under it.
    #[must_use]
    pub fn at(dir: PathBuf) -> SessionsRoot {
        SessionsRoot {
            worktrees: dir.join("worktrees"),
            dir,
        }
    }

    #[must_use]
    pub fn api_file(&self) -> PathBuf {
        self.dir.join("api.json")
    }
    #[must_use]
    pub fn db(&self) -> PathBuf {
        self.dir.join("sessions.db")
    }
    #[must_use]
    pub fn lock_file(&self) -> PathBuf {
        self.dir.join("sessions.lock")
    }
    #[must_use]
    pub fn log_file(&self) -> PathBuf {
        self.dir.join("daemon.log")
    }
}

/// `s-` + 8 lowercase hex: a valid agent id, so the `PtyManager` and the
/// hook route address sessions unchanged.
#[must_use]
pub fn session_id() -> AgentId {
    AgentId(format!("s-{}", random_hex(4)))
}
