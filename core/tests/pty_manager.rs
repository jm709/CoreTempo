//! Integration tests: `PtyManager` against a scripted fake agent (spec §13 —
//! "scripted PTY echo process, not real claude").
#![cfg(unix)]
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test helpers outside #[test] fns are not covered by allow-*-in-tests"
)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use coretempo_core::bus::EventBus;
use coretempo_core::pty::{AgentEnv, Cursor, InjectError, InjectionQueue, PtyError, PtyManager};
use coretempo_core::types::config::AgentConfig;
use coretempo_core::types::config::FrozenWorkflow;
use coretempo_core::types::{AgentExit, AgentId, AgentState, EventPayload, LifecyclePhase, Token};

const IDLE_DEBOUNCE: Duration = Duration::from_millis(100);
const DEADLINE: Duration = Duration::from_secs(10);

fn write_fake_agent(dir: &Path) -> PathBuf {
    let path = dir.join("fake-agent.sh");
    let script = concat!(
        "#!/usr/bin/env bash\n",
        "printf 'booted\\n'\n",
        "while IFS= read -r line; do\n",
        "  case \"$line\" in\n",
        "    quit) exit 3 ;;\n",
        "    die) kill -TERM $$ ;;\n",
        "    nap) sleep 30 ;;\n",
        "    size) stty size ;;\n",
        "    spam)\n",
        "      for i in $(seq 1 2000); do printf 'line-%s\\n' \"$i\"; done\n",
        "      printf 'spam-done\\n' ;;\n",
        "    *) printf 'got:%s\\n' \"$line\" ;;\n",
        "  esac\n",
        "done\n",
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn workflow(dir: &Path) -> Arc<FrozenWorkflow> {
    let mut agents = BTreeMap::new();
    agents.insert(
        AgentId("fake".into()),
        AgentConfig {
            auto_clear: false, // auto-/clear behavior is covered by queue unit tests
            ..AgentConfig::new(dir.to_path_buf(), "test prompt")
        },
    );
    Arc::new(FrozenWorkflow {
        name: "pty-test".into(),
        hash: "0".repeat(64),
        source_path: dir.join("tempo.toml"),
        ask_timeout: Duration::from_mins(30),
        idle_debounce: IDLE_DEBOUNCE,
        scrollback: 5_000,
        agents,
        mcp_servers: BTreeMap::new(),
        flows: BTreeMap::new(),
    })
}

/// Boot-scoped temp dir (no tempdir crate); the OS cleans it up. Each boot gets
/// its own dir: rewriting one script while another test's bash still reads it
/// corrupts that running agent.
fn fresh_dir() -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("coretempo-pty-{}-{n}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp);
    tmp
}

/// The manager over the scripted fake agent in `dir`, before any spawn.
fn fake_manager(dir: &Path) -> (Arc<PtyManager>, EventBus) {
    let script = write_fake_agent(dir);
    let bus = EventBus::new();
    let env = AgentEnv {
        port: 4820,
        token: Token("ab".repeat(32)),
        tempo_bin_dir: PathBuf::from("/usr/bin"),
        settings_paths: std::collections::BTreeMap::new(),
        mcp_paths: std::collections::BTreeMap::new(),
        config_dirs: std::collections::BTreeMap::new(),
        credential_store: None,
    };
    let mgr =
        PtyManager::new_with_program(workflow(dir), bus.clone(), env, script.to_str().unwrap());
    (mgr, bus)
}

async fn boot() -> (Arc<PtyManager>, EventBus, AgentId, PathBuf) {
    let tmp = fresh_dir();
    let (mgr, bus) = fake_manager(&tmp);
    mgr.spawn_all().await.unwrap();
    (mgr, bus, AgentId("fake".into()), tmp)
}

/// [`boot`] stopping short of the spawn: a `SpawnGate` has to be installed
/// before the first one. Yields the manager, the agent's frozen dir and its id.
#[expect(
    clippy::unused_async,
    reason = "sibling of boot(); awaited the same way at every call site"
)]
async fn fake_manager_unspawned() -> (Arc<PtyManager>, PathBuf, AgentId) {
    let tmp = fresh_dir();
    let (mgr, _bus) = fake_manager(&tmp);
    (mgr, tmp, AgentId("fake".into()))
}

async fn wait_state(mgr: &PtyManager, agent: &AgentId, want: AgentState) {
    let mut rx = mgr.subscribe_state_debounced(agent).unwrap();
    tokio::time::timeout(DEADLINE, rx.wait_for(|s| *s == want))
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {want:?}"))
        .unwrap();
}

