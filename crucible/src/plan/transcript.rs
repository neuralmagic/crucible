//! A playbook agent turn as its session transcript records it: text and thinking as whole
//! blocks, everything else as it arrives.

use std::time::{Duration, Instant};

use crate::agent::event::AgentEvent;

/// The most a text or thinking block keeps; the rest is counted in a marker.
const BLOCK_CHARS: usize = 64 * 1024;
/// Token samples are cumulative, so one every this often, plus the turn's last, tells the story.
const TOKENS_EVERY: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Transcript {
    text: Block,
    thinking: Block,
    tokens_at: Option<Instant>,
    unrecorded_tokens: Option<AgentEvent>,
    redact: bool,
}

#[derive(Default)]
struct Block {
    buf: String,
    chars: usize,
    dropped: usize,
}

impl Block {
    fn push(&mut self, delta: &str) {
        for c in delta.chars() {
            if self.chars < BLOCK_CHARS {
                self.buf.push(c);
                self.chars += 1;
            } else {
                self.dropped += 1;
            }
        }
    }

    fn take(&mut self) -> Option<String> {
        let mut block = std::mem::take(&mut self.buf);
        let dropped = std::mem::take(&mut self.dropped);
        self.chars = 0;
        if dropped > 0 {
            block.push_str(&format!(
                "\n\u{2026} [{dropped} more characters not recorded]"
            ));
        }
        (!block.trim().is_empty()).then_some(block)
    }
}

impl Transcript {
    /// `redact` scrubs credential-shaped fragments from tool summaries, as the console does.
    pub fn new(redact: bool) -> Self {
        Transcript {
            redact,
            ..Transcript::default()
        }
    }

    /// The events `ev` completes, oldest first.
    pub fn on_event(&mut self, ev: &AgentEvent, now: Instant) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        match ev {
            AgentEvent::Thinking { delta } => {
                self.flush_text(&mut out);
                self.thinking.push(delta);
            }
            AgentEvent::Text { delta } => {
                self.flush_thinking(&mut out);
                self.text.push(delta);
            }
            AgentEvent::Tokens(_) => {
                if self
                    .tokens_at
                    .is_none_or(|at| now.duration_since(at) >= TOKENS_EVERY)
                {
                    self.tokens_at = Some(now);
                    self.unrecorded_tokens = None;
                    out.push(ev.clone());
                } else {
                    self.unrecorded_tokens = Some(ev.clone());
                }
            }
            AgentEvent::Raw { .. } => {}
            AgentEvent::Result { .. } => {
                self.flush(&mut out);
                out.extend(self.unrecorded_tokens.take());
                out.push(ev.clone());
            }
            AgentEvent::Tool {
                name,
                summary,
                subagent,
                input,
                result,
                failed,
            } if self.redact => {
                self.flush(&mut out);
                out.push(AgentEvent::Tool {
                    name: name.clone(),
                    summary: crate::turn_trace::redact(summary),
                    subagent: *subagent,
                    input: input.clone(),
                    result: result.clone(),
                    failed: *failed,
                });
            }
            other => {
                self.flush(&mut out);
                out.push(other.clone());
            }
        }
        out
    }

    /// What the turn left unflushed when it ended.
    pub fn finish(&mut self) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        self.flush(&mut out);
        out.extend(self.unrecorded_tokens.take());
        out
    }

    fn flush(&mut self, out: &mut Vec<AgentEvent>) {
        self.flush_thinking(out);
        self.flush_text(out);
    }

    fn flush_text(&mut self, out: &mut Vec<AgentEvent>) {
        if let Some(delta) = self.text.take() {
            out.push(AgentEvent::Text { delta });
        }
    }

    fn flush_thinking(&mut self, out: &mut Vec<AgentEvent>) {
        if let Some(delta) = self.thinking.take() {
            out.push(AgentEvent::Thinking { delta });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::agent::event::{AgentEvent, RawStream, Tokens};
    use crate::plan::transcript::{BLOCK_CHARS, Transcript};

    fn text(s: &str) -> AgentEvent {
        AgentEvent::Text { delta: s.into() }
    }

    fn thinking(s: &str) -> AgentEvent {
        AgentEvent::Thinking { delta: s.into() }
    }

    fn tool(summary: &str) -> AgentEvent {
        AgentEvent::Tool {
            name: "Bash".into(),
            summary: summary.into(),
            subagent: false,
            input: None,
            result: None,
            failed: false,
        }
    }

    fn tokens(total: u64) -> AgentEvent {
        AgentEvent::Tokens(Tokens {
            total,
            ..Tokens::default()
        })
    }

    fn deltas(events: &[AgentEvent]) -> Vec<String> {
        events
            .iter()
            .map(|ev| match ev {
                AgentEvent::Text { delta } => format!("text:{delta}"),
                AgentEvent::Thinking { delta } => format!("think:{delta}"),
                AgentEvent::Tool { summary, .. } => format!("tool:{summary}"),
                AgentEvent::Tokens(t) => format!("tokens:{}", t.total),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn deltas_become_whole_blocks_that_close_when_something_else_happens() {
        let mut t = Transcript::new(false);
        let now = Instant::now();
        let mut out = Vec::new();
        for ev in [
            thinking("Let me "),
            thinking("look."),
            text("Line one.\n"),
            text("Line two, kept whole."),
            tool("$ ls"),
            text("Done."),
        ] {
            out.extend(t.on_event(&ev, now));
        }
        out.extend(t.finish());
        assert_eq!(
            deltas(&out),
            [
                "think:Let me look.",
                "text:Line one.\nLine two, kept whole.",
                "tool:$ ls",
                "text:Done.",
            ]
        );
    }

    #[test]
    fn a_runaway_block_keeps_its_head_and_counts_the_rest() {
        let mut t = Transcript::new(false);
        let now = Instant::now();
        let big = "x".repeat(BLOCK_CHARS + 10);
        assert!(t.on_event(&text(&big), now).is_empty());
        let out = t.finish();
        let AgentEvent::Text { delta } = &out[0] else {
            panic!("expected a text block, got {out:?}");
        };
        assert!(delta.starts_with(&"x".repeat(BLOCK_CHARS)));
        assert!(
            delta.ends_with("[10 more characters not recorded]"),
            "{delta}"
        );
    }

    #[test]
    fn token_samples_thin_out_but_the_last_one_survives() {
        let mut t = Transcript::new(false);
        let start = Instant::now();
        let mut out = Vec::new();
        out.extend(t.on_event(&tokens(100), start));
        out.extend(t.on_event(&tokens(200), start + Duration::from_secs(5)));
        out.extend(t.on_event(&tokens(300), start + Duration::from_secs(31)));
        out.extend(t.on_event(&tokens(400), start + Duration::from_secs(35)));
        out.extend(t.finish());
        assert_eq!(deltas(&out), ["tokens:100", "tokens:300", "tokens:400"]);
    }

    #[test]
    fn raw_lines_stay_on_the_console_and_tool_summaries_are_scrubbed_when_asked() {
        let mut t = Transcript::new(true);
        let now = Instant::now();
        let raw = AgentEvent::Raw {
            text: "noise".into(),
            stream: RawStream::Stderr,
        };
        assert!(t.on_event(&raw, now).is_empty());
        let out = t.on_event(
            &tool("curl -H 'Authorization: Bearer abc123' https://x"),
            now,
        );
        let rendered = deltas(&out).join("");
        assert!(!rendered.contains("abc123"), "{rendered}");
        assert!(rendered.contains("***"), "{rendered}");
    }
}
