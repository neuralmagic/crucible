//! Parse Claude Code's `--output-format stream-json` NDJSON into [`AgentEvent`]s.
//!
//! The parser is stateful: token totals accrue across messages, a `tool_use` block's JSON input
//! arrives in `input_json_delta` chunks and is only complete at `content_block_stop`, and
//! text/thinking deltas buffer to line boundaries so one event is one line. Unknown top-level
//! and event types are ignored, so schema drift in claude's output cannot break a viewer.

use crate::otel::RateHandle;
use crucible_contract::event::{AgentEvent, Tokens};
use serde_json::Value;

/// Emit a [`Tokens`] sample once the running total has grown by at least this much
/// (or on the first sample).
const TOKEN_EMIT_STEP: u64 = 5_000;

/// Which streamed text block is currently open (its deltas buffer to line boundaries).
#[derive(Clone, Copy, PartialEq, Eq)]
enum TextKind {
    Text,
    Thinking,
}

impl TextKind {
    fn event(self, delta: String) -> AgentEvent {
        match self {
            TextKind::Text => AgentEvent::Text { delta },
            TextKind::Thinking => AgentEvent::Thinking { delta },
        }
    }
}

/// Stateful decoder: feed it one stdout line at a time via [`StreamJsonParser::push`].
#[derive(Default)]
pub struct StreamJsonParser {
    // Cumulative token counts: input/cache from `message_start`, output from `message_delta`.
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    last_emitted_total: u64,

    // The open text/thinking block and its partial (sub-line) tail.
    block: Option<TextKind>,
    buf: String,

    // The in-progress `tool_use` block: name + accumulated `input_json_delta`.
    tool_name: Option<String>,
    tool_json: String,

    // Live token rate from the OTLP collector; `None` when no collector is running.
    rate: Option<RateHandle>,
}

impl StreamJsonParser {
    /// A parser that stamps each `tokens` sample with the collector's live 60 s-window rate.
    pub fn with_rate(rate: RateHandle) -> Self {
        StreamJsonParser {
            rate: Some(rate),
            ..Default::default()
        }
    }

