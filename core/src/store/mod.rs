//! `SQLite` persistence (spec §11): rusqlite (`bundled`), WAL mode, one dedicated writer
//! thread — never on tokio workers. All access flows through an mpsc command channel; the
//! thread owns the only `Connection`.

mod schema;

use std::fmt::Write as _;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior};
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
    /// `run_id` scopes the handle: every message it writes is stamped with it, and
    /// [`Store::pending_to_agent`]/[`Store::pending_asks`] return only rows carrying
    /// it, so concurrent runs sharing one database file never sweep each other's traffic.
    ///
    /// Concurrent runs may open one file (serve mode under `max_concurrent_runs`,
    /// plus a warm run): each `Store` is one connection and one writer thread, WAL
    /// serializes their short writes under `BUSY_TIMEOUT`, only the first-ever
    /// open pays the WAL conversion / migration write locks (later opens take the
    /// `user_version` fast path), and a shutdown checkpoint blocked by a live
    /// peer is skipped — the last store to close truncates the WAL.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sqlite`] if the database cannot be opened, configured, or
    /// migrated, or if the writer thread fails to spawn.
    pub fn open(path: &Path, run_id: RunId) -> Result<Store, StoreError> {
        let mut conn = Connection::open(path).map_err(sql_err)?;
        // Before any statement that can contend, so the rest of this function waits out a
        // concurrent cold start instead of failing on it.
        conn.busy_timeout(BUSY_TIMEOUT).map_err(sql_err)?;
        warn_unless_wal(&enable_wal(&conn, path)?, path);
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(sql_err)?;
        conn.execute_batch(SCHEMA).map_err(sql_err)?;
        migrate(&mut conn)?;
        ensure_indexes(&conn)?;
        reconcile_orphans(&conn)?;
        let (tx, rx) = mpsc::channel::<Cmd>(256);
        thread::Builder::new()
            .name("coretempo-store".to_string())
            .spawn(move || writer_loop(&conn, rx, &run_id))
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

/// How long a cold start waits out another run's contending setup work.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const BUSY_RETRY_PAUSE: Duration = Duration::from_millis(20);

/// Puts the database in WAL mode, waiting out a concurrent cold start, and returns the mode
/// the file actually ended up in.
///
/// Converting a rollback-journal database needs an exclusive lock, and `SQLite` does not
/// run the busy handler for a `journal_mode` change — the second run on a fresh or v1.0.0
/// file gets `SQLITE_BUSY` straight away — so the wait has to be explicit. Once converted the
/// mode is a persistent property of the file, so this only ever contends on first open.
///
/// The pragma's result row is the only report of what happened: a filesystem that cannot
/// give `SQLite` the shared memory the WAL index needs keeps its old journal mode and says
/// so in that row rather than failing, so the request returning `Ok` proves nothing.
fn enable_wal(conn: &Connection, path: &Path) -> Result<String, StoreError> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    loop {
        let attempt = conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0));
        let e = match attempt {
            Ok(mode) => return Ok(mode),
            Err(e) => e,
        };
        if !is_busy(&e) {
            return Err(sql_err(e));
        }
        if Instant::now() >= deadline {
            return Err(StoreError::Sqlite(format!(
                "timed out after {BUSY_TIMEOUT:?} waiting to put '{}' into WAL mode: \
                 another process holds it. Wait for that run to finish starting, or give \
                 this run its own db path.",
                path.display()
            )));
        }
        thread::sleep(BUSY_RETRY_PAUSE);
    }
}

/// Reports a refused WAL conversion. Warning, not an error: a single run works fine in
/// rollback-journal mode, so refusing to start would strand anyone whose database sits on a
/// network or FUSE mount. What degrades is the concurrency story — WAL is what lets runs
/// sharing one file read while a peer writes.
fn warn_unless_wal(mode: &str, path: &Path) {
    if mode.eq_ignore_ascii_case("wal") {
        return;
    }
    tracing::warn!(
        mode,
        path = %path.display(),
        "the database stayed in this journal mode: the filesystem refused WAL. A single run \
         is unaffected; concurrent runs sharing this file will contend for its one write lock \
         and can fail with SQLITE_BUSY. Point [server] db at a local filesystem to get WAL back."
    );
}

fn is_busy(e: &rusqlite::Error) -> bool {
    let rusqlite::Error::SqliteFailure(inner, _) = e else {
        return false;
    };
    inner.code == rusqlite::ErrorCode::DatabaseBusy
        || inner.code == rusqlite::ErrorCode::DatabaseLocked
}

/// What `PRAGMA user_version` reads on a database this build has migrated.
const SCHEMA_VERSION: i64 = 3;

