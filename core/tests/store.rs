#![expect(clippy::unwrap_used, reason = "tests assert on known-good values")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

use coretempo_core::store::Store;
use coretempo_core::time::Timestamp;
use coretempo_core::types::id::{AgentId, MessageId, RunId};
use coretempo_core::types::message::{MessageKind, MessageRecord, MessageStatus, Origin};

static DB_N: AtomicU64 = AtomicU64::new(0);

fn temp_db() -> PathBuf {
    let n = DB_N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("coretempo-store-{}-{n}.db", std::process::id()))
}

fn run(id: &str) -> RunId {
    RunId(id.to_string())
}

/// Opens a store for the tests that do not care which run owns their rows.
fn open_store(path: &Path) -> Store {
    Store::open(path, run("r-11111111")).unwrap()
}

fn queued_ask(id: &str) -> MessageRecord {
    MessageRecord {
        id: MessageId(id.to_string()),
        kind: MessageKind::Ask,
        from: Origin::Agent(AgentId("planner".to_string())),
        to: AgentId("builder".to_string()),
        body: "Is the schema migration done?".to_string(),
        status: MessageStatus::Queued,
        code: None,
        reply: None,
        created_at: Timestamp("2026-08-01T17:03:11Z".to_string()),
        injected_at: None,
        completed_at: None,
        reason: None,
        reason_code: None,
    }
}

#[tokio::test]
async fn insert_then_get_round_trips() {
    let store = open_store(&temp_db());
    let rec = queued_ask("m-a3f91c2e");
    store.insert_message(&rec).await.unwrap();
    let got = store.get_message(&rec.id).await.unwrap().unwrap();
    assert_eq!(got, rec);
}

#[tokio::test]
async fn get_missing_returns_none() {
    let store = open_store(&temp_db());
    let got = store
        .get_message(&MessageId("m-00000000".to_string()))
        .await
        .unwrap();
    assert!(got.is_none());
}

#[tokio::test]
async fn update_persists_status_reply_and_timestamps() {
    let store = open_store(&temp_db());
    let mut rec = queued_ask("m-a3f91c2e");
    store.insert_message(&rec).await.unwrap();
    rec.status = MessageStatus::Replied;
    rec.code = Some(0);
    rec.reply = Some("Yes, migration 004 applied and tested.".to_string());
    rec.injected_at = Some(Timestamp("2026-08-01T17:03:12Z".to_string()));
    rec.completed_at = Some(Timestamp("2026-08-01T17:04:40Z".to_string()));
    assert!(store.update_message(&rec).await.unwrap());
    let got = store.get_message(&rec.id).await.unwrap().unwrap();
    assert_eq!(got, rec);
}

#[tokio::test]
async fn update_missing_returns_false() {
    let store = open_store(&temp_db());
    let rec = queued_ask("m-deadbeef");
    assert!(!store.update_message(&rec).await.unwrap());
}

#[tokio::test]
async fn wal_mode_is_enabled() {
    let path = temp_db();
    let store = open_store(&path);
    store
        .insert_message(&queued_ask("m-a3f91c2e"))
        .await
        .unwrap();
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    assert!(wal.exists(), "expected WAL sidecar at {}", wal.display());
}

#[tokio::test]
async fn shutdown_then_use_returns_closed() {
    let store = open_store(&temp_db());
    store.shutdown().await.unwrap();
    let err = store
        .insert_message(&queued_ask("m-a3f91c2e"))
        .await
        .unwrap_err();
    assert!(matches!(err, coretempo_core::store::StoreError::Closed));
}

fn table_exists(conn: &rusqlite::Connection, name: &str) -> bool {
    conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")
        .unwrap()
        .exists([name])
        .unwrap()
}

