#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]
#![expect(clippy::unwrap_used, reason = "assertions are the vocabulary of tests")]
#![expect(clippy::expect_used, reason = "assertions are the vocabulary of tests")]
#![expect(clippy::panic, reason = "assertions are the vocabulary of tests")]

//! The serve scheduler (multi-flow spec §4): per-flow FIFO queues, per-agent
//! locks, the `max_concurrent_runs` cap, and settle-on-every-exit.

mod support;

use std::time::{Duration, Instant};

use support::{
    Serving, TOKEN, agent, json_of, serving, serving_flows, serving_flows_env, wait_for_exit,
};

/// Two agents, two disjoint single-member flows.
const DISJOINT: &str = "[agents.left]\ndir = \"{dir}\"\nprompt = \"You reply.\"\n\
    [agents.right]\ndir = \"{dir}\"\nprompt = \"You reply.\"\n\
    [flows.a]\nagents = [\"left\"]\n\
    trigger = { type = \"webhook\", edge = { to = \"left\", kind = \"ask\" } }\n\
    [flows.b]\nagents = [\"right\"]\n\
    trigger = { type = \"webhook\", edge = { to = \"right\", kind = \"ask\" } }\n";

/// Both flows span the same (default-exclusive) agent.
const SHARED_MEMBER: &str = "[agents.worker]\ndir = \"{dir}\"\nprompt = \"You reply.\"\n\
    [flows.a]\nagents = [\"worker\"]\n\
    trigger = { type = \"webhook\", edge = { to = \"worker\", kind = \"ask\" } }\n\
    [flows.b]\nagents = [\"worker\"]\n\
    trigger = { type = \"webhook\", edge = { to = \"worker\", kind = \"ask\" } }\n";

/// [`DISJOINT`] with a single permit, so only the cap can serialize the two.
const CAPPED: &str = "[server]\nmax_concurrent_runs = 1\n\
    [agents.left]\ndir = \"{dir}\"\nprompt = \"You reply.\"\n\
    [agents.right]\ndir = \"{dir}\"\nprompt = \"You reply.\"\n\
    [flows.a]\nagents = [\"left\"]\n\
    trigger = { type = \"webhook\", edge = { to = \"left\", kind = \"ask\" } }\n\
    [flows.b]\nagents = [\"right\"]\n\
    trigger = { type = \"webhook\", edge = { to = \"right\", kind = \"ask\" } }\n";

/// One `on_start` flow and one webhook flow over the same agent.
const MIXED: &str = "[agents.worker]\ndir = \"{dir}\"\nprompt = \"You reply.\"\n\
    [flows.batch]\nagents = [\"worker\"]\n\
    trigger = { type = \"on_start\", edge = { to = \"worker\", kind = \"ask\" }, \
    message = \"go\" }\n\
    [flows.hook]\nagents = [\"worker\"]\n\
    trigger = { type = \"webhook\", edge = { to = \"worker\", kind = \"ask\" } }\n";

/// The `/v1/flows` entry for `name`.
fn flow_entry(serve: &Serving, name: &str) -> serde_json::Value {
    let (_, flows) = serve.get("/v1/flows").expect("GET /v1/flows");
    flows
        .as_array()
        .expect("flows array")
        .iter()
        .find(|flow| flow["name"] == name)
        .unwrap_or_else(|| panic!("flow '{name}' missing from {flows}"))
        .clone()
}

/// Polls until `name` reports a live run, so what follows samples while that
/// run genuinely holds its members (or the only permit).
fn wait_until_running(serve: &Serving, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if flow_entry(serve, name)["running"] == 1 {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "flow '{name}' never started; stderr:\n{}",
        serve.stderr_text()
    );
}

