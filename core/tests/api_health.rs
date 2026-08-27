#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]

mod support;

use support::RawPost;

#[test]
fn health_is_unauthenticated_and_reports_liveness() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    // Deliberately no Authorization header.
    let (status, body) = srv.get_raw("/v1/health")?;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    let run_id = body["run_id"].as_str().unwrap_or_default();
    assert!(
        run_id.starts_with("r-") && run_id.len() == 10,
        "bad run_id: {run_id}"
    );
    assert!(body["uptime_secs"].as_u64().is_some());
    Ok(())
}

#[test]
fn health_lists_every_declared_flow_and_counts_the_running_ones() -> anyhow::Result<()> {
    // A warm run has no queue, so each declared flow is named at 0 rather than
    // omitted — a caller polling one workflow's health has to be able to tell
    // "no flow by that name" from "that flow is idle" — and `running` is the
    // count of flows with a trigger in flight, not a constant.
    let (ctx, handles) = support::test_ctx_with_flows(vec![
        support::builder_webhook_flow("a"),
        support::builder_webhook_flow("b"),
    ])?;
    let srv = support::start(ctx, handles)?;

    let (status, body) = srv.get_raw("/v1/health")?;
    assert_eq!(status, 200);
    assert_eq!(body["queued"]["a"], 0, "flow 'a' is not listed: {body}");
    assert_eq!(body["queued"]["b"], 0, "flow 'b' is not listed: {body}");
    assert_eq!(body["running"], 0, "nothing has been fired yet: {body}");

    // Firing claims the flow's in-flight slot on the request path, so health
    // reports it as soon as the 202 lands.
    let (status, text) = srv.post_raw(RawPost {
        path: "/v1/flows/a/trigger",
        content_type: Some("text/plain"),
        body: b"go",
        token: None,
    })?;
    assert_eq!(status, 202, "body: {text}");
    let (_, body) = srv.get_raw("/v1/health")?;
    assert_eq!(body["running"], 1, "the fired flow is counted: {body}");
    assert_eq!(
        body["queued"]["a"], 0,
        "a warm run runs its trigger rather than queueing it: {body}"
    );
    Ok(())
}

#[test]
fn health_counts_blocked_agents() -> anyhow::Result<()> {
    let (ctx, handles) = support::test_ctx()?;
    handles.fake_pty.set_blocked("builder", true);
    let srv = support::start(ctx, handles)?;
    let (_, body) = srv.get_raw("/v1/health")?;
    assert_eq!(body["blocked"], 1);
    Ok(())
}
