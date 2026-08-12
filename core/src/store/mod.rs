//! `SQLite` persistence (spec §11): rusqlite (`bundled`), WAL mode, one dedicated writer
//! thread — never on tokio workers. All access flows through an mpsc command channel; the
//! thread owns the only `Connection`.

mod schema;

use std::fmt::Write as _;
use std::path::Path;
use std::thread;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension as _};
use tokio::sync::{mpsc, oneshot};

use crate::router::{DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT, MessageFilter};
use crate::store::schema::SCHEMA;
use crate::time::Timestamp;
use crate::types::id::{AgentId, MessageId, RunId};
use crate::types::message::{MessageKind, MessageRecord, MessageStatus, Origin};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(String),
    #[error("store is closed (writer thread exited); restart the run")]
    Closed,
    #[error("corrupt row for message '{0}': {1}")]
    CorruptRow(String, String),
}

/// One `agent_events` row: a lifecycle/state transition kept for roster/feed history.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentEventRecord {
    pub agent: AgentId,
    pub state: String,
    pub exit_code: Option<i32>,
    pub ts: Timestamp,
}

enum Cmd {
    InsertMessage(Box<MessageRecord>, oneshot::Sender<Result<(), StoreError>>),
    UpdateMessage(
        Box<MessageRecord>,
        oneshot::Sender<Result<bool, StoreError>>,
    ),
    GetMessage(
        MessageId,
        oneshot::Sender<Result<Option<MessageRecord>, StoreError>>,
    ),
    ListMessages(
        Box<MessageFilter>,
        oneshot::Sender<Result<Vec<MessageRecord>, StoreError>>,
    ),
    PendingToAgent(
        AgentId,
        oneshot::Sender<Result<Vec<MessageRecord>, StoreError>>,
    ),
    PendingAsks(oneshot::Sender<Result<Vec<MessageRecord>, StoreError>>),
    InsertRun {
        run_id: RunId,
        name: String,
        hash: String,
        started_at: Timestamp,
        ack: oneshot::Sender<Result<(), StoreError>>,
    },
    MarkRunStopped {
        run_id: RunId,
        stopped_at: Timestamp,
        ack: oneshot::Sender<Result<(), StoreError>>,
    },
    InsertAgentEvent {
        agent: AgentId,
        state: String,
        exit_code: Option<i32>,
        ts: Timestamp,
        ack: oneshot::Sender<Result<(), StoreError>>,
    },
    ListAgentEvents(
        u32,
        oneshot::Sender<Result<Vec<AgentEventRecord>, StoreError>>,
    ),
    Shutdown(oneshot::Sender<()>),
}

/// Handle to the store; cheap to clone (all clones share the writer thread).
#[derive(Clone)]
pub struct Store {
    tx: mpsc::Sender<Cmd>,
}

