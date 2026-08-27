// `clippy::panic` is deliberately not expected here: no helper panics, and an
// unfulfilled `expect` is itself a warning under `-D warnings`.
#![expect(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns are not covered by allow-*-in-tests"
)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use coretempo_core::sessions::{LastState, ProjectRow, SessionRow, SessionStore, WorktreeRow};
use coretempo_core::time::Timestamp;
use coretempo_core::types::{AgentExit, AgentId, ProjectId, Token};

fn db_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("coretempo-sstore-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("sessions.db")
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn project(id: &str, path: &str) -> ProjectRow {
    ProjectRow {
        id: ProjectId(id.into()),
        path: PathBuf::from(path),
        name: "proj".into(),
        created_at: Timestamp("2026-08-27T10:00:00Z".into()),
    }
}

fn session(id: &str, project: &str) -> SessionRow {
    SessionRow {
        id: AgentId(id.into()),
        project: ProjectId(project.into()),
        cwd: PathBuf::from("/w/proj"),
        worktree: Some(WorktreeRow {
            path: PathBuf::from("/w/.coretempo/worktrees/p-1/brisk-otter-3f1a"),
            branch: "session/brisk-otter-3f1a".into(),
            base: "abc123".into(),
        }),
        title: "t".into(),
        claude_session_id: None,
        model: Some("haiku".into()),
        permission_mode: None,
        isolated_config: true,
        prompt: Some("hello".into()),
        hook_token: Token("cd".repeat(32)),
        last_state: LastState::Exited,
        exit: None,
        created_at: Timestamp("2026-08-27T10:00:01Z".into()),
        stopped_at: None,
    }
}

#[tokio::test]
async fn the_database_and_its_wal_are_private() {
    let path = db_path("private");
    let store = SessionStore::open(&path).unwrap();
    assert_eq!(mode(&path), 0o600, "db");
    store
        .insert_project(project("p-1", "/w/proj"))
        .await
        .unwrap();
    let wal = path.with_extension("db-wal");
    assert!(wal.exists(), "WAL mode is on");
    assert_eq!(mode(&wal), 0o600, "wal");
}

#[tokio::test]
async fn projects_round_trip_and_are_unique_by_path() {
    let store = SessionStore::open(&db_path("projects")).unwrap();
    store.insert_project(project("p-1", "/w/a")).await.unwrap();
    store.insert_project(project("p-2", "/w/b")).await.unwrap();
    let err = store
        .insert_project(project("p-3", "/w/a"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("UNIQUE"), "{err}");
    assert_eq!(store.list_projects().await.unwrap().len(), 2);
    assert_eq!(
        store
            .project_by_path(Path::new("/w/b"))
            .await
            .unwrap()
            .unwrap()
            .id,
        ProjectId("p-2".into())
    );
    assert!(
        store
            .get_project(&ProjectId("p-9".into()))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .delete_project(&ProjectId("p-2".into()))
            .await
            .unwrap()
    );
    assert!(
        !store
            .delete_project(&ProjectId("p-2".into()))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn sessions_round_trip_every_column() {
    let store = SessionStore::open(&db_path("sessions")).unwrap();
    store
        .insert_project(project("p-1", "/w/proj"))
        .await
        .unwrap();
    let row = session("s-1", "p-1");
    store.insert_session(row.clone()).await.unwrap();
    assert_eq!(store.get_session(&row.id).await.unwrap().unwrap(), row);
    assert_eq!(store.sessions_in_project(&row.project).await.unwrap(), 1);
    assert_eq!(store.count_sessions().await.unwrap(), 1);
    // A project with sessions is the manager's 409; the store just reports the count.
    let mut plain = session("s-2", "p-1");
    plain.worktree = None;
    plain.prompt = None;
    store.insert_session(plain.clone()).await.unwrap();
    let listed = store.list_sessions().await.unwrap();
    assert_eq!(listed, vec![row, plain]);
}

#[tokio::test]
async fn state_transitions_are_recorded() {
    let store = SessionStore::open(&db_path("state")).unwrap();
    store
        .insert_project(project("p-1", "/w/proj"))
        .await
        .unwrap();
    let id = AgentId("s-1".into());
    store.insert_session(session("s-1", "p-1")).await.unwrap();
    assert!(
        store
            .set_claude_session_id(&id, "first".into())
            .await
            .unwrap()
    );
    assert!(
        store
            .set_claude_session_id(&id, "second".into())
            .await
            .unwrap()
    );
    let at = Timestamp("2026-08-27T11:00:00Z".into());
    assert!(
        store
            .mark_left_live(
                &id,
                LastState::Stopped,
                Some(AgentExit::Signal("Hangup".into())),
                at.clone()
            )
            .await
            .unwrap()
    );
    let row = store.get_session(&id).await.unwrap().unwrap();
    assert_eq!(
        row.claude_session_id.as_deref(),
        Some("second"),
        "latest wins"
    );
    assert_eq!(row.last_state, LastState::Stopped);
    assert_eq!(row.exit, Some(AgentExit::Signal("Hangup".into())));
    assert_eq!(row.stopped_at, Some(at));
    assert!(store.mark_live(&id).await.unwrap());
    let row = store.get_session(&id).await.unwrap().unwrap();
    assert_eq!(row.stopped_at, None);
    assert_eq!(row.exit, None);
    assert_eq!(
        row.last_state,
        LastState::Exited,
        "a live row reads exited if the daemon dies"
    );
    assert!(
        !store
            .set_claude_session_id(&AgentId("s-9".into()), "x".into())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn shutdown_marks_every_live_row_exited_and_a_reopen_shows_it() {
    let path = db_path("shutdown");
    let store = SessionStore::open(&path).unwrap();
    store
        .insert_project(project("p-1", "/w/proj"))
        .await
        .unwrap();
    store.insert_session(session("s-1", "p-1")).await.unwrap();
    store.insert_session(session("s-2", "p-1")).await.unwrap();
    let earlier = Timestamp("2026-08-27T10:30:00Z".into());
    store
        .mark_left_live(
            &AgentId("s-2".into()),
            LastState::Stopped,
            None,
            earlier.clone(),
        )
        .await
        .unwrap();
    let at = Timestamp("2026-08-27T12:00:00Z".into());
    assert_eq!(store.mark_all_left_live(at.clone()).await.unwrap(), 1);
    drop(store);
    let store = SessionStore::open(&path).unwrap();
    let rows = store.list_sessions().await.unwrap();
    assert_eq!(
        (rows[0].last_state, rows[0].stopped_at.clone()),
        (LastState::Exited, Some(at))
    );
    assert_eq!(
        (rows[1].last_state, rows[1].stopped_at.clone()),
        (LastState::Stopped, Some(earlier)),
        "an already-stopped row is untouched"
    );
    assert!(store.delete_session(&AgentId("s-1".into())).await.unwrap());
    assert_eq!(store.count_sessions().await.unwrap(), 1);
}

#[test]
fn an_unwritable_path_is_an_io_error_naming_it() {
    let dir = db_path("unwritable");
    std::fs::create_dir_all(&dir).unwrap();
    let err = SessionStore::open(&dir).unwrap_err();
    assert!(
        err.to_string().contains(&dir.display().to_string()),
        "{err}"
    );
}