#[test]
fn disjoint_flows_overlap() {
    let serve = serving_flows("overlap", DISJOINT, "3");
    let a = serve.fire_flow_ok("a", "one");
    let b = serve.fire_flow_ok("b", "two");
    // Both must be observed running at the same time: with 3s turns and a
    // 60s deadline, overlap is guaranteed unless something serializes them.
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        let (sa, sb) = (serve.status_of(&a), serve.status_of(&b));
        if sa["status"] == "running" && sb["status"] == "running" {
            break;
        }
        assert!(
            sa["status"] != "completed" || sb["status"] != "completed",
            "both finished without ever overlapping: {sa} / {sb}"
        );
        assert!(Instant::now() < deadline, "never overlapped: {sa} / {sb}");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(serve.settled(&a)["status"], "completed");
    assert_eq!(serve.settled(&b)["status"], "completed");
}

#[test]
fn an_exclusive_agent_serializes_two_flows_fifo() {
    let serve = serving_flows("serialize", SHARED_MEMBER, "2");
    let first = serve.fire_flow_ok("a", "one");
    let second = serve.fire_flow_ok("b", "two");
    // Mirror the existing FIFO test's sampling order: second before first.
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut second_started = false;
    while Instant::now() < deadline {
        let two = serve.status_of(&second);
        let one = serve.status_of(&first);
        if two["status"] == "running" {
            second_started = true;
            assert_eq!(
                one["status"], "completed",
                "flow b started while flow a still held the exclusive agent"
            );
        }
        if two["status"] == "completed" || two["status"] == "failed" {
            assert_eq!(one["status"], "completed", "first: {one}");
            assert_eq!(two["status"], "completed", "second: {two}");
            assert!(second_started, "never observed the second running");
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "the second flow never ran; stderr:\n{}",
        serve.stderr_text()
    );
}

#[test]
fn an_all_shared_flow_overlaps_with_itself() {
    let shared = "[agents.reader]\ndir = \"{dir}\"\nprompt = \"You reply.\"\n\
        concurrency = \"shared\"\n\
        [flows.classify]\nagents = [\"reader\"]\n\
        trigger = { type = \"webhook\", edge = { to = \"reader\", kind = \"ask\" } }\n";
    let serve = serving_flows("self-overlap", shared, "3");
    let one = serve.fire_flow_ok("classify", "one");
    let two = serve.fire_flow_ok("classify", "two");
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        let (s1, s2) = (serve.status_of(&one), serve.status_of(&two));
        if s1["status"] == "running" && s2["status"] == "running" {
            break;
        }
        assert!(
            s1["status"] != "completed" || s2["status"] != "completed",
            "an all-shared flow finished both triggers without self-overlap: {s1} / {s2}"
        );
        assert!(Instant::now() < deadline, "never overlapped: {s1} / {s2}");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(serve.settled(&one)["status"], "completed");
    assert_eq!(serve.settled(&two)["status"], "completed");
}

#[test]
fn the_cap_blocks_run_n_plus_one() {
    // max_concurrent_runs = 1 in the [server] block of the tail.
    let serve = serving_flows("cap", CAPPED, "3");
    let a = serve.fire_flow_ok("a", "one");
    let b = serve.fire_flow_ok("b", "two");
    // Disjoint flows, so only the cap can serialize them: b must never be
    // running while a is, and both must complete. Same second-before-first
    // sampling as the FIFO test (statuses only move toward terminal).
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut b_started = false;
    while Instant::now() < deadline {
        let sb = serve.status_of(&b);
        let sa = serve.status_of(&a);
        if sb["status"] == "running" {
            b_started = true;
            assert_eq!(
                sa["status"], "completed",
                "flow b ran while flow a held the only permit: {sa}"
            );
        }
        if sb["status"] == "completed" || sb["status"] == "failed" {
            assert_eq!(sa["status"], "completed", "first: {sa}");
            assert_eq!(sb["status"], "completed", "second: {sb}");
            assert!(b_started, "never observed flow b running");
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("flow b never ran; stderr:\n{}", serve.stderr_text());
}

#[test]
fn queue_full_is_per_flow() -> anyhow::Result<()> {
    let serve = serving_flows("flow-limits", DISJOINT, "6");
    // Fill flow a's queue (cap 32 + the running one).
    let mut refused = false;
    for _ in 0..40_u32 {
        let (status, body) = serve.fire_flow("a", "queued")?;
        if status == 429 {
            assert_eq!(body["error"]["code"], "queue_full");
            let message = body["error"]["message"].as_str().unwrap_or_default();
            assert!(message.contains("'a'"), "names the flow: {message}");
            refused = true;
            break;
        }
        assert_eq!(status, 202, "unexpected: {body}");
    }
    assert!(refused, "flow a's queue never filled");
    // Flow b's queue is untouched.
    let (status, body) = serve.fire_flow("b", "still room")?;
    assert_eq!(
        status, 202,
        "a full flow-a queue must not refuse flow b: {body}"
    );
    Ok(())
}

/// POSTs the removed bare route, which no helper targets any more.
fn bare_trigger(serve: &Serving) -> anyhow::Result<(u16, serde_json::Value)> {
    let mut res = agent()
        .post(serve.url("/v1/trigger"))
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Content-Type", "text/plain")
        .send("hi")?;
    let status = res.status().as_u16();
    Ok((status, json_of(res.body_mut().read_to_string()?)))
}

#[test]
fn the_bare_trigger_endpoint_is_a_tombstone_naming_the_flows() -> anyhow::Result<()> {
    let serve = serving_flows("tombstone", DISJOINT, "0");
    let (status, body) = bare_trigger(&serve)?;
    assert_eq!(status, 404, "body: {body}");
    assert_eq!(body["error"]["code"], "invalid_request");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("/v1/flows/"), "names the route: {message}");
    assert!(
        message.contains("declared flows: a, b"),
        "the 404 must list every declared flow: {message}"
    );

    // One webhook flow used to make the bare route *work*: it is gone there too.
    let single = serving("tombstone-single", "0");
    let (status, body) = bare_trigger(&single)?;
    assert_eq!(status, 404, "the single-flow shim is gone: {body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("/v1/flows/"), "names the route: {message}");
    assert!(message.contains("main"), "names the flow: {message}");
    Ok(())
}

#[test]
fn flows_and_health_report_per_flow_depths() -> anyhow::Result<()> {
    let serve = serving_flows("depths", DISJOINT, "0");

    let (status, flows) = serve.get("/v1/flows")?;
    assert_eq!(status, 200, "{flows}");
    let names: Vec<&str> = flows
        .as_array()
        .expect("array")
        .iter()
        .map(|f| f["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, ["a", "b"], "name order");
    assert_eq!(flows[0]["type"], "webhook");
    assert_eq!(flows[0]["target"], "left");
    assert_eq!(flows[0]["queue_depth"], 0, "{flows}");

    let (_, health) = serve.get("/v1/health")?;
    assert_eq!(health["queued"]["a"], 0, "{health}");
    assert_eq!(health["queued"]["b"], 0, "{health}");
    assert!(health["running"].is_number(), "{health}");
    assert!(
        health.get("queue_depth").is_none(),
        "the total is replaced: {health}"
    );
    Ok(())
}

#[test]
fn a_lock_parked_trigger_still_counts_as_queued() -> anyhow::Result<()> {
    // Flow b's worker takes its job off the channel within milliseconds and
    // then parks on the exclusive member flow a is holding. A depth read from
    // channel occupancy stops seeing that trigger the moment it is dequeued, so
    // it reports nothing waiting while a caller is very much waiting.
    let serve = serving_flows("lock-parked", SHARED_MEMBER, "6");
    let running = serve.fire_flow_ok("a", "one");
    wait_until_running(&serve, "a");
    let parked = serve.fire_flow_ok("b", "two");
    std::thread::sleep(Duration::from_millis(500));

    let a = flow_entry(&serve, "a");
    assert_eq!(a["running"], 1, "flow a: {a}");
    assert_eq!(a["queue_depth"], 0, "a running trigger is not queued: {a}");
    let b = flow_entry(&serve, "b");
    assert_eq!(b["queue_depth"], 1, "the parked trigger is invisible: {b}");
    assert_eq!(b["running"], 0, "it has not started: {b}");

    let (_, health) = serve.get("/v1/health")?;
    assert_eq!(health["queued"]["b"], 1, "health hides it too: {health}");
    assert_eq!(health["queued"]["a"], 0, "{health}");
    assert_eq!(health["running"], 1, "{health}");

    // A settled trigger counts as neither.
    assert_eq!(serve.settled(&running)["status"], "completed");
    assert_eq!(serve.settled(&parked)["status"], "completed");
    let (_, health) = serve.get("/v1/health")?;
    assert_eq!(health["queued"]["a"], 0, "{health}");
    assert_eq!(health["queued"]["b"], 0, "{health}");
    assert_eq!(health["running"], 0, "{health}");
    Ok(())
}

#[test]
fn a_permit_parked_trigger_still_counts_as_queued() -> anyhow::Result<()> {
    // The same blind spot one wait later: disjoint flows contend for nothing
    // but the single permit, so b's worker holds `right` and parks on the cap.
    let serve = serving_flows("permit-parked", CAPPED, "6");
    let first = serve.fire_flow_ok("a", "one");
    wait_until_running(&serve, "a");
    let parked = serve.fire_flow_ok("b", "two");
    std::thread::sleep(Duration::from_millis(500));

    let b = flow_entry(&serve, "b");
    assert_eq!(b["queue_depth"], 1, "the parked trigger is invisible: {b}");
    assert_eq!(b["running"], 0, "it has not started: {b}");
    let (_, health) = serve.get("/v1/health")?;
    assert_eq!(health["queued"]["b"], 1, "{health}");
    assert_eq!(health["running"], 1, "the cap allows one run: {health}");

    assert_eq!(serve.settled(&first)["status"], "completed");
    assert_eq!(serve.settled(&parked)["status"], "completed");
    Ok(())
}

#[test]
fn an_on_start_flow_reports_zero_queued_and_running() -> anyhow::Result<()> {
    // An on_start flow has no ingress at all, so both surfaces have to name it
    // and report zeroes rather than omitting it — and a webhook flow running on
    // the same agent must not make it look busy.
    let serve = serving_flows("on-start-zero", MIXED, "6");
    let (_, health) = serve.get("/v1/health")?;
    assert_eq!(
        health["queued"]["batch"], 0,
        "an on_start flow is listed at 0, not omitted: {health}"
    );
    assert_eq!(health["queued"]["hook"], 0, "{health}");
    assert_eq!(health["running"], 0, "{health}");
    let batch = flow_entry(&serve, "batch");
    assert_eq!(batch["type"], "on_start", "{batch}");
    assert_eq!(batch["queue_depth"], 0, "{batch}");
    assert_eq!(batch["running"], 0, "{batch}");

    let id = serve.fire_flow_ok("hook", "go");
    wait_until_running(&serve, "hook");
    let batch = flow_entry(&serve, "batch");
    assert_eq!(
        batch["running"], 0,
        "the on_start flow shares the agent, not the run: {batch}"
    );
    assert_eq!(batch["queue_depth"], 0, "{batch}");
    let (_, health) = serve.get("/v1/health")?;
    assert_eq!(health["queued"]["batch"], 0, "{health}");
    assert_eq!(
        health["running"], 1,
        "the webhook flow is running: {health}"
    );
    assert_eq!(serve.settled(&id)["status"], "completed");
    Ok(())
}

/// One webhook flow whose reply is held to an inline schema.
const CONTRACTED: &str = "[agents.judge]\ndir = \"{dir}\"\nprompt = \"You reply.\"\n\
    [flows.review]\nagents = [\"judge\"]\n\
    trigger = { type = \"webhook\", edge = { to = \"judge\", kind = \"ask\" } }\n\
    [flows.review.output]\nschema = { type = \"object\", required = [\"verdict\"] }\n\
    max_repairs = 2\n";

#[test]
fn a_contracted_flow_repairs_in_turn_and_returns_parsed_output() {
    // The agent answers with prose first. Only the contract serve binds to this
    // kickoff can reject that, and only a rejection makes the agent send the
    // conforming repair — so an unbound kickoff cannot reach a `completed`
    // status here, it settles as schema_validation_failed at the watcher.
    let serve = serving_flows_env(
        "contract",
        CONTRACTED,
        "0",
        &[
            ("FAKE_AGENT_REPLY", "no json here"),
            ("FAKE_AGENT_REPAIR", r#"{\"verdict\":\"pass\"}"#),
        ],
    );
    let id = serve.fire_flow_ok("review", "judge this");
    let settled = serve.settled(&id);
    assert_eq!(settled["status"], "completed", "settled: {settled}");
    assert_eq!(settled["result"], "replied", "settled: {settled}");
    assert_eq!(
        settled["output"],
        serde_json::json!({ "verdict": "pass" }),
        "the caller gets the parsed value: {settled}"
    );
    assert_eq!(
        settled["reply"], r#"{"verdict":"pass"}"#,
        "the raw reply is kept alongside it: {settled}"
    );
}

#[test]
fn a_declared_on_start_flow_400s_pointing_at_run_flow() -> anyhow::Result<()> {
    let serve = serving_flows("mixed-400", MIXED, "0");
    let (status, body) = serve.fire_flow("batch", "x")?;
    assert_eq!(status, 400, "declared on_start is 400, not 404: {body}");
    assert_eq!(body["error"]["code"], "invalid_request");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("--flow batch"), "{message}");
    // An undeclared name is still the unknown_flow 404.
    let (status, body) = serve.fire_flow("nope", "x")?;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error"]["code"], "unknown_flow");
    // …and `GET /v1/flows` lists the on_start flow too, at depth 0.
    let (_, flows) = serve.get("/v1/flows")?;
    assert_eq!(flows[0]["name"], "batch", "{flows}");
    assert_eq!(flows[0]["type"], "on_start", "{flows}");
    assert_eq!(flows[0]["queue_depth"], 0, "{flows}");
    Ok(())
}

#[test]
fn an_unknown_flow_name_is_a_404() -> anyhow::Result<()> {
    let serve = serving_flows("unknown-flow", DISJOINT, "0");
    let (status, body) = serve.fire_flow("nope", "hi")?;
    assert_eq!(status, 404, "body: {body}");
    assert_eq!(body["error"]["code"], "unknown_flow");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("no flow named 'nope'"),
        "the 404 must quote the name the caller asked for: {message}"
    );
    assert!(
        message.contains("declared flows: a, b"),
        "the 404 must list every declared flow: {message}"
    );
    Ok(())
}

#[test]
fn shutdown_while_lock_blocked_settles_the_trigger() -> anyhow::Result<()> {
    let mut serve = serving_flows("lock-shutdown", SHARED_MEMBER, "8");
    let _running = serve.fire_flow_ok("a", "one");
    // Flow b parks on the worker's write lock. Long-poll it: the lock-blocked
    // worker settles its trigger as the shutdown starts — before the listener
    // stops — so ctrl-c answers this caller rather than dropping the
    // connection or making it wait out the drain grace.
    let url = serve.url("/v1/flows/b/trigger?wait=45");
    let waiter = std::thread::spawn(move || {
        let mut res = agent()
            .post(url)
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header("Content-Type", "text/plain")
            .send("two")?;
        let status = res.status().as_u16();
        Ok::<_, anyhow::Error>((status, json_of(res.body_mut().read_to_string()?)))
    });
    std::thread::sleep(Duration::from_secs(2));
    let interrupted_at = Instant::now();
    serve.interrupt();

    let (status, body) = waiter.join().expect("waiter panicked")?;
    assert_eq!(
        status, 200,
        "the long-poll must report the shutdown: {body}"
    );
    assert_eq!(body["status"], "failed", "blocked trigger: {body}");
    assert!(
        body["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("daemon_shutdown"),
        "reason: {body}"
    );
    // The blocked worker did not sit out the agent's eight-second turn, let
    // alone the drain grace.
    assert!(
        interrupted_at.elapsed() < Duration::from_secs(8),
        "the lock-blocked trigger settled late: {:?}",
        interrupted_at.elapsed()
    );
    let exit = wait_for_exit(&mut serve, Duration::from_secs(30));
    assert!(exit.success(), "stderr:\n{}", serve.stderr_text());
    assert!(
        interrupted_at.elapsed() < Duration::from_secs(25),
        "shutdown was not bounded: {:?}",
        interrupted_at.elapsed()
    );
    // The blocked worker abandoned its wait instead of taking the lock the
    // interrupted run released: a stopping daemon cold-starts nothing.
    let logs = serve.stderr_text();
    assert_eq!(
        logs.matches("starting run").count(),
        1,
        "only the in-flight trigger may have started a run; logs:\n{logs}"
    );
    Ok(())
}

/// POSTs `flow` with a long-poll, from its own thread: the response is the
/// trigger's terminal status rather than the accepted shape.
fn long_poll(
    serve: &Serving,
    flow: &'static str,
) -> std::thread::JoinHandle<(&'static str, u16, serde_json::Value)> {
    let url = serve.url(&format!("/v1/flows/{flow}/trigger?wait=45"));
    std::thread::spawn(move || {
        let mut res = agent()
            .post(url)
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header("Content-Type", "text/plain")
            .send("work")
            .expect("long-poll post");
        let status = res.status().as_u16();
        (
            flow,
            status,
            json_of(res.body_mut().read_to_string().expect("body")),
        )
    })
}

#[test]
fn shutdown_fails_every_flow_s_queued_triggers() -> anyhow::Result<()> {
    // The single-flow variant above covers one blocked worker. Each flow owns
    // its own FIFO and its own worker, so a drain that reached only the flow
    // whose worker noticed the interrupt would leave the other flow's callers
    // hanging on a daemon that is already gone.
    let mut serve = serving_flows("multi-shutdown", DISJOINT, "8");
    let waiters: Vec<_> = ["a", "b", "a", "b"]
        .into_iter()
        .map(|flow| {
            let handle = long_poll(&serve, flow);
            // Staggered so each flow's first trigger is the one that runs.
            std::thread::sleep(Duration::from_millis(300));
            handle
        })
        .collect();
    // One running and one waiting per flow, so the drain has to settle both
    // kinds in both flows.
    let (_, health) = serve.get("/v1/health")?;
    assert_eq!(health["queued"]["a"], 1, "{health}");
    assert_eq!(health["queued"]["b"], 1, "{health}");
    assert_eq!(health["running"], 2, "{health}");

    serve.interrupt();
    for waiter in waiters {
        let (flow, status, body) = waiter.join().expect("waiter panicked");
        assert_eq!(status, 200, "flow '{flow}' long-poll: {body}");
        assert_eq!(body["status"], "failed", "flow '{flow}': {body}");
        assert!(
            body["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("daemon_shutdown"),
            "flow '{flow}': {body}"
        );
    }
    let exit = wait_for_exit(&mut serve, Duration::from_secs(30));
    assert!(exit.success(), "stderr:\n{}", serve.stderr_text());
    Ok(())
}

#[test]
fn a_trigger_arriving_during_the_drain_is_refused_as_shutting_down() {
    // The listener outlives the queues: it keeps answering until the workers
    // have drained and the in-flight run has been torn down. A trigger that
    // lands in that window must be told the daemon is going away — 429
    // queue_full sends the caller into a retry loop against a daemon that will
    // never answer, and its "retry once one completes" advice is a lie.
    let mut serve = serving_flows("drain-refusal", DISJOINT, "6");
    let _running = serve.fire_flow_ok("a", "one");
    wait_until_running(&serve, "a");
    serve.interrupt();

    let mut refusal = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        match serve.fire_flow("b", "late") {
            // Beat the drain: this one is accepted and then failed instead.
            Ok((202, _)) => std::thread::sleep(Duration::from_millis(10)),
            Ok(answer) => {
                refusal = Some(answer);
                break;
            }
            // The listener is gone: the window closed without a refusal.
            Err(_) => break,
        }
    }
    let (status, body) = refusal.unwrap_or_else(|| {
        panic!(
            "no trigger reached the draining daemon; stderr:\n{}",
            serve.stderr_text()
        )
    });
    assert_eq!(status, 503, "the drain window must not answer 429: {body}");
    assert_eq!(body["error"]["code"], "shutting_down", "{body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("restart"),
        "the refusal must name the fix: {message}"
    );
    let exit = wait_for_exit(&mut serve, Duration::from_secs(30));
    assert!(exit.success(), "stderr:\n{}", serve.stderr_text());
}

#[test]
fn a_failed_run_start_settles_the_trigger_and_frees_its_slot() -> anyhow::Result<()> {
    let serve = serving_flows("start-fail", DISJOINT, "0");
    // Make Run::start fail: a directory where the db file should be.
    std::fs::create_dir(serve.scratch.root.join("tempo.db"))?;
    let id = serve.fire_flow_ok("a", "doomed");
    let failed = serve.settled(&id);
    assert_eq!(failed["status"], "failed", "settled: {failed}");
    assert_eq!(failed["reason_code"], "internal");
    // The run task settled it itself: the drop guard's placeholder reason means
    // some exit path stopped reporting what actually went wrong.
    let reason = failed["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("could not start a run"),
        "the reason must name the start failure: {reason}"
    );
    // The permit and the flow slot were released: the next trigger runs.
    std::fs::remove_dir(serve.scratch.root.join("tempo.db"))?;
    let id = serve.fire_flow_ok("a", "fine now");
    assert_eq!(serve.settled(&id)["status"], "completed");
    Ok(())
}

/// Three flows over two agents, two permits: `first` and `second` share an
/// exclusive agent, `other` is disjoint from both.
const PERMIT_PARKING: &str = "[server]\nmax_concurrent_runs = 2\n\
    [agents.hot]\ndir = \"{dir}\"\nprompt = \"You reply.\"\n\
    [agents.cold]\ndir = \"{dir}\"\nprompt = \"You reply.\"\n\
    [flows.first]\nagents = [\"hot\"]\n\
    trigger = { type = \"webhook\", edge = { to = \"hot\", kind = \"ask\" } }\n\
    [flows.second]\nagents = [\"hot\"]\n\
    trigger = { type = \"webhook\", edge = { to = \"hot\", kind = \"ask\" } }\n\
    [flows.other]\nagents = [\"cold\"]\n\
    trigger = { type = \"webhook\", edge = { to = \"cold\", kind = \"ask\" } }\n";

#[test]
fn a_lock_blocked_flow_does_not_park_a_permit() {
    let serve = serving_flows("permit-parking", PERMIT_PARKING, "6");
    let first = serve.fire_flow_ok("first", "one");
    std::thread::sleep(Duration::from_millis(500));
    // Parks on hot's write lock. Locks come before the permit, so it holds
    // nothing the cap counts while it waits.
    let _second = serve.fire_flow_ok("second", "two");
    std::thread::sleep(Duration::from_millis(500));
    let other = serve.fire_flow_ok("other", "three");
    // The second permit is free, so the disjoint flow runs alongside `first`.
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        let (one, three) = (serve.status_of(&first), serve.status_of(&other));
        if one["status"] == "running" && three["status"] == "running" {
            return;
        }
        assert!(
            one["status"] != "completed",
            "the disjoint flow waited out the blocked one's permit: {one} / {three}"
        );
        assert!(
            Instant::now() < deadline,
            "never overlapped: {one} / {three}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
