#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]

//! Warm-run trigger endpoints (multi-flow spec §5): a running workflow answers
//! `POST /v1/flows/{name}/trigger` against its live roster, one trigger in
//! flight per flow.

use std::time::Duration;

use coretempo_core::trigger::PAYLOAD_CAP_BYTES;
use coretempo_core::types::config::{Edge, EdgeKind, FlowConfig, TriggerConfig, TriggerType};
use coretempo_core::types::message::{MessageKind, MessageStatus, Origin};
use coretempo_core::types::{AgentId, FlowName, MessageId};
use support::{RawPost, TestServer};

mod support;

fn webhook(kind: EdgeKind) -> (FlowName, FlowConfig) {
    (
        FlowName("hook".to_string()),
        FlowConfig {
            agents: vec![
                AgentId("builder".to_string()),
                AgentId("planner".to_string()),
            ],
            trigger: TriggerConfig {
                trigger_type: TriggerType::Webhook,
                edge: Edge {
                    to: AgentId("builder".to_string()),
                    kind,
                    max_rounds: None,
                },
                message: None,
            },
            output: None,
        },
    )
}

fn triggerable(kind: EdgeKind) -> anyhow::Result<TestServer> {
    let (ctx, handles) = support::test_ctx_with_flow(webhook(kind))?;
    support::start(ctx, handles)
}

/// A triggerable workflow whose kickoff target owes `{"name": <string>}`.
fn triggerable_with_output(max_repairs: u32) -> anyhow::Result<TestServer> {
    let (ctx, handles) =
        support::test_ctx_with_flow_and_output(webhook(EdgeKind::Ask), max_repairs)?;
    support::start(ctx, handles)
}

fn fire(srv: &TestServer, path: &str, body: &[u8]) -> anyhow::Result<(u16, serde_json::Value)> {
    let (status, text) = srv.post_raw(RawPost {
        path,
        content_type: Some("text/plain; charset=utf-8"),
        body,
        token: None,
    })?;
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
    Ok((status, json))
}

