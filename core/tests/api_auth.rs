#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]

mod support;

use std::io::{Read, Write};

/// Raw HTTP/1.1 request so we control the Host header exactly.
fn raw_request(addr: std::net::SocketAddr, req: &str) -> anyhow::Result<String> {
    let mut stream = std::net::TcpStream::connect(addr)?;
    stream.write_all(req.as_bytes())?;
    let mut out = String::new();
    stream.read_to_string(&mut out)?;
    Ok(out)
}

#[test]
fn missing_or_wrong_token_is_401() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let (status, body) = srv.get_raw("/v1/workflow")?;
    assert_eq!(status, 401);
    assert_eq!(body["error"]["code"], "unauthorized");
    let bad = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build(),
    );
    let mut res = bad
        .get(srv.url("/v1/workflow"))
        .header("Authorization", format!("Bearer {}", "0".repeat(64)))
        .call()?;
    assert_eq!(res.status().as_u16(), 401);
    let body: serde_json::Value = res.body_mut().read_json()?;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Bearer")
    );
    Ok(())
}

#[test]
fn workflow_endpoint_returns_frozen_file_with_token() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let (status, body) = srv.get("/v1/workflow", None)?;
    assert_eq!(status, 200);
    assert_eq!(body["workflow"]["workflow"]["name"], "test");
    assert!(
        body["run_id"]
            .as_str()
            .unwrap_or_default()
            .starts_with("r-")
    );
    assert!(
        body["started_at"]
            .as_str()
            .unwrap_or_default()
            .ends_with('Z')
    );
    Ok(())
}

#[test]
fn bad_host_is_403_good_hosts_pass() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let auth = format!("Authorization: Bearer {}", srv.token);
    let bad = raw_request(
        srv.addr,
        &format!(
            "GET /v1/workflow HTTP/1.1\r\nHost: evil.example.com\r\n{auth}\r\nConnection: close\r\n\r\n"
        ),
    )?;
    assert!(bad.starts_with("HTTP/1.1 403"), "got: {bad}");
    assert!(bad.contains("invalid_host"));
    for host in [
        format!("127.0.0.1:{}", srv.addr.port()),
        "localhost".to_string(),
    ] {
        let ok = raw_request(
            srv.addr,
            &format!(
                "GET /v1/workflow HTTP/1.1\r\nHost: {host}\r\n{auth}\r\nConnection: close\r\n\r\n"
            ),
        )?;
        assert!(ok.starts_with("HTTP/1.1 200"), "host {host} got: {ok}");
    }
    Ok(())
}

#[test]
fn non_json_content_type_is_415() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build(),
    );
    let mut res = agent
        .post(srv.url("/v1/messages"))
        .header("Authorization", format!("Bearer {}", srv.token))
        .send("plain text")?;
    assert_eq!(res.status().as_u16(), 415);
    let body: serde_json::Value = res.body_mut().read_json()?;
    assert_eq!(body["error"]["code"], "unsupported_media_type");
    Ok(())
}

#[test]
fn health_stays_open_but_other_routes_do_not() -> anyhow::Result<()> {
    let srv = support::start_default()?;
    assert_eq!(srv.get_raw("/v1/health")?.0, 200);
    assert_eq!(srv.get_raw("/v1/workflow")?.0, 401);
    Ok(())
}

#[test]
fn serve_refuses_non_loopback_without_provisioned_token() -> anyhow::Result<()> {
    let (mut ctx, handles) = support::test_ctx()?;
    ctx.bind = std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);
    ctx.token_provisioned = false;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let err = rt.block_on(coretempo_core::api::serve(ctx)).err();
    let _ = handles;
    let msg = err.map(|e| e.to_string()).unwrap_or_default();
    assert!(msg.contains("provisioned token"), "got: {msg}");
    Ok(())
}

#[test]
fn write_api_file_sets_0600_and_leaves_current_alone() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let runs = support::temp_path("runs");
    let run_a = coretempo_core::types::RunId("r-aaaaaaaa".to_string());
    let token = coretempo_core::types::Token("ab".repeat(32));
    let path = coretempo_core::api::auth::write_api_file(&runs, &run_a, 4820, &token)?;
    let mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "api.json mode {mode:o}");
    let parsed: coretempo_core::types::ApiFile =
        serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    assert_eq!(parsed.port, 4820);
    assert_eq!(parsed.run_id, run_a);
    // Repointing is a separate, opt-in step (`RunOptions::repoint_current`).
    assert!(std::fs::read_link(runs.join("current")).is_err());
    Ok(())
}

#[test]
fn repoint_current_follows_the_latest_run() -> anyhow::Result<()> {
    let runs = support::temp_path("runs");
    let run_a = coretempo_core::types::RunId("r-aaaaaaaa".to_string());
    let run_b = coretempo_core::types::RunId("r-bbbbbbbb".to_string());
    let token = coretempo_core::types::Token("ab".repeat(32));
    coretempo_core::api::auth::write_api_file(&runs, &run_a, 4820, &token)?;
    coretempo_core::api::auth::repoint_current(&runs, &run_a)?;
    assert_eq!(
        std::fs::read_link(runs.join("current"))?,
        std::path::PathBuf::from("r-aaaaaaaa")
    );
    coretempo_core::api::auth::write_api_file(&runs, &run_b, 4821, &token)?;
    coretempo_core::api::auth::repoint_current(&runs, &run_b)?;
    assert_eq!(
        std::fs::read_link(runs.join("current"))?,
        std::path::PathBuf::from("r-bbbbbbbb")
    );
    Ok(())
}
