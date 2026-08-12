//! ANSI stripping for PTY output, plus the idle debouncer (Task 12) that sits
//! downstream of the raw `AgentState` signal. A false idle firing `/clear`
//! mid-task is the worst failure in the system — bias toward Working.

use crate::types::agent::AgentState;

/// Stateful ANSI stripper: CSI (`ESC [ … final`), OSC (`ESC ] … BEL|ST`), and
/// two-byte escapes are removed; only printable ASCII and `\n` pass. State is
/// kept across `feed` calls so escape sequences split across PTY chunk
/// boundaries are still stripped correctly.
pub struct AnsiStripper {
    mode: StripMode,
}

enum StripMode {
    Text,
    Esc,
    Csi,
    Osc,
    OscEsc,
}

impl AnsiStripper {
    #[must_use]
    pub fn new() -> AnsiStripper {
        AnsiStripper {
            mode: StripMode::Text,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len());
        for &b in bytes {
            self.mode = match self.mode {
                StripMode::Text => match b {
                    0x1b => StripMode::Esc,
                    b'\n' => {
                        out.push('\n');
                        StripMode::Text
                    }
                    0x20..=0x7e => {
                        out.push(char::from(b));
                        StripMode::Text
                    }
                    _ => StripMode::Text,
                },
                StripMode::Esc => match b {
                    b'[' => StripMode::Csi,
                    b']' => StripMode::Osc,
                    _ => StripMode::Text,
                },
                StripMode::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        StripMode::Text
                    } else {
                        StripMode::Csi
                    }
                }
                StripMode::Osc => match b {
                    0x07 => StripMode::Text,
                    0x1b => StripMode::OscEsc,
                    _ => StripMode::Osc,
                },
                StripMode::OscEsc => StripMode::Text,
            };
        }
        out
    }
}

impl Default for AnsiStripper {
    fn default() -> AnsiStripper {
        AnsiStripper::new()
    }
}

/// Debounced state signal: `Idle` requires `dwell` of stable raw idle before it
/// propagates; every other state propagates immediately. All ACTIONS
/// (injection gating, auto-`/clear`, `send` completion) key off this signal;
/// the raw signal is for the UI/event bus only.
pub(crate) fn spawn_debouncer(
    mut raw: tokio::sync::watch::Receiver<AgentState>,
    dwell: std::time::Duration,
) -> tokio::sync::watch::Receiver<AgentState> {
    let (tx, rx) = tokio::sync::watch::channel(*raw.borrow());
    tokio::spawn(async move {
        loop {
            let current = *raw.borrow_and_update();
            if current == AgentState::Idle && *tx.borrow() != AgentState::Idle {
                tokio::select! {
                    () = tokio::time::sleep(dwell) => {
                        let _ = tx.send(AgentState::Idle);
                    }
                    changed = raw.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                }
            } else {
                if current != *tx.borrow() {
                    let _ = tx.send(current);
                }
                if raw.changed().await.is_err() {
                    return;
                }
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::watch;

    use crate::pty::detector::{AnsiStripper, spawn_debouncer};
    use crate::types::agent::AgentState;

    const DWELL: Duration = Duration::from_secs(2);

    #[test]
    fn stripper_removes_csi_and_osc() {
        let mut s = AnsiStripper::new();
        assert_eq!(
            s.feed(b"\x1b[2K\x1b[38;5;244mhi\x1b[0m\x1b]0;title\x07!"),
            "hi!"
        );
    }

    #[test]
    fn stripper_survives_split_escape() {
        let mut s = AnsiStripper::new();
        let mut out = s.feed(b"a\x1b[38;5;2");
        out.push_str(&s.feed(b"44mb"));
        assert_eq!(out, "ab");
    }

    #[tokio::test(start_paused = true)]
    async fn idle_needs_stable_dwell() {
        let (tx, raw) = watch::channel(AgentState::Working);
        let mut deb = spawn_debouncer(raw, DWELL);
        assert_eq!(*deb.borrow(), AgentState::Working);

        tx.send(AgentState::Idle).unwrap();
        tokio::time::sleep(Duration::from_millis(1900)).await;
        assert_eq!(
            *deb.borrow(),
            AgentState::Working,
            "not yet: dwell not elapsed"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        deb.changed().await.unwrap();
        assert_eq!(*deb.borrow(), AgentState::Idle);
    }

    #[tokio::test(start_paused = true)]
    async fn working_blip_cancels_pending_idle() {
        let (tx, raw) = watch::channel(AgentState::Working);
        let mut deb = spawn_debouncer(raw, DWELL);

        tx.send(AgentState::Idle).unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        tx.send(AgentState::Working).unwrap(); // false idle interrupted
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            *deb.borrow(),
            AgentState::Working,
            "idle never debounced through"
        );

        tx.send(AgentState::Idle).unwrap();
        tokio::time::sleep(Duration::from_millis(2100)).await;
        deb.changed().await.unwrap();
        assert_eq!(*deb.borrow(), AgentState::Idle);
    }

    #[tokio::test(start_paused = true)]
    async fn non_idle_states_propagate_immediately() {
        let (tx, raw) = watch::channel(AgentState::Starting);
        let mut deb = spawn_debouncer(raw, DWELL);
        for state in [
            AgentState::Working,
            AgentState::Restarting,
            AgentState::Exited,
        ] {
            tx.send(state).unwrap();
            deb.changed().await.unwrap();
            assert_eq!(*deb.borrow(), state);
        }
    }
}
