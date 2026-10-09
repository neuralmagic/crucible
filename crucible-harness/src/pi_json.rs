//! Parse `pi --mode json` NDJSON into [`AgentEvent`]s.
//!
//! Pi emits one `type`-tagged JSON object per line: a `session` header, then the agent loop's
//! lifecycle (`agent_start`, `turn_start`, `message_start`/`message_update`/`message_end`,
//! `tool_execution_start`/`tool_execution_end`, `turn_end`, `agent_end`). Every model request is
//! one assistant message whose `message_end` carries the authoritative content blocks and that
//! request's usage, so text, thinking, and token samples come from `message_end`; tool events come
//! from `tool_execution_end`, which carries the result and the error flag. `agent_end` only closes
//! one low-level run (an automatic retry or overflow recovery can follow it), so the terminal
//! `agent_settled` closes the turn as a [`AgentEvent::Result`]. Pi prices usage from its own model
//! table, which is zero for a custom provider, so a zero total falls back to the pricing function
//! the owner installs. Lines with an unmodeled `type`, and non-JSON lines, pass through as
//! [`AgentEvent::Raw`] so a viewer never loses output to schema drift.

use crate::codex_json::PriceFn;
use crate::stream_json::{TOOL_IO_LIMIT, bounded_input, str_field, truncate_chars, u64_field};
use crate::tool_summary::summarize;
use crucible_contract::event::{AgentEvent, RawStream, Tokens};
use serde_json::Value;
use std::collections::HashMap;

/// Stateful decoder: feed it one stdout line at a time via [`PiJsonParser::push`].
pub struct PiJsonParser {
    /// The model crucible asked for; the header line does not name it.
    model: String,
    tool_io: bool,
    price: Option<PriceFn>,

    // Cumulative usage across the turn's assistant messages (each reports its own request).
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    /// The sum of what pi itself priced the turn at; zero for a model with no cost table.
    priced: f64,
    /// Assistant messages seen, the turn count reported on the result.
    messages: u32,
    /// First error of the turn, which makes the terminal Result an error.
    error: Option<String>,
    /// Arguments by tool call id, kept from `tool_execution_start` for the end event's summary.
    open: HashMap<String, Value>,
}

impl PiJsonParser {
    /// A parser for a turn running `model`, with no pricing installed.
    pub fn new(model: impl Into<String>) -> Self {
        PiJsonParser {
            model: model.into(),
            tool_io: false,
            price: None,
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            priced: 0.0,
            messages: 0,
            error: None,
            open: HashMap::new(),
        }
    }

    /// Install the pricing function used when pi's own cost table prices the turn at zero.
    pub fn with_price(mut self, price: PriceFn) -> Self {
        self.price = Some(price);
        self
    }

    /// Opt into verbose tool IO: tool events carry bounded inputs and result excerpts.
    pub fn with_tool_io(mut self, on: bool) -> Self {
        self.tool_io = on;
        self
    }

