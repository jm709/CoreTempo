//! Minimal scripted HTTP/1.1 server: serves N canned responses on an ephemeral port and
//! records each request (method, path, headers, body) for assertions.
#![expect(
    dead_code,
    reason = "each integration-test crate uses a subset of this harness"
)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;

pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub struct StubServer {
    pub port: u16,
    requests: mpsc::Receiver<Request>,
    thread: Option<JoinHandle<()>>,
}

impl StubServer {
    pub fn requests(&self) -> Vec<Request> {
        self.requests.try_iter().collect()
    }
}

impl Drop for StubServer {
    fn drop(&mut self) {
        // Wake a still-pending `accept()` so an under-requesting binary fails the test
        // instead of deadlocking the join below. Closed port => connect errors, ignored.
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Each `(status, json_body)` pair answers one request, in order, then the server exits.
pub fn serve(responses: Vec<(u16, String)>) -> anyhow::Result<StubServer> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        for (status, body) in responses {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            if handle_conn(stream, status, &body, &tx).is_err() {
                return;
            }
        }
    });
    Ok(StubServer {
        port,
        requests: rx,
        thread: Some(thread),
    })
}

fn handle_conn(
    stream: std::net::TcpStream,
    status: u16,
    body: &str,
    tx: &mpsc::Sender<Request>,
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim().is_empty() {
        anyhow::bail!("connection closed without a request line (shutdown probe)");
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    let mut content_length = 0usize;
    loop {
        let mut h = String::new();
        reader.read_line(&mut h)?;
        let h = h.trim_end().to_string();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(": ") {
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().unwrap_or(0);
            }
            headers.push((k.to_string(), v.to_string()));
        }
    }
    let mut buf = vec![0u8; content_length];
    reader.read_exact(&mut buf)?;
    let _ = tx.send(Request {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&buf).to_string(),
    });
    let mut stream = stream;
    write!(
        stream,
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    Ok(())
}

/// Runs the real `tempo` binary against the stub with token env set. Stdin is
/// closed (not inherited), so a `state` command's stdin drain sees EOF immediately
/// instead of blocking on the test harness's own stdin.
pub fn tempo(args: &[&str], port: u16, agent_id: Option<&str>) -> anyhow::Result<Output> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tempo"));
    cmd.args(args)
        .env("CORETEMPO_PORT", port.to_string())
        .env("CORETEMPO_TOKEN", "t".repeat(64))
        .env_remove("CORETEMPO_AGENT_ID")
        .stdin(Stdio::null());
    if let Some(id) = agent_id {
        cmd.env("CORETEMPO_AGENT_ID", id);
    }
    Ok(cmd.output()?)
}

/// Like `tempo`, but pipes `stdin` to the process — mirrors how Claude Code feeds a
/// hook its JSON payload on stdin.
pub fn tempo_with_stdin(
    args: &[&str],
    port: u16,
    agent_id: Option<&str>,
    stdin: &str,
) -> anyhow::Result<Output> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tempo"));
    cmd.args(args)
        .env("CORETEMPO_PORT", port.to_string())
        .env("CORETEMPO_TOKEN", "t".repeat(64))
        .env_remove("CORETEMPO_AGENT_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(id) = agent_id {
        cmd.env("CORETEMPO_AGENT_ID", id);
    }
    let mut child = cmd.spawn()?;
    if let Some(mut child_stdin) = child.stdin.take() {
        child_stdin.write_all(stdin.as_bytes())?;
    }
    Ok(child.wait_with_output()?)
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

pub fn exit_code(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

pub fn record_json(
    id: &str,
    kind: &str,
    status: &str,
    code: Option<u8>,
    reply: Option<&str>,
) -> String {
    let code = code.map_or("null".to_string(), |c| c.to_string());
    let reply = reply.map_or("null".to_string(), |r| format!("\"{r}\""));
    format!(
        concat!(
            "{{\"id\":\"{id}\",\"kind\":\"{kind}\",\"from\":\"agent:planner\",",
            "\"to\":\"builder\",\"body\":\"b\",\"status\":\"{status}\",\"code\":{code},",
            "\"reply\":{reply},\"created_at\":\"2026-08-01T17:03:11Z\",",
            "\"injected_at\":null,\"completed_at\":null}}"
        ),
        id = id,
        kind = kind,
        status = status,
        code = code,
        reply = reply
    )
}
