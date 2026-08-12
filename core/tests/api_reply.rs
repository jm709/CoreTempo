#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]

mod support;

use coretempo_core::types::event::{Event, EventPayload};
use serde_json::json;
use tokio::sync::broadcast;

fn create_ask(srv: &support::TestServer) -> anyhow::Result<String> {
    let (_, body) = srv.post(
        "/v1/messages",
        Some("planner"),
        &json!({"to": "builder", "kind": "ask", "body": "done?"}),
    )?;
    Ok(body["id"].as_str().unwrap_or_default().to_string())
}

#[test]
fn first_reply_200_identical_replay_200_conflict_409() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let id = create_ask(&srv)?;
    let path = format!("/v1/messages/{id}/reply");
    let (status, body) = srv.post(&path, Some("builder"), &json!({"code": 0, "body": "yes"}))?;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "replied");
    assert_eq!(
        (body["code"].as_u64(), body["reply"].as_str()),
        (Some(0), Some("yes"))
    );
    // Bash-retry safety: identical replay is a 200 no-op.
    let (status, _) = srv.post(&path, Some("builder"), &json!({"code": 0, "body": "yes"}))?;
    assert_eq!(status, 200);
    // Conflicting replay: different body.
    let (status, body) = srv.post(&path, Some("builder"), &json!({"code": 1, "body": "no"}))?;
    assert_eq!(status, 409);
    assert_eq!(body["error"]["code"], "already_replied");
    Ok(())
}

#[test]
fn wrong_replier_403_send_409_bad_code_400() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let id = create_ask(&srv)?;
    let path = format!("/v1/messages/{id}/reply");
    let (status, body) = srv.post(&path, Some("planner"), &json!({"code": 0, "body": "y"}))?;
    assert_eq!(
        (status, body["error"]["code"].as_str().unwrap_or_default()),
        (403, "wrong_replier")
    );
    let (status, body) = srv.post(&path, None, &json!({"code": 0, "body": "y"}))?;
    assert_eq!(
        (status, body["error"]["code"].as_str().unwrap_or_default()),
        (403, "wrong_replier")
    );
    let (status, body) = srv.post(&path, Some("builder"), &json!({"code": 7, "body": "y"}))?;
    assert_eq!(
        (status, body["error"]["code"].as_str().unwrap_or_default()),
        (400, "invalid_request")
    );
    let (_, sent) = srv.post(
        "/v1/messages",
        Some("planner"),
        &json!({"to": "builder", "kind": "send", "body": "fyi"}),
    )?;
    let sid = sent["id"].as_str().unwrap_or_default();
    let (status, body) = srv.post(
        &format!("/v1/messages/{sid}/reply"),
        Some("builder"),
        &json!({"code": 0, "body": "ack"}),
    )?;
    assert_eq!(
        (status, body["error"]["code"].as_str().unwrap_or_default()),
        (409, "not_an_ask")
    );
    Ok(())
}

/// An HTTP-origin ask to `to`. No `X-CoreTempo-Agent` header ⇒ `Origin::Http`.
fn create_http_ask_to(srv: &support::TestServer, to: &str) -> anyhow::Result<String> {
    let (_, body) = srv.post(
        "/v1/messages",
        None,
        &json!({"to": to, "kind": "ask", "body": "translate"}),
    )?;
    Ok(body["id"].as_str().unwrap_or_default().to_string())
}

/// The kickoff shape the webhook trigger creates: an HTTP-origin ask to the
/// contract's target agent.
fn create_http_ask(srv: &support::TestServer) -> anyhow::Result<String> {
    create_http_ask_to(srv, "builder")
}

fn reply_path(id: &str) -> String {
    format!("/v1/messages/{id}/reply")
}

/// Every `reply.rejected` payload sitting on the bus, as (agent, errors).
fn rejections(events: &mut broadcast::Receiver<Event>) -> Vec<(String, String)> {
    let mut found = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EventPayload::ReplyRejected { agent, errors, .. } = event.payload {
            found.push((agent.0, errors));
        }
    }
    found
}

#[test]
fn http_ask_invalid_reply_is_422_with_actionable_body() -> anyhow::Result<()> {
    let srv = support::start_with_output(2)?;
    let id = create_http_ask(&srv)?;
    let mut events = srv.handles.bus.subscribe();
    let (status, body) = srv.post(
        &reply_path(&id),
        Some("builder"),
        &json!({"code": 0, "body": "{\"wrong\":true}"}),
    )?;
    assert_eq!(status, 422);
    assert_eq!(body["error"]["code"], "schema_validation_failed");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    for needle in ["at ", "[schema:", "--code 1", "1 attempt"] {
        assert!(message.contains(needle), "no {needle:?} in: {message}");
    }
    // The ask stays open so the agent repairs inside the same turn.
    let (_, rec) = srv.get(&format!("/v1/messages/{id}"), None)?;
    let now = rec["status"].as_str().unwrap_or_default();
    assert!(
        matches!(now, "queued" | "injected" | "working"),
        "a rejected reply must leave the ask open, got '{now}'"
    );
    assert!(rec["reply"].is_null(), "no reply is stored on rejection");
    let published = rejections(&mut events);
    assert_eq!(
        published.len(),
        1,
        "one reply.rejected event: {published:?}"
    );
    assert_eq!(published[0].0, "builder");
    assert!(
        published[0].1.contains("tempo reply rejected"),
        "event carries the rendered rejection: {}",
        published[0].1
    );
    Ok(())
}

