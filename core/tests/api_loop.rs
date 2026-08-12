#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]

//! `POST /v1/agents/{id}/loop-done` (edge-semantics spec): the calling agent
//! ends its loop with `{id}`.

mod support;

use serde_json::json;

#[test]
fn loop_owner_ends_its_loop() -> anyhow::Result<()> {
    let (ctx, handles) = support::test_ctx_with_planner_loop()?;
    let srv = support::start(ctx, handles)?;
    let (status, body) = srv.post("/v1/agents/builder/loop-done", Some("planner"), &json!({}))?;
    assert_eq!(status, 200, "loop-done acks: {body}");
    assert_eq!(body["owner"], "planner");
    assert_eq!(body["target"], "builder");
    assert_eq!(body["loop"], "done");
    Ok(())
}

#[test]
fn loop_done_without_a_loop_edge_names_the_edges() -> anyhow::Result<()> {
    let (ctx, handles) = support::test_ctx_with_planner_loop()?;
    let srv = support::start(ctx, handles)?;
    // builder has no edges at all, so it cannot end a loop with planner.
    let (status, body) = srv.post("/v1/agents/planner/loop-done", Some("builder"), &json!({}))?;
    assert_eq!(status, 409, "no_loop_edge is a conflict: {body}");
    assert_eq!(body["error"]["code"], "no_loop_edge");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("builder") && message.contains("none"),
        "message names the caller and its (empty) edges: {message}"
    );
    Ok(())
}

#[test]
fn loop_done_requires_an_agent_caller() -> anyhow::Result<()> {
    let (ctx, handles) = support::test_ctx_with_planner_loop()?;
    let srv = support::start(ctx, handles)?;
    // No X-CoreTempo-Agent header: HTTP callers cannot end loops.
    let (status, body) = srv.post("/v1/agents/builder/loop-done", None, &json!({}))?;
    assert_eq!(status, 403, "non-agent callers are forbidden: {body}");
    Ok(())
}

#[test]
fn loop_done_unknown_target_is_404() -> anyhow::Result<()> {
    let (ctx, handles) = support::test_ctx_with_planner_loop()?;
    let srv = support::start(ctx, handles)?;
    let (status, body) = srv.post("/v1/agents/ghost/loop-done", Some("planner"), &json!({}))?;
    assert_eq!(status, 404, "unknown target: {body}");
    Ok(())
}