impl Store {
    /// Opens (creating if absent) the database at `path`, enables WAL, applies the schema,
    /// and spawns the dedicated writer thread. Blocking; call once at startup.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] if the database cannot be opened, configured, or
    /// migrated, or if the writer thread fails to spawn.
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        let conn = Connection::open(path).map_err(sql_err)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(sql_err)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(sql_err)?;
        conn.busy_timeout(Duration::from_secs(5)).map_err(sql_err)?;
        conn.execute_batch(SCHEMA).map_err(sql_err)?;
        let (tx, rx) = mpsc::channel::<Cmd>(256);
        thread::Builder::new()
            .name("coretempo-store".to_string())
            .spawn(move || writer_loop(&conn, rx))
            .map_err(|e| StoreError::Sqlite(format!("failed to spawn store thread: {e}")))?;
        Ok(Store { tx })
    }

    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] if the INSERT fails, [`StoreError::Closed`] after
    /// [`Store::shutdown`].
    pub async fn insert_message(&self, record: &MessageRecord) -> Result<(), StoreError> {
        let rec = Box::new(record.clone());
        self.send_cmd(|ack| Cmd::InsertMessage(rec, ack)).await
    }

    /// Full-record UPDATE by id. Returns `false` if no row matched.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] if the UPDATE fails, [`StoreError::Closed`] after
    /// [`Store::shutdown`].
    pub async fn update_message(&self, record: &MessageRecord) -> Result<bool, StoreError> {
        let rec = Box::new(record.clone());
        self.send_cmd(|ack| Cmd::UpdateMessage(rec, ack)).await
    }

    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] if the SELECT fails, [`StoreError::CorruptRow`] if a
    /// stored value fails to parse, [`StoreError::Closed`] after [`Store::shutdown`].
    pub async fn get_message(&self, id: &MessageId) -> Result<Option<MessageRecord>, StoreError> {
        let id = id.clone();
        self.send_cmd(|ack| Cmd::GetMessage(id, ack)).await
    }

    /// Traffic log, newest first (`created_at DESC`, ties by insertion order DESC).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] if the SELECT fails, [`StoreError::CorruptRow`] if a
    /// stored value fails to parse, [`StoreError::Closed`] after [`Store::shutdown`].
    pub async fn list_messages(
        &self,
        filter: &MessageFilter,
    ) -> Result<Vec<MessageRecord>, StoreError> {
        let f = Box::new(filter.clone());
        self.send_cmd(|ack| Cmd::ListMessages(f, ack)).await
    }

    /// Non-terminal messages addressed TO `agent` (restart failure sweep), oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] if the SELECT fails, [`StoreError::CorruptRow`] if a
    /// stored value fails to parse, [`StoreError::Closed`] after [`Store::shutdown`].
    pub async fn pending_to_agent(
        &self,
        agent: &AgentId,
    ) -> Result<Vec<MessageRecord>, StoreError> {
        let agent = agent.clone();
        self.send_cmd(|ack| Cmd::PendingToAgent(agent, ack)).await
    }

    /// All non-terminal asks (TTL sweep + restart suppression), oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] if the SELECT fails, [`StoreError::CorruptRow`] if a
    /// stored value fails to parse, [`StoreError::Closed`] after [`Store::shutdown`].
    pub async fn pending_asks(&self) -> Result<Vec<MessageRecord>, StoreError> {
        self.send_cmd(Cmd::PendingAsks).await
    }

    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] if the INSERT fails, [`StoreError::Closed`] after
    /// [`Store::shutdown`].
    pub async fn insert_run(
        &self,
        run_id: &RunId,
        name: &str,
        hash: &str,
        started_at: &Timestamp,
    ) -> Result<(), StoreError> {
        let (run_id, name) = (run_id.clone(), name.to_string());
        let (hash, started_at) = (hash.to_string(), started_at.clone());
        self.send_cmd(|ack| Cmd::InsertRun {
            run_id,
            name,
            hash,
            started_at,
            ack,
        })
        .await
    }

    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] if the UPDATE fails, [`StoreError::Closed`] after
    /// [`Store::shutdown`].
    pub async fn mark_run_stopped(
        &self,
        run_id: &RunId,
        stopped_at: &Timestamp,
    ) -> Result<(), StoreError> {
        let (run_id, stopped_at) = (run_id.clone(), stopped_at.clone());
        self.send_cmd(|ack| Cmd::MarkRunStopped {
            run_id,
            stopped_at,
            ack,
        })
        .await
    }

    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] if the INSERT fails, [`StoreError::Closed`] after
    /// [`Store::shutdown`].
    pub async fn insert_agent_event(
        &self,
        agent: &AgentId,
        state: &str,
        exit_code: Option<i32>,
        ts: &Timestamp,
    ) -> Result<(), StoreError> {
        let (agent, state, ts) = (agent.clone(), state.to_string(), ts.clone());
        self.send_cmd(|ack| Cmd::InsertAgentEvent {
            agent,
            state,
            exit_code,
            ts,
            ack,
        })
        .await
    }

    /// Most recent `limit` agent events, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] if the SELECT fails, [`StoreError::Closed`] after
    /// [`Store::shutdown`].
    pub async fn list_agent_events(&self, limit: u32) -> Result<Vec<AgentEventRecord>, StoreError> {
        self.send_cmd(|ack| Cmd::ListAgentEvents(limit, ack)).await
    }

    /// Checkpoints the WAL and stops the writer thread. Subsequent calls fail `Closed`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Closed`] if the writer thread has already stopped.
    pub async fn shutdown(&self) -> Result<(), StoreError> {
        let (ack, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Shutdown(ack))
            .await
            .map_err(|_| StoreError::Closed)?;
        rx.await.map_err(|_| StoreError::Closed)
    }

    async fn send_cmd<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T, StoreError>>) -> Cmd,
    ) -> Result<T, StoreError> {
        let (ack, rx) = oneshot::channel();
        self.tx
            .send(build(ack))
            .await
            .map_err(|_| StoreError::Closed)?;
        rx.await.map_err(|_| StoreError::Closed)?
    }
}

