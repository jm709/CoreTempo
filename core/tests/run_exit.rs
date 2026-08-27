//! Stop and restart wait for the old `claude` to actually exit before the
//! run dir is removed or a replacement is spawned (#94).
#![expect(clippy::panic, reason = "assertions are the vocabulary of tests")]

mod support;

use std::time::{Duration, Instant};

use coretempo_core::run::RunOptions;
use coretempo_core::trust::TrustPolicy;
use coretempo_core::types::AgentExit;
use coretempo_core::types::id::AgentId;
use support::run::RunScaffold;

const CLEANUP: RunOptions = RunOptions {
    ephemeral_port: false,
    repoint_current: false,
    cleanup_run_dir: true,
    trust: TrustPolicy { grant: true },
};

/// A fake `claude` that, like the real one, handles SIGHUP by finishing its
/// bookkeeping before exiting: it appends `start <pid>` to `<root>/log` when
/// it comes up and, on HUP, sleeps 300 ms, writes a session-end stub under its
/// `CLAUDE_CONFIG_DIR`, appends `exit <pid>`, and exits 0.
async fn scaffold(name: &str) -> RunScaffold {
    let scaffold = RunScaffold::new(name).await;
    let log = scaffold.root.join("log");
    scaffold.fake_claude(&format!(
        "echo \"start $$\" >> \"{log}\"\n\
         on_hup() {{\n\
         \x20 sleep 0.3\n\
         \x20 mkdir -p \"$CLAUDE_CONFIG_DIR/projects/x\"\n\
         \x20 echo '{{\"type\":\"cost-state\"}}' > \"$CLAUDE_CONFIG_DIR/projects/x/stub.jsonl\"\n\
         \x20 echo \"exit $$\" >> \"{log}\"\n\
         \x20 exit 0\n\
         }}\n\
         trap on_hup HUP\n\
         printf '> '\n\
         while :; do sleep 0.1; done\n",
        log = log.display()
    ));
    scaffold.write_workflow(&format!(
        "[agents.iso]\ndir = \"{}\"\nprompt = \"You exit slowly.\"\nisolated_config = true\n",
        scaffold.agent_dir.display(),
    ));
    scaffold
}

fn log_lines(scaffold: &RunScaffold) -> Vec<String> {
    std::fs::read_to_string(scaffold.root.join("log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn wait_for_log_lines(scaffold: &RunScaffold, count: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let lines = log_lines(scaffold);
        if lines.len() >= count {
            return lines;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("log never reached {count} lines: {:?}", log_lines(scaffold));
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_waits_for_the_agent_to_exit_before_removing_the_run_dir() {
    let scaffold = scaffold("exit-stop").await;
    let run = scaffold.start(CLEANUP).await.unwrap();
    let dir = scaffold.run_dir(&run);
    wait_for_log_lines(&scaffold, 1);

    run.stop().await.unwrap();

    let lines = log_lines(&scaffold);
    assert_eq!(
        lines.len(),
        2,
        "stop returned before the agent exited: {lines:?}"
    );
    assert!(lines[1].starts_with("exit "), "{lines:?}");
    assert!(
        !dir.exists(),
        "{} survived stop; the agent's session-end write resurrected it",
        dir.display()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_waits_for_the_old_agent_to_exit_before_spawning_the_new_one() {
    let scaffold = scaffold("exit-restart").await;
    let run = scaffold.start(support::run::GRANT_TRUST).await.unwrap();
    let iso = AgentId("iso".to_string());
    let first = wait_for_log_lines(&scaffold, 1);
    let first_pid = first[0].strip_prefix("start ").unwrap().to_string();

    run.pty().restart(&iso).await.unwrap();

    let lines = wait_for_log_lines(&scaffold, 3);
    assert_eq!(
        lines[1],
        format!("exit {first_pid}"),
        "the replacement came up before the old agent exited: {lines:?}"
    );
    assert!(lines[2].starts_with("start "), "{lines:?}");
    assert_ne!(lines[2], first[0], "restart did not spawn a fresh process");

    run.stop().await.unwrap();
}

/// An agent that never exits on SIGHUP is killed outright once the grace
/// period runs out, so a wedged agent cannot hang the daemon's stop.
#[tokio::test(flavor = "multi_thread")]
async fn stop_kills_an_agent_that_ignores_hangup_after_the_grace_period() {
    let scaffold = RunScaffold::new("exit-ignore").await;
    let ready = scaffold.root.join("ready");
    scaffold.fake_claude(&format!(
        "trap '' HUP\ntouch \"{}\"\nprintf '> '\nexec sleep 300\n",
        ready.display()
    ));
    let run = scaffold.start(CLEANUP).await.unwrap();
    let dir = scaffold.run_dir(&run);
    let echo = AgentId("echo".to_string());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(
            Instant::now() < deadline,
            "fake claude never armed its trap"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let started = Instant::now();
    run.stop().await.unwrap();
    let took = started.elapsed();

    assert!(
        took >= Duration::from_secs(4) && took < Duration::from_secs(9),
        "stop took {took:?}; expected the SIGHUP grace period then a SIGKILL"
    );
    assert_eq!(
        run.pty().exit(&echo).unwrap(),
        Some(AgentExit::Signal("Killed".to_string()))
    );
    assert!(!dir.exists(), "{} survived stop", dir.display());
}