fn schema_version(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

/// The v1.0.0 DDL, before `run_id`: what an existing tempo.db in the wild has.
const LEGACY_SCHEMA: &str = "
CREATE TABLE messages (
  id           TEXT PRIMARY KEY,
  kind         TEXT NOT NULL CHECK (kind IN ('ask', 'send')),
  from_origin  TEXT NOT NULL,
  to_agent     TEXT NOT NULL,
  body         TEXT NOT NULL,
  status       TEXT NOT NULL CHECK
               (status IN ('queued', 'injected', 'working', 'replied', 'done', 'failed')),
  code         INTEGER CHECK (code IN (0, 1)),
  reply        TEXT,
  created_at   TEXT NOT NULL,
  injected_at  TEXT,
  completed_at TEXT
);
CREATE INDEX idx_messages_created ON messages (created_at);
CREATE INDEX idx_messages_to_status ON messages (to_agent, status);
CREATE TABLE runs (
  run_id        TEXT PRIMARY KEY,
  workflow_name TEXT NOT NULL,
  workflow_hash TEXT NOT NULL,
  started_at    TEXT NOT NULL,
  stopped_at    TEXT
);
CREATE TABLE agent_events (
  id        INTEGER PRIMARY KEY AUTOINCREMENT,
  agent     TEXT NOT NULL,
  state     TEXT NOT NULL,
  exit_code INTEGER,
  ts        TEXT NOT NULL
);
";

#[tokio::test]
async fn opens_and_migrates_a_pre_run_id_database() {
    let path = temp_db();
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(LEGACY_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO messages (id, kind, from_origin, to_agent, body, status, created_at) \
             VALUES ('m-11111111', 'ask', 'user', 'builder', 'legacy body', 'queued', \
             '2026-08-01T00:00:00Z')",
            [],
        )
        .unwrap();
    }
    let store = open_store(&path);
    // The legacy row survives the migration and still reads back.
    let got = store
        .get_message(&MessageId("m-11111111".to_string()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.body, "legacy body");
    // New writes work against the migrated table.
    let rec = queued_ask("m-a3f91c2e5d0b7f14");
    store.insert_message(&rec).await.unwrap();
    assert_eq!(store.get_message(&rec.id).await.unwrap().unwrap(), rec);
    // A second open is idempotent (the migration must not re-ALTER).
    store.shutdown().await.unwrap();
    let reopened = open_store(&path);
    assert!(reopened.get_message(&rec.id).await.unwrap().is_some());
    reopened.shutdown().await.unwrap();
    let raw = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        schema_version(&raw),
        3,
        "migration must stamp the current schema version"
    );
}

/// `agent_events` was never read by anything but its own tests, so the table
/// goes — including from databases that already carry it, whether they predate
/// versioning or were stamped version 1 by the build that added `run_id`.
#[tokio::test]
async fn migration_drops_the_dead_agent_events_table() {
    for (label, prepare) in [
        ("legacy", ""),
        (
            "version-1",
            "ALTER TABLE messages ADD COLUMN run_id TEXT;\
             ALTER TABLE agent_events ADD COLUMN run_id TEXT;\
             PRAGMA user_version = 1;",
        ),
    ] {
        let path = temp_db();
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(LEGACY_SCHEMA).unwrap();
            conn.execute_batch(prepare).unwrap();
            conn.execute(
                "INSERT INTO agent_events (agent, state, ts) \
                 VALUES ('builder', 'exited', '2026-08-01T00:00:00Z')",
                [],
            )
            .unwrap();
            assert!(table_exists(&conn, "agent_events"), "{label} fixture");
        }
        let store = open_store(&path);
        store.shutdown().await.unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert!(
            !table_exists(&conn, "agent_events"),
            "{label}: the dead table must be dropped on open"
        );
        assert!(table_exists(&conn, "messages"), "{label}: messages stays");
        assert_eq!(schema_version(&conn), 3, "{label}");
    }
}

