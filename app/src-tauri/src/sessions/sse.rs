//! Incremental SSE framing: the bytes reqwest hands us arrive in chunks that
//! have nothing to do with event boundaries, so the parser holds whatever line
//! is half-read and emits only whole events.
//!
//! Line terminators are `\n` and `\r\n` — what the daemon's axum `Sse` writes.
//! A lone `\r`, which the SSE spec also allows, is not a terminator here; no
//! server on the other end of this socket emits one.

/// One dispatched SSE event. `data` is the payload lines joined with `\n`, per
/// the SSE spec.
pub struct SseEvent {
    pub id: Option<String>,
    pub event: Option<String>,
    pub data: String,
}

/// Feed it bytes, get back the events those bytes completed.
#[derive(Default)]
pub struct SseParser {
    /// Bytes received since the last line terminator. Bytes, not a `String`,
    /// because a chunk boundary can fall inside a multi-byte character and
    /// decoding per chunk would replace it with two replacement characters.
    buf: Vec<u8>,
    id: Option<String>,
    event: Option<String>,
    /// The `data:` values of the event being accumulated.
    data: Vec<String>,
}

impl SseParser {
    /// Feed raw bytes; returns every event completed by this chunk.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let mut events = Vec::new();
        self.buf.extend_from_slice(chunk);
        while let Some(end) = self.buf.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if let Some(event) = self.take_line(&String::from_utf8_lossy(&line)) {
                events.push(event);
            }
        }
        events
    }

    /// Applies one complete line, returning the event it dispatched if it was
    /// the blank line that ends one.
    fn take_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None; // a comment — the daemon's keep-alive is one
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "id" => self.id = Some(value.to_string()),
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        None
    }

    /// Ends the accumulated event. An event with no `data:` line dispatches
    /// nothing (SSE spec), which is what keeps a comment-only keep-alive frame
    /// from surfacing as an empty event.
    ///
    /// `id` is taken rather than kept: the SSE spec's last-event-id is sticky
    /// across events, but the shell reads `id` as *this* chunk's resume point
    /// (contracts §6.2), and a sticky one would attribute a stale cursor to a
    /// later event.
    fn dispatch(&mut self) -> Option<SseEvent> {
        let data = std::mem::take(&mut self.data);
        let id = self.id.take();
        let event = self.event.take();
        if data.is_empty() {
            return None;
        }
        Some(SseEvent {
            id,
            event,
            data: data.join("\n"),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::sessions::sse::SseParser;

    /// One event arriving in three writes: the parser must hold the partial line
    /// and emit nothing until the blank line closes the event.
    #[test]
    fn an_event_split_across_three_pushes_reassembles() {
        let mut parser = SseParser::default();
        assert!(parser.push(b"id: 42\nev").is_empty());
        assert!(parser.push(b"ent: pty\ndata: {\"seq\":").is_empty());
        let events = parser.push(b"7}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id.as_deref(), Some("42"));
        assert_eq!(events[0].event.as_deref(), Some("pty"));
        assert_eq!(events[0].data, "{\"seq\":7}");
    }

    #[test]
    fn two_events_in_one_chunk_both_emit() {
        let mut parser = SseParser::default();
        let events = parser.push(b"id: 1\ndata: one\n\nid: 2\ndata: two\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "one");
        assert_eq!(events[0].id.as_deref(), Some("1"));
        assert_eq!(events[1].data, "two");
        assert_eq!(events[1].id.as_deref(), Some("2"));
    }

    /// Multiple `data:` lines join with `\n` (SSE spec), and exactly one leading
    /// space after the colon is the separator — a second space is payload.
    #[test]
    fn multiple_data_lines_join_with_newlines() {
        let mut parser = SseParser::default();
        let events = parser.push(b"event: agent.state\ndata: first\ndata:  second\ndata:\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("agent.state"));
        assert_eq!(events[0].data, "first\n second\n");
        assert_eq!(events[0].id, None);
    }

    /// `:keep-alive` is what the daemon sends every 15 s to hold the connection
    /// open. Emitting an event for it would mean forwarding a phantom.
    #[test]
    fn comment_lines_dispatch_nothing() {
        let mut parser = SseParser::default();
        assert!(parser.push(b":keep-alive\n\n").is_empty());
        assert!(parser.push(b": another\n\n\n").is_empty());
        let events = parser.push(b"data: real\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "real");
    }

    #[test]
    fn crlf_line_endings_are_accepted() {
        let mut parser = SseParser::default();
        let events = parser.push(b"id: 9\r\nevent: pty\r\ndata: hi\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id.as_deref(), Some("9"));
        assert_eq!(events[0].event.as_deref(), Some("pty"));
        assert_eq!(events[0].data, "hi");
    }

    /// Chunk boundaries fall wherever the socket puts them, including the middle
    /// of a multi-byte character — which is why the buffer holds bytes, not a
    /// `String` built per chunk.
    #[test]
    fn a_multibyte_character_split_across_pushes_survives() {
        let mut parser = SseParser::default();
        let text = "data: caf\u{e9}\n\n".as_bytes();
        let (head, tail) = text.split_at(10);
        assert!(parser.push(head).is_empty());
        let events = parser.push(tail);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "caf\u{e9}");
    }

    /// A field the shell does not consume (`retry`) and a bare field name with no
    /// colon are both defined by the SSE spec and must not derail the event.
    #[test]
    fn unknown_and_valueless_fields_are_ignored() {
        let mut parser = SseParser::default();
        let events = parser.push(b"retry: 3000\nid\nevent: pty\ndata: x\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id.as_deref(), Some(""));
        assert_eq!(events[0].data, "x");
    }
}
