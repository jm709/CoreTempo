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
use coretempo_core::types::{AgentId, AgentState, EventPayload, LifecyclePhase, Token};

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
        "    nap) sleep 30 ;;\n",
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
            dir: dir.to_path_buf(),
            prompt: "test prompt".into(),
            model: None,
            permission_mode: None,
            auto_clear: false, // auto-/clear behavior is covered by queue unit tests
            edges: Vec::new(),
            tools: Vec::new(),
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
        output: None,
    })
}

/// Boot-scoped temp dir (no tempdir crate); the OS cleans it up. Each boot gets
/// its own dir: rewriting one script while another test's bash still reads it
/// corrupts that running agent.
async fn boot() -> (Arc<PtyManager>, EventBus, AgentId, PathBuf) {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("coretempo-pty-{}-{n}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp);
    let script = write_fake_agent(&tmp);
    let bus = EventBus::new();
    let env = AgentEnv {
        port: 4820,
        token: Token("ab".repeat(32)),
        tempo_bin_dir: PathBuf::from("/usr/bin"),
        settings_paths: std::collections::BTreeMap::new(),
    };
    let mgr =
        PtyManager::new_with_program(workflow(&tmp), bus.clone(), env, script.to_str().unwrap());
    mgr.spawn_all().await.unwrap();
    (mgr, bus, AgentId("fake".into()), tmp)
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
) -> Option<i32> {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = tokio::time::timeout(remaining, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for lifecycle {want_phase:?}"))
            .unwrap();
        if let EventPayload::AgentLifecycle {
            phase, exit_code, ..
        } = event.payload
            && phase == want_phase
        {
            return exit_code;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn exit_emits_lifecycle_fact_and_fails_new_injections() {
    let (mgr, bus, agent, _tmp) = boot().await;
    let mut events = bus.subscribe();
    wait_ring_contains(&mgr, &agent, b"booted").await;

    mgr.write(&agent, b"quit\r").await.unwrap();

    let code = wait_lifecycle(&mut events, LifecyclePhase::Exited).await;
    assert_eq!(code, Some(3), "fake agent exits 3 on 'quit'");
    assert_eq!(mgr.state(&agent).unwrap(), AgentState::Exited);
    assert_eq!(mgr.exit_code(&agent).unwrap(), Some(3));

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