/// Failure reasons are persisted, and a database written before the columns
/// existed still reads back (as `None`) after the additive migration.
#[tokio::test]
async fn failure_reason_round_trips_and_legacy_rows_read_none() {
    let store = Store::open(&temp_db(), run("r-aaaaaaaa")).unwrap();
    let mut rec = queued_ask("m-0000000000000001");
    rec.status = MessageStatus::Failed;
    rec.reason = Some("agent 'builder' exited".to_string());
    rec.reason_code = Some("agent_exited".to_string());
    store.insert_message(&rec).await.unwrap();
    let back = store.get_message(&rec.id).await.unwrap().unwrap();
    assert_eq!(back.reason.as_deref(), Some("agent 'builder' exited"));
    assert_eq!(back.reason_code.as_deref(), Some("agent_exited"));

    // A pre-v3 file: create the v2 table by hand, then open through Store.
    let legacy = temp_db();
    {
        let conn = rusqlite::Connection::open(&legacy).unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (id TEXT PRIMARY KEY, kind TEXT NOT NULL, \
             from_origin TEXT NOT NULL, to_agent TEXT NOT NULL, body TEXT NOT NULL, \
             status TEXT NOT NULL, code INTEGER, reply TEXT, created_at TEXT NOT NULL, \
             injected_at TEXT, completed_at TEXT, run_id TEXT); \
             CREATE TABLE runs (run_id TEXT PRIMARY KEY, workflow_name TEXT NOT NULL, \
             workflow_hash TEXT NOT NULL, started_at TEXT NOT NULL, stopped_at TEXT); \
             INSERT INTO messages VALUES ('m-0000000000000002','ask','user','b','hi',\
             'replied',0,'ok','2026-08-18T00:00:00Z',NULL,'2026-08-18T00:00:01Z','r-x'); \
             PRAGMA user_version = 2;",
        )
        .unwrap();
    }
    let store = Store::open(&legacy, run("r-bbbbbbbb")).unwrap();
    let back = store
        .get_message(&MessageId("m-0000000000000002".to_string()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(back.reason, None);
    assert_eq!(back.reason_code, None);
}

#[tokio::test]
async fn open_on_a_migrated_db_needs_no_write_transaction() {
    let path = temp_db();
    // First open migrates and stamps user_version = 1.
    let store = Store::open(&path, run("r-11111111")).unwrap();
    store.shutdown().await.unwrap();
    // A second connection holds the database's one write slot.
    let blocker = rusqlite::Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    // Re-opening must take the user_version fast path: no ALTER, no immediate
    // transaction, so the held write lock is irrelevant.
    let reopened = Store::open(&path, run("r-22222222")).unwrap();
    drop(reopened);
    blocker.execute_batch("COMMIT").unwrap();
}

#[tokio::test]
async fn store_survives_data_across_reopen() {
    let path = temp_db();
    let rec = queued_ask("m-a3f91c2e");
    {
        let store = open_store(&path);
        store.insert_message(&rec).await.unwrap();
        store.shutdown().await.unwrap();
    }
    let store = open_store(&path);
    let got = store.get_message(&rec.id).await.unwrap().unwrap();
    assert_eq!(got, rec);
}

use coretempo_core::router::MessageFilter;

fn record(
    id: &str,
    kind: MessageKind,
    to: &str,
    status: MessageStatus,
    created_at: &str,
) -> MessageRecord {
    MessageRecord {
        id: MessageId(id.to_string()),
        kind,
        from: Origin::Agent(AgentId("planner".to_string())),
        to: AgentId(to.to_string()),
        body: "body".to_string(),
        status,
        code: None,
        reply: None,
        created_at: Timestamp(created_at.to_string()),
        injected_at: None,
        completed_at: None,
        reason: None,
        reason_code: None,
    }
}

#[tokio::test]
async fn list_orders_newest_first_and_filters() {
    let store = open_store(&temp_db());
    let rows = [
        record(
            "m-00000001",
            MessageKind::Ask,
            "builder",
            MessageStatus::Queued,
            "2026-08-01T17:00:01Z",
        ),
        record(
            "m-00000002",
            MessageKind::Send,
            "builder",
            MessageStatus::Done,
            "2026-08-01T17:00:02Z",
        ),
        record(
            "m-00000003",
            MessageKind::Ask,
            "reviewer",
            MessageStatus::Working,
            "2026-08-01T17:00:03Z",
        ),
    ];
    for r in &rows {
        store.insert_message(r).await.unwrap();
    }
    let all = store
        .list_messages(&MessageFilter::default())
        .await
        .unwrap();
    let ids: Vec<&str> = all.iter().map(|r| r.id.0.as_str()).collect();
    assert_eq!(ids, ["m-00000003", "m-00000002", "m-00000001"]);

    let to_builder = MessageFilter {
        to: Some(AgentId("builder".to_string())),
        ..MessageFilter::default()
    };
    assert_eq!(store.list_messages(&to_builder).await.unwrap().len(), 2);

    let asks = MessageFilter {
        kind: Some(MessageKind::Ask),
        ..MessageFilter::default()
    };
    assert_eq!(store.list_messages(&asks).await.unwrap().len(), 2);

    let done = MessageFilter {
        status: Some(MessageStatus::Done),
        ..MessageFilter::default()
    };
    assert_eq!(store.list_messages(&done).await.unwrap().len(), 1);

    let from = MessageFilter {
        from: Some(Origin::Agent(AgentId("planner".to_string()))),
        ..MessageFilter::default()
    };
    assert_eq!(store.list_messages(&from).await.unwrap().len(), 3);

    let since = MessageFilter {
        since: Some(Timestamp("2026-08-01T17:00:02Z".to_string())),
        ..MessageFilter::default()
    };
    assert_eq!(store.list_messages(&since).await.unwrap().len(), 2);

    let limited = MessageFilter {
        limit: 1,
        ..MessageFilter::default()
    };
    let one = store.list_messages(&limited).await.unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].id.0, "m-00000003");
}

