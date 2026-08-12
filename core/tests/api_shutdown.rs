#![expect(
    clippy::panic_in_result_fn,
    reason = "assertions are the vocabulary of tests"
)]

//! `ApiServerHandle::shutdown` must be bounded. An SSE stream never ends on its
//! own — `pump_events` lives as long as the bus does — so an unbounded graceful
//! wait for in-flight responses hangs forever with any client attached.

mod support;

use std::time::Duration;

use coretempo_core::api::serve;
use coretempo_core::types::event::EventPayload;
use coretempo_core::types::{AgentId, AgentState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Opens `/v1/events` over raw TCP and reads until the replayed event arrives,
/// which proves the handler ran and the response is streaming.
async fn attach_sse(
    addr: std::net::SocketAddr,
    token: &str,
) -> anyhow::Result<tokio::net::TcpStream> {
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    let request = format!(
        "GET /v1/events?since=0 HTTP/1.1\r\nHost: {addr}\r\n\
         Authorization: Bearer {token}\r\nAccept: text/event-stream\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    let mut seen = String::new();
    while !seen.contains("agent.state") {
        let mut buf = [0u8; 512];
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await??;
        assert!(read > 0, "server closed the stream before sending anything");
        seen.push_str(&String::from_utf8_lossy(&buf[..read]));
    }
    assert!(
        seen.starts_with("HTTP/1.1 200"),
        "expected a streaming 200, got: {seen}"
    );
    Ok(stream)
}

#[test]
fn shutdown_is_bounded_with_an_sse_client_attached() -> anyhow::Result<()> {
    let (ctx, handles) = support::test_ctx()?;
    let token = ctx.token.0.clone();
    let bus = handles.bus.clone();
    let rt = handles.rt.clone();
    rt.block_on(async move {
        // Published before the client connects so `?since=0` replays it: the
        // client cannot report "attached" until the handler has really run.
        bus.publish(EventPayload::AgentStateChanged {
            agent: AgentId("builder".to_string()),
            state: AgentState::Working,
        });
        let server = serve(ctx).await?;
        let addr = server.local_addr();
        let _client = attach_sse(addr, &token).await?;
        tokio::time::timeout(Duration::from_secs(5), server.shutdown())
            .await
            .map_err(|_| anyhow::anyhow!("shutdown did not return with an SSE client attached"))?;
        anyhow::Ok(())
    })?;
    Ok(())
}

/// Sends one request with `Connection: close` and reads the whole response.
async fn plain_get(addr: std::net::SocketAddr, token: &str, path: &str) -> anyhow::Result<String> {
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\n\
         Authorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response)).await??;
    Ok(String::from_utf8_lossy(&response).into_owned())
}

#[test]
fn ordinary_requests_still_complete_and_shutdown_releases_the_port() -> anyhow::Result<()> {
    // The bound must not disturb non-streaming traffic, and once shutdown
    // returns the listener really is gone.
    let (ctx, handles) = support::test_ctx()?;
    let token = ctx.token.0.clone();
    let rt = handles.rt.clone();
    rt.block_on(async move {
        let server = serve(ctx).await?;
        let addr = server.local_addr();
        let response = plain_get(addr, &token, "/v1/health").await?;
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "health did not answer: {response}"
        );
        tokio::time::timeout(Duration::from_secs(5), server.shutdown())
            .await
            .map_err(|_| anyhow::anyhow!("shutdown did not return after a plain request"))?;
        // Bound means finished: the listener is gone, so a fresh connect fails
        // or immediately sees EOF.
        let probe = tokio::net::TcpStream::connect(addr).await;
        if let Ok(mut stream) = probe {
            let mut buf = [0u8; 16];
            let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await?;
            assert_eq!(read.unwrap_or(0), 0, "the listener is still serving");
        }
        anyhow::Ok(())
    })?;
    Ok(())
}