    /// Decode one line of claude `stream-json`, returning every [`AgentEvent`] it completed.
    pub fn push(&mut self, line: &str) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        let line = line.trim();
        if line.is_empty() {
            return out;
        }
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            return out; // non-JSON stdout is dropped without raising an error
        };

        match msg.get("type").and_then(Value::as_str) {
            Some("system") => self.system(&msg, &mut out),
            Some("result") => {
                self.end_block(&mut out);
                let is_error = bool_field(&msg, "is_error");
                // Only capture the CLI's text on the error path (`result`, or `error` when the
                // CLI splits them out); a clean turn's final message would fatten every line.
                let error = is_error
                    .then(|| {
                        let r = str_field(&msg, "result");
                        if r.is_empty() {
                            str_field(&msg, "error")
                        } else {
                            r
                        }
                    })
                    .filter(|s| !s.is_empty());
                out.push(AgentEvent::Result {
                    subtype: str_field(&msg, "subtype"),
                    is_error,
                    turns: u64_field(&msg, "num_turns") as u32,
                    cost_usd: f64_field(&msg, "total_cost_usd"),
                    error,
                });
            }
            Some("stream_event") => self.stream_event(&msg, &mut out),
            // `assistant`/`user` message echoes, `rate_limit_event`, unknown kinds: no-op.
            _ => {}
        }
        out
    }

    /// `system/init` -> [`AgentEvent::Init`]; `system/api_retry` -> [`AgentEvent::Retry`].
    fn system(&mut self, msg: &Value, out: &mut Vec<AgentEvent>) {
        match msg.get("subtype").and_then(Value::as_str) {
            Some("init") => out.push(AgentEvent::Init {
                model: str_field(msg, "model"),
                tools: array_len(msg, "tools"),
                agents: array_len(msg, "agents"),
            }),
            Some("api_retry") => out.push(AgentEvent::Retry {
                attempt: u64_field(msg, "attempt") as u32,
                max: u64_field(msg, "max_retries") as u32,
                error: str_field(msg, "error"),
            }),
            _ => {}
        }
    }

    /// Dispatch a wrapped Anthropic streaming event (`msg.event.type`).
    fn stream_event(&mut self, msg: &Value, out: &mut Vec<AgentEvent>) {
        let Some(event) = msg.get("event") else {
            return;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                let block = event.get("content_block").unwrap_or(&Value::Null);
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => self.block = Some(TextKind::Text),
                    Some("thinking") => self.block = Some(TextKind::Thinking),
                    Some("tool_use" | "server_tool_use") => {
                        self.tool_name = Some(str_field(block, "name"));
                        self.tool_json.clear();
                    }
                    _ => {}
                }
            }
            Some("content_block_delta") => {
                let delta = event.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        self.buffer(TextKind::Text, &str_field(delta, "text"), out)
                    }
                    Some("thinking_delta") => {
                        self.buffer(TextKind::Thinking, &str_field(delta, "thinking"), out)
                    }
                    Some("input_json_delta") => {
                        self.tool_json.push_str(&str_field(delta, "partial_json"))
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => self.end_block(out),
            Some("message_start") => {
                let usage = event
                    .get("message")
                    .and_then(|m| m.get("usage"))
                    .unwrap_or(&Value::Null);
                self.input = u64_field(usage, "input_tokens");
                self.cache_read = u64_field(usage, "cache_read_input_tokens");
                self.cache_write = u64_field(usage, "cache_creation_input_tokens");
            }
            Some("message_delta") => {
                let usage = event.get("usage").unwrap_or(&Value::Null);
                let out_tokens = u64_field(usage, "output_tokens");
                if out_tokens > 0 {
                    self.output = out_tokens;
                    self.maybe_emit_tokens(out);
                }
            }
            Some("error") => {
                let err = event.get("error").unwrap_or(&Value::Null);
                out.push(AgentEvent::Error {
                    error_type: str_field(err, "type"),
                    message: str_field(err, "message"),
                });
            }
            _ => {}
        }
    }

    /// Flush any still-open block at end of stream: a buffered text/thinking tail, or a dangling
    /// `tool_use` whose `content_block_stop` never arrived. Usually empty; exists so a consumer
    /// draining a truncated stream loses nothing.
    pub fn flush(&mut self) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        self.end_block(&mut out);
        out
    }

    /// Append a text/thinking chunk to the open block, emitting one event per completed line;
    /// the sub-line tail stays buffered until the next chunk or the block's stop.
    fn buffer(&mut self, kind: TextKind, chunk: &str, out: &mut Vec<AgentEvent>) {
        self.block = Some(kind);
        self.buf.push_str(chunk);
        while let Some(i) = self.buf.find('\n') {
            let mut line: String = self.buf.drain(..=i).collect();
            line.pop(); // drop the trailing '\n'
            out.push(kind.event(line));
        }
    }

    /// Close the open block: emit a completed tool call, or flush a text/thinking tail.
    fn end_block(&mut self, out: &mut Vec<AgentEvent>) {
        if let Some(name) = self.tool_name.take() {
            let parsed = serde_json::from_str::<Value>(&self.tool_json).ok();
            let summary = parsed
                .as_ref()
                .map(|p| format_tool(&name, p))
                .unwrap_or_default();
            let subagent = name == "Agent" || name == "Task";
            out.push(AgentEvent::Tool {
                name,
                summary,
                subagent,
            });
            self.tool_json.clear();
            return;
        }
        if let Some(kind) = self.block.take()
            && !self.buf.is_empty()
        {
            out.push(kind.event(std::mem::take(&mut self.buf)));
        }
    }

    /// Emit a [`Tokens`] sample when the running total first appears or grows by a step. `rate`
    /// carries the collector's live rate when one is attached; `None` when telemetry is off.
    fn maybe_emit_tokens(&mut self, out: &mut Vec<AgentEvent>) {
        let total = self.input + self.output + self.cache_read + self.cache_write;
        if self.last_emitted_total != 0
            && total.saturating_sub(self.last_emitted_total) < TOKEN_EMIT_STEP
        {
            return;
        }
        self.last_emitted_total = total;
        out.push(AgentEvent::Tokens(Tokens {
            input: self.input,
            output: self.output,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
            total,
            rate: self.rate.as_ref().and_then(RateHandle::get),
            cost_usd: None,
        }));
    }
}