/// Brings an older database up to [`SCHEMA_VERSION`].
///
/// Version 1 added `messages.run_id`: `CREATE TABLE IF NOT EXISTS` never alters
/// an existing table, so a database created by v1.0.0 lacks the column and needs
/// an explicit ALTER, while a fresh database gets it from the DDL and only needs
/// its version stamped. Version 2 drops `agent_events`, whose rows nothing ever
/// read back. Version 3 adds `reason`/`reason_code`.
///
/// Every step runs in one `BEGIN IMMEDIATE` transaction and is individually
/// idempotent: two runs cold-starting on the same legacy file would otherwise
/// both read the column as absent and the loser would fail with "duplicate
/// column name", and a database stranded part-way by an earlier crash still gets
/// what it is missing.
///
/// An already-current database takes a fast path that reads `user_version` and
/// returns without opening a write transaction — with concurrent runs sharing
/// one file this is every open but the first, and skipping the transaction
/// avoids contending for the database's single write lock.
fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    if schema_version(conn)? >= SCHEMA_VERSION {
        return Ok(());
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_err)?;
    // Re-read under the write lock: the peer this transaction may have waited
    // on could have done all of this already.
    if schema_version(&tx)? < SCHEMA_VERSION {
        if !has_column(&tx, "messages", "run_id")? {
            tx.execute_batch("ALTER TABLE messages ADD COLUMN run_id TEXT")
                .map_err(sql_err)?;
        }
        tx.execute_batch("DROP TABLE IF EXISTS agent_events")
            .map_err(sql_err)?;
        for column in ["reason", "reason_code"] {
            if !has_column(&tx, "messages", column)? {
                tx.execute_batch(&format!("ALTER TABLE messages ADD COLUMN {column} TEXT"))
                    .map_err(sql_err)?;
            }
        }
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(sql_err)?;
    }
    tx.commit().map_err(sql_err)?;
    Ok(())
}

fn schema_version(conn: &Connection) -> Result<i64, StoreError> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(sql_err)
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, StoreError> {
    conn.prepare("SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2")
        .map_err(sql_err)?
        .exists([table, column])
        .map_err(sql_err)
}

/// `run_id` indexes cannot live in `SCHEMA`: on a v1.0.0 file the column
/// exists only after `migrate`, and CREATE INDEX against a missing
/// column is an error. Idempotent; no write lock once the index exists.
fn ensure_indexes(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_messages_run_status \
         ON messages (run_id, status);",
    )
    .map_err(sql_err)
}

/// Shared predicate for both the read-only check and the write in
/// [`reconcile_orphans`].
fn orphan_predicate() -> String {
    format!(
        "status IN {NON_TERMINAL} AND (run_id IS NULL OR run_id IN \
         (SELECT run_id FROM runs WHERE stopped_at IS NOT NULL))"
    )
}

/// Startup reconciliation (issue #30). Non-terminal rows whose run is gone —
/// NULL `run_id` (pre-migration) or a run with `stopped_at` set — become
/// `failed`, so the shared history view stops accumulating phantom in-flight
/// messages. Rows of runs without `stopped_at` are left alone: under
/// concurrent serve runs they may be a live peer's, and a crashed run's rows
/// (`stopped_at` never set) are indistinguishable from those — they stay until
/// a liveness heuristic exists (deliberate; see issue #30).
///
/// Guarded by a read-only existence check before the write: WAL readers are
/// never blocked by a peer's held write transaction, but the UPDATE itself
/// would need the file's one write lock even when it matches zero rows. The
/// common case — nothing to sweep — must stay as lock-free as the
/// `migrate` fast path it runs alongside on every open under
/// `max_concurrent_runs`.
fn reconcile_orphans(conn: &Connection) -> Result<(), StoreError> {
    let predicate = orphan_predicate();
    let has_orphans: bool = conn
        .query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM messages WHERE {predicate})"),
            [],
            |r| r.get(0),
        )
        .map_err(sql_err)?;
    if !has_orphans {
        return Ok(());
    }
    let swept = conn
        .execute(
            &format!(
                "UPDATE messages SET status = 'failed', completed_at = ?1, \
                 reason_code = 'orphaned', reason = 'left non-terminal by a run that \
                 stopped without settling it; fire again' WHERE {predicate}"
            ),
            rusqlite::params![Timestamp::now().0],
        )
        .map_err(sql_err)?;
    if swept > 0 {
        tracing::info!(swept, "reconciled orphaned non-terminal messages");
    }
    Ok(())
}

