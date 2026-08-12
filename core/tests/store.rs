use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use coretempo_core::store::Store;
use coretempo_core::time::Timestamp;
use coretempo_core::types::id::{AgentId, MessageId};
use coretempo_core::types::message::{MessageKind, MessageRecord, MessageStatus, Origin};

static DB_N: AtomicU64 = AtomicU64::new(0);

fn temp_db() -> PathBuf {
    let n = DB_N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("coretempo-store-{}-{n}.db", std::process::id()))
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
    }
}

#[tokio::test]
async fn insert_then_get_round_trips() {
    let store = Store::open(&temp_db()).unwrap();
    let rec = queued_ask("m-a3f91c2e");
    store.insert_message(&rec).await.unwrap();
    let got = store.get_message(&rec.id).await.unwrap().unwrap();
    assert_eq!(got, rec);
}

#[tokio::test]
async fn get_missing_returns_none() {
    let store = Store::open(&temp_db()).unwrap();
    let got = store
        .get_message(&MessageId("m-00000000".to_string()))
        .await
        .unwrap();
    assert!(got.is_none());
}

#[tokio::test]
async fn update_persists_status_reply_and_timestamps() {
    let store = Store::open(&temp_db()).unwrap();
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
    let store = Store::open(&temp_db()).unwrap();
    let rec = queued_ask("m-deadbeef");
    assert!(!store.update_message(&rec).await.unwrap());
}

#[tokio::test]
async fn wal_mode_is_enabled() {
    let path = temp_db();
    let store = Store::open(&path).unwrap();
    store
        .insert_message(&queued_ask("m-a3f91c2e"))
        .await
        .unwrap();
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    assert!(wal.exists(), "expected WAL sidecar at {}", wal.display());
}

#[tokio::test]
async fn shutdown_then_use_returns_closed() {
    let store = Store::open(&temp_db()).unwrap();
    store.shutdown().await.unwrap();
    let err = store
        .insert_message(&queued_ask("m-a3f91c2e"))
        .await
        .unwrap_err();
    assert!(matches!(err, coretempo_core::store::StoreError::Closed));
}

#[tokio::test]
async fn store_survives_data_across_reopen() {
    let path = temp_db();
    let rec = queued_ask("m-a3f91c2e");
    {
        let store = Store::open(&path).unwrap();
        store.insert_message(&rec).await.unwrap();
        store.shutdown().await.unwrap();
    }
    let store = Store::open(&path).unwrap();
    let got = store.get_message(&rec.id).await.unwrap().unwrap();
    assert_eq!(got, rec);
}

use coretempo_core::router::MessageFilter;
use coretempo_core::types::id::RunId;

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
    }
}

#[tokio::test]
async fn list_orders_newest_first_and_filters() {
    let store = Store::open(&temp_db()).unwrap();
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
    let store = Store::open(&temp_db()).unwrap();
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
async fn runs_and_agent_events_persist() {
    let path = temp_db();
    let store = Store::open(&path).unwrap();
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
    store
        .insert_agent_event(
            &AgentId("builder".to_string()),
            "exited",
            Some(1),
            &Timestamp("2026-08-01T17:30:00Z".to_string()),
        )
        .await
        .unwrap();
    let events = store.list_agent_events(10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].agent.0, "builder");
    assert_eq!(events[0].state, "exited");
    assert_eq!(events[0].exit_code, Some(1));
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