#[tokio::test]
async fn pending_queries_exclude_terminal_rows() {
    let store = open_store(&temp_db());
    let rows = [
        record(
            "m-00000001",
            MessageKind::Ask,
            "builder",
            MessageStatus::Queued,
            "2026-08-01T17:00:01Z",
        ),
        record(
            "m-00000002",
            MessageKind::Ask,
            "builder",
            MessageStatus::Replied,
            "2026-08-01T17:00:02Z",
        ),
        record(
            "m-00000003",
            MessageKind::Send,
            "builder",
            MessageStatus::Injected,
            "2026-08-01T17:00:03Z",
        ),
        record(
            "m-00000004",
            MessageKind::Ask,
            "reviewer",
            MessageStatus::Working,
            "2026-08-01T17:00:04Z",
        ),
    ];
    for r in &rows {
        store.insert_message(r).await.unwrap();
    }
    let to_builder = store
        .pending_to_agent(&AgentId("builder".to_string()))
        .await
        .unwrap();
    let ids: Vec<&str> = to_builder.iter().map(|r| r.id.0.as_str()).collect();
    assert_eq!(ids, ["m-00000001", "m-00000003"]);

    let asks = store.pending_asks().await.unwrap();
    let ids: Vec<&str> = asks.iter().map(|r| r.id.0.as_str()).collect();
    assert_eq!(ids, ["m-00000001", "m-00000004"]);
}

