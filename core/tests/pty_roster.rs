//! Integration tests for the dynamic roster (spec 2026-08-27 §4, amendment 46):
//! agents added at runtime, `--resume` consumed per spawn, per-agent stop and
//! removal. The fake agent dumps its argv so the spawn recipe is observed
//! from outside `core` (`spawn_spec` is crate-private).
#![cfg(unix)]
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns are not covered by allow-*-in-tests"
)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use coretempo_core::bus::EventBus;
use coretempo_core::pty::{AgentEnv, McpPolicy, PtyError, PtyManager, PtyRoster, RosterEntry};
use coretempo_core::types::config::AgentConfig;
use coretempo_core::types::{AgentExit, AgentId, AgentState, Token};

const IDLE_DEBOUNCE: Duration = Duration::from_millis(100);
const DEADLINE: Duration = Duration::from_secs(10);

/// A fake agent that records its argv in `$PWD/argv.txt` (one arg per line,
/// appended per spawn with a blank separator) and then echoes like the
/// `pty_manager` fake: `quit` exits 3, anything else is echoed as `got:<line>`.
fn write_argv_agent(dir: &Path) -> PathBuf {
    let path = dir.join("argv-agent.sh");
    let script = concat!(
        "#!/usr/bin/env bash\n",
        "{ printf '%s\\n' \"$@\"; printf -- '===ARGV===\\n'; } >> \"$PWD/argv.txt\"\n",
        "printf 'booted\\n'\n",
        "while IFS= read -r line; do\n",
        "  case \"$line\" in\n",
        "    quit) exit 3 ;;\n",
        "    *) printf 'got:%s\\n' \"$line\" ;;\n",
        "  esac\n",
        "done\n",
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn fresh_dir() -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("coretempo-roster-{}-{n}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp);
    let _ = std::fs::remove_file(tmp.join("argv.txt"));
    tmp
}

fn entry(dir: &Path) -> RosterEntry {
    RosterEntry {
        mcp: McpPolicy::Inherit,
        ..RosterEntry::new(AgentConfig {
            auto_clear: false,
            ..AgentConfig::new(dir.to_path_buf(), "")
        })
    }
}

/// An empty manager over the argv-dumping fake in `dir`.
fn empty_manager(dir: &Path) -> (Arc<PtyManager>, EventBus) {
    let script = write_argv_agent(dir);
    let bus = EventBus::new();
    let env = AgentEnv {
        port: 4820,
        token: Token("ab".repeat(32)),
        tempo_bin_dir: PathBuf::from("/usr/bin"),
        credential_store: None,
    };
    let mgr = PtyManager::new_with_program(
        PtyRoster::empty(IDLE_DEBOUNCE),
        bus.clone(),
        env,
        script.to_str().unwrap(),
    );
    (mgr, bus)
}

async fn wait_state(mgr: &PtyManager, agent: &AgentId, want: AgentState) {
    let mut rx = mgr.subscribe_state_debounced(agent).unwrap();
    tokio::time::timeout(DEADLINE, rx.wait_for(|s| *s == want))
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {want:?}"))
        .unwrap();
}

/// Argv of every spawn so far, one `Vec` per spawn.
fn recorded_argv(dir: &Path) -> Vec<Vec<String>> {
    let text = std::fs::read_to_string(dir.join("argv.txt")).unwrap_or_default();
    text.split("===ARGV===\n")
        .filter(|block| !block.is_empty())
        .map(|block| {
            block
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_added_agent_spawns_with_the_entry_recipe() {
    let dir = fresh_dir();
    let (mgr, _bus) = empty_manager(&dir);
    let id = AgentId("s-1".into());
    let mut e = entry(&dir);
    e.cfg.model = Some("haiku".into());
    mgr.add_agent(id.clone(), e).unwrap();
    let mut out = mgr.subscribe_output(&id, None).unwrap();
    mgr.spawn(&id).await.unwrap();
    // The fake writes argv.txt before it prints "booted", so draining until
    // that shows up makes the argv read below deterministic.
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains("booted") {
        let chunk = tokio::time::timeout(DEADLINE, out.recv())
            .await
            .unwrap()
            .unwrap();
        seen.extend(chunk.bytes);
    }
    assert_eq!(
        recorded_argv(&dir),
        vec![vec!["--model".to_string(), "haiku".to_string()]],
        "Inherit + no prompt + no settings = just the model"
    );
    mgr.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn adding_an_existing_id_fails() {
    let dir = fresh_dir();
    let (mgr, _bus) = empty_manager(&dir);
    let id = AgentId("s-1".into());
    mgr.add_agent(id.clone(), entry(&dir)).unwrap();
    assert!(
        !dir.join("argv.txt").exists(),
        "add_agent creates the handle without spawning"
    );
    let err = mgr.add_agent(id.clone(), entry(&dir)).unwrap_err();
    assert!(
        matches!(err, PtyError::AgentExists(ref got) if *got == id),
        "{err}"
    );
    // A rejected add leaves the original entry usable.
    mgr.spawn(&id).await.unwrap();
    mgr.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_is_passed_once_and_then_cleared() {
    let dir = fresh_dir();
    let (mgr, _bus) = empty_manager(&dir);
    let id = AgentId("s-1".into());
    mgr.add_agent(id.clone(), entry(&dir)).unwrap();
    mgr.spawn(&id).await.unwrap();
    mgr.write(&id, b"quit\n").await.unwrap();
    wait_state(&mgr, &id, AgentState::Exited).await;

    mgr.set_resume(&id, Some("abc-123".into())).unwrap();
    mgr.spawn(&id).await.unwrap();
    // `wait_state`'s freshly-subscribed `wait_for` checks its predicate
    // against the *current* debounced value first, which right after
    // `spawn()` returns is still the previous cycle's Exited (spawn has no
    // internal `.await`, so it never yields to let the debouncer forward
    // Starting). Waiting for Starting here drains that stale value through
    // the same channel before we wait for this cycle's real Exited.
    wait_state(&mgr, &id, AgentState::Starting).await;
    mgr.write(&id, b"quit\n").await.unwrap();
    wait_state(&mgr, &id, AgentState::Exited).await;

    mgr.spawn(&id).await.unwrap();
    wait_state(&mgr, &id, AgentState::Starting).await;
    mgr.write(&id, b"quit\n").await.unwrap();
    wait_state(&mgr, &id, AgentState::Exited).await;

    let argv = recorded_argv(&dir);
    assert_eq!(argv.len(), 3, "{argv:?}");
    assert_eq!(argv[0], Vec::<String>::new());
    assert_eq!(argv[1], vec!["--resume".to_string(), "abc-123".to_string()]);
    assert_eq!(
        argv[2],
        Vec::<String>::new(),
        "resume is consumed by the spawn it was set for"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_resume_on_an_unknown_agent_fails() {
    let dir = fresh_dir();
    let (mgr, _bus) = empty_manager(&dir);
    let err = mgr.set_resume(&AgentId("nope".into()), None).unwrap_err();
    assert!(matches!(err, PtyError::UnknownAgent(_)), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_spawn_keeps_the_resume_id_armed() {
    // A gate that refuses once, then allows.
    struct RefuseOnce(std::sync::atomic::AtomicBool);
    impl coretempo_core::pty::SpawnGate for RefuseOnce {
        fn before_spawn(&self, _agent: &AgentId, _dir: &Path) -> Result<(), String> {
            if self.0.swap(false, Ordering::SeqCst) {
                Err("refused once".into())
            } else {
                Ok(())
            }
        }
    }
    let dir = fresh_dir();
    let (mgr, _bus) = empty_manager(&dir);
    mgr.set_spawn_gate(Arc::new(RefuseOnce(std::sync::atomic::AtomicBool::new(
        true,
    ))));
    let id = AgentId("s-1".into());
    mgr.add_agent(id.clone(), entry(&dir)).unwrap();
    mgr.set_resume(&id, Some("abc-123".into())).unwrap();
    assert!(matches!(
        mgr.spawn(&id).await.unwrap_err(),
        PtyError::Spawn { .. }
    ));
    mgr.spawn(&id).await.unwrap();
    mgr.write(&id, b"quit\n").await.unwrap();
    wait_state(&mgr, &id, AgentState::Exited).await;
    assert_eq!(
        recorded_argv(&dir),
        vec![vec!["--resume".to_string(), "abc-123".to_string()]],
        "the refused attempt did not consume the id"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_reaps_records_the_exit_and_keeps_the_handle() {
    let dir = fresh_dir();
    let (mgr, _bus) = empty_manager(&dir);
    let id = AgentId("s-1".into());
    mgr.add_agent(id.clone(), entry(&dir)).unwrap();
    mgr.spawn(&id).await.unwrap();
    let mut out = mgr.subscribe_output(&id, None).unwrap();
    mgr.write(&id, b"ping\n").await.unwrap();
    // Drain until the echo shows up so the ring has content before the stop.
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains("got:ping") {
        let chunk = tokio::time::timeout(DEADLINE, out.recv())
            .await
            .unwrap()
            .unwrap();
        seen.extend(chunk.bytes);
    }

    mgr.stop(&id).await.unwrap();

    assert_eq!(mgr.state(&id).unwrap(), AgentState::Exited);
    assert!(
        matches!(mgr.exit(&id).unwrap(), Some(AgentExit::Signal(_))),
        "SIGHUP-terminated bash reports a signal; got {:?}",
        mgr.exit(&id).unwrap()
    );
    let (_, tail) = mgr.read_ring(&id, None).unwrap();
    assert!(
        String::from_utf8_lossy(&tail).contains("got:ping"),
        "ring kept after stop"
    );
    assert!(
        !matches!(
            out.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ),
        "subscribers outlive the process so a resume continues the same stream"
    );

    mgr.spawn(&id).await.unwrap();
    mgr.write(&id, b"again\n").await.unwrap();
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains("got:again") {
        let chunk = tokio::time::timeout(DEADLINE, out.recv())
            .await
            .unwrap()
            .unwrap();
        seen.extend(chunk.bytes);
    }
    mgr.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_clears_the_blocked_flag() {
    let dir = fresh_dir();
    let (mgr, bus) = empty_manager(&dir);
    let id = AgentId("s-1".into());
    mgr.add_agent(id.clone(), entry(&dir)).unwrap();
    mgr.spawn(&id).await.unwrap();
    mgr.report_state(&id, AgentState::Working).unwrap();
    let mut events = bus.subscribe();
    mgr.report_blocked(&id, Some("Bash".into()), None).unwrap();
    assert!(mgr.blocked(&id).unwrap());

    mgr.stop(&id).await.unwrap();

    assert!(!mgr.blocked(&id).unwrap());
    let mut saw_unblock = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await {
        if let coretempo_core::types::EventPayload::AgentBlocked { agent, blocked, .. } = ev.payload
            && agent == id
            && !blocked
        {
            saw_unblock = true;
            break;
        }
    }
    assert!(saw_unblock, "stop publishes blocked: false");
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_without_a_live_session_is_an_error() {
    let dir = fresh_dir();
    let (mgr, _bus) = empty_manager(&dir);
    let id = AgentId("s-1".into());
    mgr.add_agent(id.clone(), entry(&dir)).unwrap();
    let err = mgr.stop(&id).await.unwrap_err();
    assert!(matches!(err, PtyError::AgentExited(_)), "{err}");
    let err = mgr.stop(&AgentId("nope".into())).await.unwrap_err();
    assert!(matches!(err, PtyError::UnknownAgent(_)), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn remove_agent_stops_it_and_ends_its_subscribers() {
    let dir = fresh_dir();
    let (mgr, _bus) = empty_manager(&dir);
    let id = AgentId("s-1".into());
    mgr.add_agent(id.clone(), entry(&dir)).unwrap();
    mgr.spawn(&id).await.unwrap();
    let mut out = mgr.subscribe_output(&id, None).unwrap();
    let mut state = mgr.subscribe_state_raw(&id).unwrap();

    mgr.remove_agent(&id).await.unwrap();

    // Drain whatever was buffered; the channel must then close.
    let closed = tokio::time::timeout(DEADLINE, async { while out.recv().await.is_some() {} })
        .await
        .is_ok();
    assert!(closed, "output subscriber ends when the agent is removed");
    assert!(
        tokio::time::timeout(DEADLINE, async { while state.changed().await.is_ok() {} })
            .await
            .is_ok(),
        "state subscriber ends when the agent is removed"
    );
    assert!(matches!(
        mgr.state(&id).unwrap_err(),
        PtyError::UnknownAgent(_)
    ));
    assert!(
        mgr.add_agent(id.clone(), entry(&dir)).is_ok(),
        "the id is free again"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn remove_agent_works_on_a_stopped_agent_too() {
    let dir = fresh_dir();
    let (mgr, _bus) = empty_manager(&dir);
    let id = AgentId("s-1".into());
    mgr.add_agent(id.clone(), entry(&dir)).unwrap();
    mgr.spawn(&id).await.unwrap();
    mgr.stop(&id).await.unwrap();
    mgr.remove_agent(&id).await.unwrap();
    assert!(matches!(
        mgr.remove_agent(&id).await.unwrap_err(),
        PtyError::UnknownAgent(_)
    ));
}
