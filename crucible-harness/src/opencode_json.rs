//! Parse `opencode run --format json` NDJSON into [`AgentEvent`]s.
//!
//! OpenCode emits one `type`-tagged JSON object per line, each wrapping a session `part`:
//! `step_start`/`step_finish` bracket one model request (the finish carries that request's
//! token usage and cost), `text` and `reasoning` carry a completed block, `tool_use` a finished
//! tool call with its state, and `error` a session error. The `run` command mirrors its
//! server's event stream and exits on the session's idle signal, which in a container can land
//! before the last `text`/`step_finish` are mirrored, so this decoder never closes the turn: the
//! [`AgentEvent::Result`] comes from the post-turn session export the opencode backend reads
//! (`backfill_required`). Lines with an unmodeled `type`, and non-JSON lines, pass through as
//! [`AgentEvent::Raw`] so a viewer never loses output to schema drift.

use crate::codex_json::PriceFn;
use crate::stream_json::{TOOL_IO_LIMIT, bounded_input, str_field, truncate_chars, u64_field};
use crate::tool_summary::{SUMMARY_CAP, summarize};
use crucible_contract::event::{AgentEvent, RawStream, Tokens};
use serde_json::Value;

/// Stateful decoder: feed it one stdout line at a time via [`OpenCodeJsonParser::push`].
pub struct OpenCodeJsonParser {
    /// The model crucible asked for; the stream never names it.
    model: String,
    tool_io: bool,
    price: Option<PriceFn>,
    init_sent: bool,

    // Cumulative usage across the turn's steps (each `step_finish` reports its own request).
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    /// The sum of what opencode itself priced the steps at; zero for a model with no cost table.
    priced: f64,
}

impl OpenCodeJsonParser {
    /// A parser for a turn running `model`, with no pricing installed.
    pub fn new(model: impl Into<String>) -> Self {
        OpenCodeJsonParser {
            model: model.into(),
            tool_io: false,
            price: None,
            init_sent: false,
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            priced: 0.0,
        }
    }

    /// Install the pricing function used when opencode's own cost table prices the turn at zero.
    pub fn with_price(mut self, price: PriceFn) -> Self {
        self.price = Some(price);
        self
    }

    /// Opt into verbose tool IO: tool events carry bounded inputs and result excerpts.
    pub fn with_tool_io(mut self, on: bool) -> Self {
        self.tool_io = on;
        self
    }

    /// Decode one line of `opencode run --format json`, returning every [`AgentEvent`] it
    /// completed.
    pub fn push(&mut self, line: &str) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        let line = line.trim();
        if line.is_empty() {
            return out;
        }
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            out.push(raw(line));
            return out;
        };
        let kind = str_field(&msg, "type");
        if !self.init_sent && msg.get("sessionID").is_some() {
            self.init_sent = true;
            out.push(AgentEvent::Init {
                model: self.model.clone(),
                tools: 0,
                agents: 0,
            });
        }
        let part = msg.get("part").unwrap_or(&Value::Null);
        match kind.as_str() {
            "step_start" => {}
            "text" => {
                for delta in lines_of(&str_field(part, "text")) {
                    out.push(AgentEvent::Text { delta });
                }
            }
            "reasoning" => {
                for delta in lines_of(&str_field(part, "text")) {
                    out.push(AgentEvent::Thinking { delta });
                }
            }
            "tool_use" => out.push(self.tool(part)),
            "step_finish" => {
                let tokens = part.get("tokens").unwrap_or(&Value::Null);
                self.input += u64_field(tokens, "input");
                self.output += u64_field(tokens, "output");
                let cache = tokens.get("cache").unwrap_or(&Value::Null);
                self.cache_read += u64_field(cache, "read");
                self.cache_write += u64_field(cache, "write");
                self.priced += part.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
                out.push(AgentEvent::Tokens(self.tokens()));
            }
            "error" => {
                let error = msg.get("error").unwrap_or(&Value::Null);
                let name = str_field(error, "name");
                let message = error
                    .get("data")
                    .map(|d| str_field(d, "message"))
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| {
                        if name.is_empty() {
                            error.to_string()
                        } else {
                            name.clone()
                        }
                    });
                out.push(AgentEvent::Error {
                    error_type: if name.is_empty() {
                        "opencode".to_string()
                    } else {
                        name
                    },
                    message,
                });
            }
            _ => out.push(raw(line)),
        }
        out
    }

    /// A finished tool part: the tool name, a summary of its input (or the title opencode gave
    /// it), and (verbose only) the bounded input and output.
    fn tool(&self, part: &Value) -> AgentEvent {
        let name = str_field(part, "tool");
        let state = part.get("state").unwrap_or(&Value::Null);
        let input = state.get("input").cloned().unwrap_or(Value::Null);
        let status = str_field(state, "status");
        let mut summary = summarize(&name, &input);
        if summary.is_empty() {
            summary = str_field(state, "title");
        }
        if status == "error" {
            let detail = str_field(state, "error");
            summary = truncate_chars(
                &if detail.is_empty() {
                    format!("{summary} [error]")
                } else {
                    format!("{summary} [error: {detail}]")
                },
                SUMMARY_CAP,
            );
        }
        let (input, result) = if self.tool_io {
            let output = str_field(state, "output");
            (
                Some(bounded_input(&input)),
                (!output.is_empty()).then(|| truncate_chars(&output, TOOL_IO_LIMIT)),
            )
        } else {
            (None, None)
        };
        AgentEvent::Tool {
            name,
            summary,
            subagent: false,
            input,
            result,
        }
    }

    fn tokens(&self) -> Tokens {
        let mut t = Tokens {
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
            self.price.map(|p| p(&self.model, &t))
        };
        t
    }
}

