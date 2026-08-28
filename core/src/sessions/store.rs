//! `sessions.db` (spec §9): projects and sessions. Its own file and schema —
//! the message `Store` has an orphan sweep and a writer thread this does not
//! need; the WAL, `user_version` and `spawn_blocking` conventions are kept.
//! The file holds bearer tokens, so it is created 0600 before SQLite opens
//! it (the `-wal`/`-shm` siblings inherit that mode).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension as _, params};

use crate::time::Timestamp;
use crate::types::agent::AgentExit;
use crate::types::id::{AgentId, ProjectId, Token};

#[derive(Debug, thiserror::Error)]
pub enum SessionStoreError {
    #[error("sessions.db: {0}")]
    Sqlite(String),
    #[error("cannot create '{path}': {source}; check the directory exists and is writable")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "{} is schema version {found}; this coretempod understands {SCHEMA_VERSION} — \
         upgrade coretempod or move the file",
        path.display()
    )]
    Schema { path: PathBuf, found: i64 },
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "map_err hands the error over by value"
)]
fn sql_err(e: rusqlite::Error) -> SessionStoreError {
    SessionStoreError::Sqlite(e.to_string())
}

const SCHEMA_VERSION: i64 = 1;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
  id         TEXT PRIMARY KEY,
  path       TEXT NOT NULL UNIQUE,
  name       TEXT NOT NULL,
  created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
  id                TEXT PRIMARY KEY,
  project           TEXT NOT NULL REFERENCES projects(id),
  cwd               TEXT NOT NULL,
  worktree_path     TEXT,
  worktree_branch   TEXT,
  base_commit       TEXT,
  title             TEXT NOT NULL,
  harness           TEXT NOT NULL DEFAULT 'claude',
  claude_session_id TEXT,
  model             TEXT,
  permission_mode   TEXT,
  isolated_config   INTEGER NOT NULL DEFAULT 0,
  prompt            TEXT,
  hook_token        TEXT NOT NULL,
  last_state        TEXT NOT NULL CHECK (last_state IN ('stopped', 'exited')),
  exit_code         INTEGER,
  exit_signal       TEXT,
  created_at        TEXT NOT NULL,
  stopped_at        TEXT
);
CREATE INDEX IF NOT EXISTS idx_sessions_project ON sessions (project);
";

/// How a non-live row left `live` (spec §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastState {
    Stopped,
    Exited,
}

