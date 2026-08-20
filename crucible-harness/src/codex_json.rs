//! Parse `codex exec --json` NDJSON into [`AgentEvent`]s.
//!
//! Codex emits one `type`-tagged JSON object per line: a `thread.started` header, `item.started` /
//! `item.completed` pairs per work item, and a terminal `turn.completed` carrying token usage. The
//! stream declares no cost, so the parser stamps a pricing-table estimate through the `price`
//! function its owner installs. Lines with an unmodeled `type`, and non-JSON lines, pass through as
//! [`AgentEvent::Raw`] so a viewer never loses output to schema drift.

use crate::stream_json::{TOOL_IO_LIMIT, bounded_input, str_field, truncate_chars, u64_field};
use crucible_contract::event::{AgentEvent, RawStream, Tokens};
use serde_json::Value;
use std::collections::HashMap;

/// Per-token pricing for a model, supplied by the caller so this crate stays free of the pricing
/// table. Signature matches `crucible::event::estimate_cost`.
pub type PriceFn = fn(&str, &Tokens) -> f64;

/// Stateful decoder: feed it one stdout line at a time via [`CodexJsonParser::push`].
pub struct CodexJsonParser {
    /// The model crucible asked for; codex's stream does not name it.
    model: String,
    tool_io: bool,
    price: Option<PriceFn>,

    /// The thread codex opened, kept for a future `codex exec resume`.
    thread_id: Option<String>,
    /// First error line of the turn, which makes the terminal Result an error.
    error: Option<String>,
    /// Items seen at `item.started`, so `item.completed` can recover fields the completion drops.
    /// Only populated under verbose tool IO.
    open: HashMap<String, Value>,
}

impl CodexJsonParser {
    /// A parser for a turn running `model`, with no pricing installed (cost reports as zero).
    pub fn new(model: impl Into<String>) -> Self {
        CodexJsonParser {
            model: model.into(),
            tool_io: false,
            price: None,
            thread_id: None,
            error: None,
            open: HashMap::new(),
        }
    }

    /// Install the pricing function used to estimate the turn's cost from `turn.completed` usage.
    pub fn with_price(mut self, price: PriceFn) -> Self {
        self.price = Some(price);
        self
    }

    /// Opt into verbose tool IO: tool events carry bounded inputs and result excerpts.
    pub fn with_tool_io(mut self, on: bool) -> Self {
        self.tool_io = on;
        self
    }

    /// The thread codex opened for this turn, once `thread.started` has been seen.
    pub fn thread_id(&self) -> Option<&str> {
        self.thread_id.as_deref()
    }