/// Split a block of text into one event per line, dropping a trailing newline's empty tail.
fn lines_of(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    text.trim_end_matches('\n')
        .split('\n')
        .map(str::to_string)
        .collect()
}

fn raw(line: &str) -> AgentEvent {
    AgentEvent::Raw {
        text: line.to_string(),
        stream: RawStream::Stdout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pricing stand-in: $1/MTok in, $10/MTok out, cached reads a tenth of input.
    fn price(_model: &str, t: &Tokens) -> f64 {
        (t.input as f64 + t.cache_read as f64 * 0.1) * 1e-6 + t.output as f64 * 10e-6
    }

    fn run_with(mut p: OpenCodeJsonParser, lines: &[&str]) -> Vec<AgentEvent> {
        lines.iter().flat_map(|l| p.push(l)).collect()
    }

    fn run(lines: &[&str]) -> Vec<AgentEvent> {
        run_with(OpenCodeJsonParser::new("qwen-3-8-27b"), lines)
    }

    #[test]
    fn the_first_session_event_reports_the_configured_model_once() {
        let ev = run(&[
            r#"{"type":"step_start","timestamp":1,"sessionID":"ses_1","part":{"type":"step-start"}}"#,
            r#"{"type":"step_start","timestamp":2,"sessionID":"ses_1","part":{"type":"step-start"}}"#,
        ]);
        assert!(
            matches!(&ev[..], [AgentEvent::Init { model, .. }] if model == "qwen-3-8-27b"),
            "{ev:?}"
        );
    }

    #[test]
    fn text_and_reasoning_parts_become_one_event_per_line() {
        let ev = run(&[
            r#"{"type":"reasoning","sessionID":"s","part":{"type":"reasoning","text":"think\nharder"}}"#,
            r#"{"type":"text","sessionID":"s","part":{"type":"text","text":"line one\nline two\n"}}"#,
        ]);
        match &ev[..] {
            [
                AgentEvent::Init { .. },
                AgentEvent::Thinking { delta: t1 },
                AgentEvent::Thinking { delta: t2 },
                AgentEvent::Text { delta: a },
                AgentEvent::Text { delta: b },
            ] => {
                assert_eq!((t1.as_str(), t2.as_str()), ("think", "harder"));
                assert_eq!((a.as_str(), b.as_str()), ("line one", "line two"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn a_tool_part_summarizes_its_input_and_flags_an_error_state() {
        let ev = run(&[
            r#"{"type":"tool_use","sessionID":"s","part":{"type":"tool","tool":"bash","callID":"c1","state":{"status":"completed","input":{"command":"cargo test","description":"Run tests"},"output":"ok","title":"Run tests"}}}"#,
            r#"{"type":"tool_use","sessionID":"s","part":{"type":"tool","tool":"edit","callID":"c2","state":{"status":"error","input":{"filePath":"a.rs","oldString":"x"},"error":"oldString not found"}}}"#,
            r#"{"type":"tool_use","sessionID":"s","part":{"type":"tool","tool":"mystery","callID":"c3","state":{"status":"completed","input":{},"title":"did a thing"}}}"#,
        ]);
        match &ev[..] {
            [
                AgentEvent::Init { .. },
                AgentEvent::Tool {
                    name,
                    summary,
                    subagent,
                    input,
                    result,
                },
                AgentEvent::Tool {
                    summary: failed, ..
                },
                AgentEvent::Tool {
                    summary: titled, ..
                },
            ] => {
                assert_eq!(name, "bash");
                assert_eq!(summary, "$ cargo test  # Run tests");
                assert!(!subagent);
                assert!(input.is_none() && result.is_none(), "compact by default");
                assert_eq!(failed, "a.rs: x [error: oldString not found]");
                assert_eq!(
                    titled, "did a thing",
                    "the title stands in for an empty summary"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn verbose_tool_io_carries_bounded_input_and_output() {
        let big = "x".repeat(TOOL_IO_LIMIT * 2);
        let line = format!(
            r#"{{"type":"tool_use","sessionID":"s","part":{{"type":"tool","tool":"bash","state":{{"status":"completed","input":{{"command":"echo {big}"}},"output":"{big}"}}}}}}"#
        );
        let ev = run_with(
            OpenCodeJsonParser::new("m").with_tool_io(true),
            &[line.as_str()],
        );
        match &ev[..] {
            [
                AgentEvent::Init { .. },
                AgentEvent::Tool { input, result, .. },
            ] => {
                let stored = input
                    .as_ref()
                    .and_then(Value::as_str)
                    .expect("oversized input degrades to a truncated string");
                assert!(stored.chars().count() <= TOOL_IO_LIMIT + 1);
                let excerpt = result.as_deref().expect("result excerpt");
                assert!(excerpt.chars().count() <= TOOL_IO_LIMIT + 1);
                assert!(excerpt.ends_with('…'));
            }
            other => panic!("expected Init + Tool, got {other:?}"),
        }
    }

    #[test]
    fn step_finish_accrues_usage_and_never_closes_the_turn() {
        let ev = run_with(
            OpenCodeJsonParser::new("m").with_price(price),
            &[
                r#"{"type":"step_finish","sessionID":"s","part":{"type":"step-finish","reason":"tool-calls","tokens":{"input":200,"output":20,"reasoning":0,"cache":{"read":10,"write":5}},"cost":0}}"#,
                r#"{"type":"step_finish","sessionID":"s","part":{"type":"step-finish","reason":"stop","tokens":{"input":300,"output":30,"reasoning":0,"cache":{"read":0,"write":0}},"cost":0.25}}"#,
            ],
        );
        match &ev[..] {
            [
                AgentEvent::Init { .. },
                AgentEvent::Tokens(first),
                AgentEvent::Tokens(second),
            ] => {
                assert_eq!(
                    (
                        first.input,
                        first.output,
                        first.cache_read,
                        first.cache_write
                    ),
                    (200, 20, 10, 5)
                );
                assert_eq!(
                    first.cost_usd,
                    Some(price("m", first)),
                    "estimate while opencode says zero"
                );
                assert_eq!((second.input, second.output, second.total), (500, 50, 565));
                assert_eq!(
                    second.cost_usd,
                    Some(0.25),
                    "opencode's own price once it has one"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(
            !ev.iter().any(|e| matches!(e, AgentEvent::Result { .. })),
            "the session export closes the turn, not the stream"
        );
    }

    #[test]
    fn a_session_error_is_reported_with_its_name_and_message() {
        let ev = run(&[
            r#"{"type":"error","sessionID":"s","error":{"name":"APIError","data":{"message":"Rate limit exceeded","statusCode":429}}}"#,
            r#"{"type":"error","sessionID":"s","error":{"name":"UnknownError"}}"#,
        ]);
        match &ev[..] {
            [
                AgentEvent::Init { .. },
                AgentEvent::Error {
                    error_type,
                    message,
                },
                AgentEvent::Error {
                    error_type: t2,
                    message: m2,
                },
            ] => {
                assert_eq!(error_type, "APIError");
                assert_eq!(message, "Rate limit exceeded");
                assert_eq!(t2, "UnknownError");
                assert_eq!(m2, "UnknownError");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unknown_types_and_non_json_pass_through_as_raw() {
        let ev = run(&[
            r#"{"type":"session.compacted","x":1}"#,
            "warning: plain text",
            "",
        ]);
        match &ev[..] {
            [
                AgentEvent::Raw {
                    text: a,
                    stream: RawStream::Stdout,
                },
                AgentEvent::Raw {
                    text: b,
                    stream: RawStream::Stdout,
                },
            ] => {
                assert!(a.contains("session.compacted"));
                assert_eq!(b, "warning: plain text");
            }
            other => panic!("expected two Raw events, got {other:?}"),
        }
    }

    /// Golden test against a real `opencode run --format json` capture (paths redacted) against a
    /// scripted chat-completions endpoint: `hello.txt` through the write tool, then the closing
    /// sentence.
    #[test]
    fn golden_hello_capture() {
        let fixture = include_str!("testdata/opencode_run_hello.jsonl");
        let ev = run_with(
            OpenCodeJsonParser::new("qwen-3-8-27b").with_price(price),
            &fixture.lines().collect::<Vec<_>>(),
        );
        match &ev[..] {
            [
                AgentEvent::Init { model, .. },
                AgentEvent::Tool { name, summary, .. },
                AgentEvent::Tokens(t1),
                AgentEvent::Text { delta: line1 },
                AgentEvent::Text { delta: line2 },
                AgentEvent::Tokens(t2),
            ] => {
                assert_eq!(model, "qwen-3-8-27b");
                assert_eq!(name, "write");
                assert_eq!(summary, "hello.txt");
                assert_eq!((t1.input, t1.output), (200, 20));
                assert_eq!(line1, "Ran echo and wrote hello.txt.");
                assert_eq!(line2, "Done. ");
                assert_eq!((t2.input, t2.output, t2.total), (500, 50, 550));
                assert!(
                    t2.cost_usd.is_some_and(|c| c > 0.0),
                    "the estimate stamped a cost"
                );
            }
            other => panic!("unexpected event sequence from the capture: {other:?}"),
        }
    }
}