#[tokio::test]
async fn pending_queries_are_scoped_to_the_opening_run() {
    let path = temp_db();
    let store_a = Store::open(&path, run("r-aaaaaaaa")).unwrap();
    let store_b = Store::open(&path, run("r-bbbbbbbb")).unwrap();

    store_a
        .insert_message(&queued_ask("m-aaaaaaaaaaaaaaaa"))
        .await
        .unwrap();
    store_b
        .insert_message(&queued_ask("m-bbbbbbbbbbbbbbbb"))
        .await
        .unwrap();

    let pending_a = store_a
        .pending_to_agent(&AgentId("builder".to_string()))
        .await
        .unwrap();
    assert_eq!(
        pending_a.len(),
        1,
        "run A must not see run B's pending rows"
    );
    assert_eq!(pending_a[0].id.0, "m-aaaaaaaaaaaaaaaa");

    let asks_b = store_b.pending_asks().await.unwrap();
    assert_eq!(asks_b.len(), 1, "run B must not see run A's pending asks");
    assert_eq!(asks_b[0].id.0, "m-bbbbbbbbbbbbbbbb");

    // Unscoped reads still cross runs: get-by-id is unique, and the traffic
    // log is the shipped cross-run history view.
    assert!(
        store_a
            .get_message(&MessageId("m-bbbbbbbbbbbbbbbb".to_string()))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn legacy_rows_without_run_id_are_excluded_from_pending() {
    let path = temp_db();
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(LEGACY_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO messages (id, kind, from_origin, to_agent, body, status, created_at) \
             VALUES ('m-22222222', 'ask', 'user', 'builder', 'legacy', 'queued', \
             '2026-08-01T00:00:00Z')",
            [],
        )
        .unwrap();
    }
    let store = Store::open(&path, run("r-cccccccc")).unwrap();
    let pending = store
        .pending_to_agent(&AgentId("builder".to_string()))
        .await
        .unwrap();
    assert!(
        pending.is_empty(),
        "NULL-run legacy rows must not enter the sweep"
    );
    // ...but the row is not lost: unscoped reads still find it.
    assert!(
        store
            .get_message(&MessageId("m-22222222".to_string()))
            .await
            .unwrap()
            .is_some()
    );
}

/// Two runs cold-starting on the same legacy file. The loser of the race must
/// wait for the migration and find it done, not repeat it and fail with
/// "duplicate column name".
#[test]
fn concurrent_opens_of_a_legacy_database_both_migrate_cleanly() {
    let path = temp_db();
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(LEGACY_SCHEMA).unwrap();
    }
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = ["r-11111111", "r-22222222"]
        .into_iter()
        .map(|id| {
            let (path, barrier) = (path.clone(), Arc::clone(&barrier));
            std::thread::spawn(move || {
                barrier.wait();
                Store::open(&path, run(id)).map(|_| ())
            })
        })
        .collect();
    for handle in handles {
        let opened = handle.join().unwrap();
        assert!(
            opened.is_ok(),
            "both opens must migrate cleanly: {opened:?}"
        );
    }
}

/// A database left half-migrated by a crash after the ALTER but before the
/// version stamp: the column is there, `user_version` still reads 0. Re-running
/// the migration must not fail with "duplicate column name".
#[tokio::test]
async fn migrates_a_database_stranded_after_the_run_id_alter() {
    let path = temp_db();
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(LEGACY_SCHEMA).unwrap();
        conn.execute_batch("ALTER TABLE messages ADD COLUMN run_id TEXT;")
            .unwrap();
    }
    let store = Store::open(&path, run("r-eeeeeeee")).unwrap();
    let rec = queued_ask("m-a3f91c2e5d0b7f14");
    store.insert_message(&rec).await.unwrap();
    assert_eq!(store.get_message(&rec.id).await.unwrap().unwrap(), rec);
    store.shutdown().await.unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(schema_version(&conn), 3);
}