fn writer_loop(conn: &Connection, mut rx: mpsc::Receiver<Cmd>, writer_run_id: &RunId) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            Cmd::InsertMessage(rec, ack) => {
                let _ = ack.send(insert_message(conn, writer_run_id, &rec));
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
                let _ = ack.send(pending_to_agent(conn, writer_run_id, &agent));
            }
            Cmd::PendingAsks(ack) => {
                let _ = ack.send(pending_asks(conn, writer_run_id));
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
            Cmd::Shutdown(ack) => {
                checkpoint_on_shutdown(conn);
                rx.close();
                let _ = ack.send(());
                return;
            }
        }
    }
}

/// Checkpoints the WAL and truncates it, unless a peer connection holds the file busy.
///
/// `PRAGMA wal_checkpoint(TRUNCATE)` reports contention through its result row's
/// `busy` column (index 0), not by returning `Err`: `SQLite` runs the pragma to
/// completion and calls that a success even when the checkpoint itself could not
/// finish because another connection holds a conflicting lock. So `Err` here means
/// the pragma call itself failed, not that it declined to make progress — a
/// genuine warning, distinct from the expected skip.
fn checkpoint_on_shutdown(conn: &Connection) {
    match conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
        r.get::<_, i64>(0)
    }) {
        Ok(1) => {
            // Another run holds the file; the last store to close will truncate
            // the WAL. Expected whenever serve-mode runs overlap.
            tracing::debug!("skipping the shutdown WAL checkpoint: file is busy");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "wal checkpoint on shutdown failed"),
    }
}

const SELECT_COLS: &str = "id, kind, from_origin, to_agent, body, status, code, reply, \
    created_at, injected_at, completed_at, reason, reason_code";

fn insert_message(conn: &Connection, run_id: &RunId, r: &MessageRecord) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO messages (id, kind, from_origin, to_agent, body, status, code, reply, \
         created_at, injected_at, completed_at, run_id, reason, reason_code) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
            run_id.0,
            r.reason,
            r.reason_code,
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn update_message(conn: &Connection, r: &MessageRecord) -> Result<bool, StoreError> {
    let n = conn
        .execute(
            "UPDATE messages SET status = ?2, code = ?3, reply = ?4, injected_at = ?5, \
             completed_at = ?6, reason = ?7, reason_code = ?8 WHERE id = ?1",
            rusqlite::params![
                r.id.0,
                r.status.as_str(),
                r.code,
                r.reply,
                r.injected_at.as_ref().map(|t| t.0.clone()),
                r.completed_at.as_ref().map(|t| t.0.clone()),
                r.reason,
                r.reason_code,
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
    reason: Option<String>,
    reason_code: Option<String>,
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
        reason: row.get(11)?,
        reason_code: row.get(12)?,
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
        reason: raw.reason,
        reason_code: raw.reason_code,
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

fn pending_to_agent(
    conn: &Connection,
    run_id: &RunId,
    agent: &AgentId,
) -> Result<Vec<MessageRecord>, StoreError> {
    let sql = format!(
        "SELECT {SELECT_COLS} FROM messages \
         WHERE to_agent = ?1 AND run_id = ?2 AND status IN {NON_TERMINAL} ORDER BY rowid"
    );
    let params: Vec<Box<dyn rusqlite::types::ToSql>> =
        vec![Box::new(agent.0.clone()), Box::new(run_id.0.clone())];
    query_records(conn, &sql, &params)
}

fn pending_asks(conn: &Connection, run_id: &RunId) -> Result<Vec<MessageRecord>, StoreError> {
    let sql = format!(
        "SELECT {SELECT_COLS} FROM messages \
         WHERE kind = 'ask' AND run_id = ?1 AND status IN {NON_TERMINAL} ORDER BY rowid"
    );
    let params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(run_id.0.clone())];
    query_records(conn, &sql, &params)
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

#[expect(
    clippy::needless_pass_by_value,
    reason = "map_err hands the error over by value"
)]
fn sql_err(e: rusqlite::Error) -> StoreError {
    StoreError::Sqlite(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::enable_wal;

    /// A file database converts, and the pragma's own result row is what says
    /// so — the request succeeding does not.
    #[test]
    fn enable_wal_reports_the_mode_a_file_database_ended_up_in() {
        let path = std::env::temp_dir().join(format!("coretempo-wal-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(enable_wal(&conn, &path).unwrap(), "wal");
        let _ = std::fs::remove_file(&path);
    }

    /// The refusal case, and the reason the mode has to be read back: an
    /// in-memory database has no WAL, and `PRAGMA journal_mode = WAL` reports
    /// the mode it kept instead of failing. A filesystem without the shared
    /// memory the WAL index needs behaves the same way.
    #[test]
    fn enable_wal_reports_a_refusal_instead_of_claiming_success() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let mode = enable_wal(&conn, std::path::Path::new(":memory:")).unwrap();
        assert_ne!(mode, "wal", "the refusal must be visible to the caller");
    }
}
