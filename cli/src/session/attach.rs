//! `tempo session attach <id>`: the PTY stream to stdout, stdin (raw) to the
//! PTY, the terminal size on start and whenever it changes, `Ctrl-]` to
//! detach. Consumes `/v1/events?agent=<id>` beside the PTY stream because the
//! PTY stream never ends on exit (subscribers outlive the child and span
//! resumes).

use std::io::{BufReader, IsTerminal, Read, Write};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use coretempo_core::types::{SessionState, SessionView};

use crate::client::Client;
use crate::session::b64;
use crate::session::sse::SseReader;

const DETACH: u8 = 0x1d;
const SIZE_POLL: Duration = Duration::from_millis(500);

/// Everything the four reader threads can tell the main loop.
enum Msg {
    Output(Vec<u8>),
    Input(Vec<u8>),
    Resize(u16, u16),
    Detach,
    Exited(String),
    Transport(String),
}

/// How an attachment ended, once the terminal has been put back.
enum Outcome {
    Detached,
    Exited(String),
}

/// Raw-mode guard: restores the terminal on drop, so every exit path — detach,
/// session exit, transport error — leaves the operator's shell usable. `None`
/// when stdin is not a terminal (a pipe, as the tests attach over) or termios
/// refused, in which case there is nothing to restore.
struct RawMode(Option<libc::termios>);

impl RawMode {
    fn enable() -> RawMode {
        if !std::io::stdin().is_terminal() {
            return RawMode(None);
        }
        // SAFETY: `termios` is plain data, and tcgetattr/tcsetattr take a
        // pointer to one plus fd 0, which this process owns.
        unsafe {
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &raw mut original) != 0 {
                return RawMode(None);
            }
            let mut settings = original;
            libc::cfmakeraw(&raw mut settings);
            if libc::tcsetattr(0, libc::TCSANOW, &raw const settings) != 0 {
                return RawMode(None);
            }
            RawMode(Some(original))
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(original) = self.0 {
            // SAFETY: restoring the very termios `enable` read off fd 0.
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &raw const original);
            }
        }
    }
}

/// The attached terminal's size, or `None` when stdout is not one.
fn terminal_size() -> Option<(u16, u16)> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    // SAFETY: `winsize` is plain data, and TIOCGWINSZ takes a pointer to one
    // plus fd 1, which this process owns.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &raw mut ws) != 0 || ws.ws_col == 0 {
            return None;
        }
        Some((ws.ws_col, ws.ws_row))
    }
}

fn is_live(state: SessionState) -> bool {
    match state {
        SessionState::Starting | SessionState::Idle | SessionState::Working => true,
        SessionState::Stopped | SessionState::Exited => false,
    }
}

pub(crate) fn run(client: &Client, id: &str) -> anyhow::Result<ExitCode> {
    let view: SessionView = serde_json::from_value(client.get(&format!("/sessions/{id}"))?)?;
    if !is_live(view.state) {
        eprintln!(
            "session {id} is not live ({}); resume it first with 'tempo session resume {id}'",
            super::state_word(&view)
        );
        return Ok(ExitCode::from(1));
    }
    // The guard outlives `attached`, so the terminal is back before anything
    // is printed — including the error `?` propagates out of here.
    let raw = RawMode::enable();
    let outcome = attached(client, id);
    drop(raw);
    match outcome? {
        Outcome::Detached => Ok(ExitCode::SUCCESS),
        Outcome::Exited(how) => {
            eprintln!("\r\nsession {id} exited ({how})");
            Ok(ExitCode::from(1))
        }
    }
}

