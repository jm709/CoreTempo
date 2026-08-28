//! `SessionManager` against a temp git repository and the argv-dumping fake
//! agent (spec 2026-08-27 §10). No HOME mutation: the trust store, the
//! sessions root and the tempo path are explicit inputs. The harness itself
//! is `support::sessions`, shared with the HTTP suite.
#![cfg(unix)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use coretempo_core::bus::EventBus;
use coretempo_core::pty::{AgentEnv, PtyManager, PtyRoster};
use coretempo_core::sessions::manager::{SessionManager, SessionManagerInputs};
use coretempo_core::sessions::{SessionError, SessionStore, SessionsRoot};
use coretempo_core::trust::{TrustPolicy, TrustStore};
use coretempo_core::types::{AgentState, EventPayload, SessionState, Token, WorktreeStatus};
use support::sessions::{DEADLINE, git, harness, harness_with, plain_req};

#[tokio::test(flavor = "multi_thread")]
async fn a_plain_session_spawns_in_the_project_root_with_the_session_recipe() {
    let h = harness("plain").await;
    let project = h.project().await;
    let view = h.mgr.create(plain_req(&project)).await.unwrap();
    assert!(view.id.0.starts_with("s-"));
    assert_eq!(view.cwd, h.repo.display().to_string());
    assert_eq!(view.worktree, None);
    assert_eq!(view.worktree_status, WorktreeStatus::None);
    assert_eq!(view.title, "repo", "defaults to the directory name");
    assert_eq!(view.state, SessionState::Starting);
    assert_eq!(view.branch.as_deref(), Some("main"));
    h.wait_argv(1).await;
    let argv = h.argv();
    assert_eq!(argv[0][0], format!("cwd={}", h.repo.display()));
    let settings = h
        .root
        .join("sessions")
        .join(&view.id.0)
        .join("settings.json");
    assert_eq!(
        argv[0][1..],
        ["--settings".to_string(), settings.display().to_string()],
        "no system prompt, no MCP flags, no resume: {argv:?}"
    );
    assert!(settings.is_file());
    h.hook_idle(&view.id);
    h.wait_state(&view.id, AgentState::Idle).await;
    assert_eq!(h.mgr.get(&view.id).await.unwrap().state, SessionState::Idle);
    h.mgr.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worktree_session_lives_outside_the_repo_on_a_session_branch() {
    let h = harness("worktree").await;
    let project = h.project().await;
    let mut req = plain_req(&project);
    req.worktree = true;
    req.cwd = Some("pkg".into());
    req.prompt = Some("first line\nsecond".into());
    let view = h.mgr.create(req).await.unwrap();
    let wt = view.worktree.clone().expect("worktree");
    assert!(
        wt.path.starts_with(
            &h.root
                .join("sessions/worktrees")
                .join(&project.0)
                .display()
                .to_string()
        )
    );
    assert!(!wt.path.starts_with(&h.repo.display().to_string()));
    assert!(wt.branch.starts_with("session/"));
    assert_eq!(
        view.cwd,
        format!("{}/pkg", wt.path),
        "the relative cwd applies to the worktree"
    );
    assert_eq!(view.title, "first line");
    assert_eq!(view.branch.as_deref(), Some(wt.branch.as_str()));
    assert_eq!(view.ahead, Some(0));
    assert_eq!(view.worktree_status, WorktreeStatus::Present);
    // The derived grant is on the operator store, keyed by the worktree.
    assert!(
        h.trust
            .untrusted_roots([Path::new(&wt.path)])
            .unwrap()
            .is_empty()
    );
    // The prompt is injected once the hooks report idle.
    let mut out = h.mgr.pty().subscribe_output(&view.id, None).unwrap();
    h.hook_idle(&view.id);
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains("got:first line") {
        let chunk = tokio::time::timeout(DEADLINE, out.recv())
            .await
            .unwrap()
            .unwrap();
        seen.extend(chunk.bytes);
    }
    h.mgr.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cwd_outside_the_project_is_rejected_before_anything_is_made() {
    let h = harness("outside").await;
    let project = h.project().await;
    let mut req = plain_req(&project);
    req.worktree = true;
    req.cwd = Some(h.root.display().to_string());
    let err = h.mgr.create(req).await.unwrap_err();
    assert!(
        matches!(err, SessionError::CwdOutsideProject { .. }),
        "{err}"
    );
    assert!(err.to_string().contains(&h.repo.display().to_string()));
    assert!(
        !h.root.join("sessions/worktrees").exists(),
        "no worktree made"
    );
    assert!(h.mgr.list().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_untrusted_project_root_fails_create_and_leaves_no_worktree() {
    let h = harness_with("untrusted", false).await;
    let project = h.project().await;
    let mut req = plain_req(&project);
    req.worktree = true;
    let err = h.mgr.create(req).await.unwrap_err();
    assert!(matches!(err, SessionError::Trust(_)), "{err}");
    assert!(err.to_string().contains("trust_agent_dirs = true"), "{err}");
    assert!(!h.root.join("sessions/worktrees").exists());
    assert!(h.mgr.list().await.unwrap().is_empty());
    assert_eq!(
        git(&h.repo, &["branch", "--list", "session/*"]),
        "",
        "no branch left"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_spawn_rolls_back_row_files_and_worktree() {
    let h = harness("rollback").await;
    // Break the fake so the spawn fails.
    std::fs::remove_file(h.root.join("fake-claude.sh")).unwrap();
    let project = h.project().await;
    let mut req = plain_req(&project);
    req.worktree = true;
    let err = h.mgr.create(req).await.unwrap_err();
    assert!(matches!(err, SessionError::Spawn(_)), "{err}");
    assert!(h.mgr.list().await.unwrap().is_empty(), "row rolled back");
    assert!(h.mgr.pty().agent_ids().is_empty(), "handle rolled back");
    let sessions_dir = h.root.join("sessions");
    let leftovers: Vec<String> = std::fs::read_dir(&sessions_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("s-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "session files rolled back: {leftovers:?}"
    );
    assert_eq!(
        git(&h.repo, &["worktree", "list"]).lines().count(),
        1,
        "worktree removed"
    );
    assert_eq!(
        git(&h.repo, &["branch", "--list", "session/*"]),
        "",
        "fresh branch deleted"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_resume_and_delete_follow_the_lifecycle() {
    let h = harness("lifecycle").await;
    let project = h.project().await;
    let view = h.mgr.create(plain_req(&project)).await.unwrap();
    let id = view.id.clone();
    h.wait_argv(1).await;
    h.hook_idle(&id);
    h.wait_state(&id, AgentState::Idle).await;
    let mut events = h.bus.subscribe();

    // resume while live → wrong state
    let err = h.mgr.resume(&id).await.unwrap_err();
    assert!(matches!(err, SessionError::WrongState { .. }), "{err}");
    assert!(
        err.to_string().contains("stop"),
        "names the valid action: {err}"
    );
    // delete while live → wrong state
    assert!(matches!(
        h.mgr.delete(&id, false, false).await.unwrap_err(),
        SessionError::WrongState { .. }
    ));

    let stopped = h.mgr.stop(&id).await.unwrap();
    assert_eq!(stopped.state, SessionState::Stopped);
    assert!(stopped.stopped_at.is_some());
    assert!(
        stopped.exit.is_some(),
        "the exit is recorded before stop returns"
    );
    assert!(matches!(
        h.mgr.stop(&id).await.unwrap_err(),
        SessionError::WrongState { .. }
    ));
    assert!(
        h.root
            .join("sessions")
            .join(&id.0)
            .join("settings.json")
            .is_file(),
        "files kept"
    );

    // A SessionStart reported an id while it was live; resume passes it.
    h.mgr.record_claude_session_id(&id, "first".into()).await;
    h.mgr.record_claude_session_id(&id, "second".into()).await;
    let resumed = h.mgr.resume(&id).await.unwrap();
    assert!(resumed.resumed);
    assert_eq!(resumed.session.claude_session_id.as_deref(), Some("second"));
    assert_eq!(resumed.session.stopped_at, None);
    h.wait_argv(2).await;
    let argv = h.argv();
    assert!(
        argv[1].windows(2).any(|w| w == ["--resume", "second"]),
        "{argv:?}"
    );
    h.wait_state(&id, AgentState::Starting).await;
    h.hook_idle(&id);
    h.wait_state(&id, AgentState::Idle).await;

    // The child leaves on its own → exited, recorded by the watcher.
    h.mgr.pty().write(&id, b"quit\n").await.unwrap();
    h.wait_state(&id, AgentState::Exited).await;
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let view = h.mgr.get(&id).await.unwrap();
        if view.state == SessionState::Exited && view.stopped_at.is_some() {
            assert_eq!(view.exit, Some(coretempo_core::types::AgentExit::Code(3)));
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "exit never recorded: {view:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    h.mgr.delete(&id, false, false).await.unwrap();
    assert!(matches!(
        h.mgr.get(&id).await.unwrap_err(),
        SessionError::UnknownSession { .. }
    ));
    assert!(
        !h.root.join("sessions").join(&id.0).exists(),
        "files removed"
    );

    let mut kinds = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
        match ev.payload {
            EventPayload::SessionStopped { .. } => kinds.push("stopped"),
            EventPayload::SessionResumed { resumed, .. } => {
                kinds.push(if resumed { "resumed" } else { "fresh" });
            }
            EventPayload::SessionDeleted { .. } => kinds.push("deleted"),
            _ => {}
        }
    }
    assert_eq!(kinds, ["stopped", "resumed", "deleted"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_then_resume_racing_is_serialized_by_the_session_lock() {
    let h = harness("race").await;
    let project = h.project().await;
    let id = h.mgr.create(plain_req(&project)).await.unwrap().id;
    h.wait_argv(1).await;
    let (stop, resume) = tokio::join!(h.mgr.stop(&id), h.mgr.resume(&id));
    stop.unwrap();
    // The resume queued behind the stop, saw `stopped`, and respawned.
    let resumed = resume.unwrap();
    assert!(!resumed.resumed, "no claude_session_id yet → fresh");
    h.wait_argv(2).await;
    assert!(h.mgr.pty().is_live(&id).unwrap());
    h.mgr.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_rechecks_the_project_roots_trust_and_refuses_as_untrusted() {
    let h = harness("retrust").await;
    let project = h.project().await;
    let id = h.mgr.create(plain_req(&project)).await.unwrap().id;
    h.wait_argv(1).await;
    h.mgr.stop(&id).await.unwrap();
    // A live Claude session flushed its own copy and reverted the key.
    std::fs::write(&h.trust.path, r#"{"projects":{}}"#).unwrap();
    let err = h.mgr.resume(&id).await.unwrap_err();
    assert!(
        matches!(err, SessionError::Trust(_)),
        "not a spawn failure: {err}"
    );
    assert!(
        err.to_string().contains(&h.repo.display().to_string()),
        "{err}"
    );
    assert!(err.to_string().contains("trust_agent_dirs = true"), "{err}");
    assert_eq!(
        h.mgr.get(&id).await.unwrap().state,
        SessionState::Stopped,
        "row untouched"
    );
    h.trust.grant(std::slice::from_ref(&h.repo)).unwrap();
    h.mgr.resume(&id).await.unwrap();
    h.wait_argv(2).await;
    h.mgr.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_with_remove_worktree_refuses_dirty_forces_and_reports_the_branch() {
    let h = harness("remove").await;
    let project = h.project().await;
    let mut req = plain_req(&project);
    req.worktree = true;
    let view = h.mgr.create(req).await.unwrap();
    let wt = view.worktree.clone().unwrap();
    h.wait_argv(1).await;
    h.mgr.stop(&view.id).await.unwrap();
    std::fs::write(Path::new(&wt.path).join("wip.txt"), "wip").unwrap();
    assert_eq!(h.mgr.get(&view.id).await.unwrap().changed_files, Some(1));
    let err = h.mgr.delete(&view.id, true, false).await.unwrap_err();
    assert!(matches!(err, SessionError::Dirty { .. }), "{err}");
    assert!(err.to_string().contains("wip.txt") && err.to_string().contains("force"));
    assert!(
        h.mgr.get(&view.id).await.is_ok(),
        "nothing deleted on refusal"
    );
    // Commit on the branch → kept after a forced remove.
    git(Path::new(&wt.path), &["add", "."]);
    git(Path::new(&wt.path), &["commit", "-q", "-m", "wip"]);
    assert_eq!(h.mgr.get(&view.id).await.unwrap().ahead, Some(1));
    let deleted = h.mgr.delete(&view.id, true, true).await.unwrap();
    assert!(deleted.branch_kept);
    assert!(!Path::new(&wt.path).exists());
    assert_eq!(
        git(&h.repo, &["branch", "--list", &wt.branch])
            .trim_start_matches("* ")
            .trim(),
        wt.branch
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_worktree_blocks_resume_and_is_pruned_on_delete() {
    let h = harness("missing").await;
    let project = h.project().await;
    let mut req = plain_req(&project);
    req.worktree = true;
    let view = h.mgr.create(req).await.unwrap();
    let wt = view.worktree.clone().unwrap();
    h.wait_argv(1).await;
    h.mgr.stop(&view.id).await.unwrap();
    std::fs::remove_dir_all(&wt.path).unwrap();
    assert_eq!(
        h.mgr.get(&view.id).await.unwrap().worktree_status,
        WorktreeStatus::Missing
    );
    let err = h.mgr.resume(&view.id).await.unwrap_err();
    assert!(matches!(err, SessionError::WorktreeMissing { .. }), "{err}");
    assert!(err.to_string().contains("delete"), "{err}");
    let deleted = h.mgr.delete(&view.id, true, false).await.unwrap();
    assert!(!deleted.branch_kept, "an unmoved branch is deleted");
    assert_eq!(
        git(&h.repo, &["worktree", "list"]).lines().count(),
        1,
        "pruned"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_isolated_session_spawns_against_a_seeded_config_dir_with_a_trust_mirror() {
    let h = harness("isolated").await;
    let project = h.project().await;
    let mut req = plain_req(&project);
    req.isolated_config = true;
    req.model = Some("haiku".into());
    let view = h.mgr.create(req).await.unwrap();
    assert!(view.isolated_config);
    h.wait_argv(1).await;
    let config_dir = h
        .root
        .join("sessions")
        .join(&view.id.0)
        .join("claude-config");
    let mirror = TrustStore::at(config_dir.join(".claude.json"));
    assert!(
        mirror
            .untrusted_roots([h.repo.as_path()])
            .unwrap()
            .is_empty(),
        "mirrored"
    );
    let argv = h.argv();
    assert!(
        argv[0].windows(2).any(|w| w == ["--model", "haiku"]),
        "{argv:?}"
    );
    h.mgr.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_marks_rows_exited_and_a_reboot_lists_them_stopped_cold() {
    let h = harness("reboot").await;
    let project = h.project().await;
    let id = h.mgr.create(plain_req(&project)).await.unwrap().id;
    h.wait_argv(1).await;
    h.mgr.shutdown().await;
    let view = h.mgr.get(&id).await.unwrap();
    assert_eq!(view.state, SessionState::Exited);
    assert!(view.stopped_at.is_some());
    // A second manager over the same root sees the row and can resume it.
    let store = SessionStore::open(&h.root.join("sessions/sessions.db")).unwrap();
    let bus = EventBus::new();
    let pty = PtyManager::new_with_program(
        PtyRoster::empty(Duration::from_millis(100)),
        bus.clone(),
        AgentEnv {
            port: 4821,
            token: Token("ab".repeat(32)),
            tempo_bin_dir: PathBuf::from("/usr/bin"),
            credential_store: None,
        },
        h.root.join("fake-claude.sh").to_str().unwrap(),
    );
    let again = SessionManager::boot(SessionManagerInputs {
        root: SessionsRoot::at(h.root.join("sessions")),
        store,
        pty,
        bus,
        trust_store: h.trust.clone(),
        policy: TrustPolicy { grant: false },
        tempo_bin: PathBuf::from("/usr/bin/tempo"),
        operator_token: Token("ab".repeat(32)),
    })
    .await
    .unwrap();
    let cold = again.get(&id).await.unwrap();
    assert_eq!(
        cold.state,
        SessionState::Exited,
        "not `starting`: an added handle is not live"
    );
    assert_eq!(again.counts().await.unwrap().total, 1);
    let resumed = again.resume(&id).await.unwrap();
    assert!(!resumed.resumed);
    h.wait_argv(2).await;
    again.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn projects_register_once_refuse_non_repos_and_forget_only_when_unused() {
    let h = harness("projects").await;
    let first = h
        .mgr
        .register_project(&h.repo.join("pkg"), None)
        .await
        .unwrap();
    assert_eq!(
        first.path,
        h.repo.display().to_string(),
        "a subdir registers its root"
    );
    assert_eq!(first.name, "repo");
    let err = h
        .mgr
        .register_project(&h.repo, Some("again".into()))
        .await
        .unwrap_err();
    assert!(matches!(err, SessionError::ProjectExists { .. }), "{err}");
    let err = h.mgr.register_project(&h.root, None).await.unwrap_err();
    assert!(matches!(err, SessionError::NotAGitRepo { .. }), "{err}");
    assert!(err.to_string().contains("not a git repository"), "{err}");
    let err = h
        .mgr
        .register_project(Path::new("/nonexistent/coretempo"), None)
        .await
        .unwrap_err();
    assert!(matches!(err, SessionError::Io { .. }), "{err}");
    let id = h.mgr.create(plain_req(&first.id)).await.unwrap().id;
    let err = h.mgr.forget_project(&first.id).await.unwrap_err();
    assert!(
        matches!(err, SessionError::ProjectInUse { sessions: 1, .. }),
        "{err}"
    );
    h.mgr.stop(&id).await.unwrap();
    h.mgr.delete(&id, false, false).await.unwrap();
    h.mgr.forget_project(&first.id).await.unwrap();
    assert!(h.mgr.list_projects().await.unwrap().is_empty());
    let err = h.mgr.forget_project(&first.id).await.unwrap_err();
    assert!(matches!(err, SessionError::UnknownProject { .. }), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn tokens_classify_the_operator_and_each_sessions_hook_token() {
    use coretempo_core::api::{Caller, TokenAuth};
    let h = harness("tokens").await;
    let project = h.project().await;
    let a = h.mgr.create(plain_req(&project)).await.unwrap().id;
    let b = h.mgr.create(plain_req(&project)).await.unwrap().id;
    h.wait_argv(2).await;
    assert_eq!(h.mgr.classify(&"ab".repeat(32)), Caller::Operator);
    assert_eq!(h.mgr.classify("nope"), Caller::Unknown);
    let store = SessionStore::open(&h.root.join("sessions/sessions.db")).unwrap();
    let token_a = store.get_session(&a).await.unwrap().unwrap().hook_token;
    let token_b = store.get_session(&b).await.unwrap().unwrap().hook_token;
    assert_ne!(token_a, token_b);
    assert_eq!(h.mgr.classify(&token_a.0), Caller::Hook(a.clone()));
    assert_eq!(h.mgr.classify(&token_b.0), Caller::Hook(b.clone()));
    h.mgr.stop(&b).await.unwrap();
    assert_eq!(
        h.mgr.classify(&token_b.0),
        Caller::Hook(b.clone()),
        "lives as long as the row"
    );
    h.mgr.delete(&b, false, false).await.unwrap();
    assert_eq!(h.mgr.classify(&token_b.0), Caller::Unknown);
    h.mgr.shutdown().await;
}

/// `delete` must not give up the row's handle until the persistent state it
/// owns is gone: a failure after detaching would leave an undeletable row.
#[tokio::test(flavor = "multi_thread")]
async fn a_delete_that_fails_partway_can_be_retried() {
    let h = harness("retry-delete").await;
    let project = h.project().await;
    let view = h.mgr.create(plain_req(&project)).await.unwrap();
    h.wait_argv(1).await;
    h.mgr.stop(&view.id).await.unwrap();
    // Read-only sessions root: `remove_session_files` cannot unlink `<id>/`.
    let sessions = h.root.join("sessions");
    let restore = std::fs::metadata(&sessions).unwrap().permissions();
    let mut locked = restore.clone();
    locked.set_mode(0o500);
    std::fs::set_permissions(&sessions, locked).unwrap();
    let err = h.mgr.delete(&view.id, false, false).await.unwrap_err();
    assert!(matches!(err, SessionError::Io { .. }), "{err}");
    std::fs::set_permissions(&sessions, restore).unwrap();
    h.mgr.delete(&view.id, false, false).await.unwrap();
    assert!(matches!(
        h.mgr.get(&view.id).await.unwrap_err(),
        SessionError::UnknownSession { .. }
    ));
    h.mgr.shutdown().await;
}
