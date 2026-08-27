#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]
#![expect(clippy::unwrap_used, reason = "assertions are the vocabulary of tests")]
#![expect(clippy::expect_used, reason = "assertions are the vocabulary of tests")]
#![expect(clippy::panic, reason = "assertions are the vocabulary of tests")]

//! `coretempod serve` end to end (spec triggers §3): a standing listener that
//! cold-starts one run per trigger, FIFO, against a scripted fake agent.

mod support;

use std::process::Stdio;
use std::time::{Duration, Instant};

use support::{
    TOKEN, WEBHOOK, agent, daemon_command, daemon_command_without_token, json_of, log_file,
    scratch, serving, wait_for_child_exit, wait_for_exit,
};

/// One agent, no flows: validation needs at least one agent, serve needs a
/// webhook flow.
const AGENT_ONLY: &str = "[agents.worker]\ndir = \"{dir}\"\nprompt = \"You reply.\"\n";

/// Two flows, both `on_start`: serve has nothing to listen for, but unlike
/// [`AGENT_ONLY`] the caller has flows to run — the refusal has to name them.
const ON_START_ONLY: &str = "[agents.worker]\ndir = \"{dir}\"\nprompt = \"You reply.\"\n\
                             [flows.nightly]\nagents = [\"worker\"]\n\
                             trigger = { type = \"on_start\", message = \"go\", \
                             edge = { to = \"worker\", kind = \"ask\" } }\n\
                             [flows.weekly]\nagents = [\"worker\"]\n\
                             trigger = { type = \"on_start\", message = \"go\", \
                             edge = { to = \"worker\", kind = \"ask\" } }\n";

#[test]
fn a_trigger_cold_starts_a_run_and_reports_the_reply() -> anyhow::Result<()> {
    let serve = serving("basic", "0");
    let (status, health) = serve.get("/v1/health")?;
    assert_eq!(status, 200);
    assert_eq!(health["queued"]["main"], 0, "health: {health}");
    assert_eq!(health["running"], 0, "health: {health}");

    let id = serve.fire_ok("please do the thing");
    assert!(id.starts_with("t-"), "id: {id}");
    let done = serve.settled(&id);
    assert_eq!(done["status"], "completed", "final: {done}");
    assert_eq!(done["result"], "replied");
    assert_eq!(done["code"], 0);
    assert_eq!(done["reply"], "ok");

    // Amendment 31 (#42): a cold-started kickoff names its flow, same as a warm
    // one — the serve path builds it through the same `create_kickoff`.
    let prompts =
        std::fs::read_to_string(serve.scratch.root.join("prompts.log")).unwrap_or_default();
    assert!(
        prompts.contains("from http, flow main"),
        "the kickoff typed at the agent names its flow; prompts:\n{prompts}"
    );

    // The run was torn down: no agents are held between triggers.
    let (_, health) = serve.get("/v1/health")?;
    assert_eq!(health["running"], 0, "health: {health}");
    // And its artifact directory was cleaned up, so a long-lived daemon does not
    // accumulate one per trigger.
    let runs = serve.scratch.home.join(".coretempo/runs");
    let leftovers: Vec<_> = std::fs::read_dir(&runs)
        .map(|dir| {
            dir.filter_map(Result::ok)
                .map(|e| e.file_name())
                .filter(|name| name != "current")
                .collect()
        })
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "run dirs left behind: {leftovers:?}");
    // Serve-mode runs never steal the `current` symlink from an interactive run.
    assert!(!runs.join("current").exists(), "serve repointed current");
    Ok(())
}

#[test]
fn health_is_open_but_triggers_need_the_token() -> anyhow::Result<()> {
    // Serve mode authenticates its own requests, without core's ApiContext, so
    // the rule is implemented twice and has to be pinned twice.
    let serve = serving("auth", "0");

    // Health carries no secret and is what a supervisor polls: no auth.
    let (status, body) = serve.get_as("/v1/health", None)?;
    assert_eq!(status, 200, "health must answer unauthenticated: {body}");
    assert_eq!(body["status"], "ok");

    // Firing a workflow is not open, with no token or with the wrong one.
    for token in [None, Some("not-the-token"), Some("")] {
        let (status, body) = serve.fire_as("", "let me in", token)?;
        assert_eq!(status, 401, "token {token:?} was accepted: {body}");
        assert_eq!(body["error"]["code"], "unauthorized", "token {token:?}");
        let (status, body) = serve.get_as("/v1/trigger/t-deadbeef", token)?;
        assert_eq!(status, 401, "token {token:?} was accepted: {body}");
    }
    // None of the refused requests queued anything.
    let (_, health) = serve.get("/v1/health")?;
    assert_eq!(
        health["queued"]["main"], 0,
        "a refused trigger queued: {health}"
    );
    assert_eq!(health["running"], 0, "health: {health}");

    // A Host the daemon does not answer to is refused even with the token:
    // this is what blocks DNS-rebinding at the public port.
    let (status, body) = serve.get_with_host("/v1/health", "evil.example.com")?;
    assert_eq!(status, 403, "foreign Host accepted: {body}");
    assert_eq!(body["error"]["code"], "invalid_host");

    // And the real token still works, so the checks are not refusing everything.
    let id = serve.fire_ok("do the thing");
    assert_eq!(serve.settled(&id)["status"], "completed");
    Ok(())
}

