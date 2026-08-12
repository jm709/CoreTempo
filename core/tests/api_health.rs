#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]

mod support;

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
