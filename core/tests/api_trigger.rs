#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]

//! Warm-run trigger endpoints (spec triggers §4): a webhook workflow running
//! interactively answers `POST /v1/trigger` against its live roster.

use std::time::Duration;

use coretempo_core::trigger::PAYLOAD_CAP_BYTES;
use coretempo_core::types::config::{Edge, EdgeKind, TriggerConfig, TriggerType};
use coretempo_core::types::message::{MessageKind, MessageStatus, Origin};
use coretempo_core::types::{AgentId, MessageId};
use support::{RawPost, TestServer};

mod support;

fn webhook(kind: EdgeKind) -> TriggerConfig {
    TriggerConfig {
        trigger_type: TriggerType::Webhook,
        edge: Edge {
            to: AgentId("builder".to_string()),
            kind,
            max_rounds: None,
        },
        message: None,
        output: None,
    }
}

fn triggerable(kind: EdgeKind) -> anyhow::Result<TestServer> {
    let (ctx, handles) = support::test_ctx_with_trigger(webhook(kind))?;
    support::start(ctx, handles)
}

/// A triggerable workflow whose kickoff target owes `{"name": <string>}`.
fn triggerable_with_output(max_repairs: u32) -> anyhow::Result<TestServer> {
    let (ctx, handles) =
        support::test_ctx_with_trigger_and_output(webhook(EdgeKind::Ask), max_repairs)?;
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

/// The id of the one message the trigger created.
fn kickoff_id(srv: &TestServer) -> anyhow::Result<String> {
    let (_, body) = srv.get("/v1/messages", None)?;
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    assert_eq!(messages.len(), 1, "expected exactly one kickoff: {body}");
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
    let (status, body) = fire(&srv, "/v1/trigger", b"ship the thing")?;
    assert_eq!(status, 202, "body: {body}");
    let id = body["trigger_id"].as_str().unwrap_or_default().to_string();
    assert!(id.starts_with("t-"), "trigger_id: {id}");
    assert_eq!(body["position"], 0);

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

#[test]
fn carriage_returns_are_normalized_out_of_the_payload() -> anyhow::Result<()> {
    // A raw CR is Enter to the injection queue: it would submit the prompt
    // mid-payload and lose the rest.
    let srv = triggerable(EdgeKind::Ask)?;
    let (status, _) = fire(&srv, "/v1/trigger", b"line one\r\nline two\rline three")?;
    assert_eq!(status, 202);
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
fn a_workflow_without_a_webhook_trigger_says_how_to_declare_one() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let (status, body) = fire(&srv, "/v1/trigger", b"hi")?;
    assert_eq!(status, 404, "body: {body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert_eq!(body["error"]["code"], "invalid_request");
    assert!(
        message.contains("[trigger]") && message.contains("webhook"),
        "the error must name the fix: {message}"
    );
    Ok(())
}

#[test]
fn a_second_trigger_while_one_runs_is_a_conflict_naming_the_active_id() -> anyhow::Result<()> {
    let srv = triggerable(EdgeKind::Ask)?;
    let (_, first) = fire(&srv, "/v1/trigger", b"one")?;
    let active = first["trigger_id"].as_str().unwrap_or_default().to_string();

    let (status, body) = fire(&srv, "/v1/trigger", b"two")?;
    assert_eq!(status, 409, "body: {body}");
    assert_eq!(body["error"]["code"], "trigger_in_flight");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(&active),
        "the conflict must name the active trigger '{active}': {message}"
    );
    // The rejected trigger created no second kickoff.
    let (_, messages) = srv.get("/v1/messages", None)?;
    assert_eq!(messages["messages"].as_array().map(Vec::len), Some(1));
    Ok(())
}

#[test]
fn status_runs_then_completes_with_the_reply_body() -> anyhow::Result<()> {
    let srv = triggerable(EdgeKind::Ask)?;
    let (_, body) = fire(&srv, "/v1/trigger", b"do it")?;
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
    let (status, _) = fire(&srv, "/v1/trigger", b"again")?;
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

    let (status, body) = fire(&srv, "/v1/trigger?wait=10", b"do it")?;
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
    let (status, body) = fire(&srv, "/v1/trigger?wait=1", b"never answered")?;
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
        path: "/v1/trigger",
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
        path: "/v1/trigger",
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
        path: "/v1/trigger",
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
        path: "/v1/trigger",
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
    assert!(message.contains("POST /v1/trigger"), "{message}");
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
    let (status, body) = fire(&srv, "/v1/trigger?wait=10", b"do it")?;
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
    let (status, body) = fire(&srv, "/v1/trigger?wait=10", b"do it")?;
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
    let (status, _) = fire(&srv, "/v1/trigger", b"fire and forget")?;
    assert_eq!(status, 202);
    let (_, body) = srv.get("/v1/messages", None)?;
    assert_eq!(body["messages"][0]["kind"], "send");
    assert_eq!(body["messages"][0]["to"], "builder");
    Ok(())
}