#[test]
fn a_serve_401_points_at_the_daemons_own_token() -> anyhow::Result<()> {
    // #57: api.json belongs to interactive runs. A serve daemon writes none and
    // never repoints `current`, so pointing a 401'd caller there sends it to a
    // stale token from somebody else's run.
    let serve = serving("401-hint", "0");
    let (status, body) = serve.fire_as("", "let me in", None)?;
    assert_eq!(status, 401, "{body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        !message.contains("runs/current"),
        "serve keeps no api.json under runs/current: {message}"
    );
    for hint in ["CORETEMPO_TOKEN", "token_file"] {
        assert!(
            message.contains(hint),
            "the 401 must name '{hint}': {message}"
        );
    }
    Ok(())
}

#[test]
fn triggers_run_one_at_a_time_in_order() -> anyhow::Result<()> {
    let serve = serving("fifo", "2");
    let first = serve.fire_ok("one");
    let (status, second) = serve.fire("", "two")?;
    assert_eq!(status, 202, "second trigger: {second}");
    assert_eq!(
        second["position"], 1,
        "the second trigger queues behind the first: {second}"
    );
    let second = second["trigger_id"].as_str().unwrap().to_string();

    // The second must never be running while the first still is.
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut second_started = false;
    while Instant::now() < deadline {
        // Sample the second trigger before the first. run_worker settles a
        // trigger before it begins the next one, and statuses only move toward
        // terminal, so observing `two == running` here already implies `one` is
        // terminal — reading the other order across two HTTP round trips would
        // let the worker settle the first and start the second in between,
        // catching `one` mid-flight and false-failing this assertion.
        let two = serve.status_of(&second);
        let one = serve.status_of(&first);
        let one_status = one["status"].as_str().unwrap_or_default().to_string();
        let two_status = two["status"].as_str().unwrap_or_default().to_string();
        if two_status == "running" {
            second_started = true;
            assert_eq!(
                one_status, "completed",
                "the second trigger started while the first was {one_status}"
            );
        }
        if two_status == "completed" || two_status == "failed" {
            assert_eq!(one["status"], "completed", "first: {one}");
            assert_eq!(two["status"], "completed", "second: {two}");
            assert!(second_started, "never observed the second trigger running");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "the queued trigger never ran; stderr:\n{}",
        serve.stderr_text()
    );
}

/// How many distinct 64-hex words a message carries — a hash-mismatch reason
/// names both the frozen hash and the one on disk.
fn hash_words(text: &str) -> usize {
    let mut hashes: Vec<&str> = text
        .split(|c: char| !c.is_ascii_hexdigit())
        .filter(|word| word.len() == 64)
        .collect();
    hashes.sort_unstable();
    hashes.dedup();
    hashes.len()
}

#[test]
fn an_edited_workflow_fails_the_trigger_not_the_daemon() -> anyhow::Result<()> {
    let serve = serving("hash", "2");
    let first = serve.fire_ok("one");
    let queued = serve.fire_ok("two");
    // Edit while the first trigger holds the worker: serve froze the workflow at
    // startup, so the queued trigger must refuse to adopt the edit.
    let text = std::fs::read_to_string(&serve.scratch.config)?;
    std::fs::write(&serve.scratch.config, format!("{text}# edited\n"))?;

    let done = serve.settled(&first);
    assert_eq!(done["status"], "completed", "first: {done}");
    let failed = serve.settled(&queued);
    assert_eq!(failed["status"], "failed", "queued: {failed}");
    let reason = failed["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("restart the daemon"),
        "the reason must say how to adopt the edit: {reason}"
    );
    assert_eq!(
        hash_words(reason),
        2,
        "the reason must name both the frozen hash and the one on disk: {reason}"
    );

    // The daemon itself is unharmed.
    let (status, health) = serve.get("/v1/health")?;
    assert_eq!(status, 200, "health after a hash mismatch: {health}");
    assert_eq!(health["status"], "ok");
    Ok(())
}

#[test]
fn serve_refuses_a_workflow_without_a_webhook_trigger() {
    // A workflow with an agent but no flows at all: serve has nothing to listen
    // for and must say so.
    let scratch = scratch("no-trigger", AGENT_ONLY);
    let out = daemon_command(&scratch, "serve", 0, "0").output().unwrap();
    assert!(!out.status.success(), "serve accepted a plain workflow");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("webhook") && stderr.contains("coretempod run"),
        "the error must point at run mode: {stderr}"
    );
}