/// Polls `probe` until it reports at least `count` of `what`, 5 s cap.
/// `fire_flow` creates the kickoff inside its spawned task — behind the flow's
/// member locks — so a 202 no longer means the message exists yet.
fn wait_until(
    count: usize,
    what: &str,
    probe: impl Fn() -> anyhow::Result<usize>,
) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let seen = probe()?;
        if seen >= count {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("only {seen} {what} after 5s, expected {count}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn injected_count(srv: &TestServer) -> anyhow::Result<usize> {
    let injected = srv
        .handles
        .injector
        .injected
        .lock()
        .map_err(|e| anyhow::anyhow!("injector lock poisoned: {e}"))?;
    Ok(injected.len())
}

fn message_count(srv: &TestServer) -> anyhow::Result<usize> {
    let (_, body) = srv.get("/v1/messages", None)?;
    Ok(body["messages"].as_array().map_or(0, Vec::len))
}

/// The id of the one message the trigger created.
fn kickoff_id(srv: &TestServer) -> anyhow::Result<String> {
    wait_until(1, "messages", || message_count(srv))?;
    let (_, body) = srv.get("/v1/messages", None)?;
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    assert_eq!(messages.len(), 1, "expected exactly one kickoff: {body}");
    assert!(
        messages[0]["from"]
            .as_str()
            .unwrap_or_default()
            .starts_with("trigger:"),
        "a flow kickoff carries the trigger origin (#24): {}",
        messages[0]
    );
    Ok(messages[0]["id"].as_str().unwrap_or_default().to_string())
}

/// The newest of `count` messages, once that many exist — the second kickoff in
/// the per-flow contract test (`/v1/messages` is newest first).
fn newest_kickoff_id(srv: &TestServer, count: usize) -> anyhow::Result<String> {
    wait_until(count, "messages", || message_count(srv))?;
    let (_, body) = srv.get("/v1/messages", None)?;
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    Ok(messages[0]["id"].as_str().unwrap_or_default().to_string())
}

/// Answers the kickoff ask as the target agent, from inside the server runtime.
fn reply_to_kickoff(srv: &TestServer, id: &str, text: &str) -> anyhow::Result<()> {
    let router = srv.handles.router.clone();
    let id = MessageId(id.to_string());
    let text = text.to_string();
    srv.block_on(async move {
        router
            .reply(Origin::Agent(AgentId("builder".to_string())), &id, 0, text)
            .await
    })?;
    Ok(())
}

/// Answers the kickoff with `body` as soon as it is injected, from inside the
/// server runtime. A `?wait` long-poll parks this thread, so the reply that
/// resolves it has to be driven from elsewhere.
fn spawn_replier(srv: &TestServer, body: &str) {
    let router = srv.handles.router.clone();
    let body = body.to_string();
    srv.handles.rt.spawn(async move {
        for _ in 0..400_u32 {
            let pending = router
                .list_messages(coretempo_core::router::MessageFilter {
                    to: Some(AgentId("builder".to_string())),
                    from: None,
                    status: None,
                    kind: Some(MessageKind::Ask),
                    since: None,
                    limit: 10,
                })
                .await
                .unwrap_or_default();
            if let Some(record) = pending
                .iter()
                .find(|r| r.status == MessageStatus::Working || r.status == MessageStatus::Injected)
            {
                let _ = router
                    .reply(
                        Origin::Agent(AgentId("builder".to_string())),
                        &record.id,
                        0,
                        body,
                    )
                    .await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
}

fn poll_status(srv: &TestServer, id: &str) -> anyhow::Result<serde_json::Value> {
    for _ in 0..100_u32 {
        let (status, body) = srv.get(&format!("/v1/trigger/{id}"), None)?;
        assert_eq!(status, 200, "status lookup failed: {body}");
        if body["status"] != "running" {
            return Ok(body);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    anyhow::bail!("trigger {id} never left running")
}

#[test]
fn a_text_body_is_accepted_and_becomes_the_kickoff() -> anyhow::Result<()> {
    // No 415: the JSON content-type guard is exempted for this one route, since
    // webhook callers post whatever their sender sends.
    let srv = triggerable(EdgeKind::Ask)?;
    let (status, body) = fire(&srv, "/v1/flows/hook/trigger", b"ship the thing")?;
    assert_eq!(status, 202, "body: {body}");
    let id = body["trigger_id"].as_str().unwrap_or_default().to_string();
    assert!(id.starts_with("t-"), "trigger_id: {id}");
    assert_eq!(body["position"], 0);

    wait_until(1, "injections", || injected_count(&srv))?;
    let injected = srv.handles.injector.injected.lock().unwrap_or_else(|e| {
        panic!("injector lock poisoned: {e}");
    });
    assert_eq!(injected.len(), 1, "one injection expected: {injected:?}");
    assert_eq!(injected[0].0, AgentId("builder".to_string()));
    assert!(
        injected[0].1.contains("ship the thing"),
        "the body is the kickoff message: {}",
        injected[0].1
    );
    Ok(())
}

/// Amendment 31 (#42): the kickoff's header names the flow it belongs to. A
/// target holding two flows' output contracts otherwise learns which schema
/// applies only from a 422.
#[test]
fn the_injected_kickoff_names_its_flow() -> anyhow::Result<()> {
    let srv = triggerable(EdgeKind::Ask)?;
    let (status, body) = fire(&srv, "/v1/flows/hook/trigger", b"ship the thing")?;
    assert_eq!(status, 202, "body: {body}");

    wait_until(1, "injections", || injected_count(&srv))?;
    let injected = srv
        .handles
        .injector
        .injected
        .lock()
        .unwrap_or_else(|e| panic!("injector lock poisoned: {e}"));
    let text = &injected[0].1;
    let id = text
        .split_whitespace()
        .find(|word| word.starts_with("m-"))
        .unwrap_or_default();
    assert_eq!(
        text.lines().next().unwrap_or_default(),
        format!("[CoreTempo {id} from http, flow hook — reply expected] ship the thing"),
        "the kickoff header names its flow: {text}"
    );
    Ok(())
}

/// An agent-to-agent ask belongs to no flow, so its header is unchanged — the
/// label means "this is a flow kickoff", and a false one would point the target
/// at a contract that does not gate its reply.
#[test]
fn an_agent_ask_carries_no_flow_label() -> anyhow::Result<()> {
    let srv = triggerable(EdgeKind::Ask)?;
    let router = srv.handles.router.clone();
    srv.block_on(async move {
        router
            .create_message(
                Origin::Agent(AgentId("planner".to_string())),
                AgentId("builder".to_string()),
                MessageKind::Ask,
                "is it done?".to_string(),
            )
            .await
    })?;
    wait_until(1, "injections", || injected_count(&srv))?;
    let injected = srv
        .handles
        .injector
        .injected
        .lock()
        .unwrap_or_else(|e| panic!("injector lock poisoned: {e}"));
    assert!(
        !injected[0].1.contains("flow"),
        "an agent ask names no flow: {}",
        injected[0].1
    );
    Ok(())
}

#[test]
fn carriage_returns_are_normalized_out_of_the_payload() -> anyhow::Result<()> {
    // A raw CR is Enter to the injection queue: it would submit the prompt
    // mid-payload and lose the rest.
    let srv = triggerable(EdgeKind::Ask)?;
    let (status, _) = fire(
        &srv,
        "/v1/flows/hook/trigger",
        b"line one\r\nline two\rline three",
    )?;
    assert_eq!(status, 202);
    wait_until(1, "injections", || injected_count(&srv))?;
    let injected = srv
        .handles
        .injector
        .injected
        .lock()
        .unwrap_or_else(|e| panic!("injector lock poisoned: {e}"));
    assert!(
        !injected[0].1.contains('\r'),
        "payload kept a carriage return: {:?}",
        injected[0].1
    );
    assert!(injected[0].1.contains("line one\nline two\nline three"));
    Ok(())
}

#[test]
fn a_flowless_workflow_404s_with_the_toml_that_declares_one() -> anyhow::Result<()> {
    // Nothing is declared, so naming the (empty) flow roster tells the caller
    // nothing: the fix is a flow, and the error has to carry it — the file to
    // edit, a pasteable section, and the agent ids valid inside it.
    let srv = support::start_default()?;
    let (status, body) = fire(&srv, "/v1/flows/any/trigger", b"hi")?;
    assert_eq!(status, 404, "body: {body}");
    assert_eq!(body["error"]["code"], "unknown_flow");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("declares no [flows.<name>]"),
        "the error must say there are none to fire: {message}"
    );
    assert!(
        message.contains("tempo.toml"),
        "the error must name the file to edit: {message}"
    );
    assert!(
        message.contains("[flows.any]")
            && message.contains("agents = [\"builder\"]")
            && message.contains("type = \"webhook\"")
            && message.contains("edge = { to = \"builder\", kind = \"ask\" }"),
        "the error must carry a pasteable webhook flow: {message}"
    );
    assert!(
        message.contains("builder, planner"),
        "the error must list the roster the flow can name: {message}"
    );
    Ok(())
}

#[test]
fn the_flowless_404s_snippet_falls_back_to_a_valid_flow_name() -> anyhow::Result<()> {
    // The requested name is echoed into the snippet so pasting it makes the
    // caller's own request work — but only when it is a legal flow name.
    let srv = support::start_default()?;
    let (status, body) = fire(&srv, "/v1/flows/NOT%20A%20NAME/trigger", b"hi")?;
    assert_eq!(status, 404, "body: {body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("[flows.main]"),
        "an unusable name must not be pasted back as TOML: {message}"
    );
    Ok(())
}

#[test]
fn a_second_trigger_while_one_runs_is_a_conflict_naming_the_active_id() -> anyhow::Result<()> {
    let srv = triggerable(EdgeKind::Ask)?;
    let (_, first) = fire(&srv, "/v1/flows/hook/trigger", b"one")?;
    let active = first["trigger_id"].as_str().unwrap_or_default().to_string();

    let (status, body) = fire(&srv, "/v1/flows/hook/trigger", b"two")?;
    assert_eq!(status, 409, "body: {body}");
    assert_eq!(body["error"]["code"], "trigger_in_flight");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(&active),
        "the conflict must name the active trigger '{active}': {message}"
    );
    // The rejected trigger created no second kickoff.
    wait_until(1, "messages", || message_count(&srv))?;
    let (_, messages) = srv.get("/v1/messages", None)?;
    assert_eq!(messages["messages"].as_array().map(Vec::len), Some(1));
    Ok(())
}

#[test]
fn status_runs_then_completes_with_the_reply_body() -> anyhow::Result<()> {
    let srv = triggerable(EdgeKind::Ask)?;
    let (_, body) = fire(&srv, "/v1/flows/hook/trigger", b"do it")?;
    let id = body["trigger_id"].as_str().unwrap_or_default().to_string();

    let (status, running) = srv.get(&format!("/v1/trigger/{id}"), None)?;
    assert_eq!(status, 200);
    assert_eq!(running["trigger_id"], id.as_str());
    assert_eq!(running["status"], "running");

    reply_to_kickoff(&srv, &kickoff_id(&srv)?, "shipped")?;
    let done = poll_status(&srv, &id)?;
    assert_eq!(done["status"], "completed");
    assert_eq!(done["result"], "replied");
    assert_eq!(done["code"], 0);
    assert_eq!(done["reply"], "shipped");

    // The workflow is free again once the kickoff completed.
    let (status, _) = fire(&srv, "/v1/flows/hook/trigger", b"again")?;
    assert_eq!(status, 202, "a completed trigger must release the workflow");
    Ok(())
}

#[test]
fn an_unknown_trigger_id_is_404_showing_the_id_shape() -> anyhow::Result<()> {
    let srv = triggerable(EdgeKind::Ask)?;
    let (status, body) = srv.get("/v1/trigger/t-deadbeef", None)?;
    assert_eq!(status, 404, "body: {body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("t-"),
        "the error must show the id shape: {message}"
    );
    Ok(())
}

#[test]
fn wait_returns_the_completed_status_inline() -> anyhow::Result<()> {
    let srv = triggerable(EdgeKind::Ask)?;
    // The reply has to land while the long-poll is parked, so it is driven from
    // the server runtime while this thread blocks in the POST.
    spawn_replier(&srv, "inline");

    let (status, body) = fire(&srv, "/v1/flows/hook/trigger?wait=10", b"do it")?;
    assert_eq!(status, 200, "body: {body}");
    assert!(
        body["trigger_id"]
            .as_str()
            .unwrap_or_default()
            .starts_with("t-")
    );
    assert_eq!(body["status"], "completed");
    assert_eq!(body["result"], "replied");
    assert_eq!(body["reply"], "inline");
    Ok(())
}

#[test]
fn a_wait_that_expires_falls_back_to_the_accepted_shape() -> anyhow::Result<()> {
    let srv = triggerable(EdgeKind::Ask)?;
    let (status, body) = fire(&srv, "/v1/flows/hook/trigger?wait=1", b"never answered")?;
    assert_eq!(status, 202, "body: {body}");
    assert_eq!(body["position"], 0);
    assert!(
        body["trigger_id"]
            .as_str()
            .unwrap_or_default()
            .starts_with("t-")
    );
    Ok(())
}

#[test]
fn an_oversize_payload_is_refused() -> anyhow::Result<()> {
    let srv = triggerable(EdgeKind::Ask)?;
    let body = vec![b'x'; PAYLOAD_CAP_BYTES + 1];
    let (status, text) = srv.post_raw(RawPost {
        path: "/v1/flows/hook/trigger",
        content_type: Some("text/plain"),
        body: &body,
        token: None,
    })?;
    assert_eq!(status, 413, "body: {text}");
    assert!(
        text.contains("65536"),
        "the error must state the cap: {text}"
    );
    // Nothing was injected.
    let (_, messages) = srv.get("/v1/messages", None)?;
    assert_eq!(messages["messages"].as_array().map(Vec::len), Some(0));
    Ok(())
}

#[test]
fn a_payload_at_the_cap_is_accepted() -> anyhow::Result<()> {
    let srv = triggerable(EdgeKind::Ask)?;
    let body = vec![b'x'; PAYLOAD_CAP_BYTES];
    let (status, text) = srv.post_raw(RawPost {
        path: "/v1/flows/hook/trigger",
        content_type: Some("text/plain"),
        body: &body,
        token: None,
    })?;
    assert_eq!(status, 202, "the cap is inclusive: {text}");
    Ok(())
}

#[test]
fn a_non_utf8_payload_is_rejected() -> anyhow::Result<()> {
    let srv = triggerable(EdgeKind::Ask)?;
    let (status, text) = srv.post_raw(RawPost {
        path: "/v1/flows/hook/trigger",
        content_type: Some("application/octet-stream"),
        body: &[0xff, 0xfe, 0x00],
        token: None,
    })?;
    assert_eq!(status, 400, "body: {text}");
    assert!(text.contains("UTF-8"), "body: {text}");
    Ok(())
}

#[test]
fn the_content_type_exemption_does_not_weaken_auth() -> anyhow::Result<()> {
    let srv = triggerable(EdgeKind::Ask)?;
    let (status, text) = srv.post_raw(RawPost {
        path: "/v1/flows/hook/trigger",
        content_type: Some("text/plain"),
        body: b"let me in",
        token: Some("not-the-token"),
    })?;
    assert_eq!(status, 401, "body: {text}");
    let (status, text) = srv.post_raw(RawPost {
        path: "/v1/trigger/t-deadbeef",
        content_type: Some("text/plain"),
        body: b"",
        token: Some("not-the-token"),
    })?;
    assert_eq!(status, 401, "body: {text}");
    Ok(())
}

#[test]
fn the_route_list_mentions_the_trigger_endpoints() -> anyhow::Result<()> {
    let srv = triggerable(EdgeKind::Ask)?;
    let (status, body) = srv.get("/v1/nope", None)?;
    assert_eq!(status, 404);
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("GET /v1/flows"), "{message}");
    assert!(
        message.contains("POST /v1/flows/{name}/trigger"),
        "{message}"
    );
    assert!(message.contains("GET /v1/trigger/{id}"), "{message}");
    Ok(())
}

#[test]
fn completed_wire_includes_output_object() -> anyhow::Result<()> {
    // Dual emission: the parsed value for a caller that wants fields, the raw
    // reply text for one that wants what the agent actually wrote. The fence is
    // what makes the two differ — it survives into `reply` and is repaired away
    // before `output`.
    let srv = triggerable_with_output(2)?;
    spawn_replier(&srv, "```json\n{\"name\":\"x\"}\n```");
    let (status, body) = fire(&srv, "/v1/flows/hook/trigger?wait=10", b"do it")?;
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["result"], "replied");
    assert_eq!(body["output"]["name"], "x");
    assert_eq!(body["reply"], "```json\n{\"name\":\"x\"}\n```");
    Ok(())
}

#[test]
fn failed_wire_includes_reason_code() -> anyhow::Result<()> {
    // max_repairs 0: the router has no budget to reject with, so the off-schema
    // reply is accepted there and the trigger boundary fails it instead.
    let srv = triggerable_with_output(0)?;
    spawn_replier(&srv, "{}");
    let (status, body) = fire(&srv, "/v1/flows/hook/trigger?wait=10", b"do it")?;
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(body["status"], "failed");
    assert_eq!(body["reason_code"], "schema_validation_failed");
    assert!(
        body["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("at (root)"),
        "the reason must carry the violations: {body}"
    );
    Ok(())
}

#[test]
fn a_send_kickoff_uses_the_configured_kind() -> anyhow::Result<()> {
    let srv = triggerable(EdgeKind::Send)?;
    let (status, _) = fire(&srv, "/v1/flows/hook/trigger", b"fire and forget")?;
    assert_eq!(status, 202);
    wait_until(1, "messages", || message_count(&srv))?;
    let (_, body) = srv.get("/v1/messages", None)?;
    assert_eq!(body["messages"][0]["kind"], "send");
    assert_eq!(body["messages"][0]["to"], "builder");
    Ok(())
}

/// The `on_start` twin of `test_ctx_with_two_webhook_flows`'s "a": a flow that
/// fires its message at launch, which `fire_flow` must refuse over HTTP.
fn on_start_flow(name: &str) -> (FlowName, FlowConfig) {
    let (name, mut flow) = support::builder_webhook_flow(name);
    flow.trigger.trigger_type = TriggerType::OnStart;
    flow.trigger.message = Some("go".to_string());
    (name, flow)
}

/// `builder` alone, the member every flow in these tests contends on.
fn builder_only() -> std::collections::BTreeSet<AgentId> {
    [AgentId("builder".to_string())].into_iter().collect()
}

#[test]
fn a_torn_down_trigger_task_releases_its_flows_slot() -> anyhow::Result<()> {
    // The trigger task claims the flow's in-flight slot before it can settle
    // anything; without a settle-on-drop guard a task that ends without
    // reaching `finish` — a panic, a cancelled runtime — wedges the flow and
    // every later trigger to it 409s for the life of the process.
    let (ctx, handles) = support::test_ctx_with_two_webhook_flows()?;
    let hub = std::sync::Arc::clone(&ctx.triggers);
    let flow = FlowName("b".to_string());
    // Partial move: the rest of `handles` (the stop signal included) stays
    // alive, so the parked task is torn down by the runtime and nothing else.
    let rt = handles.rt;
    let held = rt.block_on({
        let ctx = ctx.clone();
        async move { ctx.agent_locks.acquire(&builder_only()).await }
    });
    rt.block_on(coretempo_core::api::trigger::fire_flow(
        ctx,
        flow.clone(),
        None,
        "payload".to_string(),
    ))
    .map_err(|err| anyhow::anyhow!("expected 202, got {}: {}", err.status, err.message))?;
    let id = hub
        .in_flight(&flow)
        .ok_or_else(|| anyhow::anyhow!("the accepted trigger did not claim its flow"))?;

    // Dropping the runtime cancels the parked task mid-await, the way a panic
    // in it would unwind: its guards drop without any explicit settle.
    drop(rt);
    drop(held);

    assert_eq!(
        hub.in_flight(&flow),
        None,
        "the torn-down task left flow '{}' claimed; every later trigger 409s",
        flow.0
    );
    let status = hub
        .get(&id)
        .ok_or_else(|| anyhow::anyhow!("trigger {id} vanished from the hub"))?;
    match status {
        coretempo_core::trigger::TriggerStatus::Failed { reason_code, .. } => {
            assert_eq!(reason_code, "internal");
        }
        other => anyhow::bail!("expected a settled failure, got {other:?}"),
    }
    Ok(())
}

#[test]
fn a_trigger_parked_on_a_lock_fails_when_the_run_stops() -> anyhow::Result<()> {
    // A trigger waiting on a contended member when the run stops must not go
    // on to create its kickoff: `Run::stop` has already killed the PTY
    // manager, so the injection would vanish into a dead session.
    let (ctx, handles) = support::test_ctx_with_two_webhook_flows()?;
    let rt = handles.rt.clone();
    let flow = FlowName("b".to_string());
    rt.block_on(async move {
        let held = ctx.agent_locks.acquire(&builder_only()).await;
        coretempo_core::api::trigger::fire_flow(
            ctx.clone(),
            flow.clone(),
            None,
            "payload".to_string(),
        )
        .await
        .map_err(|err| anyhow::anyhow!("expected 202, got {}: {}", err.status, err.message))?;
        let id = ctx
            .triggers
            .in_flight(&flow)
            .ok_or_else(|| anyhow::anyhow!("the accepted trigger did not claim its flow"))?;

        // What `Run::stop` trips before it tears the run down.
        handles.stopping.send(true)?;
        let status =
            coretempo_core::trigger::await_terminal(&ctx.triggers, &id, Duration::from_secs(5))
                .await
                .ok_or_else(|| {
                    anyhow::anyhow!("the parked trigger never settled after the stop")
                })?;
        match status {
            coretempo_core::trigger::TriggerStatus::Failed {
                reason,
                reason_code,
                ..
            } => {
                assert_eq!(reason_code, "run_stopped");
                assert!(
                    reason.contains("stopped"),
                    "the reason must say the run stopped: {reason}"
                );
            }
            other => anyhow::bail!("expected a failed trigger, got {other:?}"),
        }

        // Releasing the contended member must not revive the abandoned kickoff.
        drop(held);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let messages = handles
            .router
            .list_messages(coretempo_core::router::MessageFilter::default())
            .await?;
        assert!(
            messages.is_empty(),
            "the stopped run must create no kickoff: {messages:?}"
        );
        assert_eq!(
            ctx.triggers.in_flight(&flow),
            None,
            "the settled trigger must release its flow"
        );
        Ok(())
    })
}

#[test]
fn a_warm_trigger_waits_for_a_contended_exclusive_member() -> anyhow::Result<()> {
    let (ctx, handles) = support::test_ctx_with_two_webhook_flows()?;
    let rt = handles.rt.clone();
    rt.block_on(async move {
        // Hold builder's lock the way another flow's trigger would.
        let members: std::collections::BTreeSet<_> =
            [AgentId("builder".to_string())].into_iter().collect();
        let held = ctx.agent_locks.acquire(&members).await;

        // Fire flow "b": accepted (per-flow slot is free) but its kickoff
        // message must not exist while the lock is held.
        let response = coretempo_core::api::trigger::fire_flow(
            ctx.clone(),
            FlowName("b".to_string()),
            None,
            "payload".to_string(),
        )
        .await
        .map_err(|err| anyhow::anyhow!("expected 202, got {}: {}", err.status, err.message))?;
        assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let messages = handles
            .router
            .list_messages(coretempo_core::router::MessageFilter::default())
            .await?;
        assert!(
            messages.is_empty(),
            "the kickoff must not be created while the member lock is held: {messages:?}"
        );

        drop(held);
        // Released: the kickoff lands and the trigger proceeds.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let messages = handles
                .router
                .list_messages(coretempo_core::router::MessageFilter::default())
                .await?;
            if !messages.is_empty() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "kickoff never created"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    })
}

#[test]
fn fire_flow_rejects_unknown_and_on_start_flows() -> anyhow::Result<()> {
    let (ctx, handles) = support::test_ctx_with_two_webhook_flows()?;
    let rt = handles.rt.clone();
    rt.block_on(async move {
        let err = coretempo_core::api::trigger::fire_flow(
            ctx.clone(),
            FlowName("nope".to_string()),
            None,
            "x".to_string(),
        )
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("an undeclared flow must not be accepted"))?;
        assert_eq!(err.status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(err.code, "unknown_flow");
        assert!(
            err.message.contains("no flow named 'nope'"),
            "quotes the name the caller asked for: {}",
            err.message
        );
        assert!(
            err.message.contains("declared flows: a, b"),
            "lists every declared flow: {}",
            err.message
        );
        Ok::<(), anyhow::Error>(())
    })?;

    let (ctx, handles) = support::test_ctx_with_flows(vec![
        support::builder_webhook_flow("a"),
        on_start_flow("batch"),
    ])?;
    let rt = handles.rt.clone();
    rt.block_on(async move {
        let err = coretempo_core::api::trigger::fire_flow(
            ctx.clone(),
            FlowName("batch".to_string()),
            None,
            "x".to_string(),
        )
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("an on_start flow must not be triggerable over HTTP"))?;
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(err.code, "invalid_request");
        assert!(
            err.message.contains("--flow batch"),
            "names the fix: {}",
            err.message
        );
        Ok(())
    })
}

#[test]
fn a_named_flow_fires_and_an_unknown_name_lists_the_flows() -> anyhow::Result<()> {
    let (ctx, handles) = support::test_ctx_with_two_webhook_flows()?;
    let srv = support::start(ctx, handles)?;

    let (status, body) = fire(&srv, "/v1/flows/a/trigger", b"do it")?;
    assert_eq!(status, 202, "body: {body}");
    assert!(
        body["trigger_id"]
            .as_str()
            .unwrap_or_default()
            .starts_with("t-"),
        "{body}"
    );

    let (status, body) = fire(&srv, "/v1/flows/nope/trigger", b"x")?;
    assert_eq!(status, 404, "body: {body}");
    assert_eq!(body["error"]["code"], "unknown_flow");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("no flow named 'nope'"),
        "quotes the name the caller asked for: {message}"
    );
    assert!(
        message.contains("declared flows: a, b"),
        "lists every declared flow: {message}"
    );
    Ok(())
}

#[test]
fn an_on_start_flow_400s_pointing_at_run_flow() -> anyhow::Result<()> {
    let (ctx, handles) = support::test_ctx_with_flows(vec![
        support::builder_webhook_flow("hook"),
        on_start_flow("batch"),
    ])?;
    let srv = support::start(ctx, handles)?;
    let (status, body) = fire(&srv, "/v1/flows/batch/trigger", b"x")?;
    assert_eq!(status, 400, "body: {body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("--flow batch"), "names the fix: {message}");
    Ok(())
}

#[test]
fn bare_post_trigger_is_gone_and_names_the_new_route() -> anyhow::Result<()> {
    let (ctx, handles) = support::test_ctx_with_flow(webhook(EdgeKind::Ask))?;
    let srv = support::start(ctx, handles)?;
    let (status, body) = fire(&srv, "/v1/trigger", b"x")?;
    assert_eq!(status, 404, "body: {body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("/v1/flows/"), "{message}");
    assert!(
        message.contains("hook"),
        "names the declared flows: {message}"
    );
    Ok(())
}

#[test]
fn get_flows_lists_names_types_targets_and_inflight() -> anyhow::Result<()> {
    let (ctx, handles) = support::test_ctx_with_flows(vec![
        on_start_flow("batch"),
        support::builder_webhook_flow("post"),
    ])?;
    let srv = support::start(ctx, handles)?;
    let (status, body) = srv.get("/v1/flows", None)?;
    assert_eq!(status, 200, "body: {body}");
    let flows = body.as_array().cloned().unwrap_or_default();
    assert_eq!(flows.len(), 2, "{body}");
    assert_eq!(flows[0]["name"], "batch");
    assert_eq!(flows[0]["type"], "on_start");
    assert_eq!(flows[0]["target"], "builder");
    assert_eq!(flows[1]["name"], "post");
    assert_eq!(flows[1]["queue_depth"], 0, "warm runs have no queue");
    assert_eq!(flows[1]["running"], 0);

    let (status, _) = fire(&srv, "/v1/flows/post/trigger", b"go")?;
    assert_eq!(status, 202);
    let (_, body) = srv.get("/v1/flows", None)?;
    assert_eq!(body[1]["running"], 1, "the fired flow is running: {body}");
    assert_eq!(body[0]["running"], 0, "per flow, not global: {body}");
    Ok(())
}

#[test]
fn a_second_trigger_to_the_same_flow_409s_per_flow() -> anyhow::Result<()> {
    // Two webhook flows: one in flight on `a` must not block `b`.
    let (ctx, handles) = support::test_ctx_with_two_webhook_flows()?;
    let srv = support::start(ctx, handles)?;
    let (status, _) = fire(&srv, "/v1/flows/a/trigger", b"one")?;
    assert_eq!(status, 202);
    let (status, body) = fire(&srv, "/v1/flows/a/trigger", b"two")?;
    assert_eq!(status, 409, "body: {body}");
    assert_eq!(body["error"]["code"], "trigger_in_flight");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("per flow"),
        "the 409 copy is per-flow now: {message}"
    );
    let (status, body) = fire(&srv, "/v1/flows/b/trigger", b"three")?;
    assert_eq!(status, 202, "per-flow, not global: {body}");
    Ok(())
}

/// `{"type": "object", "required": [field]}` — the smallest schema that names
/// its own field in the rejection.
fn requires(field: &str) -> serde_json::Value {
    serde_json::json!({ "type": "object", "required": [field] })
}

#[test]
fn each_kickoff_repairs_against_its_own_flows_contract() -> anyhow::Result<()> {
    // Flow "a" requires {"name"}; flow "b" requires {"count"}. Both target
    // `builder`, so a workflow-wide selection would apply "a"'s schema (first
    // in name order) to both — that is the mutation test, built in.
    let (ctx, handles) = support::test_ctx_with_flow_contracts(vec![
        support::webhook_flow_with_schema("a", requires("name"), 3)?,
        support::webhook_flow_with_schema("b", requires("count"), 3)?,
    ])?;
    let srv = support::start(ctx, handles)?;

    // Fire "a"; a reply satisfying b's schema must be rejected naming "name".
    let (status, _) = fire(&srv, "/v1/flows/a/trigger", b"go")?;
    assert_eq!(status, 202);
    let id = kickoff_id(&srv)?;
    let err = reply_to_kickoff(&srv, &id, r#"{"count": 1}"#).expect_err("a's schema applies");
    assert!(err.to_string().contains("name"), "{err}");
    reply_to_kickoff(&srv, &id, r#"{"name": "x"}"#)?;

    // Fire "b"; a reply satisfying a's schema must be rejected naming "count".
    let (status, _) = fire(&srv, "/v1/flows/b/trigger", b"go")?;
    assert_eq!(status, 202);
    let id = newest_kickoff_id(&srv, 2)?;
    let err = reply_to_kickoff(&srv, &id, r#"{"name": "x"}"#).expect_err("b's own schema, not a's");
    assert!(err.to_string().contains("count"), "{err}");
    Ok(())
}

#[test]
fn a_kickoff_with_no_contract_is_not_gated_by_another_flows_schema() -> anyhow::Result<()> {
    // The behaviour contract of the narrowed refusal, converted rather than
    // deleted. Flow "hook" declares a contract targeting `builder`; an
    // on_start-style kickoff to the same agent binds no contract, so its
    // off-schema reply is accepted.
    let (ctx, handles) =
        support::test_ctx_with_flow_contracts(vec![support::webhook_flow_with_schema(
            "hook",
            requires("name"),
            3,
        )?])?;
    let router = std::sync::Arc::clone(&handles.router);
    let rt = handles.rt.clone();
    let srv = support::start(ctx, handles)?;
    let id = rt.block_on(async move {
        // No bind_kickoff_contract call: the reply must not be schema-gated.
        router
            .create_message(
                Origin::Http("deadbeef".to_string()),
                AgentId("builder".to_string()),
                MessageKind::Ask,
                "batch work".to_string(),
            )
            .await
    })?;
    reply_to_kickoff(&srv, &id.id.0, "plain prose, no JSON")?;
    Ok(())
}
