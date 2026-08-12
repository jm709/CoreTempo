#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]

mod support;

use coretempo_core::types::AgentState;

/// Test-local RFC 4648 decoder so assertions verify bytes, not our own encoder.
fn b64_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = TABLE
            .iter()
            .position(|t| *t == c)
            .ok_or_else(|| anyhow::anyhow!("bad b64 char {c}"))?;
        acc = (acc << 6) | u32::try_from(v)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

fn seeded() -> anyhow::Result<support::TestServer> {
    let (ctx, handles) = support::test_ctx()?;
    handles.fake_pty.set_agent(
        "builder",
        AgentState::Working,
        None,
        vec![(0, b"hello "), (6, b"\x1b[32mworld\x1b[0m")],
    );
    support::start(ctx, handles)
}

#[test]
fn replays_ring_tail_with_cursor_ids_then_goes_live() -> anyhow::Result<()> {
    let srv = seeded()?;
    let mut sse = srv.open_sse("/v1/agents/builder/pty", None)?;
    let first = sse.next_event()?;
    assert_eq!(first.event.as_deref(), Some("pty"));
    assert_eq!(first.id.as_deref(), Some("0"));
    let data: serde_json::Value = serde_json::from_str(&first.data)?;
    assert_eq!(data["seq"], 0);
    assert_eq!(
        b64_decode(data["b64"].as_str().unwrap_or_default())?,
        b"hello "
    );
    let second = sse.next_event()?;
    assert_eq!(second.id.as_deref(), Some("6"));
    let data: serde_json::Value = serde_json::from_str(&second.data)?;
    assert_eq!(
        b64_decode(data["b64"].as_str().unwrap_or_default())?,
        b"\x1b[32mworld\x1b[0m".to_vec()
    );
    srv.handles.fake_pty.push_live("builder", 21, b"$ ");
    let live = sse.next_event()?;
    assert_eq!(live.id.as_deref(), Some("21"));
    Ok(())
}

#[test]
fn since_skips_already_seen_bytes() -> anyhow::Result<()> {
    let srv = seeded()?;
    let mut sse = srv.open_sse("/v1/agents/builder/pty?since=6", None)?;
    assert_eq!(sse.next_event()?.id.as_deref(), Some("6"));
    Ok(())
}

#[test]
fn unknown_agent_is_404() -> anyhow::Result<()> {
    let srv = seeded()?;
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build(),
    );
    let mut res = agent
        .get(srv.url("/v1/agents/ghost/pty"))
        .header("Authorization", format!("Bearer {}", srv.token))
        .call()?;
    assert_eq!(res.status().as_u16(), 404);
    let body: serde_json::Value = res.body_mut().read_json()?;
    assert_eq!(body["error"]["code"], "unknown_agent");
    Ok(())
}