#[test]
fn serve_refuses_an_on_start_only_workflow_naming_its_flows() {
    // The flows exist, they just fire at launch: the caller needs their names
    // and the command that runs one, not the webhook advice the flowless case
    // gets (spec §6 — "naming what it found").
    let scratch = scratch("on-start-only", ON_START_ONLY);
    let out = daemon_command(&scratch, "serve", 0, "0").output().unwrap();
    assert!(
        !out.status.success(),
        "serve accepted an on_start-only file"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("nightly") && stderr.contains("weekly"),
        "the refusal must name the on_start flows it found: {stderr}"
    );
    let run_one = format!("coretempod run {} --flow nightly", scratch.config.display());
    assert!(
        stderr.contains(&run_one),
        "the refusal must point at a runnable `{run_one}`: {stderr}"
    );
}

#[test]
fn serve_refuses_to_start_without_a_provisioned_token() {
    // #57: serve writes no api.json and never repoints ~/.coretempo/runs/current,
    // so a generated token reaches nobody — the daemon would listen and 401
    // every trigger forever. It has to refuse at startup instead, naming all
    // three ways to provision one.
    let scratch = scratch("no-token", WEBHOOK);
    let mut child = daemon_command_without_token(&scratch, "serve", 0, "0")
        .stdout(Stdio::from(log_file(&scratch.root, "out.log")))
        .stderr(Stdio::from(log_file(&scratch.root, "err.log")))
        .spawn()
        .unwrap();
    let status = wait_for_child_exit(&mut child, Duration::from_secs(20));
    let stderr = std::fs::read_to_string(scratch.root.join("err.log")).unwrap_or_default();
    assert!(
        !status.success(),
        "serve started with no token anyone could send: {stderr}"
    );
    for hint in ["CORETEMPO_TOKEN", "--token-file", "token_file", "headless"] {
        assert!(
            stderr.contains(hint),
            "the refusal must name '{hint}': {stderr}"
        );
    }
}

#[test]
fn interrupting_the_daemon_fails_the_queued_triggers() -> anyhow::Result<()> {
    let mut serve = serving("shutdown", "6");
    let _running = serve.fire_ok("one");

    // A long-poll on the queued trigger: ctrl-c must answer it rather than drop
    // the connection.
    let url = serve.url("/v1/flows/main/trigger?wait=45");
    let waiter = std::thread::spawn(move || {
        let mut res = agent()
            .post(url)
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header("Content-Type", "text/plain")
            .send("two")?;
        let status = res.status().as_u16();
        Ok::<_, anyhow::Error>((status, json_of(res.body_mut().read_to_string()?)))
    });
    // Let the queued trigger reach the queue before interrupting.
    std::thread::sleep(Duration::from_secs(2));
    let interrupted = Instant::now();
    serve.interrupt();

    let (status, body) = waiter.join().expect("waiter panicked")?;
    assert_eq!(
        status, 200,
        "the long-poll must report the shutdown: {body}"
    );
    assert_eq!(body["status"], "failed", "queued trigger: {body}");
    let reason = body["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("daemon_shutdown"),
        "the reason must name the shutdown: {reason}"
    );

    let exit = wait_for_exit(&mut serve, Duration::from_secs(30));
    assert!(
        exit.success(),
        "ctrl-c is a clean exit, got {exit:?}; stderr:\n{}",
        serve.stderr_text()
    );
    // The in-flight run had a six-second turn ahead of it: shutdown does not
    // wait it out.
    assert!(
        interrupted.elapsed() < Duration::from_secs(20),
        "shutdown was not bounded: {:?}",
        interrupted.elapsed()
    );
    // A stopping daemon cold-starts nothing. `Run::start_with` brings up the
    // whole roster and cannot be cancelled once underway, so a queued trigger
    // picked up after the interrupt would spend the shutdown grace on work
    // nobody collects — and leave PTY children behind if it overran.
    let logs = serve.stderr_text();
    assert_eq!(
        logs.matches("starting run").count(),
        1,
        "only the in-flight trigger may have started a run; logs:\n{logs}"
    );
    Ok(())
}

#[test]
fn the_queue_and_payload_limits_are_enforced() -> anyhow::Result<()> {
    let serve = serving("limits", "6");
    let (status, body) = serve.fire("", &"x".repeat(64 * 1024 + 1))?;
    assert_eq!(status, 413, "body: {body}");

    // One runs, QUEUE_CAP wait, the next is refused.
    for _ in 0..40_u32 {
        let (status, body) = serve.fire("", "queued")?;
        if status == 429 {
            let message = body["error"]["message"].as_str().unwrap_or_default();
            assert_eq!(body["error"]["code"], "queue_full");
            assert!(
                message.contains("32"),
                "the refusal must state the depth: {message}"
            );
            return Ok(());
        }
        assert_eq!(status, 202, "unexpected: {body}");
    }
    panic!("the queue never filled; stderr:\n{}", serve.stderr_text());
}
