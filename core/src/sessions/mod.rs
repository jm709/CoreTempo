//! The session manager (spec 2026-08-27): many independent Claude Code
//! sessions across registered projects, each optionally in its own git
//! worktree, owned by the `coretempod sessions` daemon. Nothing here is a
//! workflow: no frozen prompt, no router, no auto-`/clear`.

pub mod files;
pub mod manager;
pub mod store;
pub mod trust;
pub mod worktree;

use std::path::{Path, PathBuf};

use crate::pty::PtyError;
use crate::sessions::files::SessionFilesError;
use crate::sessions::worktree::GitError;
use crate::trust::TrustError;
use crate::types::id::{AgentId, ProjectId, random_hex};
use crate::types::session::SessionState;

pub use manager::{SessionManager, SessionManagerInputs};
pub use store::{LastState, ProjectRow, SessionRow, SessionStore, SessionStoreError, WorktreeRow};

fn list<T: std::fmt::Display>(items: &[T]) -> String {
    if items.is_empty() {
        return "(none)".to_string();
    }
    items
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every failure names the fix (spec §8).
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("no session '{id}'; sessions: {}", list(roster))]
    UnknownSession { id: AgentId, roster: Vec<AgentId> },
    #[error("no project '{id}'; projects: {}", list(roster))]
    UnknownProject {
        id: ProjectId,
        roster: Vec<ProjectId>,
    },
    #[error("{} is already registered as project '{id}'", path.display())]
    ProjectExists { id: ProjectId, path: PathBuf },
    #[error(
        "project '{id}' still has {sessions} session(s); delete them first \
         (tempo session rm <id>), then forget the project"
    )]
    ProjectInUse { id: ProjectId, sessions: usize },
    #[error("{} is not a git repository: {stderr}; register a repository root", path.display())]
    NotAGitRepo { path: PathBuf, stderr: String },
    #[error(
        "cwd {} is outside project root {}; a session's cwd must be the root or a \
         directory under it",
        cwd.display(), root.display()
    )]
    CwdOutsideProject { cwd: PathBuf, root: PathBuf },
    #[error("cwd {} does not exist (in a worktree session the same relative path must \
             exist in the worktree)", cwd.display())]
    CwdMissing { cwd: PathBuf },
    #[error("session '{id}' is {state:?} — cannot {action}; valid now: {valid}")]
    WrongState {
        id: AgentId,
        state: SessionState,
        action: &'static str,
        valid: &'static str,
    },
    #[error(
        "session '{id}'s worktree {} is gone from `git worktree list`; delete the session \
         (with remove_worktree to prune it) instead of resuming",
        path.display()
    )]
    WorktreeMissing { id: AgentId, path: PathBuf },
    #[error(
        "worktree {} has uncommitted changes:\n{summary}\ncommit or stash them, or delete \
         with force = true to discard them",
        path.display()
    )]
    Dirty { path: PathBuf, summary: String },
    #[error("the sessions daemon is shutting down; start it again and retry")]
    ShuttingDown,
    #[error(transparent)]
    Trust(#[from] TrustError),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("cannot spawn claude: {0}")]
    Spawn(#[from] PtyError),
    #[error(transparent)]
    Store(#[from] SessionStoreError),
    #[error(transparent)]
    Files(#[from] SessionFilesError),
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl From<worktree::WorktreeError> for SessionError {
    fn from(error: worktree::WorktreeError) -> SessionError {
        match error {
            worktree::WorktreeError::Git(git) => SessionError::Git(git),
            worktree::WorktreeError::Dirty { path, summary } => {
                SessionError::Dirty { path, summary }
            }
        }
    }
}

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