    /// Decode one line of `codex exec --json`, returning every [`AgentEvent`] it completed.
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
            Some("thread.started") => {
                let id = str_field(&msg, "thread_id");
                self.thread_id = (!id.is_empty()).then_some(id);
                out.push(AgentEvent::Init {
                    model: self.model.clone(),
                    tools: 0,
                    agents: 0,
                });
            }
            Some("turn.started") => {}
            Some("item.started") => {
                if self.tool_io
                    && let Some(item) = msg.get("item")
                {
                    let id = str_field(item, "id");
                    if !id.is_empty() {
                        self.open.insert(id, item.clone());
                    }
                }
            }
            Some("item.completed") => {
                if let Some(item) = msg.get("item") {
                    self.item(item, &mut out);
                }
            }
            Some("turn.completed") => self.turn_completed(&msg, &mut out),
            Some("turn.failed") => {
                let message = error_message(msg.get("error").unwrap_or(&Value::Null));
                self.fail(message, &mut out);
                self.turn_completed(&msg, &mut out);
            }
            Some("error") => {
                let message = error_message(msg.get("error").unwrap_or(&Value::Null));
                self.fail(message, &mut out);
            }
            _ => out.push(raw(line)),
        }
        out
    }

    /// A completed work item: assistant text, reasoning, or a tool call.
    fn item(&mut self, item: &Value, out: &mut Vec<AgentEvent>) {
        let kind = str_field(item, "type");
        let started = self.tool_io.then(|| {
            let id = str_field(item, "id");
            self.open.remove(&id)
        });
        match kind.as_str() {
            "agent_message" => {
                for delta in lines_of(&str_field(item, "text")) {
                    out.push(AgentEvent::Text { delta });
                }
            }
            "reasoning" => {
                for delta in lines_of(&reasoning_text(item)) {
                    out.push(AgentEvent::Thinking { delta });
                }
            }
            "command_execution" | "file_change" | "file_changes" | "mcp_tool_call"
            | "web_search" | "plan_update" | "todo_list" => {
                let (input, result) = if self.tool_io {
                    let source = started.flatten();
                    let merged = source.as_ref().unwrap_or(item);
                    (
                        Some(bounded_input(merged)),
                        item_result(item).map(|r| truncate_chars(&r, TOOL_IO_LIMIT)),
                    )
                } else {
                    (None, None)
                };
                out.push(AgentEvent::Tool {
                    name: tool_name(&kind, item),
                    summary: tool_summary(&kind, item),
                    subagent: false,
                    input,
                    result,
                });
            }
            _ => {}
        }
    }

    /// Latch the turn's first error and report it immediately.
    fn fail(&mut self, message: String, out: &mut Vec<AgentEvent>) {
        if self.error.is_none() {
            self.error = Some(message.clone());
        }
        out.push(AgentEvent::Error {
            error_type: "codex".to_string(),
            message,
        });
    }

    /// Terminal usage + result. `input_tokens` counts the cached prefix too, so the cached half
    /// moves to `cache_read` and the totals stay honest.
    fn turn_completed(&mut self, msg: &Value, out: &mut Vec<AgentEvent>) {
        let usage = msg.get("usage").unwrap_or(&Value::Null);
        let cache_read = u64_field(usage, "cached_input_tokens");
        let input = u64_field(usage, "input_tokens").saturating_sub(cache_read);
        let output = u64_field(usage, "output_tokens");
        let mut tokens = Tokens {
            input,
            output,
            cache_read,
            cache_write: 0,
            total: input + output + cache_read,
            rate: None,
            cost_usd: None,
        };
        let cost_usd = self.price.map_or(0.0, |p| p(&self.model, &tokens));
        tokens.cost_usd = self.price.map(|_| cost_usd);
        out.push(AgentEvent::Tokens(tokens));

        let error = self.error.clone();
        out.push(AgentEvent::Result {
            subtype: if error.is_some() { "error" } else { "success" }.to_string(),
            is_error: error.is_some(),
            turns: 1,
            cost_usd,
            error,
        });
    }
}

/// The short tool label for a completed item.
fn tool_name(kind: &str, item: &Value) -> String {
    match kind {
        "command_execution" => "shell".to_string(),
        "file_change" | "file_changes" => "apply_patch".to_string(),
        "mcp_tool_call" => {
            let server = str_field(item, "server");
            let tool = str_field(item, "tool");
            match (server.is_empty(), tool.is_empty()) {
                (true, true) => "mcp".to_string(),
                (true, false) => tool,
                (false, true) => server,
                (false, false) => format!("{server}/{tool}"),
            }
        }
        "plan_update" | "todo_list" => "plan_update".to_string(),
        other => other.to_string(),
    }
}

/// A compact one-line summary per item type; cosmetic only.
fn tool_summary(kind: &str, item: &Value) -> String {
    let summary = match kind {
        "command_execution" => {
            let cmd = str_field(item, "command");
            match item.get("exit_code").and_then(Value::as_i64) {
                Some(0) | None => format!("$ {cmd}"),
                Some(code) => format!("$ {cmd}  # exit {code}"),
            }
        }
        "file_change" | "file_changes" => changed_paths(item),
        "mcp_tool_call" => {
            let args = item.get("arguments").map(compact).unwrap_or_default();
            if args.is_empty() {
                str_field(item, "tool")
            } else {
                args
            }
        }
        "web_search" => str_field(item, "query"),
        "plan_update" | "todo_list" => plan_summary(item),
        _ => String::new(),
    };
    let status = str_field(item, "status");
    if status.is_empty() || status == "completed" {
        truncate_chars(&summary, SUMMARY_CAP)
    } else {
        truncate_chars(&format!("{summary} [{status}]"), SUMMARY_CAP)
    }
}