#[test]
fn http_ask_valid_reply_after_rejection_succeeds() -> anyhow::Result<()> {
    let srv = support::start_with_output(2)?;
    let id = create_http_ask(&srv)?;
    let (status, _) = srv.post(
        &reply_path(&id),
        Some("builder"),
        &json!({"code": 0, "body": "{\"wrong\":true}"}),
    )?;
    assert_eq!(status, 422);
    let (status, body) = srv.post(
        &reply_path(&id),
        Some("builder"),
        &json!({"code": 0, "body": "{\"name\":\"ada\"}"}),
    )?;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "replied");
    assert_eq!(body["reply"], "{\"name\":\"ada\"}", "stored verbatim");
    Ok(())
}

#[test]
fn budget_exhaustion_accepts_the_reply() -> anyhow::Result<()> {
    let srv = support::start_with_output(1)?;
    let id = create_http_ask(&srv)?;
    let invalid = json!({"code": 0, "body": "{\"wrong\":true}"});
    let (status, _) = srv.post(&reply_path(&id), Some("builder"), &invalid)?;
    assert_eq!(status, 422);
    let (status, body) = srv.post(&reply_path(&id), Some("builder"), &invalid)?;
    assert_eq!(
        status, 200,
        "the budget is spent; the trigger fails it later"
    );
    assert_eq!(body["status"], "replied");
    assert_eq!(body["reply"], "{\"wrong\":true}");
    Ok(())
}

#[test]
fn code_1_bypasses_validation() -> anyhow::Result<()> {
    let srv = support::start_with_output(2)?;
    let id = create_http_ask(&srv)?;
    let (status, body) = srv.post(
        &reply_path(&id),
        Some("builder"),
        &json!({"code": 1, "body": "cannot do this"}),
    )?;
    assert_eq!(status, 200, "the --code 1 escape hatch is never validated");
    assert_eq!(body["code"], 1);
    assert_eq!(body["reply"], "cannot do this");
    Ok(())
}

#[test]
fn agent_to_agent_ask_is_not_validated() -> anyhow::Result<()> {
    let srv = support::start_with_output(2)?;
    let id = create_ask(&srv)?; // Origin::Agent(planner), not the kickoff
    let (status, body) = srv.post(
        &reply_path(&id),
        Some("builder"),
        &json!({"code": 0, "body": "not json at all"}),
    )?;
    assert_eq!(status, 200, "the contract binds the kickoff ask only");
    assert_eq!(body["status"], "replied");
    Ok(())
}

#[test]
fn http_ask_to_a_non_target_agent_is_not_validated() -> anyhow::Result<()> {
    let srv = support::start_with_output(2)?;
    let id = create_http_ask_to(&srv, "planner")?; // HTTP origin, but not the target
    let (status, body) = srv.post(
        &reply_path(&id),
        Some("planner"),
        &json!({"code": 0, "body": "not json at all"}),
    )?;
    assert_eq!(status, 200, "the contract binds its target agent only");
    assert_eq!(body["status"], "replied");
    Ok(())
}

#[test]
fn max_repairs_zero_validates_once_never_reasks() -> anyhow::Result<()> {
    let srv = support::start_with_output(0)?;
    let id = create_http_ask(&srv)?;
    let (status, body) = srv.post(
        &reply_path(&id),
        Some("builder"),
        &json!({"code": 0, "body": "{\"wrong\":true}"}),
    )?;
    assert_eq!(status, 200, "max_repairs 0 never re-asks");
    assert_eq!(body["status"], "replied");
    Ok(())
}

#[test]
fn fenced_but_valid_reply_is_accepted() -> anyhow::Result<()> {
    let srv = support::start_with_output(2)?;
    let id = create_http_ask(&srv)?;
    let fenced = "```json\n{\"name\":\"ada\"}\n```";
    let (status, body) = srv.post(
        &reply_path(&id),
        Some("builder"),
        &json!({"code": 0, "body": fenced}),
    )?;
    assert_eq!(status, 200, "repair strips the fence before validating");
    assert_eq!(body["reply"], fenced, "stored verbatim, fence and all");
    Ok(())
}

#[test]
fn reply_to_unknown_message_is_404() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let (status, body) = srv.post(
        "/v1/messages/m-00000000/reply",
        Some("builder"),
        &json!({"code": 0, "body": "y"}),
    )?;
    assert_eq!(
        (status, body["error"]["code"].as_str().unwrap_or_default()),
        (404, "unknown_message")
    );
    Ok(())
}