/// A compact one-line summary for known tools; unknown tools fall back to a truncated
/// `key=value` join. Cosmetic only.
fn format_tool(name: &str, p: &Value) -> String {
    let get = |keys: &[&str]| -> String {
        for k in keys {
            let v = str_field(p, k);
            if !v.is_empty() {
                return v;
            }
        }
        String::new()
    };
    match name {
        "Bash" => {
            let cmd = get(&["command"]);
            let desc = get(&["description"]);
            if desc.is_empty() {
                format!("$ {cmd}")
            } else {
                format!("$ {cmd}  # {desc}")
            }
        }
        "Read" => {
            let mut s = get(&["file_path", "filePath"]);
            if let Some(o) = p.get("offset") {
                s.push_str(&format!(" L{o}"));
            }
            if let Some(l) = p.get("limit") {
                s.push_str(&format!(" +{l}"));
            }
            s
        }
        "Write" => get(&["file_path", "filePath"]),
        "Edit" => {
            let path = get(&["file_path", "filePath"]);
            let old = get(&["old_string", "oldString"]);
            let first = old.lines().next().unwrap_or("");
            let preview: String = first.chars().take(60).collect();
            let ell = if first.chars().count() > 60 {
                "…"
            } else {
                ""
            };
            format!("{path}: {preview}{ell}")
        }
        "Glob" => {
            let path = get(&["path"]);
            let path = if path.is_empty() { ".".into() } else { path };
            format!("{} in {path}", get(&["pattern"]))
        }
        "Grep" => {
            let path = get(&["path"]);
            let path = if path.is_empty() { ".".into() } else { path };
            format!("/{}/ in {path}", get(&["pattern"]))
        }
        "Agent" | "Task" => {
            let desc = get(&["description"]);
            let at = get(&["subagent_type", "subagentType"]);
            if at.is_empty() {
                desc
            } else {
                format!("[{at}] {desc}")
            }
        }
        "Skill" => format!("/{} {}", get(&["skill"]), get(&["args"]))
            .trim()
            .to_string(),
        _ => {
            let mut parts = Vec::new();
            if let Some(obj) = p.as_object() {
                for (k, v) in obj {
                    let mut val = match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    if val.chars().count() > 60 {
                        val = val.chars().take(60).collect::<String>() + "…";
                    }
                    parts.push(format!("{k}={val}"));
                }
            }
            let joined = parts.join(", ");
            if joined.chars().count() > 200 {
                joined.chars().take(200).collect::<String>() + "…"
            } else {
                joined
            }
        }
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn u64_field(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn bool_field(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn f64_field(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn array_len(v: &Value, key: &str) -> u32 {
    v.get(key)
        .and_then(Value::as_array)
        .map_or(0, |a| a.len() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a sequence of lines and collect every event produced, in order.
    fn run(lines: &[&str]) -> Vec<AgentEvent> {
        let mut p = StreamJsonParser::default();
        lines.iter().flat_map(|l| p.push(l)).collect()
    }

    #[test]
    fn init_reports_model_and_counts() {
        let ev = run(&[
            r#"{"type":"system","subtype":"init","model":"claude-opus-4-8","tools":["a","b","c"],"agents":["x","y"]}"#,
        ]);
        assert!(
            matches!(&ev[..], [AgentEvent::Init { model, tools, agents }]
            if model == "claude-opus-4-8" && *tools == 3 && *agents == 2)
        );
    }

    #[test]
    fn unknown_top_level_types_are_ignored() {
        // Real capture shapes the parser deliberately ignores.
        let ev = run(&[
            r#"{"type":"system","subtype":"status","model":"","tools":[]}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{},"uuid":"u","session_id":"s"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text"}]}}"#,
        ]);
        assert!(ev.is_empty(), "no events for unmodeled types, got {ev:?}");
    }

    #[test]
    fn text_block_buffers_to_lines_and_flushes_tail() {
        let ev = run(&[
            r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"text","text":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"line one\nline "}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"two"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop"}}"#,
        ]);
        let deltas: Vec<&str> = ev
            .iter()
            .map(|e| match e {
                AgentEvent::Text { delta } => delta.as_str(),
                other => panic!("expected Text, got {other:?}"),
            })
            .collect();
        assert_eq!(deltas, vec!["line one", "line two"]);
    }

    #[test]
    fn thinking_block_maps_to_thinking() {
        let ev = run(&[
            r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"thinking"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hmm"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop"}}"#,
        ]);
        assert!(matches!(&ev[..], [AgentEvent::Thinking { delta }] if delta == "hmm"));
    }

    #[test]
    fn tool_use_accumulates_json_and_emits_on_stop() {
        let ev = run(&[
            r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","name":"Edit"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"p.go\",\"old_string\""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":":\"snap := x\\nmore\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop"}}"#,
        ]);
        match &ev[..] {
            [
                AgentEvent::Tool {
                    name,
                    summary,
                    subagent,
                },
            ] => {
                assert_eq!(name, "Edit");
                assert_eq!(summary, "p.go: snap := x");
                assert!(!subagent);
            }
            other => panic!("expected one Tool, got {other:?}"),
        }
    }

    #[test]
    fn subagent_tool_is_flagged() {
        let ev = run(&[
            r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","name":"Task"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"subagent_type\":\"Explore\",\"description\":\"map it\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop"}}"#,
        ]);
        assert!(
            matches!(&ev[..], [AgentEvent::Tool { subagent: true, summary, .. }]
            if summary == "[Explore] map it")
        );
    }

    #[test]
    fn tokens_accrue_from_message_start_and_delta() {
        let ev = run(&[
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"usage":{"input_tokens":3968,"cache_creation_input_tokens":6439,"cache_read_input_tokens":14376,"output_tokens":1}}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"output_tokens":4}}}"#,
        ]);
        match &ev[..] {
            [AgentEvent::Tokens(t)] => {
                assert_eq!(t.input, 3968);
                assert_eq!(t.output, 4);
                assert_eq!(t.cache_read, 14376);
                assert_eq!(t.cache_write, 6439);
                assert_eq!(t.total, 3968 + 4 + 14376 + 6439);
                assert!(t.rate.is_none() && t.cost_usd.is_none());
            }
            other => panic!("expected one Tokens, got {other:?}"),
        }
    }

    #[test]
    fn tokens_carry_the_live_rate_when_a_handle_is_attached() {
        let handle = RateHandle::default();
        let mut p = StreamJsonParser::with_rate(handle.clone());
        let drive = |p: &mut StreamJsonParser| -> Vec<AgentEvent> {
            let mut evs = p.push(
                r#"{"type":"stream_event","event":{"type":"message_start","message":{"usage":{"input_tokens":6000,"output_tokens":1}}}}"#,
            );
            evs.extend(p.push(
                r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"output_tokens":4}}}"#,
            ));
            evs
        };

        // No rate established yet -> None.
        match drive(&mut p).into_iter().find_map(|e| match e {
            AgentEvent::Tokens(t) => Some(t),
            _ => None,
        }) {
            Some(t) => assert!(t.rate.is_none(), "no rate stored yet"),
            None => panic!("expected a tokens sample"),
        }

        // Now a rate is live -> it rides the sample.
        handle.store(318.4);
        let mut p2 = StreamJsonParser::with_rate(handle);
        match drive(&mut p2).into_iter().find_map(|e| match e {
            AgentEvent::Tokens(t) => Some(t),
            _ => None,
        }) {
            Some(t) => assert_eq!(t.rate, Some(318.4)),
            None => panic!("expected a tokens sample"),
        }
    }

    #[test]
    fn result_carries_cost_and_turns() {
        let ev = run(&[
            r#"{"type":"result","subtype":"success","stop_reason":"end_turn","num_turns":1,"total_cost_usd":0.09208}"#,
        ]);
        match &ev[..] {
            [
                AgentEvent::Result {
                    subtype,
                    is_error,
                    turns,
                    cost_usd,
                    error,
                },
            ] => {
                assert_eq!(subtype, "success");
                assert!(!*is_error, "a clean result is not an error");
                assert!(error.is_none(), "no error text on a clean turn");
                assert_eq!(*turns, 1);
                assert!((cost_usd - 0.09208).abs() < 1e-9);
            }
            other => panic!("expected one Result, got {other:?}"),
        }
    }

    #[test]
    fn result_surfaces_is_error_and_text() {
        // Gotcha: the CLI can send subtype "success" with is_error=true and the reason in
        // `result` (e.g. not logged in). Both must survive decode.
        let ev = run(&[
            r#"{"type":"result","subtype":"success","is_error":true,"num_turns":1,"total_cost_usd":0.0,"result":"Not logged in"}"#,
        ]);
        match &ev[..] {
            [
                AgentEvent::Result {
                    is_error, error, ..
                },
            ] => {
                assert!(*is_error, "the CLI flagged the turn as an error");
                assert_eq!(error.as_deref(), Some("Not logged in"));
            }
            other => panic!("expected one Result, got {other:?}"),
        }
    }

    #[test]
    fn result_flushes_an_open_text_tail() {
        let ev = run(&[
            r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"text"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"trailing"}}}"#,
            r#"{"type":"result","subtype":"success","num_turns":1,"total_cost_usd":0.5}"#,
        ]);
        assert!(matches!(&ev[..],
            [AgentEvent::Text { delta }, AgentEvent::Result { .. }] if delta == "trailing"));
    }

    #[test]
    fn api_retry_maps_to_retry() {
        let ev = run(&[
            r#"{"type":"system","subtype":"api_retry","attempt":2,"max_retries":10,"error":"overloaded_error"}"#,
        ]);
        assert!(
            matches!(&ev[..], [AgentEvent::Retry { attempt, max, error }]
            if *attempt == 2 && *max == 10 && error == "overloaded_error")
        );
    }

    /// Golden test against a real `claude --output-format stream-json` capture. The parser is
    /// model-agnostic, so this asserts on tool count, text, and cost rather than the model string.
    #[test]
    fn golden_real_hello_capture() {
        let fixture = include_str!("testdata/claude_stream_hello.jsonl");
        let ev = run(&fixture.lines().collect::<Vec<_>>());
        match &ev[..] {
            [
                AgentEvent::Init { tools, .. },
                AgentEvent::Text { delta },
                AgentEvent::Tokens(t),
                AgentEvent::Result {
                    subtype,
                    turns,
                    cost_usd,
                    ..
                },
            ] => {
                assert_eq!(*tools, 29, "real init advertised 29 tools");
                assert_eq!(
                    delta, "hello",
                    "the assistant text, line-flushed at block stop"
                );
                assert_eq!(t.input, 3968);
                assert_eq!(t.cache_read, 14376);
                assert_eq!(t.cache_write, 6439);
                assert_eq!(t.output, 4);
                assert_eq!(t.total, 3968 + 4 + 14376 + 6439);
                assert_eq!(subtype, "success");
                assert_eq!(*turns, 1);
                assert!((cost_usd - 0.09208).abs() < 1e-9, "real total_cost_usd");
            }
            other => panic!("unexpected event sequence from real capture: {other:?}"),
        }
    }

    #[test]
    fn stream_error_maps_to_error() {
        let ev = run(&[
            r#"{"type":"stream_event","event":{"type":"error","error":{"type":"overloaded_error","message":"busy"}}}"#,
        ]);
        assert!(
            matches!(&ev[..], [AgentEvent::Error { error_type, message }]
            if error_type == "overloaded_error" && message == "busy")
        );
    }

    #[test]
    fn flush_drains_an_open_text_tail() {
        let mut p = StreamJsonParser::default();
        let mut ev: Vec<AgentEvent> = Vec::new();
        ev.extend(p.push(
            r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"text"}}}"#,
        ));
        ev.extend(p.push(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"dangling"}}}"#,
        ));
        assert!(ev.is_empty(), "no completed line yet, got {ev:?}");
        let tail = p.flush();
        assert!(matches!(&tail[..], [AgentEvent::Text { delta }] if delta == "dangling"));
        // Idempotent: nothing left to flush.
        assert!(p.flush().is_empty());
    }
}
