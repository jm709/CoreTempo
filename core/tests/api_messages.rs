#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]

mod support;

use coretempo_core::types::message::Origin;
use coretempo_core::types::{AgentId, MessageId};
use serde_json::json;

#[test]
fn create_ask_as_agent_returns_201_record() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let (status, body) = srv.post(
        "/v1/messages",
        Some("planner"),
        &json!({"to": "builder", "kind": "ask", "body": "migration done?"}),
    )?;
    assert_eq!(status, 201);
    let id = body["id"].as_str().unwrap_or_default();
    assert!(id.starts_with("m-") && id.len() == 10, "bad id: {id}");
    assert_eq!(body["kind"], "ask");
    assert_eq!(body["from"], "agent:planner");
    assert_eq!(body["to"], "builder");
    assert_eq!(body["body"], "migration done?");
    assert!(body["code"].is_null() && body["reply"].is_null());
    Ok(())
}

#[test]
fn create_without_agent_header_gets_http_origin() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let (status, body) = srv.post(
        "/v1/messages",
        None,
        &json!({"to": "builder", "kind": "send", "body": "hi"}),
    )?;
    assert_eq!(status, 201);
    let from = body["from"].as_str().unwrap_or_default();
    assert!(
        from.starts_with("http:") && from.len() == "http:".len() + 8,
        "from: {from}"
    );
    Ok(())
}

#[test]
fn unknown_target_is_404_with_roster() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let (status, body) = srv.post(
        "/v1/messages",
        Some("planner"),
        &json!({"to": "buidler", "kind": "ask", "body": "?"}),
    )?;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "unknown_agent");
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("builder, planner"), "msg: {msg}");
    Ok(())
}

#[test]
fn bad_agent_header_and_bad_body_are_400() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let (status, body) = srv.post(
        "/v1/messages",
        Some("Not An Agent"),
        &json!({"to": "builder", "kind": "ask", "body": "?"}),
    )?;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_request");
    let (status, body) = srv.post(
        "/v1/messages",
        Some("planner"),
        &json!({"to": "builder", "kind": "shout", "body": "?"}),
    )?;
    assert_eq!(
        (status, body["error"]["code"].as_str().unwrap_or_default()),
        (400, "invalid_request")
    );
    Ok(())
}

#[test]
fn get_message_roundtrip_and_unknown_404() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let (_, created) = srv.post(
        "/v1/messages",
        Some("planner"),
        &json!({"to": "builder", "kind": "send", "body": "x"}),
    )?;
    let id = created["id"].as_str().unwrap_or_default().to_string();
    let (status, body) = srv.get(&format!("/v1/messages/{id}"), None)?;
    assert_eq!(status, 200);
    assert_eq!(body["id"], id.as_str());
    let (status, body) = srv.get("/v1/messages/m-00000000", None)?;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "unknown_message");
    Ok(())
}

#[test]
fn list_filters_and_rejects_bad_status() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    srv.post(
        "/v1/messages",
        Some("planner"),
        &json!({"to": "builder", "kind": "ask", "body": "a"}),
    )?;
    srv.post(
        "/v1/messages",
        Some("builder"),
        &json!({"to": "planner", "kind": "send", "body": "b"}),
    )?;
    let (status, body) = srv.get("/v1/messages?to=builder&kind=ask", None)?;
    assert_eq!(status, 200);
    let msgs = body["messages"].as_array().cloned().unwrap_or_default();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["to"], "builder");
    let (status, body) = srv.get("/v1/messages?status=bogus", None)?;
    assert_eq!(status, 400);
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("queued"), "should list valid statuses: {msg}");
    Ok(())
}

#[test]
fn wait_returns_200_with_current_record_on_timeout() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let (_, created) = srv.post(
        "/v1/messages",
        Some("planner"),
        &json!({"to": "builder", "kind": "ask", "body": "slow"}),
    )?;
    let id = created["id"].as_str().unwrap_or_default().to_string();
    let t0 = std::time::Instant::now();
    let (status, body) = srv.get(&format!("/v1/messages/{id}?wait=1"), None)?;
    assert_eq!(status, 200);
    assert!(t0.elapsed() >= std::time::Duration::from_millis(900));
    let s = body["status"].as_str().unwrap_or_default();
    assert!(
        s == "queued" || s == "injected" || s == "working",
        "status: {s}"
    );
    Ok(())
}

#[test]
fn wait_resolves_when_reply_lands() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let (_, created) = srv.post(
        "/v1/messages",
        Some("planner"),
        &json!({"to": "builder", "kind": "ask", "body": "done?"}),
    )?;
    let id = created["id"].as_str().unwrap_or_default().to_string();
    let router = srv.handles.router.clone();
    let mid = MessageId(id.clone());
    let replier = Origin::Agent(AgentId("builder".to_string()));
    let waker = std::thread::spawn({
        let srv_rt = srv.block_on(async { tokio::runtime::Handle::current() });
        move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            srv_rt.block_on(router.reply(replier, &mid, 0, "yes".to_string()))
        }
    });
    let (status, body) = srv.get(&format!("/v1/messages/{id}?wait=10"), None)?;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "replied");
    assert_eq!(body["code"], 0);
    assert_eq!(body["reply"], "yes");
    waker
        .join()
        .map_err(|_| anyhow::anyhow!("waker panicked"))?
        .map_err(|e| anyhow::anyhow!("reply failed: {e}"))?;
    Ok(())
}