fn writer_loop(conn: &Connection, mut rx: mpsc::Receiver<Cmd>) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            Cmd::InsertMessage(rec, ack) => {
                let _ = ack.send(insert_message(conn, &rec));
            }
            Cmd::UpdateMessage(rec, ack) => {
                let _ = ack.send(update_message(conn, &rec));
            }
            Cmd::GetMessage(id, ack) => {
                let _ = ack.send(get_message(conn, &id));
            }
            Cmd::ListMessages(f, ack) => {
                let _ = ack.send(list_messages(conn, &f));
            }
            Cmd::PendingToAgent(agent, ack) => {
                let _ = ack.send(pending_to_agent(conn, &agent));
            }
            Cmd::PendingAsks(ack) => {
                let _ = ack.send(pending_asks(conn));
            }
            Cmd::InsertRun {
                run_id,
                name,
                hash,
                started_at,
                ack,
            } => {
                let _ = ack.send(insert_run(conn, &run_id, &name, &hash, &started_at));
            }
            Cmd::MarkRunStopped {
                run_id,
                stopped_at,
                ack,
            } => {
                let _ = ack.send(mark_run_stopped(conn, &run_id, &stopped_at));
            }
            Cmd::InsertAgentEvent {
                agent,
                state,
                exit_code,
                ts,
                ack,
            } => {
                let _ = ack.send(insert_agent_event(conn, &agent, &state, exit_code, &ts));
            }
            Cmd::ListAgentEvents(limit, ack) => {
                let _ = ack.send(list_agent_events(conn, limit));
            }
            Cmd::Shutdown(ack) => {
                if let Err(e) = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);") {
                    tracing::warn!(error = %e, "wal checkpoint on shutdown failed");
                }
                rx.close();
                let _ = ack.send(());
                return;
            }
        }
    }
}

const SELECT_COLS: &str = "id, kind, from_origin, to_agent, body, status, code, reply, \
    created_at, injected_at, completed_at";