/// Char bound on a tool summary line.
const SUMMARY_CAP: usize = 200;

/// `kind path` per entry of a `file_change` item's `changes` array.
fn changed_paths(item: &Value) -> String {
    let Some(changes) = item.get("changes").and_then(Value::as_array) else {
        return String::new();
    };
    changes
        .iter()
        .map(|c| {
            let path = str_field(c, "path");
            let kind = str_field(c, "kind");
            if kind.is_empty() {
                path
            } else {
                format!("{kind} {path}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Step count plus the first pending step of a plan item.
fn plan_summary(item: &Value) -> String {
    let steps = item
        .get("items")
        .or_else(|| item.get("plan"))
        .and_then(Value::as_array);
    let Some(steps) = steps else {
        return String::new();
    };
    let next = steps
        .iter()
        .find(|s| str_field(s, "status") != "completed")
        .map(|s| {
            let t = str_field(s, "step");
            if t.is_empty() {
                str_field(s, "text")
            } else {
                t
            }
        })
        .unwrap_or_default();
    if next.is_empty() {
        format!("{} steps", steps.len())
    } else {
        format!("{} steps: {next}", steps.len())
    }
}

/// The output an item produced, for a verbose-tool-IO result excerpt.
fn item_result(item: &Value) -> Option<String> {
    for key in ["aggregated_output", "output", "result", "content"] {
        match item.get(key) {
            Some(Value::String(s)) if !s.is_empty() => return Some(s.clone()),
            Some(other @ (Value::Array(_) | Value::Object(_))) => return Some(other.to_string()),
            _ => {}
        }
    }
    None
}

/// Reasoning items carry their text in `text`, or as a `summary` array of chunks.
fn reasoning_text(item: &Value) -> String {
    let text = str_field(item, "text");
    if !text.is_empty() {
        return text;
    }
    match item.get("summary") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => str_field(other, "text"),
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// An error payload that is either a bare string or an object with a `message`.
fn error_message(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(_) => {
            let m = str_field(v, "message");
            if m.is_empty() { v.to_string() } else { m }
        }
        _ => String::new(),
    }
}

/// Split a block of assistant text into one event per line, dropping a trailing newline's
/// empty tail so one line is one event.
fn lines_of(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    text.trim_end_matches('\n')
        .split('\n')
        .map(str::to_string)
        .collect()
}

/// A JSON value rendered for a summary line: strings bare, everything else serialized.
fn compact(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
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

    fn parser() -> CodexJsonParser {
        CodexJsonParser::new("gpt-5.6-sol")
    }

    /// Feed a sequence of lines through one parser and collect every event, in order.
    fn run_with(mut p: CodexJsonParser, lines: &[&str]) -> Vec<AgentEvent> {
        lines.iter().flat_map(|l| p.push(l)).collect()
    }

    fn run(lines: &[&str]) -> Vec<AgentEvent> {
        run_with(parser(), lines)
    }

    #[test]
    fn thread_started_reports_the_configured_model() {
        let ev = run(&[r#"{"type":"thread.started","thread_id":"t-1"}"#]);
        assert!(
            matches!(&ev[..], [AgentEvent::Init { model, tools, agents }]
            if model == "gpt-5.6-sol" && *tools == 0 && *agents == 0)
        );
    }

    #[test]
    fn thread_id_is_kept_for_resume() {
        let mut p = parser();
        assert_eq!(p.thread_id(), None);
        p.push(r#"{"type":"thread.started","thread_id":"t-42"}"#);
        assert_eq!(p.thread_id(), Some("t-42"));
    }

    #[test]
    fn agent_message_becomes_one_text_event_per_line() {
        let ev = run(&[
            r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"line one\nline two\n"}}"#,
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
    fn reasoning_becomes_thinking_from_text_or_summary_chunks() {
        let ev = run(&[
            r#"{"type":"item.completed","item":{"id":"i1","type":"reasoning","text":"weighing it"}}"#,
        ]);
        assert!(matches!(&ev[..], [AgentEvent::Thinking { delta }] if delta == "weighing it"));

        let ev = run(&[
            r#"{"type":"item.completed","item":{"id":"i1","type":"reasoning","summary":[{"type":"summary_text","text":"step one"}]}}"#,
        ]);
        assert!(matches!(&ev[..], [AgentEvent::Thinking { delta }] if delta == "step one"));
    }

    #[test]
    fn item_started_emits_nothing() {
        let ev = run(&[
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.started","item":{"id":"i1","type":"command_execution","command":"ls","status":"in_progress"}}"#,
        ]);
        assert!(ev.is_empty(), "only completions emit, got {ev:?}");
    }

    #[test]
    fn command_execution_summarizes_the_command_and_a_bad_exit() {
        let ev = run(&[
            r#"{"type":"item.completed","item":{"id":"i1","type":"command_execution","command":"cargo test","exit_code":0,"status":"completed"}}"#,
            r#"{"type":"item.completed","item":{"id":"i2","type":"command_execution","command":"cargo build","exit_code":101,"status":"completed"}}"#,
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
                    summary: failed, ..
                },
            ] => {
                assert_eq!(name, "shell");
                assert_eq!(summary, "$ cargo test");
                assert!(!subagent);
                assert!(input.is_none(), "compact by default");
                assert!(result.is_none());
                assert_eq!(failed, "$ cargo build  # exit 101");
            }
            other => panic!("expected two Tool events, got {other:?}"),
        }
    }

    #[test]
    fn file_change_mcp_web_search_and_plan_map_to_tools() {
        let ev = run(&[
            r#"{"type":"item.completed","item":{"id":"i1","type":"file_change","status":"completed","changes":[{"path":"src/a.rs","kind":"add"},{"path":"src/b.rs","kind":"update"}]}}"#,
            r#"{"type":"item.completed","item":{"id":"i2","type":"mcp_tool_call","server":"broker","tool":"search","status":"completed","arguments":{"q":"rust"}}}"#,
            r#"{"type":"item.completed","item":{"id":"i3","type":"web_search","query":"codex exec json"}}"#,
            r#"{"type":"item.completed","item":{"id":"i4","type":"todo_list","items":[{"step":"read","status":"completed"},{"step":"write","status":"pending"}]}}"#,
        ]);
        let seen: Vec<(&str, &str)> = ev
            .iter()
            .map(|e| match e {
                AgentEvent::Tool { name, summary, .. } => (name.as_str(), summary.as_str()),
                other => panic!("expected Tool, got {other:?}"),
            })
            .collect();
        assert_eq!(
            seen,
            vec![
                ("apply_patch", "add src/a.rs, update src/b.rs"),
                ("broker/search", r#"{"q":"rust"}"#),
                ("web_search", "codex exec json"),
                ("plan_update", "2 steps: write"),
            ]
        );
    }

    #[test]
    fn a_non_terminal_status_rides_the_summary() {
        let ev = run(&[
            r#"{"type":"item.completed","item":{"id":"i1","type":"command_execution","command":"sleep 99","status":"failed"}}"#,
        ]);
        assert!(matches!(&ev[..], [AgentEvent::Tool { summary, .. }]
            if summary == "$ sleep 99 [failed]"));
    }

    #[test]
    fn turn_completed_splits_cached_input_and_estimates_cost() {
        let ev = run_with(
            parser().with_price(price),
            &[
                r#"{"type":"turn.completed","usage":{"input_tokens":4212,"cached_input_tokens":3840,"output_tokens":96,"reasoning_output_tokens":64}}"#,
            ],
        );
        match &ev[..] {
            [
                AgentEvent::Tokens(t),
                AgentEvent::Result {
                    subtype,
                    is_error,
                    turns,
                    cost_usd,
                    error,
                },
            ] => {
                assert_eq!(t.input, 372, "cached prefix moved out of input");
                assert_eq!(t.cache_read, 3840);
                assert_eq!(t.output, 96);
                assert_eq!(t.cache_write, 0);
                assert_eq!(t.total, 372 + 96 + 3840);
                assert!(t.rate.is_none());
                let want = price("gpt-5.6-sol", t);
                assert_eq!(t.cost_usd, Some(want));
                assert!((cost_usd - want).abs() < 1e-12);
                assert_eq!(subtype, "success");
                assert!(!*is_error);
                assert_eq!(*turns, 1);
                assert!(error.is_none());
            }
            other => panic!("expected Tokens + Result, got {other:?}"),
        }
    }

    #[test]
    fn without_a_price_function_cost_is_zero_and_unstamped() {
        let ev = run(&[
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":2}}"#,
        ]);
        match &ev[..] {
            [AgentEvent::Tokens(t), AgentEvent::Result { cost_usd, .. }] => {
                assert_eq!(t.cost_usd, None, "no pricing installed, nothing to stamp");
                assert_eq!(*cost_usd, 0.0);
            }
            other => panic!("expected Tokens + Result, got {other:?}"),
        }
    }

    #[test]
    fn an_error_line_makes_the_result_an_error() {
        let ev = run(&[
            r#"{"type":"error","error":"stream disconnected"}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":5,"cached_input_tokens":0,"output_tokens":1}}"#,
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
                assert_eq!(error_type, "codex");
                assert_eq!(message, "stream disconnected");
                assert_eq!(subtype, "error");
                assert!(*is_error);
                assert_eq!(error.as_deref(), Some("stream disconnected"));
            }
            other => panic!("expected Error + Tokens + Result, got {other:?}"),
        }
    }

    #[test]
    fn turn_failed_terminates_the_turn_as_an_error() {
        let ev = run(&[
            r#"{"type":"turn.failed","error":{"message":"usage limit reached"},"usage":{"input_tokens":7,"cached_input_tokens":0,"output_tokens":0}}"#,
        ]);
        match &ev[..] {
            [
                AgentEvent::Error { message, .. },
                AgentEvent::Tokens(_),
                AgentEvent::Result {
                    is_error, error, ..
                },
            ] => {
                assert_eq!(message, "usage limit reached");
                assert!(*is_error);
                assert_eq!(error.as_deref(), Some("usage limit reached"));
            }
            other => panic!("expected Error + Tokens + Result, got {other:?}"),
        }
    }

    #[test]
    fn unknown_types_and_non_json_pass_through_as_raw() {
        let ev = run(&[
            r#"{"type":"turn.deltas","delta":"x"}"#,
            "warning: something on stdout",
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
                assert!(a.contains("turn.deltas"));
                assert_eq!(b, "warning: something on stdout");
            }
            other => panic!("expected two Raw events, got {other:?}"),
        }
    }

    #[test]
    fn verbose_tool_io_carries_the_started_item_and_a_result_excerpt() {
        let ev = run_with(
            parser().with_tool_io(true),
            &[
                r#"{"type":"item.started","item":{"id":"i1","type":"command_execution","command":"ls -1","status":"in_progress"}}"#,
                r#"{"type":"item.completed","item":{"id":"i1","type":"command_execution","command":"ls -1","aggregated_output":"a.txt\nb.txt","exit_code":0,"status":"completed"}}"#,
            ],
        );
        match &ev[..] {
            [AgentEvent::Tool { input, result, .. }] => {
                assert_eq!(
                    input
                        .as_ref()
                        .and_then(|i| i.get("command"))
                        .and_then(Value::as_str),
                    Some("ls -1")
                );
                assert_eq!(result.as_deref(), Some("a.txt\nb.txt"));
            }
            other => panic!("expected one Tool, got {other:?}"),
        }
    }

    #[test]
    fn verbose_tool_io_bounds_oversized_input_and_result() {
        let big = "x".repeat(TOOL_IO_LIMIT * 2);
        let line = format!(
            r#"{{"type":"item.completed","item":{{"id":"i1","type":"command_execution","command":"echo {big}","aggregated_output":"{big}","exit_code":0,"status":"completed"}}}}"#
        );
        let ev = run_with(parser().with_tool_io(true), &[line.as_str()]);
        match &ev[..] {
            [
                AgentEvent::Tool {
                    summary,
                    input,
                    result,
                    ..
                },
            ] => {
                assert!(summary.chars().count() <= SUMMARY_CAP + 1);
                let stored = input
                    .as_ref()
                    .and_then(Value::as_str)
                    .expect("oversized input degrades to a truncated string");
                assert!(stored.chars().count() <= TOOL_IO_LIMIT + 1);
                assert!(stored.ends_with('…'));
                let excerpt = result.as_deref().expect("result excerpt");
                assert!(excerpt.chars().count() <= TOOL_IO_LIMIT + 1);
                assert!(excerpt.ends_with('…'));
            }
            other => panic!("expected one Tool, got {other:?}"),
        }
    }

    /// Golden test against a real `codex exec --json` capture (paths redacted): `echo hi` through
    /// the shell tool, then `hello.txt` through apply_patch.
    #[test]
    fn golden_hello_capture() {
        let fixture = include_str!("testdata/codex_exec_hello.jsonl");
        let ev = run_with(
            parser().with_price(price),
            &fixture.lines().collect::<Vec<_>>(),
        );
        match &ev[..] {
            [
                AgentEvent::Init { model, .. },
                AgentEvent::Text { delta: text },
                AgentEvent::Tool {
                    name: shell,
                    summary: cmd,
                    ..
                },
                AgentEvent::Tool {
                    name: patch,
                    summary: changed,
                    ..
                },
                AgentEvent::Tokens(t),
                AgentEvent::Result {
                    subtype,
                    is_error,
                    turns,
                    cost_usd,
                    ..
                },
            ] => {
                assert_eq!(model, "gpt-5.6-sol");
                assert_eq!(
                    text,
                    "I\u{2019}ll run the command, then create the file exactly as requested."
                );
                assert_eq!(shell, "shell");
                assert_eq!(cmd, "$ /bin/zsh -lc 'echo hi'");
                assert_eq!(patch, "apply_patch");
                assert_eq!(changed, "add /sandbox/workspace/hello.txt");
                // `input_tokens` includes the cached prefix, so the split must not double-count.
                assert_eq!(t.input, 51925 - 38144);
                assert_eq!(t.cache_read, 38144);
                assert_eq!(t.output, 158);
                assert_eq!(t.total, 51925 + 158);
                assert_eq!(subtype, "success");
                assert!(!*is_error);
                assert_eq!(*turns, 1);
                assert!(*cost_usd > 0.0, "pricing seam stamped a cost");
                // The capture's empty trailing `agent_message` emits nothing, and the run had no
                // `reasoning` items at all.
                assert!(!ev.iter().any(|e| matches!(e, AgentEvent::Thinking { .. })));
            }
            other => panic!("unexpected event sequence from the capture: {other:?}"),
        }
    }

    /// Golden test against a real capture of an MCP tool call (`codex exec --json` against a
    /// local streamable-HTTP server with `http_headers` bearer auth and
    /// `--dangerously-bypass-approvals-and-sandbox`; without the bypass flag, non-interactive
    /// exec auto-cancels every MCP call, openai/codex#16685).
    #[test]
    fn golden_mcp_capture() {
        let fixture = include_str!("testdata/codex_exec_mcp.jsonl");
        let ev = run_with(
            parser().with_price(price),
            &fixture.lines().collect::<Vec<_>>(),
        );
        match &ev[..] {
            [
                AgentEvent::Init { .. },
                AgentEvent::Tool { name, summary, .. },
                AgentEvent::Text { delta },
                AgentEvent::Tokens(t),
                AgentEvent::Result {
                    subtype, is_error, ..
                },
            ] => {
                assert_eq!(name, "crucible-broker/add");
                assert_eq!(summary, r#"{"a":2,"b":3}"#, "summary is the arguments");
                assert_eq!(delta, "5");
                assert_eq!(t.cache_read, 48384);
                assert_eq!(subtype, "success");
                assert!(!*is_error);
            }
            other => panic!("unexpected event sequence from the capture: {other:?}"),
        }
    }
}
