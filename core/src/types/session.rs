//! Session-manager wire types (spec 2026-08-27 §2, §6; contracts amendment 47).
//! Types-only: the `tempo` CLI reads `SessionsApiFile` and renders
//! `SessionView` without the `server` feature.

use serde::{Deserialize, Serialize};

use crate::time::Timestamp;
use crate::types::agent::AgentExit;
use crate::types::id::{AgentId, ProjectId, Token};

/// Live states come from the `PtyManager`; `stopped`/`exited` are the row's
/// `last_state` once the process is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Starting,
    Idle,
    Working,
    Stopped,
    Exited,
}

/// `present`: the worktree path is still in `git worktree list`; `missing`:
/// it was removed behind the daemon's back (resume 409s, delete prunes);
/// `none`: the session has no worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeStatus {
    Present,
    Missing,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: String,
    /// The `HEAD` commit the worktree was created at.
    pub base: String,
}

/// The permission dialog the session is parked on (spec §2, `blocked` flag).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedView {
    pub tool: Option<String>,
    pub since: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectView {
    pub id: ProjectId,
    /// Canonical repository root.
    pub path: String,
    pub name: String,
    pub created_at: Timestamp,
}

/// One session row plus its runtime fields (spec §2 table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionView {
    pub id: AgentId,
    pub project: ProjectId,
    pub cwd: String,
    pub worktree: Option<WorktreeInfo>,
    pub title: String,
    pub claude_session_id: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    pub isolated_config: bool,
    pub prompt: Option<String>,
    pub created_at: Timestamp,
    pub stopped_at: Option<Timestamp>,
    pub state: SessionState,
    pub blocked: Option<BlockedView>,
    pub exit: Option<AgentExit>,
    pub pty_cursor: u64,
    /// The `cwd`'s current branch; `None` when detached or not a git dir.
    pub branch: Option<String>,
    /// `git status --porcelain` line count in `cwd`; `None` when git failed.
    pub changed_files: Option<u64>,
    /// `git rev-list --count <base>..HEAD` for worktree sessions, else `None`.
    pub ahead: Option<u64>,
    pub worktree_status: WorktreeStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateProjectRequest {
    pub path: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub project: ProjectId,
    #[serde(default)]
    pub worktree: bool,
    /// A directory under the project root; for worktree sessions the same
    /// relative path is applied to the worktree.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub isolated_config: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumeResponse {
    pub session: SessionView,
    /// Whether `--resume <claude_session_id>` was passed.
    pub resumed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteSessionResponse {
    /// `true` when `session/<slug>` had commits of its own and was kept.
    pub branch_kept: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCounts {
    pub live: usize,
    pub total: usize,
}

/// `GET /v1/health` on the sessions daemon (the run `Health` needs a `run_id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionsHealth {
    pub ok: bool,
    pub sessions: SessionCounts,
}

/// `~/.coretempo/sessions/api.json`: the operator token, the bound port, and
/// the daemon's pid so a reader can tell a stale file from a live daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionsApiFile {
    pub port: u16,
    pub token: Token,
    pub pid: u32,
}

#[cfg(test)]
mod tests {
    use crate::time::Timestamp;
    use crate::types::id::{AgentId, ProjectId, Token};
    use crate::types::session::{
        BlockedView, CreateSessionRequest, SessionCounts, SessionState, SessionView,
        SessionsApiFile, SessionsHealth, WorktreeInfo, WorktreeStatus,
    };

    #[test]
    fn session_state_wire_strings() {
        for (state, wire) in [
            (SessionState::Starting, "\"starting\""),
            (SessionState::Idle, "\"idle\""),
            (SessionState::Working, "\"working\""),
            (SessionState::Stopped, "\"stopped\""),
            (SessionState::Exited, "\"exited\""),
        ] {
            assert_eq!(serde_json::to_string(&state).unwrap(), wire);
        }
        assert_eq!(
            serde_json::to_string(&WorktreeStatus::None).unwrap(),
            "\"none\""
        );
    }

    #[test]
    fn create_session_request_defaults_everything_but_project() {
        let req: CreateSessionRequest =
            serde_json::from_str(r#"{"project":"p-0a1b2c3d"}"#).unwrap();
        assert_eq!(req.project, ProjectId("p-0a1b2c3d".into()));
        assert!(!req.worktree);
        assert!(!req.isolated_config);
        assert_eq!(req.cwd, None);
        assert_eq!(req.prompt, None);
    }

    #[test]
    fn session_view_serializes_explicit_nulls_and_flat_fields() {
        let view = SessionView {
            id: AgentId("s-1f2e3d4c".into()),
            project: ProjectId("p-0a1b2c3d".into()),
            cwd: "/home/u/proj".into(),
            worktree: Some(WorktreeInfo {
                path: "/home/u/.coretempo/worktrees/p-0a1b2c3d/brisk-otter-3f1a".into(),
                branch: "session/brisk-otter-3f1a".into(),
                base: "abc123".into(),
            }),
            title: "fix the parser".into(),
            claude_session_id: None,
            model: None,
            permission_mode: None,
            isolated_config: false,
            prompt: Some("fix the parser".into()),
            created_at: Timestamp("2026-08-27T10:00:00Z".into()),
            stopped_at: None,
            state: SessionState::Idle,
            blocked: Some(BlockedView {
                tool: Some("Bash".into()),
                since: Timestamp("2026-08-27T10:00:05Z".into()),
            }),
            exit: None,
            pty_cursor: 4096,
            branch: Some("session/brisk-otter-3f1a".into()),
            changed_files: Some(2),
            ahead: Some(0),
            worktree_status: WorktreeStatus::Present,
        };
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["state"], "idle");
        assert_eq!(json["claude_session_id"], serde_json::Value::Null);
        assert_eq!(json["stopped_at"], serde_json::Value::Null);
        assert_eq!(json["blocked"]["tool"], "Bash");
        assert_eq!(json["worktree"]["branch"], "session/brisk-otter-3f1a");
        assert_eq!(json["worktree_status"], "present");
        let back: SessionView = serde_json::from_value(json).unwrap();
        assert_eq!(back, view);
    }

    #[test]
    fn health_and_api_file_shapes() {
        let health = SessionsHealth {
            ok: true,
            sessions: SessionCounts { live: 1, total: 3 },
        };
        assert_eq!(
            serde_json::to_value(&health).unwrap(),
            serde_json::json!({"ok": true, "sessions": {"live": 1, "total": 3}})
        );
        let file = SessionsApiFile {
            port: 4821,
            token: Token("ab".repeat(32)),
            pid: 4242,
        };
        let json = serde_json::to_string(&file).unwrap();
        assert!(json.contains(r#""pid":4242"#), "{json}");
        let back: SessionsApiFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, file);
    }
}
