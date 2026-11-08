//! A server-sent events decoder, fed whatever chunks the wire hands over.

/// One dispatched event. `event` is `message` when the frame named none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
    pub id: Option<String>,
}

#[derive(Default)]
pub struct SseParser {
    pending: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
    id: Option<String>,
}

impl SseParser {
    /// Feed one chunk; returns the events it completed. A line split across chunks waits for the
    /// rest of itself.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.pending.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            if let Some(event) = self.line(&line) {
                events.push(event);
            }
        }
        events
    }

    fn line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            "id" => self.id = Some(value.to_string()),
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let event = self.event.take();
        if self.data.is_empty() {
            return None;
        }
        Some(SseEvent {
            event: event.unwrap_or_else(|| "message".to_string()),
            data: std::mem::take(&mut self.data).join("\n"),
            id: self.id.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::sse::{SseEvent, SseParser};

    fn event(event: &str, data: &str, id: Option<&str>) -> SseEvent {
        SseEvent {
            event: event.into(),
            data: data.into(),
            id: id.map(Into::into),
        }
    }

    #[test]
    fn frames_dispatch_on_the_blank_line_and_keep_their_id() {
        let mut parser = SseParser::default();
        let events = parser.push(
            b"id: 3.0\nevent: session\ndata: {\"kind\":\"note\"}\n\n: keep-alive\n\nevent: end\ndata: bridge-closed\n\n",
        );
        assert_eq!(
            events,
            [
                event("session", r#"{"kind":"note"}"#, Some("3.0")),
                event("end", "bridge-closed", Some("3.0")),
            ]
        );
    }

    #[test]
    fn a_frame_split_across_chunks_waits_for_its_end() {
        let mut parser = SseParser::default();
        assert!(parser.push(b"event: sess").is_empty());
        assert!(parser.push(b"ion\r\ndata: a").is_empty());
        assert!(parser.push(b"bc\r\n").is_empty());
        assert_eq!(parser.push(b"\r\n"), [event("session", "abc", None)]);
    }

    #[test]
    fn multiple_data_lines_join_and_an_unnamed_frame_is_a_message() {
        let mut parser = SseParser::default();
        assert_eq!(
            parser.push(b"data: one\ndata:two\n\n"),
            [event("message", "one\ntwo", None)]
        );
    }

    #[test]
    fn a_frame_without_data_dispatches_nothing() {
        let mut parser = SseParser::default();
        assert!(parser.push(b"event: ping\n\n").is_empty());
        assert_eq!(parser.push(b"data: x\n\n"), [event("message", "x", None)]);
    }
}