/// Stands in for the Claude Code hook that reports the state, then waits for it
/// to clear the idle debouncer.
async fn go_idle(mgr: &PtyManager, agent: &AgentId) {
    mgr.report_state(agent, AgentState::Idle).unwrap();
    wait_state(mgr, agent, AgentState::Idle).await;
}

async fn wait_ring_contains(mgr: &PtyManager, agent: &AgentId, needle: &[u8]) {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let (_, bytes) = mgr.read_ring(agent, None).unwrap();
        if bytes.windows(needle.len()).any(|w| w == needle) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "ring never contained {:?}",
            String::from_utf8_lossy(needle)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_state_event(
    rx: &mut tokio::sync::broadcast::Receiver<coretempo_core::types::Event>,
) -> AgentState {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = tokio::time::timeout(remaining, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for an agent.state event"))
            .unwrap();
        if let EventPayload::AgentStateChanged { state, .. } = event.payload {
            return state;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn reported_state_drives_debounced_signal_and_bus() {
    let (mgr, bus, agent, _tmp) = boot().await;
    let mut events = bus.subscribe();
    assert_eq!(
        mgr.state(&agent).unwrap(),
        AgentState::Starting,
        "an agent stays starting until a state is reported"
    );

    mgr.report_state(&agent, AgentState::Idle).unwrap();

    wait_state(&mgr, &agent, AgentState::Idle).await;
    assert_eq!(wait_state_event(&mut events).await, AgentState::Idle);
}

#[tokio::test(flavor = "multi_thread")]
async fn reporting_the_same_state_twice_publishes_one_event() {
    let (mgr, bus, agent, _tmp) = boot().await;
    let mut events = bus.subscribe();

    mgr.report_state(&agent, AgentState::Idle).unwrap();
    mgr.report_state(&agent, AgentState::Idle).unwrap();
    mgr.report_state(&agent, AgentState::Working).unwrap();

    assert_eq!(wait_state_event(&mut events).await, AgentState::Idle);
    assert_eq!(
        wait_state_event(&mut events).await,
        AgentState::Working,
        "the duplicate idle report published nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn report_state_rejects_an_unknown_agent() {
    let (mgr, _bus, _agent, _tmp) = boot().await;
    let ghost = AgentId("ghost".into());

    let err = mgr.report_state(&ghost, AgentState::Idle).unwrap_err();

    let PtyError::UnknownAgent(id) = &err else {
        panic!("expected UnknownAgent, got {err:?}")
    };
    assert_eq!(*id, ghost);
}

#[tokio::test(flavor = "multi_thread")]
async fn injects_on_reported_debounced_idle() {
    let (mgr, _bus, agent, _tmp) = boot().await;
    go_idle(&mgr, &agent).await;

    let done = mgr.enqueue(
        agent.clone(),
        "[CoreTempo m-11111111 from planner] hello".into(),
    );
    let injected = tokio::time::timeout(DEADLINE, done)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    wait_ring_contains(
        &mgr,
        &agent,
        b"got:[CoreTempo m-11111111 from planner] hello",
    )
    .await;
    let (end, _) = mgr.read_ring(&agent, None).unwrap();
    assert!(
        injected.cursor.0 <= end.0,
        "injection cursor is a valid stream position"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn subscribe_output_replays_tail_contiguously() {
    let (mgr, _bus, agent, _tmp) = boot().await;
    wait_ring_contains(&mgr, &agent, b"booted").await;

    let (end, all) = mgr.read_ring(&agent, None).unwrap();
    let since = Cursor(end.0.saturating_sub(4));
    let mut rx = mgr.subscribe_output(&agent, Some(since)).unwrap();
    let first = tokio::time::timeout(DEADLINE, rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        first.start, since,
        "tail replay starts exactly at the cursor"
    );
    assert_eq!(first.bytes, all[all.len() - 4..].to_vec());

    // live continuation is gap-free: next chunk starts where the tail ended
    mgr.write(&agent, b"ping\r").await.unwrap();
    let next = tokio::time::timeout(DEADLINE, rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.start.0, first.start.0 + first.bytes.len() as u64);
}

#[tokio::test(flavor = "multi_thread")]
async fn backpressure_pause_stops_and_resume_restores() {
    let (mgr, _bus, agent, _tmp) = boot().await;
    wait_ring_contains(&mgr, &agent, b"booted").await;

    mgr.pause_output(&agent, true);
    mgr.write(&agent, b"spam\r").await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (c1, _) = mgr.read_ring(&agent, None).unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (c2, paused_bytes) = mgr.read_ring(&agent, None).unwrap();
    assert_eq!(c1, c2, "cursor must stop advancing while paused");
    assert!(
        !paused_bytes.windows(9).any(|w| w == b"spam-done"),
        "a parked reader must not drain the whole burst (at most one in-flight read)"
    );

    mgr.pause_output(&agent, false);
    wait_ring_contains(&mgr, &agent, b"spam-done").await;
}

async fn wait_lifecycle(
    rx: &mut tokio::sync::broadcast::Receiver<coretempo_core::types::Event>,
    want_phase: LifecyclePhase,
) -> Option<AgentExit> {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = tokio::time::timeout(remaining, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for lifecycle {want_phase:?}"))
            .unwrap();
        if let EventPayload::AgentLifecycle { phase, exit, .. } = event.payload
            && phase == want_phase
        {
            return exit;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn exit_emits_lifecycle_fact_and_fails_new_injections() {
    let (mgr, bus, agent, _tmp) = boot().await;
    let mut events = bus.subscribe();
    wait_ring_contains(&mgr, &agent, b"booted").await;

    mgr.write(&agent, b"quit\r").await.unwrap();

    let exit = wait_lifecycle(&mut events, LifecyclePhase::Exited).await;
    assert_eq!(
        exit,
        Some(AgentExit::Code(3)),
        "fake agent exits 3 on 'quit'"
    );
    assert_eq!(mgr.state(&agent).unwrap(), AgentState::Exited);
    assert_eq!(mgr.exit(&agent).unwrap(), Some(AgentExit::Code(3)));

    // messages to an exited agent fail fast rather than queueing forever
    let done = mgr.enqueue(agent.clone(), "too late".into());
    assert_eq!(
        tokio::time::timeout(DEADLINE, done)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err(),
        InjectError::AgentExited(agent.clone())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_fails_queued_injections_and_respawns() {
    let (mgr, bus, agent, _tmp) = boot().await;
    let mut events = bus.subscribe();
    go_idle(&mgr, &agent).await;

    // put the agent to work, then queue a message behind the busy turn.
    // Wait on the DEBOUNCED working signal (non-idle propagates immediately):
    // the queue worker gates on the same channel, so once we observe working,
    // the next enqueue is guaranteed to sit in the queue rather than inject.
    let nap = mgr.enqueue(agent.clone(), "nap".into());
    tokio::time::timeout(DEADLINE, nap)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    mgr.report_state(&agent, AgentState::Working).unwrap();
    wait_state(&mgr, &agent, AgentState::Working).await;
    let queued = mgr.enqueue(agent.clone(), "stuck behind nap".into());

    let (before_restart, _) = mgr.read_ring(&agent, None).unwrap();
    mgr.restart(&agent).await.unwrap();

    // the lifecycle facts: restarting → spawned, in that order
    assert_eq!(
        wait_lifecycle(&mut events, LifecyclePhase::Restarting).await,
        None
    );
    assert_eq!(
        wait_lifecycle(&mut events, LifecyclePhase::Spawned).await,
        None
    );

    // queued message to the restarted agent failed (messaging maps this to `failed`)
    assert_eq!(
        tokio::time::timeout(DEADLINE, queued)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err(),
        InjectError::AgentRestarted(agent.clone())
    );

    // the respawned session reports idle and accepts injections again
    assert_eq!(
        mgr.state(&agent).unwrap(),
        AgentState::Starting,
        "a respawned session starts over at starting"
    );
    go_idle(&mgr, &agent).await;
    let again = mgr.enqueue(agent.clone(), "hello again".into());
    tokio::time::timeout(DEADLINE, again)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    wait_ring_contains(&mgr, &agent, b"got:hello again").await;

    // monotonic byte cursors survive the restart (ring is per-agent, not per-session)
    let (after_restart, _) = mgr.read_ring(&agent, None).unwrap();
    assert!(
        after_restart.0 > before_restart.0,
        "cursor keeps growing across restart"
    );
}

/// A hook from a dying session can fire after the PTY is gone. Reviving an
/// exited agent would let the queue inject into a dead PTY, where writes are
/// silently dropped.
#[tokio::test(flavor = "multi_thread")]
async fn reported_state_cannot_revive_an_exited_agent() {
    let (mgr, bus, agent, _tmp) = boot().await;
    let mut events = bus.subscribe();
    wait_ring_contains(&mgr, &agent, b"booted").await;

    mgr.write(&agent, b"quit\r").await.unwrap();
    wait_lifecycle(&mut events, LifecyclePhase::Exited).await;
    assert_eq!(mgr.state(&agent).unwrap(), AgentState::Exited);

    mgr.report_state(&agent, AgentState::Idle).unwrap();

    assert_eq!(
        mgr.state(&agent).unwrap(),
        AgentState::Exited,
        "a late hook must not resurrect an exited agent"
    );
    assert_eq!(
        tokio::time::timeout(DEADLINE, mgr.enqueue(agent.clone(), "after".into()))
            .await
            .unwrap()
            .unwrap()
            .unwrap_err(),
        InjectError::AgentExited(agent.clone()),
        "injection must still fail fast after the late report"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_depth_rejects_an_unknown_agent() {
    let (mgr, _bus, _agent, _tmp) = boot().await;
    let ghost = AgentId("ghost".into());

    let err = mgr.queue_depth(&ghost).unwrap_err();

    let PtyError::UnknownAgent(id) = &err else {
        panic!("expected UnknownAgent, got {err:?}")
    };
    assert_eq!(*id, ghost);
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_depth_tracks_enqueue_and_delivery() {
    let (mgr, _bus, agent, _tmp) = boot().await;
    go_idle(&mgr, &agent).await;

    // hold the agent busy so the enqueue below sits in the queue, not delivered.
    mgr.report_state(&agent, AgentState::Working).unwrap();
    wait_state(&mgr, &agent, AgentState::Working).await;

    assert_eq!(mgr.queue_depth(&agent).unwrap(), 0);
    let done = mgr.enqueue(agent.clone(), "hello".into());
    assert_eq!(mgr.queue_depth(&agent).unwrap(), 1, "queued while working");

    go_idle(&mgr, &agent).await;
    tokio::time::timeout(DEADLINE, done)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        mgr.queue_depth(&agent).unwrap(),
        0,
        "delivered injections drain the depth"
    );
}

/// Drains the bus up to the next `agent.blocked` event, ignoring everything else.
fn next_blocked(
    rx: &mut tokio::sync::broadcast::Receiver<coretempo_core::types::Event>,
) -> Option<(bool, Option<String>)> {
    while let Ok(event) = rx.try_recv() {
        if let EventPayload::AgentBlocked { blocked, tool, .. } = event.payload {
            return Some((blocked, tool));
        }
    }
    None
}

/// [`next_blocked`], but waiting: the clearing event for a child exit is
/// published by the exit watcher *after* it sends the raw state the test woke
/// on, so a bare `try_recv` there races the publish.
async fn wait_blocked(
    rx: &mut tokio::sync::broadcast::Receiver<coretempo_core::types::Event>,
) -> Option<(bool, Option<String>)> {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        if let Some(event) = next_blocked(rx) {
            return Some(event);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Drains the bus up to the next `agent.permission_refused` event.
fn next_refused(
    rx: &mut tokio::sync::broadcast::Receiver<coretempo_core::types::Event>,
) -> Option<(AgentId, Option<String>, Option<String>)> {
    while let Ok(event) = rx.try_recv() {
        if let EventPayload::AgentPermissionRefused { agent, tool, input } = event.payload {
            return Some((agent, tool, input));
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread")]
async fn refused_report_publishes_the_tool_and_leaves_the_flag_clear() {
    let (mgr, bus, agent, _tmp) = boot().await;
    let mut rx = bus.subscribe();
    mgr.report_state(&agent, AgentState::Working).unwrap();
    mgr.report_refused(&agent, Some("Bash".into()), Some("mkdir x".into()))
        .unwrap();
    assert_eq!(
        next_refused(&mut rx),
        Some((agent.clone(), Some("Bash".into()), Some("mkdir x".into())))
    );
    assert!(!mgr.blocked(&agent).unwrap(), "a refusal is not a dialog");
    assert_eq!(mgr.blocked_count(), 0);
    assert_eq!(
        next_blocked(&mut rx),
        None,
        "no agent.blocked for a refusal"
    );
    let ghost = AgentId("ghost".into());
    assert!(matches!(
        mgr.report_refused(&ghost, None, None),
        Err(PtyError::UnknownAgent(_))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn blocked_report_sets_the_flag_once_while_working() {
    let (mgr, bus, agent, _tmp) = boot().await;
    let mut rx = bus.subscribe();
    mgr.report_state(&agent, AgentState::Working).unwrap();
    mgr.report_blocked(&agent, Some("Read".into()), None)
        .unwrap();
    assert!(mgr.blocked(&agent).unwrap());
    assert_eq!(mgr.blocked_count(), 1);
    assert_eq!(next_blocked(&mut rx), Some((true, Some("Read".into()))));
    mgr.report_blocked(&agent, Some("Read".into()), None)
        .unwrap();
    assert_eq!(
        next_blocked(&mut rx),
        None,
        "repeat report does not republish"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn blocked_report_is_accepted_at_idle_and_dropped_at_starting() {
    // A Claude Code subagent's dialog fires the parent's PermissionRequest hook
    // while the parent is idle (spec 2026-08-17 §4.2, live 2026-08-18).
    let (mgr, bus, agent, _tmp) = boot().await;
    let mut rx = bus.subscribe();
    assert_eq!(mgr.state(&agent).unwrap(), AgentState::Starting);
    mgr.report_blocked(&agent, None, None).unwrap();
    assert!(
        !mgr.blocked(&agent).unwrap(),
        "starting: no session, no dialog"
    );
    go_idle(&mgr, &agent).await;
    mgr.report_blocked(&agent, Some("Bash".into()), None)
        .unwrap();
    assert!(
        mgr.blocked(&agent).unwrap(),
        "idle: a subagent dialog counts"
    );
    assert_eq!(next_blocked(&mut rx), Some((true, Some("Bash".into()))));
    let since = mgr.blocked_since(&agent).unwrap().unwrap();
    assert_eq!(since.tool.as_deref(), Some("Bash"));
    // Repeat keeps the first `since` and is silent.
    mgr.report_blocked(&agent, Some("Read".into()), None)
        .unwrap();
    assert_eq!(
        mgr.blocked_since(&agent).unwrap().unwrap().since,
        since.since
    );
    assert_eq!(next_blocked(&mut rx), None);
    // An idle report while already idle (Stop already fired) keeps the flag.
    mgr.report_state(&agent, AgentState::Idle).unwrap();
    assert!(
        mgr.blocked(&agent).unwrap(),
        "idle-while-idle must not clear"
    );
    // A real transition clears it.
    mgr.report_state(&agent, AgentState::Working).unwrap();
    assert!(!mgr.blocked(&agent).unwrap());
    assert_eq!(next_blocked(&mut rx), Some((false, None)));
}

#[tokio::test(flavor = "multi_thread")]
async fn unblocked_and_state_reports_clear_the_flag_once() {
    let (mgr, bus, agent, _tmp) = boot().await;
    let mut rx = bus.subscribe();
    mgr.report_state(&agent, AgentState::Working).unwrap();
    mgr.report_blocked(&agent, Some("Bash".into()), None)
        .unwrap();
    assert_eq!(next_blocked(&mut rx), Some((true, Some("Bash".into()))));
    mgr.report_unblocked(&agent, None).unwrap();
    assert_eq!(next_blocked(&mut rx), Some((false, None)));
    mgr.report_unblocked(&agent, None).unwrap();
    assert_eq!(next_blocked(&mut rx), None, "second clear is silent");

    mgr.report_blocked(&agent, None, None).unwrap();
    assert_eq!(next_blocked(&mut rx), Some((true, None)));
    // Same raw state again: no transition, so the dialog is still up.
    mgr.report_state(&agent, AgentState::Working).unwrap();
    assert!(mgr.blocked(&agent).unwrap());
    assert_eq!(next_blocked(&mut rx), None);
    mgr.report_unblocked(&agent, None).unwrap();
    assert_eq!(next_blocked(&mut rx), Some((false, None)));

    mgr.report_blocked(&agent, None, None).unwrap();
    assert_eq!(next_blocked(&mut rx), Some((true, None)));
    go_idle(&mgr, &agent).await;
    assert_eq!(next_blocked(&mut rx), Some((false, None)));
}

/// Live 2026-08-18: a Claude Code helper agent fired `PostToolBatch` ("No tools
/// needed for summary") 28 s into a *subagent's* permission dialog, clearing the
/// flag while the dialog was still on screen and disarming the fail-fast. Only a
/// report from the agent the dialog belongs to may clear it.
#[tokio::test(flavor = "multi_thread")]
async fn unblocked_from_another_agent_does_not_clear_the_dialog() {
    let (mgr, bus, agent, _tmp) = boot().await;
    let mut rx = bus.subscribe();
    go_idle(&mgr, &agent).await;
    mgr.report_blocked(&agent, Some("Bash".into()), Some("a9c8".into()))
        .unwrap();
    assert_eq!(next_blocked(&mut rx), Some((true, Some("Bash".into()))));

    mgr.report_unblocked(&agent, Some("ac3c".into())).unwrap();
    assert!(
        mgr.blocked(&agent).unwrap(),
        "a sibling helper agent's PostToolBatch leaves the dialog up"
    );
    assert_eq!(next_blocked(&mut rx), None, "and publishes nothing");

    mgr.report_unblocked(&agent, Some("a9c8".into())).unwrap();
    assert!(!mgr.blocked(&agent).unwrap());
    assert_eq!(next_blocked(&mut rx), Some((false, None)));
    assert_eq!(next_blocked(&mut rx), None, "cleared exactly once");
}

/// A main-session `PermissionRequest`/`PostToolBatch` payload has no `agent_id`
/// at all, so `None` is a value that must match `None` and nothing else.
#[tokio::test(flavor = "multi_thread")]
async fn main_session_unblock_matches_a_main_session_block() {
    let (mgr, bus, agent, _tmp) = boot().await;
    let mut rx = bus.subscribe();
    mgr.report_state(&agent, AgentState::Working).unwrap();
    mgr.report_blocked(&agent, Some("Read".into()), None)
        .unwrap();
    assert_eq!(next_blocked(&mut rx), Some((true, Some("Read".into()))));

    mgr.report_unblocked(&agent, Some("x".into())).unwrap();
    assert!(
        mgr.blocked(&agent).unwrap(),
        "a subagent cannot answer the main session's dialog"
    );
    assert_eq!(next_blocked(&mut rx), None);

    mgr.report_unblocked(&agent, None).unwrap();
    assert!(!mgr.blocked(&agent).unwrap());
    assert_eq!(next_blocked(&mut rx), Some((false, None)));
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_and_exit_clear_the_flag() {
    let (mgr, bus, agent, _tmp) = boot().await;
    let mut rx = bus.subscribe();
    mgr.report_state(&agent, AgentState::Working).unwrap();
    mgr.report_blocked(&agent, None, None).unwrap();
    assert_eq!(next_blocked(&mut rx), Some((true, None)));
    mgr.restart(&agent).await.unwrap();
    assert!(!mgr.blocked(&agent).unwrap());
    assert_eq!(next_blocked(&mut rx), Some((false, None)));

    wait_ring_contains(&mgr, &agent, b"booted").await;
    mgr.report_state(&agent, AgentState::Working).unwrap();
    mgr.report_blocked(&agent, None, None).unwrap();
    assert_eq!(next_blocked(&mut rx), Some((true, None)));
    mgr.write(&agent, b"quit\r").await.unwrap();
    wait_state(&mgr, &agent, AgentState::Exited).await;
    assert!(!mgr.blocked(&agent).unwrap());
    assert_eq!(wait_blocked(&mut rx).await, Some((false, None)));
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_clears_the_flag() {
    let (mgr, bus, agent, _tmp) = boot().await;
    let mut rx = bus.subscribe();
    mgr.report_state(&agent, AgentState::Working).unwrap();
    mgr.report_blocked(&agent, Some("Read".into()), None)
        .unwrap();
    assert_eq!(next_blocked(&mut rx), Some((true, Some("Read".into()))));

    mgr.shutdown().await;

    assert!(!mgr.blocked(&agent).unwrap());
    assert_eq!(mgr.blocked_count(), 0);
    assert_eq!(next_blocked(&mut rx), Some((false, None)));
}

#[tokio::test(flavor = "multi_thread")]
async fn blocked_reports_reject_an_unknown_agent() {
    let (mgr, _bus, _agent, _tmp) = boot().await;
    let ghost = AgentId("ghost".into());

    for err in [
        mgr.report_blocked(&ghost, Some("Read".into()), None)
            .unwrap_err(),
        mgr.report_unblocked(&ghost, None).unwrap_err(),
    ] {
        let PtyError::UnknownAgent(id) = &err else {
            panic!("expected UnknownAgent, got {err:?}")
        };
        assert_eq!(*id, ghost);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn trust_gate_regrants_a_reverted_key_on_restart() {
    use coretempo_core::trust::{TrustGate, TrustPolicy, TrustStore, trust_root};
    let (mgr, dir, agent) = fake_manager_unspawned().await;
    let store_path = dir.join("claude.json");
    let store = TrustStore::at(store_path.clone());
    mgr.set_spawn_gate(std::sync::Arc::new(TrustGate::new(
        store.clone(),
        TrustPolicy { grant: true },
        std::collections::BTreeMap::new(),
    )));
    mgr.spawn(&agent).await.unwrap();
    let root = trust_root(&dir);
    assert!(
        store.untrusted_roots([dir.as_path()]).unwrap().is_empty(),
        "spawn granted {root:?}"
    );
    // A live Claude session flushes its own copy: revert the key.
    std::fs::write(&store_path, r#"{"projects": {}}"#).unwrap();
    assert_eq!(store.untrusted_roots([dir.as_path()]).unwrap(), vec![root]);
    mgr.restart(&agent).await.unwrap();
    assert!(
        store.untrusted_roots([dir.as_path()]).unwrap().is_empty(),
        "restart re-granted"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn trust_gate_without_grant_fails_the_spawn_naming_the_root() {
    use coretempo_core::pty::PtyError;
    use coretempo_core::trust::{TrustGate, TrustPolicy, TrustStore, trust_root};
    let (mgr, dir, agent) = fake_manager_unspawned().await;
    let store = TrustStore::at(dir.join("claude.json"));
    mgr.set_spawn_gate(std::sync::Arc::new(TrustGate::new(
        store,
        TrustPolicy::default(),
        std::collections::BTreeMap::new(),
    )));
    let err = mgr.spawn(&agent).await.expect_err("untrusted, no grant");
    let PtyError::Spawn { agent: who, reason } = &err else {
        panic!("expected Spawn, got {err:?}");
    };
    assert_eq!(*who, agent);
    let root = trust_root(&dir).display().to_string();
    assert!(
        reason.contains(&root) && reason.contains("trust_agent_dirs"),
        "{reason}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn trust_gate_without_grant_fails_a_restart_after_the_key_is_reverted() {
    use coretempo_core::pty::PtyError;
    use coretempo_core::trust::{TrustGate, TrustPolicy, TrustStore, trust_root};
    let (mgr, dir, agent) = fake_manager_unspawned().await;
    let store_path = dir.join("claude.json");
    let root = trust_root(&dir);
    // Pre-trusted, so the first spawn passes without a grant …
    std::fs::write(
        &store_path,
        serde_json::json!({"projects": {root.to_string_lossy(): {"hasTrustDialogAccepted": true}}})
            .to_string(),
    )
    .unwrap();
    let store = TrustStore::at(store_path.clone());
    mgr.set_spawn_gate(std::sync::Arc::new(TrustGate::new(
        store,
        TrustPolicy::default(),
        std::collections::BTreeMap::new(),
    )));
    mgr.spawn(&agent).await.unwrap();
    // … then the key is reverted and the restart must fail rather than park.
    std::fs::write(&store_path, r#"{"projects": {}}"#).unwrap();
    let err = mgr.restart(&agent).await.expect_err("no grant, untrusted");
    assert!(matches!(err, PtyError::Spawn { .. }), "{err:?}");
}

/// A gate-refused restart must not leave the agent parked in `Restarting` with
/// no session: nothing downstream would ever move it again, and the API's
/// `begin_restart` only logs the error. The refusal ends the agent instead.
#[tokio::test(flavor = "multi_thread")]
async fn a_gate_refused_restart_leaves_the_agent_exited() {
    use coretempo_core::pty::PtyError;
    use coretempo_core::trust::{TrustGate, TrustPolicy, TrustStore, trust_root};
    let tmp = fresh_dir();
    let (mgr, bus) = fake_manager(&tmp);
    let agent = AgentId("fake".into());
    let store_path = tmp.join("claude.json");
    let root = trust_root(&tmp);
    std::fs::write(
        &store_path,
        serde_json::json!({"projects": {root.to_string_lossy(): {"hasTrustDialogAccepted": true}}})
            .to_string(),
    )
    .unwrap();
    mgr.set_spawn_gate(Arc::new(TrustGate::new(
        TrustStore::at(store_path.clone()),
        TrustPolicy::default(),
        std::collections::BTreeMap::new(),
    )));
    mgr.spawn(&agent).await.unwrap();
    std::fs::write(&store_path, r#"{"projects": {}}"#).unwrap();

    let err = mgr.restart(&agent).await.expect_err("no grant, untrusted");

    assert!(matches!(err, PtyError::Spawn { .. }), "{err:?}");
    assert_eq!(
        mgr.state(&agent).unwrap(),
        AgentState::Exited,
        "a session-less agent left in Restarting never moves again"
    );
    let events = bus.replay_since(0).unwrap();
    let last_state = events.iter().rev().find_map(|event| match &event.payload {
        EventPayload::AgentStateChanged { state, .. } => Some(*state),
        _ => None,
    });
    assert_eq!(last_state, Some(AgentState::Exited), "events: {events:?}");
    assert!(
        events.iter().any(|event| matches!(
            &event.payload,
            EventPayload::AgentLifecycle {
                phase: LifecyclePhase::Exited,
                exit: None,
                ..
            }
        )),
        "no agent.lifecycle Exited was published: {events:?}"
    );
}

/// #63: a blocked report at idle (a subagent's dialog after the parent's
/// `Stop`) parks injections — the queue worker sees the manager's flag — and
/// releases them when the dialog is reported answered.
#[tokio::test(flavor = "multi_thread")]
async fn injection_parks_while_idle_and_blocked_until_unblocked() {
    let (mgr, _bus, agent, _tmp) = boot().await;
    go_idle(&mgr, &agent).await;
    mgr.report_blocked(&agent, Some("Bash".into()), None)
        .unwrap();

    let mut done = mgr.enqueue(agent.clone(), "into the dialog?".into());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(done.try_recv().is_err(), "parked while the dialog is up");
    let (_, bytes) = mgr.read_ring(&agent, None).unwrap();
    assert!(
        !bytes.windows(4).any(|w| w == b"got:"),
        "nothing typed into a dialog"
    );
    assert_eq!(mgr.queue_depth(&agent).unwrap(), 1);

    mgr.report_unblocked(&agent, None).unwrap();
    tokio::time::timeout(DEADLINE, done)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    wait_ring_contains(&mgr, &agent, b"got:into the dialog?").await;
}

/// Bytes the ring gained after `since` contain `needle`. The restart test
/// below needs "after the respawn", not "ever": the first session's output
/// stays in the ring.
async fn wait_ring_contains_since(mgr: &PtyManager, agent: &AgentId, since: Cursor, needle: &[u8]) {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let (_, bytes) = mgr.read_ring(agent, Some(since)).unwrap();
        if bytes.windows(needle.len()).any(|w| w == needle) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "ring never contained {:?} after cursor {}; saw {:?}",
            String::from_utf8_lossy(needle),
            since.0,
            String::from_utf8_lossy(&bytes)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A restarted session opens at the size the pane last reported, not the spawn
/// default. The desktop only sends a resize when xterm's own dimensions change,
/// which a restart never does, so a respawn at the default left Claude Code
/// drawing a 120×40 TUI inside a pane of some other size.
#[tokio::test(flavor = "multi_thread")]
async fn restart_reopens_the_pty_at_the_last_reported_size() {
    let (mgr, bus, agent, _tmp) = boot().await;
    let mut events = bus.subscribe();
    wait_ring_contains(&mgr, &agent, b"booted").await;

    mgr.resize(&agent, 100, 30).await.unwrap();
    mgr.write(&agent, b"size\n").await.unwrap();
    wait_ring_contains(&mgr, &agent, b"30 100").await;

    let (before_restart, _) = mgr.read_ring(&agent, None).unwrap();
    mgr.restart(&agent).await.unwrap();
    assert_eq!(
        wait_lifecycle(&mut events, LifecyclePhase::Restarting).await,
        None
    );
    assert_eq!(
        wait_lifecycle(&mut events, LifecyclePhase::Spawned).await,
        None
    );
    wait_ring_contains_since(&mgr, &agent, before_restart, b"booted").await;

    mgr.write(&agent, b"size\n").await.unwrap();
    wait_ring_contains_since(&mgr, &agent, before_restart, b"30 100").await;
}

/// A child killed by a signal is reported as that signal, not as an exit code:
/// portable-pty hands a signalled `ExitStatus` back with `exit_code() == 1`
/// and the signal on `signal()`, and the desktop used to show `[exited 1]`
/// for a SIGTERM (#90).
#[tokio::test(flavor = "multi_thread")]
async fn signal_death_is_reported_as_the_signal() {
    let (mgr, bus, agent, _tmp) = boot().await;
    let mut events = bus.subscribe();
    wait_ring_contains(&mgr, &agent, b"booted").await;

    mgr.write(&agent, b"die\r").await.unwrap();

    let exit = wait_lifecycle(&mut events, LifecyclePhase::Exited).await;
    assert_eq!(exit, Some(AgentExit::Signal("Terminated".to_string())));
    assert_eq!(mgr.state(&agent).unwrap(), AgentState::Exited);
    assert_eq!(
        mgr.exit(&agent).unwrap(),
        Some(AgentExit::Signal("Terminated".to_string()))
    );
}
