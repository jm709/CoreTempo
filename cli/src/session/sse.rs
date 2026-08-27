//! A line reader for `text/event-stream` bodies: `id:`, `event:` and `data:`
//! fields, one event per blank line, `:` comments (the keep-alives axum sends
//! every 15 s) skipped, multi-line `data:` joined with `\n`.

use std::io::BufRead;

#[derive(Debug, Default)]
pub(crate) struct SseEvent {
    pub id: Option<String>,
    pub event: Option<String>,
    pub data: String,
}

pub(crate) struct SseReader<R: BufRead> {
    inner: R,
}

impl<R: BufRead> SseReader<R> {
    pub(crate) fn new(inner: R) -> SseReader<R> {
        SseReader { inner }
    }

    /// The next complete event; `None` at EOF (a partial event there is dropped).
    ///
    /// # Errors
    /// Whatever the underlying reader fails with.
    pub(crate) fn next_event(&mut self) -> std::io::Result<Option<SseEvent>> {
        let mut event = SseEvent::default();
        let mut data_lines: Vec<String> = Vec::new();
        let mut seen_field = false;
        let mut line = String::new();
        loop {
            line.clear();
            if self.inner.read_line(&mut line)? == 0 {
                return Ok(None);
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                if seen_field {
                    event.data = data_lines.join("\n");
                    return Ok(Some(event));
                }
                continue;
            }
            if trimmed.starts_with(':') {
                continue;
            }
            let (field, value) = trimmed.split_once(':').unwrap_or((trimmed, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            seen_field = true;
            match field {
                "id" => event.id = Some(value.to_string()),
                "event" => event.event = Some(value.to_string()),
                "data" => data_lines.push(value.to_string()),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SseReader;

    #[test]
    fn events_are_split_on_blank_lines_and_carry_id_event_data() {
        let text = concat!(
            "id: 5\nevent: pty\ndata: {\"seq\":0,\"b64\":\"Zg==\"}\n\n",
            ": keep-alive\n\n",
            "data: a\ndata: b\n\n"
        );
        let mut reader = SseReader::new(std::io::Cursor::new(text));
        let first = reader.next_event().unwrap().unwrap();
        assert_eq!(first.id.as_deref(), Some("5"));
        assert_eq!(first.event.as_deref(), Some("pty"));
        assert_eq!(first.data, "{\"seq\":0,\"b64\":\"Zg==\"}");
        let second = reader.next_event().unwrap().unwrap();
        assert_eq!(
            (second.id, second.event, second.data.as_str()),
            (None, None, "a\nb")
        );
        assert!(reader.next_event().unwrap().is_none(), "EOF");
    }
}
