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
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("Bearer"), "{message}");
    // A run publishes its token, so the 401 keeps pointing at the file that
    // carries it — only serve mode, which publishes none, says something else.
    assert!(message.contains("api.json"), "{message}");
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

/// One raw POST with exact headers: `headers` is spliced in as written, so a
/// test can send a request with no `Content-Type` at all — which no HTTP client
/// will do for you.
fn raw_post(
    srv: &support::TestServer,
    path: &str,
    headers: &str,
    body: &str,
) -> anyhow::Result<String> {
    raw_request(
        srv.addr,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\n\
             {headers}Connection: close\r\n\r\n{body}",
            srv.token
        ),
    )
}

#[test]
fn a_body_less_post_needs_no_content_type() -> anyhow::Result<()> {
    // #57: `curl -X POST .../restart` sends no body and no Content-Type, and
    // the body-less endpoints have nothing to parse — a 415 there is a retry
    // that teaches the operator nothing.
    let srv = support::start_default()?;
    let with_length = raw_post(
        &srv,
        "/v1/agents/planner/restart",
        "Content-Length: 0\r\n",
        "",
    )?;
    assert!(
        with_length.starts_with("HTTP/1.1 202"),
        "empty POST refused: {with_length}"
    );
    // No Content-Length either: HTTP/1.1 reads that as no body at all.
    let bare = raw_post(&srv, "/v1/agents/planner/restart", "", "")?;
    assert!(
        bare.starts_with("HTTP/1.1 202"),
        "bare POST refused: {bare}"
    );

    // A declared content type still has to be JSON, body or no body.
    let typed = raw_post(
        &srv,
        "/v1/agents/planner/restart",
        "Content-Type: text/plain\r\nContent-Length: 0\r\n",
        "",
    )?;
    assert!(
        typed.starts_with("HTTP/1.1 415"),
        "text/plain accepted: {typed}"
    );
    // And a body without a JSON content type is still refused.
    let untyped_body = raw_post(&srv, "/v1/messages", "Content-Length: 10\r\n", "plain text")?;
    assert!(
        untyped_body.starts_with("HTTP/1.1 415"),
        "an untyped body was accepted: {untyped_body}"
    );
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
    ctx.core.bind = std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);
    ctx.core.token_provisioned = false;
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

/// A hook token is scoped to its own agent's state route (spec 2026-08-27 §3).
#[test]
fn a_hook_token_reports_its_own_state_and_nothing_else() -> anyhow::Result<()> {
    let (mut ctx, handles) = support::test_ctx()?;
    let hook = coretempo_core::types::Token::generate();
    ctx.core.auth = std::sync::Arc::new(support::HookTokens {
        operator: handles.token.clone(),
        hooks: vec![(
            coretempo_core::types::AgentId("builder".into()),
            hook.clone(),
        )],
    });
    let srv = support::start(ctx, handles)?;
    // Its own state route, no X-CoreTempo-Agent header: identity comes from the token.
    let (status, body) = srv.post_json_as(
        "/v1/agents/builder/state",
        &serde_json::json!({"state": "working"}),
        Some(&hook.0),
    )?;
    assert_eq!(status, 200, "{body}");
    // Another agent's state route.
    let (status, body) = srv.post_json_as(
        "/v1/agents/planner/state",
        &serde_json::json!({"state": "working"}),
        Some(&hook.0),
    )?;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "forbidden_scope");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("POST /v1/agents/builder/state"),
        "{body}"
    );
    // A spoofed header on its own route.
    let (status, body) = srv.post_json_as_agent(
        "/v1/agents/builder/state",
        &serde_json::json!({"state": "working"}),
        Some(&hook.0),
        "planner",
    )?;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "wrong_agent");
    // The production shape: `tempo state` always sends X-CoreTempo-Agent with
    // the hook token, and a matching header is accepted.
    let (status, body) = srv.post_json_as_agent(
        "/v1/agents/builder/state",
        &serde_json::json!({"state": "idle"}),
        Some(&hook.0),
        "builder",
    )?;
    assert_eq!(status, 200, "{body}");
    // Every other route.
    for path in [
        "/v1/agents",
        "/v1/messages",
        "/v1/events",
        "/v1/agents/builder/pty",
    ] {
        let (status, body) = srv.get_as(path, Some(&hook.0))?;
        assert_eq!(status, 403, "{path}: {body}");
    }
    Ok(())
}