fn insert_message(conn: &Connection, r: &MessageRecord) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO messages (id, kind, from_origin, to_agent, body, status, code, reply, \
         created_at, injected_at, completed_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            r.id.0,
            r.kind.as_str(),
            r.from.to_string(),
            r.to.0,
            r.body,
            r.status.as_str(),
            r.code,
            r.reply,
            r.created_at.0,
            r.injected_at.as_ref().map(|t| t.0.clone()),
            r.completed_at.as_ref().map(|t| t.0.clone()),
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn update_message(conn: &Connection, r: &MessageRecord) -> Result<bool, StoreError> {
    let n = conn
        .execute(
            "UPDATE messages SET status = ?2, code = ?3, reply = ?4, injected_at = ?5, \
             completed_at = ?6 WHERE id = ?1",
            rusqlite::params![
                r.id.0,
                r.status.as_str(),
                r.code,
                r.reply,
                r.injected_at.as_ref().map(|t| t.0.clone()),
                r.completed_at.as_ref().map(|t| t.0.clone()),
            ],
        )
        .map_err(sql_err)?;
    Ok(n > 0)
}

fn get_message(conn: &Connection, id: &MessageId) -> Result<Option<MessageRecord>, StoreError> {
    let sql = format!("SELECT {SELECT_COLS} FROM messages WHERE id = ?1");
    let raw = conn
        .query_row(&sql, rusqlite::params![id.0], read_row)
        .optional()
        .map_err(sql_err)?;
    raw.map(to_record).transpose()
}

struct RawMessageRow {
    id: String,
    kind: String,
    from_origin: String,
    to_agent: String,
    body: String,
    status: String,
    code: Option<u8>,
    reply: Option<String>,
    created_at: String,
    injected_at: Option<String>,
    completed_at: Option<String>,
}

fn read_row(row: &rusqlite::Row<'_>) -> Result<RawMessageRow, rusqlite::Error> {
    Ok(RawMessageRow {
        id: row.get(0)?,
        kind: row.get(1)?,
        from_origin: row.get(2)?,
        to_agent: row.get(3)?,
        body: row.get(4)?,
        status: row.get(5)?,
        code: row.get(6)?,
        reply: row.get(7)?,
        created_at: row.get(8)?,
        injected_at: row.get(9)?,
        completed_at: row.get(10)?,
    })
}

fn to_record(raw: RawMessageRow) -> Result<MessageRecord, StoreError> {
    let kind = raw
        .kind
        .parse::<MessageKind>()
        .map_err(|e| StoreError::CorruptRow(raw.id.clone(), e.to_string()))?;
    let status = raw
        .status
        .parse::<MessageStatus>()
        .map_err(|e| StoreError::CorruptRow(raw.id.clone(), e.to_string()))?;
    let from = raw
        .from_origin
        .parse::<Origin>()
        .map_err(|e| StoreError::CorruptRow(raw.id.clone(), e.to_string()))?;
    Ok(MessageRecord {
        id: MessageId(raw.id),
        kind,
        from,
        to: AgentId(raw.to_agent),
        body: raw.body,
        status,
        code: raw.code,
        reply: raw.reply,
        created_at: Timestamp(raw.created_at),
        injected_at: raw.injected_at.map(Timestamp),
        completed_at: raw.completed_at.map(Timestamp),
    })
}

const NON_TERMINAL: &str = "('queued', 'injected', 'working')";

fn list_messages(conn: &Connection, f: &MessageFilter) -> Result<Vec<MessageRecord>, StoreError> {
    let mut sql = format!("SELECT {SELECT_COLS} FROM messages");
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(to) = &f.to {
        params.push(Box::new(to.0.clone()));
        clauses.push(format!("to_agent = ?{}", params.len()));
    }
    if let Some(from) = &f.from {
        params.push(Box::new(from.to_string()));
        clauses.push(format!("from_origin = ?{}", params.len()));
    }
    if let Some(status) = &f.status {
        params.push(Box::new(status.as_str()));
        clauses.push(format!("status = ?{}", params.len()));
    }
    if let Some(kind) = &f.kind {
        params.push(Box::new(kind.as_str()));
        clauses.push(format!("kind = ?{}", params.len()));
    }
    if let Some(since) = &f.since {
        params.push(Box::new(since.0.clone()));
        clauses.push(format!("created_at >= ?{}", params.len()));
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    let limit = if f.limit == 0 {
        DEFAULT_LIST_LIMIT
    } else {
        f.limit.min(MAX_LIST_LIMIT)
    };
    params.push(Box::new(limit));
    let _ = write!(
        sql,
        " ORDER BY created_at DESC, rowid DESC LIMIT ?{}",
        params.len()
    );
    query_records(conn, &sql, &params)
}

fn pending_to_agent(conn: &Connection, agent: &AgentId) -> Result<Vec<MessageRecord>, StoreError> {
    let sql = format!(
        "SELECT {SELECT_COLS} FROM messages \
         WHERE to_agent = ?1 AND status IN {NON_TERMINAL} ORDER BY rowid"
    );
    let params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(agent.0.clone())];
    query_records(conn, &sql, &params)
}

fn pending_asks(conn: &Connection) -> Result<Vec<MessageRecord>, StoreError> {
    let sql = format!(
        "SELECT {SELECT_COLS} FROM messages \
         WHERE kind = 'ask' AND status IN {NON_TERMINAL} ORDER BY rowid"
    );
    query_records(conn, &sql, &[])
}

fn query_records(
    conn: &Connection,
    sql: &str,
    params: &[Box<dyn rusqlite::types::ToSql>],
) -> Result<Vec<MessageRecord>, StoreError> {
    let mut stmt = conn.prepare(sql).map_err(sql_err)?;
    let refs: Vec<&dyn rusqlite::types::ToSql> =
        params.iter().map(std::convert::AsRef::as_ref).collect();
    let rows = stmt.query_map(refs.as_slice(), read_row).map_err(sql_err)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(to_record(row.map_err(sql_err)?)?);
    }
    Ok(out)
}

fn insert_run(
    conn: &Connection,
    run_id: &RunId,
    name: &str,
    hash: &str,
    started_at: &Timestamp,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO runs (run_id, workflow_name, workflow_hash, started_at) \
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![run_id.0, name, hash, started_at.0],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn mark_run_stopped(
    conn: &Connection,
    run_id: &RunId,
    stopped_at: &Timestamp,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE runs SET stopped_at = ?2 WHERE run_id = ?1",
        rusqlite::params![run_id.0, stopped_at.0],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn insert_agent_event(
    conn: &Connection,
    agent: &AgentId,
    state: &str,
    exit_code: Option<i32>,
    ts: &Timestamp,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO agent_events (agent, state, exit_code, ts) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![agent.0, state, exit_code, ts.0],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn list_agent_events(conn: &Connection, limit: u32) -> Result<Vec<AgentEventRecord>, StoreError> {
    let mut stmt = conn
        .prepare(
            "SELECT agent, state, exit_code, ts FROM agent_events \
             ORDER BY id DESC LIMIT ?1",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(rusqlite::params![limit], |row| {
            Ok(AgentEventRecord {
                agent: AgentId(row.get(0)?),
                state: row.get(1)?,
                exit_code: row.get(2)?,
                ts: Timestamp(row.get(3)?),
            })
        })
        .map_err(sql_err)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(sql_err)?);
    }
    Ok(out)
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "map_err hands the error over by value"
)]
fn sql_err(e: rusqlite::Error) -> StoreError {
    StoreError::Sqlite(e.to_string())
}