#[tokio::test]
async fn runs_persist() {
    let path = temp_db();
    let store = open_store(&path);
    let run = RunId("r-1f2e3d4c".to_string());
    store
        .insert_run(
            &run,
            "core-tempo-dev",
            &"a".repeat(64),
            &Timestamp("2026-08-01T17:00:00Z".to_string()),
        )
        .await
        .unwrap();
    store
        .mark_run_stopped(&run, &Timestamp("2026-08-01T18:00:00Z".to_string()))
        .await
        .unwrap();
    store.shutdown().await.unwrap();

    let conn = rusqlite::Connection::open(&path).unwrap();
    let stopped: Option<String> = conn
        .query_row(
            "SELECT stopped_at FROM runs WHERE run_id = ?1",
            rusqlite::params!["r-1f2e3d4c"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stopped.as_deref(), Some("2026-08-01T18:00:00Z"));
}

#[tokio::test]
async fn shutdown_with_a_concurrent_reader_still_succeeds() {
    let path = temp_db();
    let store = Store::open(&path, run("r-11111111")).unwrap();
    store
        .insert_run(&run("r-11111111"), "w", "h", &Timestamp::now())
        .await
        .unwrap();
    // A held read transaction blocks wal_checkpoint(TRUNCATE) exclusivity.
    let blocker = rusqlite::Connection::open(&path).unwrap();
    blocker
        .execute_batch("BEGIN; SELECT count(*) FROM runs;")
        .unwrap();
    // Shutdown must still succeed: skipping the truncate is by design when
    // another run holds the file. (~5s: the checkpoint's busy wait.)
    store.shutdown().await.unwrap();
    blocker.execute_batch("COMMIT").unwrap();
}

/// `PRAGMA wal_checkpoint(TRUNCATE)` reports contention in its result row, not
/// via `Err` — so shutdown must return `Ok` whether or not a peer store is live,
/// and this must hold through the real `Store` API, not just a raw connection.
#[tokio::test]
async fn shutdown_succeeds_with_a_second_store_still_open() {
    let path = temp_db();
    let a = Store::open(&path, run("r-aaaaaaaa")).unwrap();
    let b = Store::open(&path, run("r-bbbbbbbb")).unwrap();
    a.insert_run(&run("r-aaaaaaaa"), "w", "h", &Timestamp::now())
        .await
        .unwrap();
    assert!(a.shutdown().await.is_ok());
    b.shutdown().await.unwrap();
}

/// The success path the busy-flag rework must not weaken: an uncontended
/// shutdown still truncates the WAL sidecar to nothing.
#[tokio::test]
async fn shutdown_truncates_the_wal_when_uncontended() {
    let path = temp_db();
    let store = open_store(&path);
    store
        .insert_message(&queued_ask("m-a3f91c2e"))
        .await
        .unwrap();
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    assert!(wal.exists(), "expected a WAL sidecar before shutdown");
    store.shutdown().await.unwrap();
    let len = std::fs::metadata(&wal).map_or(0, |m| m.len());
    assert_eq!(
        len, 0,
        "an uncontended shutdown must truncate the WAL, not just skip it"
    );
}

#[tokio::test]
async fn two_stores_share_one_file_without_tripping_each_other() {
    let path = temp_db();
    let a = Store::open(&path, run("r-aaaaaaaa")).unwrap();
    let b = Store::open(&path, run("r-bbbbbbbb")).unwrap();
    a.insert_run(&run("r-aaaaaaaa"), "w", "h", &Timestamp::now())
        .await
        .unwrap();
    b.insert_run(&run("r-bbbbbbbb"), "w", "h", &Timestamp::now())
        .await
        .unwrap();
    // Interleaved writes through both handles, via the existing `record`
    // fixture (queued = non-terminal, so pending_to_agent sees them).
    for i in 0..20_u32 {
        a.insert_message(&record(
            &format!("m-{i:08x}"),
            MessageKind::Send,
            "x",
            MessageStatus::Queued,
            "2026-01-01T00:00:00Z",
        ))
        .await
        .unwrap();
        b.insert_message(&record(
            &format!("m-{:08x}", i + 100),
            MessageKind::Send,
            "y",
            MessageStatus::Queued,
            "2026-01-01T00:00:00Z",
        ))
        .await
        .unwrap();
    }
    // Each handle's pending view is its own run's rows only (phase 1 scoping).
    assert_eq!(
        a.pending_to_agent(&AgentId("x".into()))
            .await
            .unwrap()
            .len(),
        20
    );
    assert_eq!(
        a.pending_to_agent(&AgentId("y".into()))
            .await
            .unwrap()
            .len(),
        0
    );
    b.shutdown().await.unwrap();
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn open_sweeps_orphaned_non_terminal_rows() {
    let path = temp_db();
    // Run A: cleanly stopped, leaves a working row behind.
    let a = Store::open(&path, run("r-aaaaaaaa")).unwrap();
    a.insert_run(&run("r-aaaaaaaa"), "w", "h", &Timestamp::now())
        .await
        .unwrap();
    a.insert_message(&record(
        "m-0000000a",
        MessageKind::Send,
        "x",
        MessageStatus::Working,
        "2026-01-01T00:00:00Z",
    ))
    .await
    .unwrap();
    a.mark_run_stopped(&run("r-aaaaaaaa"), &Timestamp::now())
        .await
        .unwrap();
    a.shutdown().await.unwrap();
    // A legacy NULL-run_id non-terminal row, written by hand. The INSERT
    // deliberately omits run_id — NULL is the point.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO messages (id, kind, from_origin, to_agent, body, status, created_at) \
             VALUES ('m-000000ff', 'send', 'user', 'x', 'b', 'queued', '2026-01-01')",
            [],
        )
        .unwrap();
    }
    // Run B: still live (no stopped_at), with its own working row.
    let b = Store::open(&path, run("r-bbbbbbbb")).unwrap();
    b.insert_run(&run("r-bbbbbbbb"), "w", "h", &Timestamp::now())
        .await
        .unwrap();
    b.insert_message(&record(
        "m-0000000b",
        MessageKind::Send,
        "x",
        MessageStatus::Working,
        "2026-01-01T00:00:00Z",
    ))
    .await
    .unwrap();
    // Run C's open reconciles: A's row and the NULL row fail; B's is untouched.
    let c = Store::open(&path, run("r-cccccccc")).unwrap();
    let status = |id: &str| {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.query_row(
            "SELECT status, completed_at IS NOT NULL FROM messages WHERE id = ?1",
            [id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)),
        )
        .unwrap()
    };
    assert_eq!(status("m-0000000a"), ("failed".into(), true));
    assert_eq!(status("m-000000ff"), ("failed".into(), true));
    assert_eq!(
        status("m-0000000b"),
        ("working".into(), false),
        "a live run's rows must never be swept by a peer's open"
    );
    // `orphaned` is the one failure code written as a raw SQL literal rather
    // than through `FailReason`, and it is documented on the wire.
    let diagnosis = |id: &str| {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.query_row(
            "SELECT reason_code, reason FROM messages WHERE id = ?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .unwrap()
    };
    for id in ["m-0000000a", "m-000000ff"] {
        let (reason_code, reason) = diagnosis(id);
        assert_eq!(reason_code.as_deref(), Some("orphaned"), "{id}");
        let reason = reason.unwrap_or_else(|| panic!("{id} swept with no reason"));
        assert!(!reason.is_empty(), "{id} swept with an empty reason");
    }
    assert_eq!(
        diagnosis("m-0000000b"),
        (None, None),
        "a live run's rows carry no diagnosis"
    );
    c.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

#[tokio::test]
async fn pending_asks_uses_the_run_status_index() {
    let path = temp_db();
    let store = Store::open(&path, run("r-11111111")).unwrap();
    store.shutdown().await.unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    let plan: String = conn
        .query_row(
            "EXPLAIN QUERY PLAN SELECT id FROM messages \
             WHERE kind = 'ask' AND run_id = 'r' AND status IN ('queued','injected','working')",
            [],
            |r| r.get(3),
        )
        .unwrap();
    assert!(
        plan.contains("idx_messages_run_status"),
        "pending_asks must be index-served under concurrent runs: {plan}"
    );
}