    /// Decode one line of `pi --mode json`, returning every [`AgentEvent`] it completed.
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
        match msg.get("type").and_then(Value::as_str) {
            Some("session") => out.push(AgentEvent::Init {
                model: self.model.clone(),
                tools: 0,
                agents: 0,
            }),
            Some("message_end") => {
                if let Some(m) = msg.get("message")
                    && str_field(m, "role") == "assistant"
                {
                    self.assistant(m, &mut out);
                }
            }
            Some("tool_execution_start") => {
                let id = str_field(&msg, "toolCallId");
                if !id.is_empty() {
                    self.open
                        .insert(id, msg.get("args").cloned().unwrap_or(Value::Null));
                }
            }
            Some("tool_execution_end") => self.tool_end(&msg, &mut out),
            Some("auto_retry_end") => {
                if msg.get("success").and_then(Value::as_bool) == Some(true) {
                    self.error = None;
                } else if self.error.is_none() {
                    let detail = str_field(&msg, "finalError");
                    self.error = Some(if detail.is_empty() {
                        "automatic retry failed".to_string()
                    } else {
                        detail
                    });
                }
            }
            Some("compaction_end") => {
                if msg.get("willRetry").and_then(Value::as_bool) == Some(true) {
                    self.error = None;
                }
            }
            Some("agent_settled") => {
                if msg.get("aborted").and_then(Value::as_bool) == Some(true) && self.error.is_none()
                {
                    self.error = Some("aborted".to_string());
                }
                out.push(self.result());
            }
            Some(
                "agent_start"
                | "agent_end"
                | "turn_start"
                | "turn_end"
                | "message_start"
                | "message_update"
                | "tool_execution_update"
                | "queue_update"
                | "compaction_start",
            ) => {}
            _ => out.push(raw(line)),
        }
        out
    }

    /// The turn's terminal event: an error when one stayed latched through the last retry.
    fn result(&self) -> AgentEvent {
        AgentEvent::Result {
            subtype: if self.error.is_some() {
                "error"
            } else {
                "success"
            }
            .to_string(),
            is_error: self.error.is_some(),
            turns: self.messages,
            cost_usd: self.cost(),
            error: self.error.clone(),
        }
    }

    /// One completed assistant message: its blocks in order, then this request's usage folded
    /// into the turn totals as a token sample. An `error`/`aborted` stop is latched as the turn's
    /// error.
    fn assistant(&mut self, m: &Value, out: &mut Vec<AgentEvent>) {
        self.messages += 1;
        if let Some(blocks) = m.get("content").and_then(Value::as_array) {
            for block in blocks {
                match str_field(block, "type").as_str() {
                    "text" => {
                        for delta in lines_of(&str_field(block, "text")) {
                            out.push(AgentEvent::Text { delta });
                        }
                    }
                    "thinking" => {
                        for delta in lines_of(&str_field(block, "thinking")) {
                            out.push(AgentEvent::Thinking { delta });
                        }
                    }
                    _ => {}
                }
            }
        }
        let usage = m.get("usage").unwrap_or(&Value::Null);
        self.input += u64_field(usage, "input");
        self.output += u64_field(usage, "output");
        self.cache_read += u64_field(usage, "cacheRead");
        self.cache_write += u64_field(usage, "cacheWrite");
        self.priced += usage
            .get("cost")
            .and_then(|c| c.get("total"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let stop = str_field(m, "stopReason");
        if stop == "error" || stop == "aborted" {
            let detail = str_field(m, "errorMessage");
            let message = if detail.is_empty() { stop } else { detail };
            if self.error.is_none() {
                self.error = Some(message.clone());
            }
            out.push(AgentEvent::Error {
                error_type: "pi".to_string(),
                message,
            });
        }
        out.push(AgentEvent::Tokens(self.tokens()));
    }

    /// A finished tool execution: the name, the summary of the arguments its start carried, and
    /// (verbose only) the bounded arguments and result text.
    fn tool_end(&mut self, msg: &Value, out: &mut Vec<AgentEvent>) {
        let id = str_field(msg, "toolCallId");
        let name = str_field(msg, "toolName");
        let args = self.open.remove(&id).unwrap_or(Value::Null);
        let is_error = msg.get("isError").and_then(Value::as_bool).unwrap_or(false);
        let mut summary = summarize(&name, &args);
        if is_error {
            summary = truncate_chars(
                &format!("{summary} [error]"),
                crate::tool_summary::SUMMARY_CAP,
            );
        }
        let (input, result) = if self.tool_io {
            let text = result_text(msg.get("result").unwrap_or(&Value::Null));
            (
                Some(bounded_input(&args)),
                (!text.is_empty()).then(|| truncate_chars(&text, TOOL_IO_LIMIT)),
            )
        } else {
            (None, None)
        };
        out.push(AgentEvent::Tool {
            name,
            summary,
            subagent: false,
            input,
            result,
        });
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
        t.cost_usd = self.cost_for(&t);
        t
    }

    /// The turn's cost so far: pi's own number when it priced the model, else the installed
    /// estimate, else nothing.
    fn cost_for(&self, t: &Tokens) -> Option<f64> {
        if self.priced > 0.0 {
            Some(self.priced)
        } else {
            self.price.map(|p| p(&self.model, t))
        }
    }

    fn cost(&self) -> f64 {
        self.cost_for(&self.tokens()).unwrap_or(0.0)
    }
}

/// The text of a tool result: its `content` blocks' text joined, or a bare string.
fn result_text(result: &Value) -> String {
    match result.get("content") {
        Some(Value::Array(items)) => items
            .iter()
            .filter(|b| str_field(b, "type") == "text")
            .map(|b| str_field(b, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::String(s)) => s.clone(),
        _ => match result {
            Value::String(s) => s.clone(),
            _ => String::new(),
        },
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
    use crate::pi_json::*;

    /// Pricing stand-in: $1/MTok in, $10/MTok out, cached reads a tenth of input.
    fn price(_model: &str, t: &Tokens) -> f64 {
        (t.input as f64 + t.cache_read as f64 * 0.1) * 1e-6 + t.output as f64 * 10e-6
    }

    fn run_with(mut p: PiJsonParser, lines: &[&str]) -> Vec<AgentEvent> {
        lines.iter().flat_map(|l| p.push(l)).collect()
    }

    fn run(lines: &[&str]) -> Vec<AgentEvent> {
        run_with(PiJsonParser::new("qwen-3-8-27b"), lines)
    }

    #[test]
    fn the_session_header_reports_the_configured_model() {
        let ev = run(&[r#"{"type":"session","version":3,"id":"u","timestamp":"t","cwd":"/w"}"#]);
        assert!(
            matches!(&ev[..], [AgentEvent::Init { model, tools, agents }]
            if model == "qwen-3-8-27b" && *tools == 0 && *agents == 0)
        );
    }

    #[test]
    fn an_assistant_message_yields_text_thinking_and_a_token_sample() {
        let ev = run(&[
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"plan\nit"},{"type":"text","text":"line one\nline two\n"}],"usage":{"input":100,"output":10,"cacheRead":5,"cacheWrite":1,"totalTokens":116,"cost":{"total":0}},"stopReason":"stop"}}"#,
        ]);
        match &ev[..] {
            [
                AgentEvent::Thinking { delta: t1 },
                AgentEvent::Thinking { delta: t2 },
                AgentEvent::Text { delta: a },
                AgentEvent::Text { delta: b },
                AgentEvent::Tokens(t),
            ] => {
                assert_eq!((t1.as_str(), t2.as_str()), ("plan", "it"));
                assert_eq!((a.as_str(), b.as_str()), ("line one", "line two"));
                assert_eq!(
                    (t.input, t.output, t.cache_read, t.cache_write),
                    (100, 10, 5, 1)
                );
                assert_eq!(t.total, 116);
                assert_eq!(t.cost_usd, None, "no price installed, pi priced zero");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn user_and_tool_result_messages_emit_nothing() {
        let ev = run(&[
            r#"{"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
            r#"{"type":"message_end","message":{"role":"toolResult","toolCallId":"c","toolName":"bash","content":[{"type":"text","text":"hi\n"}],"isError":false}}"#,
            r#"{"type":"message_start","message":{"role":"assistant","content":[]}}"#,
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"x"}}"#,
            r#"{"type":"turn_start"}"#,
            r#"{"type":"turn_end","message":{},"toolResults":[]}"#,
        ]);
        assert!(ev.is_empty(), "{ev:?}");
    }

    #[test]
    fn a_tool_execution_pairs_its_start_arguments_with_the_end() {
        let ev = run(&[
            r#"{"type":"tool_execution_start","toolCallId":"call_1","toolName":"bash","args":{"command":"cargo test"}}"#,
            r#"{"type":"tool_execution_update","toolCallId":"call_1","toolName":"bash","args":{},"partialResult":{}}"#,
            r#"{"type":"tool_execution_end","toolCallId":"call_1","toolName":"bash","result":{"content":[{"type":"text","text":"ok"}]},"isError":false}"#,
            r#"{"type":"tool_execution_start","toolCallId":"call_2","toolName":"write","args":{"path":"a.txt","content":"x"}}"#,
            r#"{"type":"tool_execution_end","toolCallId":"call_2","toolName":"write","result":{"content":[{"type":"text","text":"denied"}]},"isError":true}"#,
        ]);
        match &ev[..] {
            [
                AgentEvent::Tool {
                    name,
                    summary,
                    subagent,
                    input,
                    result,
                },
                AgentEvent::Tool {
                    name: n2,
                    summary: s2,
                    ..
                },
            ] => {
                assert_eq!(name, "bash");
                assert_eq!(summary, "$ cargo test");
                assert!(!subagent);
                assert!(input.is_none() && result.is_none(), "compact by default");
                assert_eq!(n2, "write");
                assert_eq!(s2, "a.txt [error]");
            }
            other => panic!("expected two Tool events, got {other:?}"),
        }
    }

    #[test]
    fn verbose_tool_io_carries_bounded_arguments_and_the_result_text() {
        let big = "x".repeat(TOOL_IO_LIMIT * 2);
        let start = format!(
            r#"{{"type":"tool_execution_start","toolCallId":"c","toolName":"bash","args":{{"command":"echo {big}"}}}}"#
        );
        let end = format!(
            r#"{{"type":"tool_execution_end","toolCallId":"c","toolName":"bash","result":{{"content":[{{"type":"text","text":"{big}"}}]}},"isError":false}}"#
        );
        let ev = run_with(
            PiJsonParser::new("m").with_tool_io(true),
            &[start.as_str(), end.as_str()],
        );
        match &ev[..] {
            [AgentEvent::Tool { input, result, .. }] => {
                let stored = input
                    .as_ref()
                    .and_then(Value::as_str)
                    .expect("oversized input degrades to a truncated string");
                assert!(stored.chars().count() <= TOOL_IO_LIMIT + 1);
                let excerpt = result.as_deref().expect("result excerpt");
                assert!(excerpt.chars().count() <= TOOL_IO_LIMIT + 1);
                assert!(excerpt.ends_with('…'));
            }
            other => panic!("expected one Tool, got {other:?}"),
        }
    }

    #[test]
    fn usage_accrues_across_messages_and_pi_pricing_beats_the_estimate() {
        let ev = run_with(
            PiJsonParser::new("m").with_price(price),
            &[
                r#"{"type":"message_end","message":{"role":"assistant","content":[],"usage":{"input":100,"output":10,"cacheRead":0,"cacheWrite":0,"cost":{"total":0}},"stopReason":"toolUse"}}"#,
                r#"{"type":"message_end","message":{"role":"assistant","content":[],"usage":{"input":200,"output":20,"cacheRead":0,"cacheWrite":0,"cost":{"total":0.5}},"stopReason":"stop"}}"#,
                r#"{"type":"agent_end","messages":[],"willRetry":false}"#,
                r#"{"type":"agent_settled","aborted":false}"#,
            ],
        );
        match &ev[..] {
            [
                AgentEvent::Tokens(first),
                AgentEvent::Tokens(second),
                AgentEvent::Result {
                    subtype,
                    is_error,
                    turns,
                    cost_usd,
                    error,
                },
            ] => {
                assert_eq!(
                    first.cost_usd,
                    Some(price("m", first)),
                    "estimate while pi says zero"
                );
                assert_eq!((second.input, second.output), (300, 30));
                assert_eq!(second.cost_usd, Some(0.5), "pi's own price once it has one");
                assert_eq!(subtype, "success");
                assert!(!*is_error);
                assert_eq!(*turns, 2);
                assert_eq!(*cost_usd, 0.5);
                assert!(error.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn an_error_stop_makes_the_result_an_error() {
        let ev = run(&[
            r#"{"type":"message_end","message":{"role":"assistant","content":[],"usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"cost":{"total":0}},"stopReason":"error","errorMessage":"No API key for provider: anthropic"}}"#,
            r#"{"type":"agent_end","messages":[],"willRetry":false}"#,
            r#"{"type":"agent_settled","aborted":false}"#,
        ]);
        match &ev[..] {
            [
                AgentEvent::Error {
                    error_type,
                    message,
                },
                AgentEvent::Tokens(_),
                AgentEvent::Result {
                    subtype,
                    is_error,
                    error,
                    ..
                },
            ] => {
                assert_eq!(error_type, "pi");
                assert_eq!(message, "No API key for provider: anthropic");
                assert_eq!(subtype, "error");
                assert!(*is_error);
                assert_eq!(error.as_deref(), Some("No API key for provider: anthropic"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    const ERROR_STOP: &str = r#"{"type":"message_end","message":{"role":"assistant","content":[],"usage":{"input":5,"output":0,"cacheRead":0,"cacheWrite":0,"cost":{"total":0}},"stopReason":"error","errorMessage":"529 overloaded"}}"#;
    const OK_STOP: &str = r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"done"}],"usage":{"input":5,"output":1,"cacheRead":0,"cacheWrite":0,"cost":{"total":0}},"stopReason":"stop"}}"#;

    fn results(ev: &[AgentEvent]) -> Vec<(bool, Option<String>, u32)> {
        ev.iter()
            .filter_map(|e| match e {
                AgentEvent::Result {
                    is_error,
                    error,
                    turns,
                    ..
                } => Some((*is_error, error.clone(), *turns)),
                _ => None,
            })
            .collect()
    }

    /// `agent_end` closes one low-level run, which an automatic retry follows; only
    /// `agent_settled` closes the turn, and a retry that succeeds clears the failed attempt's error.
    #[test]
    fn a_successful_retry_settles_once_as_success() {
        let ev = run(&[
            ERROR_STOP,
            r#"{"type":"agent_end","messages":[],"willRetry":false}"#,
            r#"{"type":"auto_retry_start","attempt":1,"maxAttempts":3,"delayMs":2000,"errorMessage":"529 overloaded"}"#,
            r#"{"type":"agent_start"}"#,
            OK_STOP,
            r#"{"type":"auto_retry_end","success":true,"attempt":2}"#,
            r#"{"type":"agent_end","messages":[],"willRetry":false}"#,
            r#"{"type":"agent_settled","aborted":false}"#,
        ]);
        assert_eq!(results(&ev), [(false, None, 2)]);
        assert!(
            ev.iter().any(
                |e| matches!(e, AgentEvent::Error { message, .. } if message == "529 overloaded")
            ),
            "the failed attempt is still reported as it happened"
        );
    }

    #[test]
    fn an_exhausted_retry_settles_as_its_final_error() {
        let ev = run(&[
            r#"{"type":"auto_retry_end","success":false,"attempt":3,"finalError":"529 overloaded after 3 attempts"}"#,
            r#"{"type":"agent_settled","aborted":false}"#,
        ]);
        assert_eq!(
            results(&ev),
            [(true, Some("529 overloaded after 3 attempts".to_string()), 0)]
        );

        let ev = run(&[
            r#"{"type":"auto_retry_end","success":false,"attempt":3}"#,
            r#"{"type":"agent_settled","aborted":false}"#,
        ]);
        assert_eq!(
            results(&ev),
            [(true, Some("automatic retry failed".to_string()), 0)]
        );
    }

    /// Overflow recovery: the overflowing request errors, compaction succeeds with
    /// `willRetry`, and the retried prompt finishes.
    #[test]
    fn overflow_recovery_clears_the_overflow_error() {
        let ev = run(&[
            ERROR_STOP,
            r#"{"type":"agent_end","messages":[],"willRetry":true}"#,
            r#"{"type":"compaction_start","reason":"overflow"}"#,
            r#"{"type":"compaction_end","reason":"overflow","result":{},"aborted":false,"willRetry":true}"#,
            OK_STOP,
            r#"{"type":"agent_settled","aborted":false}"#,
        ]);
        assert_eq!(results(&ev), [(false, None, 2)]);

        let ev = run(&[
            ERROR_STOP,
            r#"{"type":"compaction_end","reason":"overflow","aborted":false,"willRetry":false,"errorMessage":"too big"}"#,
            r#"{"type":"agent_settled","aborted":false}"#,
        ]);
        assert_eq!(
            results(&ev),
            [(true, Some("529 overloaded".to_string()), 1)]
        );
    }

    #[test]
    fn an_aborted_settle_is_an_error() {
        let ev = run(&[OK_STOP, r#"{"type":"agent_settled","aborted":true}"#]);
        assert_eq!(results(&ev), [(true, Some("aborted".to_string()), 1)]);
    }

    #[test]
    fn agent_end_alone_does_not_close_the_turn() {
        let ev = run(&[
            OK_STOP,
            r#"{"type":"agent_end","messages":[],"willRetry":false}"#,
        ]);
        assert!(results(&ev).is_empty(), "{ev:?}");
    }

    #[test]
    fn unknown_types_and_non_json_pass_through_as_raw() {
        let ev = run(&[
            r#"{"type":"extension_ui","x":1}"#,
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
                assert!(a.contains("extension_ui"));
                assert_eq!(b, "warning: plain text");
            }
            other => panic!("expected two Raw events, got {other:?}"),
        }
    }

    /// Golden test against a real pi 1.1.0 `--mode json --print` capture (paths redacted) against
    /// a scripted chat-completions endpoint: `echo hi` through bash, `hello.txt` through write, then
    /// the closing sentence.
    #[test]
    fn golden_hello_capture() {
        let fixture = include_str!("testdata/pi_json_hello.jsonl");
        let ev = run_with(
            PiJsonParser::new("qwen-3-8-27b").with_price(price),
            &fixture.lines().collect::<Vec<_>>(),
        );
        match &ev[..] {
            [
                AgentEvent::Init { model, .. },
                AgentEvent::Tokens(t1),
                AgentEvent::Tool {
                    name: bash,
                    summary: cmd,
                    ..
                },
                AgentEvent::Tokens(t2),
                AgentEvent::Tool {
                    name: write,
                    summary: path,
                    ..
                },
                AgentEvent::Text { delta: line1 },
                AgentEvent::Text { delta: line2 },
                AgentEvent::Tokens(t3),
                AgentEvent::Result {
                    subtype,
                    is_error,
                    turns,
                    cost_usd,
                    ..
                },
            ] => {
                assert_eq!(model, "qwen-3-8-27b");
                assert_eq!((t1.input, t1.output), (100, 10));
                assert_eq!(bash, "bash");
                assert_eq!(cmd, "$ echo hi  # Print hi");
                assert_eq!((t2.input, t2.output), (300, 30));
                assert_eq!(write, "write");
                assert_eq!(path, "hello.txt");
                assert_eq!(line1, "Ran echo and wrote hello.txt.");
                assert_eq!(line2, "Done. ");
                assert_eq!((t3.input, t3.output, t3.total), (600, 60, 660));
                assert_eq!(subtype, "success");
                assert!(!*is_error);
                assert_eq!(*turns, 3);
                assert!(*cost_usd > 0.0, "the estimate stamped a cost");
            }
            other => panic!("unexpected event sequence from the capture: {other:?}"),
        }
    }

    /// Golden test against a real pi 1.1.0 capture with `--tools +codemode` and one streamable-HTTP
    /// MCP server behind a bearer, against a scripted chat-completions endpoint: a codemode script
    /// calls `mcp__vuln__lookup`, then the closing sentence. The nested MCP call finishes inside
    /// the script, so its tool event lands before the codemode tool's own.
    #[test]
    fn golden_codemode_mcp_capture() {
        let fixture = include_str!("testdata/pi_json_codemode.jsonl");
        let ev = run_with(
            PiJsonParser::new("qwen-3-8-27b").with_tool_io(true),
            &fixture.lines().collect::<Vec<_>>(),
        );
        match &ev[..] {
            [
                AgentEvent::Init { .. },
                AgentEvent::Tokens(t1),
                AgentEvent::Tool {
                    name: nested,
                    result: nested_result,
                    ..
                },
                AgentEvent::Tool {
                    name: script,
                    input: script_input,
                    result: script_result,
                    ..
                },
                AgentEvent::Text { delta },
                AgentEvent::Tokens(t2),
                AgentEvent::Result {
                    is_error, turns, ..
                },
            ] => {
                assert_eq!((t1.input, t1.output), (100, 10));
                assert_eq!(nested, "mcp__vuln__lookup");
                assert_eq!(
                    nested_result.as_deref(),
                    Some("CVE-2026-0001: golang.org/x/net < 0.38.0")
                );
                assert_eq!(script, "codemode");
                assert!(
                    script_input.as_ref().is_some_and(|i| i["code"]
                        .as_str()
                        .is_some_and(|c| c.contains("tools.mcp__vuln__lookup"))),
                    "{script_input:?}"
                );
                assert!(
                    script_result
                        .as_deref()
                        .is_some_and(|r| r.starts_with("Script completed")
                            && r.contains("golang.org/x/net < 0.38.0")),
                    "{script_result:?}"
                );
                assert_eq!(
                    delta,
                    "CVE-2026-0001 affects golang.org/x/net below 0.38.0."
                );
                assert_eq!((t2.input, t2.output, t2.total), (300, 30, 330));
                assert!(!*is_error);
                assert_eq!(*turns, 2);
            }
            other => panic!("unexpected event sequence from the capture: {other:?}"),
        }
    }
}
