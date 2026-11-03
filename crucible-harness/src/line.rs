//! The framing every harness stream shares: one JSON object per stdout line, anything else kept
//! verbatim. What a decoded object means is the harness's own [`LineFormat::line`].

use crucible_contract::event::{AgentEvent, RawStream};
use serde_json::Value;

/// One harness's stream format.
pub trait LineFormat {
    /// What one decoded JSON line means, appended to `out`. `text` is the line as it arrived,
    /// for an object the format does not know.
    fn line(&mut self, msg: &Value, text: &str, out: &mut Vec<AgentEvent>);

    /// Decode one stdout line, returning every [`AgentEvent`] it completed.
    fn push(&mut self, line: &str) -> Vec<AgentEvent>
    where
        Self: Sized,
    {
        let mut out = Vec::new();
        let line = line.trim();
        if line.is_empty() {
            return out;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(msg) => self.line(&msg, line, &mut out),
            Err(_) => out.push(raw(line)),
        }
        out
    }
}

/// An undecoded stdout line, kept verbatim so nothing the agent printed is lost.
pub(crate) fn raw(line: &str) -> AgentEvent {
    AgentEvent::Raw {
        text: line.to_string(),
        stream: RawStream::Stdout,
    }
}

/// Split a block of text into one event per line, dropping a trailing newline's empty tail.
pub(crate) fn lines_of(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    text.trim_end_matches('\n')
        .split('\n')
        .map(str::to_string)
        .collect()
}

/// A turn's token counts as a key-priced stream (opencode, pi) reports them, and the cost the
/// harness put on them when it priced the model.
pub(crate) struct Usage<'a> {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub priced: f64,
    pub model: &'a str,
    pub price: Option<crate::codex_json::PriceFn>,
}

impl Usage<'_> {
    /// The counts as [`Tokens`], costed at the harness's own figure when it has one, else at the
    /// installed estimate, else not at all.
    pub(crate) fn tokens(&self) -> crucible_contract::event::Tokens {
        let mut t = crucible_contract::event::Tokens {
            input: self.input,
            output: self.output,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
            total: self.input + self.output + self.cache_read + self.cache_write,
            rate: None,
            cost_usd: None,
        };
        t.cost_usd = if self.priced > 0.0 {
            Some(self.priced)
        } else {
            self.price.map(|p| p(self.model, &t))
        };
        t
    }
}