impl LastState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            LastState::Stopped => "stopped",
            LastState::Exited => "exited",
        }
    }

    fn parse(s: &str) -> Result<LastState, SessionStoreError> {
        match s {
            "stopped" => Ok(LastState::Stopped),
            "exited" => Ok(LastState::Exited),
            other => Err(SessionStoreError::Sqlite(format!(
                "corrupt last_state '{other}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRow {
    pub id: ProjectId,
    pub path: PathBuf,
    pub name: String,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRow {
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub id: AgentId,
    pub project: ProjectId,
    pub cwd: PathBuf,
    pub worktree: Option<WorktreeRow>,
    pub title: String,
    pub claude_session_id: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    pub isolated_config: bool,
    pub prompt: Option<String>,
    pub hook_token: Token,
    pub last_state: LastState,
    pub exit: Option<AgentExit>,
    pub created_at: Timestamp,
    pub stopped_at: Option<Timestamp>,
}

/// Cheap to clone; every clone shares the one connection.
#[derive(Clone)]
pub struct SessionStore {
    conn: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore").finish_non_exhaustive()
    }
}

impl SessionStore {
    /// Opens (creating, 0600) `path`, enables WAL, applies the schema.
    /// Blocking; the daemon calls it under `spawn_blocking`.
    ///
    /// # Errors
    /// [`SessionStoreError::Io`] when the file cannot be created,
    /// [`SessionStoreError::Schema`] for a file a newer `coretempod` wrote,
    /// [`SessionStoreError::Sqlite`] for anything SQLite refuses.
    pub fn open(path: &Path) -> Result<SessionStore, SessionStoreError> {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(|source| SessionStoreError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        let conn = Connection::open(path).map_err(sql_err)?;
        conn.busy_timeout(BUSY_TIMEOUT).map_err(sql_err)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(sql_err)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(sql_err)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(sql_err)?;
        // Read the version before the schema batch: a file a newer coretempod
        // wrote is refused untouched. Later versions add their migrations as
        // an `if version < N { ALTER … }` ladder here, before the bump.
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(sql_err)?;
        if version > SCHEMA_VERSION {
            return Err(SessionStoreError::Schema {
                path: path.to_path_buf(),
                found: version,
            });
        }
        conn.execute_batch(SCHEMA).map_err(sql_err)?;
        if version < SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(sql_err)?;
        }
        Ok(SessionStore {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Runs `f` on the connection off the tokio workers.
    async fn blocking<T, F>(&self, f: F) -> Result<T, SessionStoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, SessionStoreError> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().unwrap_or_else(PoisonError::into_inner);
            f(&guard)
        })
        .await
        .map_err(|e| SessionStoreError::Sqlite(format!("store task failed: {e}")))?
    }

    /// # Errors
    /// `UNIQUE` on `path` surfaces as [`SessionStoreError::Sqlite`].
    pub async fn insert_project(&self, row: ProjectRow) -> Result<(), SessionStoreError> {
        self.blocking(move |conn| {
            conn.execute(
                "INSERT INTO projects (id, path, name, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![
                    row.id.0,
                    row.path.to_string_lossy().into_owned(),
                    row.name,
                    row.created_at.0
                ],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    /// # Errors
    /// [`SessionStoreError::Sqlite`].
    pub async fn list_projects(&self) -> Result<Vec<ProjectRow>, SessionStoreError> {
        self.blocking(|conn| {
            let mut stmt = conn
                .prepare("SELECT id, path, name, created_at FROM projects ORDER BY created_at, id")
                .map_err(sql_err)?;
            let rows = stmt
                .query_map([], project_row)
                .map_err(sql_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_err)?;
            Ok(rows)
        })
        .await
    }

    /// # Errors
    /// [`SessionStoreError::Sqlite`].
    pub async fn get_project(
        &self,
        id: &ProjectId,
    ) -> Result<Option<ProjectRow>, SessionStoreError> {
        let id = id.0.clone();
        self.blocking(move |conn| {
            conn.query_row(
                "SELECT id, path, name, created_at FROM projects WHERE id = ?1",
                [id],
                project_row,
            )
            .optional()
            .map_err(sql_err)
        })
        .await
    }

    /// # Errors
    /// [`SessionStoreError::Sqlite`].
    pub async fn project_by_path(
        &self,
        path: &Path,
    ) -> Result<Option<ProjectRow>, SessionStoreError> {
        let path = path.to_string_lossy().into_owned();
        self.blocking(move |conn| {
            conn.query_row(
                "SELECT id, path, name, created_at FROM projects WHERE path = ?1",
                [path],
                project_row,
            )
            .optional()
            .map_err(sql_err)
        })
        .await
    }

    /// # Errors
    /// [`SessionStoreError::Sqlite`].
    pub async fn sessions_in_project(&self, id: &ProjectId) -> Result<usize, SessionStoreError> {
        let id = id.0.clone();
        self.blocking(move |conn| {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sessions WHERE project = ?1",
                    [id],
                    |r| r.get(0),
                )
                .map_err(sql_err)?;
            Ok(usize::try_from(n).unwrap_or(0))
        })
        .await
    }

    /// `false` when no such project.
    ///
    /// # Errors
    /// [`SessionStoreError::Sqlite`] — including the foreign-key refusal when
    /// sessions still reference it (the manager checks first).
    pub async fn delete_project(&self, id: &ProjectId) -> Result<bool, SessionStoreError> {
        let id = id.0.clone();
        self.blocking(move |conn| {
            let n = conn
                .execute("DELETE FROM projects WHERE id = ?1", [id])
                .map_err(sql_err)?;
            Ok(n == 1)
        })
        .await
    }

    /// # Errors
    /// [`SessionStoreError::Sqlite`].
    pub async fn insert_session(&self, row: SessionRow) -> Result<(), SessionStoreError> {
        self.blocking(move |conn| {
            let (exit_code, exit_signal) = exit_columns(row.exit.as_ref());
            conn.execute(
                "INSERT INTO sessions (id, project, cwd, worktree_path, worktree_branch, \
                 base_commit, title, claude_session_id, model, permission_mode, \
                 isolated_config, prompt, hook_token, last_state, exit_code, exit_signal, \
                 created_at, stopped_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
                 ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
                params![
                    row.id.0,
                    row.project.0,
                    row.cwd.to_string_lossy().into_owned(),
                    row.worktree
                        .as_ref()
                        .map(|w| w.path.to_string_lossy().into_owned()),
                    row.worktree.as_ref().map(|w| w.branch.clone()),
                    row.worktree.as_ref().map(|w| w.base.clone()),
                    row.title,
                    row.claude_session_id,
                    row.model,
                    row.permission_mode,
                    i64::from(row.isolated_config),
                    row.prompt,
                    row.hook_token.0,
                    row.last_state.as_str(),
                    exit_code,
                    exit_signal,
                    row.created_at.0,
                    row.stopped_at.map(|t| t.0),
                ],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    /// # Errors
    /// [`SessionStoreError::Sqlite`].
    pub async fn get_session(&self, id: &AgentId) -> Result<Option<SessionRow>, SessionStoreError> {
        let id = id.0.clone();
        self.blocking(move |conn| {
            conn.query_row(
                &format!("{SESSION_SELECT} WHERE id = ?1"),
                [id],
                session_row,
            )
            .optional()
            .map_err(sql_err)
        })
        .await
    }

    /// # Errors
    /// [`SessionStoreError::Sqlite`].
    pub async fn list_sessions(&self) -> Result<Vec<SessionRow>, SessionStoreError> {
        self.blocking(|conn| {
            let mut stmt = conn
                .prepare(&format!("{SESSION_SELECT} ORDER BY created_at, id"))
                .map_err(sql_err)?;
            let rows = stmt
                .query_map([], session_row)
                .map_err(sql_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_err)?;
            Ok(rows)
        })
        .await
    }

    /// # Errors
    /// [`SessionStoreError::Sqlite`].
    pub async fn count_sessions(&self) -> Result<usize, SessionStoreError> {
        self.blocking(|conn| {
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
                .map_err(sql_err)?;
            Ok(usize::try_from(n).unwrap_or(0))
        })
        .await
    }

    /// Latest wins (spec §2). `false` when no such session.
    ///
    /// # Errors
    /// [`SessionStoreError::Sqlite`].
    pub async fn set_claude_session_id(
        &self,
        id: &AgentId,
        sid: String,
    ) -> Result<bool, SessionStoreError> {
        let id = id.0.clone();
        self.blocking(move |conn| {
            let n = conn
                .execute(
                    "UPDATE sessions SET claude_session_id = ?2 WHERE id = ?1",
                    params![id, sid],
                )
                .map_err(sql_err)?;
            Ok(n == 1)
        })
        .await
    }

    /// The row is live again: `stopped_at` and the exit are cleared, and
    /// `last_state` resets to `exited` — the only truthful reading of a live
    /// row that the daemon later dies without marking.
    ///
    /// # Errors
    /// [`SessionStoreError::Sqlite`].
    pub async fn mark_live(&self, id: &AgentId) -> Result<bool, SessionStoreError> {
        let id = id.0.clone();
        self.blocking(move |conn| {
            let n = conn
                .execute(
                    "UPDATE sessions SET stopped_at = NULL, exit_code = NULL, \
                     exit_signal = NULL, last_state = 'exited' WHERE id = ?1",
                    [id],
                )
                .map_err(sql_err)?;
            Ok(n == 1)
        })
        .await
    }

    /// The row left live: how (`last`), the process exit, and when.
    ///
    /// # Errors
    /// [`SessionStoreError::Sqlite`].
    pub async fn mark_left_live(
        &self,
        id: &AgentId,
        last: LastState,
        exit: Option<AgentExit>,
        at: Timestamp,
    ) -> Result<bool, SessionStoreError> {
        let id = id.0.clone();
        self.blocking(move |conn| {
            let (code, signal) = exit_columns(exit.as_ref());
            let n = conn
                .execute(
                    "UPDATE sessions SET last_state = ?2, exit_code = ?3, exit_signal = ?4, \
                     stopped_at = ?5 WHERE id = ?1",
                    params![id, last.as_str(), code, signal, at.0],
                )
                .map_err(sql_err)?;
            Ok(n == 1)
        })
        .await
    }

    /// Daemon shutdown: every row still live becomes `exited` at `at`.
    /// Returns how many rows changed.
    ///
    /// # Errors
    /// [`SessionStoreError::Sqlite`].
    pub async fn mark_all_left_live(&self, at: Timestamp) -> Result<usize, SessionStoreError> {
        self.blocking(move |conn| {
            conn.execute(
                "UPDATE sessions SET last_state = 'exited', stopped_at = ?1 \
                 WHERE stopped_at IS NULL",
                [at.0],
            )
            .map_err(sql_err)
        })
        .await
    }

    /// `false` when no such session.
    ///
    /// # Errors
    /// [`SessionStoreError::Sqlite`].
    pub async fn delete_session(&self, id: &AgentId) -> Result<bool, SessionStoreError> {
        let id = id.0.clone();
        self.blocking(move |conn| {
            let n = conn
                .execute("DELETE FROM sessions WHERE id = ?1", [id])
                .map_err(sql_err)?;
            Ok(n == 1)
        })
        .await
    }
}

const SESSION_SELECT: &str = "SELECT id, project, cwd, worktree_path, worktree_branch, \
    base_commit, title, claude_session_id, model, permission_mode, isolated_config, prompt, \
    hook_token, last_state, exit_code, exit_signal, created_at, stopped_at FROM sessions";

fn exit_columns(exit: Option<&AgentExit>) -> (Option<i64>, Option<String>) {
    match exit {
        Some(AgentExit::Code(code)) => (Some(i64::from(*code)), None),
        Some(AgentExit::Signal(signal)) => (None, Some(signal.clone())),
        None => (None, None),
    }
}

fn project_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectRow> {
    Ok(ProjectRow {
        id: ProjectId(r.get(0)?),
        path: PathBuf::from(r.get::<_, String>(1)?),
        name: r.get(2)?,
        created_at: Timestamp(r.get(3)?),
    })
}

fn worktree_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<Option<WorktreeRow>> {
    Ok(
        match (
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
        ) {
            (Some(path), Some(branch), Some(base)) => Some(WorktreeRow {
                path: PathBuf::from(path),
                branch,
                base,
            }),
            _ => None,
        },
    )
}

fn exit_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<Option<AgentExit>> {
    Ok(
        match (
            r.get::<_, Option<i64>>(14)?,
            r.get::<_, Option<String>>(15)?,
        ) {
            (Some(code), _) => Some(AgentExit::Code(i32::try_from(code).unwrap_or(i32::MAX))),
            (None, Some(signal)) => Some(AgentExit::Signal(signal)),
            (None, None) => None,
        },
    )
}

fn session_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    let worktree = worktree_of(r)?;
    let last_state = LastState::parse(&r.get::<_, String>(13)?).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(13, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let exit = exit_of(r)?;
    Ok(SessionRow {
        id: AgentId(r.get(0)?),
        project: ProjectId(r.get(1)?),
        cwd: PathBuf::from(r.get::<_, String>(2)?),
        worktree,
        title: r.get(6)?,
        claude_session_id: r.get(7)?,
        model: r.get(8)?,
        permission_mode: r.get(9)?,
        isolated_config: r.get::<_, i64>(10)? != 0,
        prompt: r.get(11)?,
        hook_token: Token(r.get(12)?),
        last_state,
        exit,
        created_at: Timestamp(r.get(16)?),
        stopped_at: r.get::<_, Option<String>>(17)?.map(Timestamp),
    })
}