/// The attachment proper: four reader threads feeding one loop that owns the
/// terminal and the HTTP client.
fn attached(client: &Client, id: &str) -> anyhow::Result<Outcome> {
    let (tx, rx) = mpsc::channel::<Msg>();
    spawn_pty_reader(client, id, tx.clone())?;
    spawn_event_watcher(client, id, tx.clone())?;
    spawn_stdin_reader(tx.clone());
    spawn_size_poller(tx);
    let mut stdout = std::io::stdout().lock();
    let mut last_size = None;
    loop {
        match rx.recv() {
            Ok(Msg::Output(bytes)) => {
                stdout.write_all(&bytes)?;
                stdout.flush()?;
            }
            Ok(Msg::Input(bytes)) => client.post_raw(&format!("/sessions/{id}/pty"), &bytes)?,
            Ok(Msg::Resize(cols, rows)) => {
                if last_size != Some((cols, rows)) {
                    last_size = Some((cols, rows));
                    client.post(
                        &format!("/sessions/{id}/pty/resize"),
                        &serde_json::json!({"cols": cols, "rows": rows}),
                    )?;
                }
            }
            Ok(Msg::Detach) => return Ok(Outcome::Detached),
            Ok(Msg::Exited(how)) => return Ok(Outcome::Exited(how)),
            Ok(Msg::Transport(error)) => anyhow::bail!("lost the sessions daemon: {error}"),
            Err(_) => anyhow::bail!("lost the sessions daemon: every stream ended"),
        }
    }
}

/// The PTY SSE stream: `pty` events carry base64 chunks; anything else on that
/// stream is ignored.
fn spawn_pty_reader(client: &Client, id: &str, tx: mpsc::Sender<Msg>) -> anyhow::Result<()> {
    let body = client.stream(&format!("/sessions/{id}/pty"))?;
    std::thread::spawn(move || {
        let mut reader = SseReader::new(BufReader::new(body));
        loop {
            match reader.next_event() {
                Ok(Some(event)) if event.event.as_deref() == Some("pty") => {
                    let bytes = serde_json::from_str::<serde_json::Value>(&event.data)
                        .ok()
                        .and_then(|v| v["b64"].as_str().and_then(b64::decode));
                    if let Some(bytes) = bytes
                        && tx.send(Msg::Output(bytes)).is_err()
                    {
                        return;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => {
                    let _ = tx.send(Msg::Transport("the pty stream closed".to_string()));
                    return;
                }
                Err(error) => {
                    let _ = tx.send(Msg::Transport(error.to_string()));
                    return;
                }
            }
        }
    });
    Ok(())
}

/// The control-plane stream, filtered to this session: the one place an exit
/// shows up, since the PTY stream stays open across it.
fn spawn_event_watcher(client: &Client, id: &str, tx: mpsc::Sender<Msg>) -> anyhow::Result<()> {
    let body = client.stream(&format!("/events?agent={id}"))?;
    std::thread::spawn(move || {
        let mut reader = SseReader::new(BufReader::new(body));
        while let Ok(Some(event)) = reader.next_event() {
            if event.event.as_deref() != Some("agent.lifecycle") {
                continue;
            }
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&event.data) else {
                continue;
            };
            if json["phase"] == "exited" {
                let _ = tx.send(Msg::Exited(exit_word(&json["exit"])));
                return;
            }
        }
    });
    Ok(())
}

/// `AgentExit` as the message names it: `{"code":3}` or `{"signal":"Hangup"}`.
fn exit_word(exit: &serde_json::Value) -> String {
    match (&exit["code"], &exit["signal"]) {
        (serde_json::Value::Number(code), _) => format!("code {code}"),
        (_, serde_json::Value::String(signal)) => format!("signal {signal}"),
        _ => "reason unknown".to_string(),
    }
}

/// stdin verbatim, except `Ctrl-]`, which detaches. EOF (a closed pipe) ends
/// the thread without detaching — the attachment lives on the PTY, not stdin.
fn spawn_stdin_reader(tx: mpsc::Sender<Msg>) {
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            let n = match stdin.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            let chunk = &buf[..n];
            match chunk.iter().position(|b| *b == DETACH) {
                Some(at) => {
                    if at > 0 {
                        let _ = tx.send(Msg::Input(chunk[..at].to_vec()));
                    }
                    let _ = tx.send(Msg::Detach);
                    return;
                }
                None => {
                    if tx.send(Msg::Input(chunk.to_vec())).is_err() {
                        return;
                    }
                }
            }
        }
    });
}

/// Polls `TIOCGWINSZ` rather than handling `SIGWINCH`: a signal handler may
/// call almost nothing, and 500 ms is imperceptible for a window drag.
fn spawn_size_poller(tx: mpsc::Sender<Msg>) {
    std::thread::spawn(move || {
        loop {
            if let Some((cols, rows)) = terminal_size()
                && tx.send(Msg::Resize(cols, rows)).is_err()
            {
                return;
            }
            std::thread::sleep(SIZE_POLL);
        }
    });
}
