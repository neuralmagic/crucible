//! Parse the agent's session transcript (JSONL) two ways from the same walk:
//!
//! - [`parse_transcript`]: timed tool invocations for span synthesis: `tool_use`/`tool_result`
//!   blocks paired by id give each call a start/end and an error verdict. Input hints are
//!   whitelisted per tool and redacted before truncation, no contents, output, or env ever ride a
//!   span.
//! - [`parse_messages`]: the FULL conversation (prompts, assistant text + reasoning, tool calls
//!   with full args, tool results) as [`GenAiRecord`]s for the opt-in OpenTelemetry GenAI content
//!   log records ([`crate::engine::emit_conversation_logs`]). This carries the real message bodies,
//!   so it is gated off by default and its bodies are run through the same [`redact`] machinery.

use jiff::Timestamp;
use serde_json::Value;
use std::collections::HashMap;
use std::time::SystemTime;

/// The longest input hint kept on a span (a `Bash` command line, mostly). Long enough to identify
/// the call, short enough that no meaningful payload rides along.
const SUMMARY_CAP: usize = 200;

/// One tool call reconstructed from the transcript. `end` is `None` only when the turn ended before
/// the tool reported a result (the engine closes the span at turn end and marks it errored); a
/// result whose own timestamp is unparseable still closes the call (zero duration at `start`).
/// `summary` is the sanitized one-line input hint (possibly empty); `error` is the tool's verdict.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolInvocation {
    pub(crate) name: String,
    pub(crate) start: SystemTime,
    pub(crate) end: Option<SystemTime>,
    pub(crate) summary: String,
    pub(crate) error: bool,
}

/// The parse accumulator: invocations in first-seen order, plus the `tool_use_id` → slot map that
/// pairs each result with its use (parallel and nested calls resolve through it). `redact` carries
/// the credential-scrub toggle down to each hint.
struct Calls {
    order: Vec<ToolInvocation>,
    by_id: HashMap<String, usize>,
    redact: bool,
}

/// Whether to scrub credential-shaped fragments from span hints. Defaults ON; set
/// `CRUCIBLE_TURN_TRACE_REDACT` to `0`/`false`/`off` to disable it (e.g. when the harness already
/// sanitizes tool inputs and the extra scrub just muddies the trace).
/// `pub(crate)` so the hermes reader defaults to the same toggle.
pub(crate) fn redact_enabled() -> bool {
    !matches!(
        std::env::var("CRUCIBLE_TURN_TRACE_REDACT").ok().as_deref(),
        Some("0") | Some("false") | Some("FALSE") | Some("off")
    )
}

/// Parse a native session transcript into its tool invocations, in call order. Robust to a torn
/// tail and interleaved noise: any line that isn't valid JSON, or carries no `message.content`
/// array, is skipped; a `tool_result` with no known id is ignored.
pub(crate) fn parse_transcript(jsonl: &str) -> Vec<ToolInvocation> {
    parse_transcript_with(jsonl, redact_enabled())
}

/// [`parse_transcript`] with the redaction toggle passed explicitly (the env read lifted out so the
/// disabled path is testable without mutating process env).
fn parse_transcript_with(jsonl: &str, redact: bool) -> Vec<ToolInvocation> {
    let mut calls = Calls {
        order: Vec::new(),
        by_id: HashMap::new(),
        redact,
    };
    for line in jsonl.lines().map(str::trim).filter(|l| !l.is_empty()) {
        ingest_line(line, &mut calls);
    }
    calls.order
}

/// One tool call inside an assistant message: the tool name and its FULL input arguments,
/// JSON-encoded (redacted when the toggle is on), unlike the span path's whitelisted one-line
/// hint, this carries every argument. `id` pairs the call with its later `gen_ai.tool.message`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
}

/// One conversation record from the transcript, already mapped to an OpenTelemetry GenAI record
/// kind. Bodies are the FULL message content (redacted when the toggle is on); the engine's logs
/// exporter turns each into a `gen_ai.*` log record correlated to the turn span.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GenAiRecord {
    /// A system prompt → `gen_ai.system.message`.
    System { content: String },
    /// A user prompt → `gen_ai.user.message`.
    User { content: String },
    /// An assistant turn (text, reasoning, and any tool calls) → `gen_ai.assistant.message`, or
    /// `gen_ai.choice` when it is the transcript's final assistant turn (the completion).
    Assistant {
        text: Option<String>,
        reasoning: Option<String>,
        tool_calls: Vec<ToolCall>,
        model: Option<String>,
    },
    /// A tool result → `gen_ai.tool.message`. `id` is the originating `tool_use` id.
    Tool {
        id: String,
        content: String,
        is_error: bool,
    },
}

/// Parse a native session transcript into its full conversation records, in transcript order,
/// with message bodies redacted per [`redact_enabled`]. Robust to a torn tail and interleaved
/// noise exactly like [`parse_transcript`]: a non-JSON line, or one with no usable message, is
/// skipped.
pub(crate) fn parse_messages(jsonl: &str) -> Vec<GenAiRecord> {
    parse_messages_with(jsonl, redact_enabled())
}

/// [`parse_messages`] with the redaction toggle passed explicitly (env read lifted out for tests).
fn parse_messages_with(jsonl: &str, redact: bool) -> Vec<GenAiRecord> {
    let mut out = Vec::new();
    for line in jsonl.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            ingest_message_line(&v, redact, &mut out);
        }
    }
    out
}

/// Fold one transcript line into conversation records, dispatched by message role.
fn ingest_message_line(v: &Value, redact: bool, out: &mut Vec<GenAiRecord>) {
    let msg = v.get("message");
    // Role from `message.role`, falling back to the line's top-level `type`.
    let role = msg
        .and_then(|m| m.get("role"))
        .and_then(Value::as_str)
        .or_else(|| v.get("type").and_then(Value::as_str))
        .unwrap_or("");
    let content = msg
        .and_then(|m| m.get("content"))
        .or_else(|| v.get("content"));
    match role {
        "assistant" => {
            let model = msg
                .and_then(|m| m.get("model"))
                .and_then(Value::as_str)
                .map(str::to_string);
            ingest_assistant(content, model, redact, out);
        }
        "user" => ingest_user(content, redact, out),
        "system" => {
            if let Some(c) = content_to_text(content, redact) {
                out.push(GenAiRecord::System { content: c });
            }
        }
        _ => {}
    }
}

/// An assistant line collapses its blocks into one record: text blocks joined, `thinking` blocks as
/// reasoning, `tool_use` blocks as tool calls (full input args). A string-content assistant line is
/// treated as a single text block. Nothing is pushed for an empty message.
fn ingest_assistant(
    content: Option<&Value>,
    model: Option<String>,
    redact: bool,
    out: &mut Vec<GenAiRecord>,
) {
    let mut text_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    match content {
        Some(Value::String(s)) if !s.trim().is_empty() => {
            text_parts.push(scrub(s, redact));
        }
        Some(Value::Array(blocks)) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(Value::as_str) {
                            text_parts.push(scrub(t, redact));
                        }
                    }
                    // Extended thinking rides as `thinking` (older transcripts: `reasoning`).
                    Some("thinking") | Some("reasoning") => {
                        if let Some(t) = block
                            .get("thinking")
                            .or_else(|| block.get("reasoning"))
                            .and_then(Value::as_str)
                        {
                            reasoning_parts.push(scrub(t, redact));
                        }
                    }
                    Some("tool_use") => {
                        if let Some(call) = tool_call(block, redact) {
                            tool_calls.push(call);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    if text_parts.is_empty() && reasoning_parts.is_empty() && tool_calls.is_empty() {
        return;
    }
    out.push(GenAiRecord::Assistant {
        text: join_opt(text_parts),
        reasoning: join_opt(reasoning_parts),
        tool_calls,
        model,
    });
}

/// A `tool_use` block → a [`ToolCall`] with its full input args JSON-encoded (then redacted). `None`
/// when the block has no id (nothing to correlate the later result against).
fn tool_call(block: &Value, redact: bool) -> Option<ToolCall> {
    let id = block.get("id").and_then(Value::as_str)?.to_string();
    let name = block
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string();
    let arguments = block
        .get("input")
        .map(|v| serde_json::to_string(v).unwrap_or_default())
        .unwrap_or_default();
    Some(ToolCall {
        id,
        name,
        arguments: scrub(&arguments, redact),
    })
}

/// A user line: `tool_result` blocks become [`GenAiRecord::Tool`] (in order), and any user text
/// (string content, or `text` blocks) becomes one [`GenAiRecord::User`]. A plain-string content is
/// the prompt itself. Anthropic transcripts carry tool results on a user-role message, hence both
/// kinds are handled here.
fn ingest_user(content: Option<&Value>, redact: bool, out: &mut Vec<GenAiRecord>) {
    match content {
        Some(Value::String(s)) if !s.trim().is_empty() => {
            out.push(GenAiRecord::User {
                content: scrub(s, redact),
            });
        }
        Some(Value::Array(blocks)) => {
            let mut texts: Vec<String> = Vec::new();
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_result") => {
                        let id = block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let is_error = block
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let body =
                            content_to_text(block.get("content"), redact).unwrap_or_default();
                        out.push(GenAiRecord::Tool {
                            id,
                            content: body,
                            is_error,
                        });
                    }
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(Value::as_str) {
                            texts.push(scrub(t, redact));
                        }
                    }
                    _ => {}
                }
            }
            if let Some(joined) = join_opt(texts) {
                out.push(GenAiRecord::User { content: joined });
            }
        }
        _ => {}
    }
}

/// Flatten a message/tool-result `content` (a plain string, or an array of `text` blocks) into one
/// redacted string, or `None` when there is no text.
fn content_to_text(content: Option<&Value>, redact: bool) -> Option<String> {
    match content? {
        Value::String(s) if !s.trim().is_empty() => Some(scrub(s, redact)),
        Value::Array(blocks) => {
            let joined: Vec<String> = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .map(|t| scrub(t, redact))
                .collect();
            join_opt(joined)
        }
        _ => None,
    }
}

/// Join parts on newlines, `None` when there is nothing to join.
fn join_opt(parts: Vec<String>) -> Option<String> {
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// Redact a message body when the toggle is on, else pass it through. Unlike the span hint, message
/// bodies keep their newlines: [`redact_body`] scrubs each line in place rather than collapsing the
/// whole thing to one line.
fn scrub(s: &str, redact: bool) -> String {
    if redact {
        redact_body(s)
    } else {
        s.to_string()
    }
}

/// Scrub credential-shaped fragments from a multi-line body: run the per-line [`redact`] over each
/// line and rejoin on newlines, so a code block or multi-line tool output keeps its line structure
/// (intra-line whitespace still collapses, a display body, not a byte-exact replay).
fn redact_body(s: &str) -> String {
    s.lines().map(redact).collect::<Vec<_>>().join("\n")
}

/// Fold one transcript line's `tool_use`/`tool_result` blocks into the accumulator.
fn ingest_line(line: &str, calls: &mut Calls) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let ts = v
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_ts);
    let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) else {
        return;
    };
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") => ingest_use(block, ts, calls),
            Some("tool_result") => ingest_result(block, ts, calls),
            _ => {}
        }
    }
}

/// A `tool_use` block opens a new invocation (skipped when its line has no parseable timestamp,
/// without a start there is nothing to graft).
fn ingest_use(block: &Value, ts: Option<SystemTime>, calls: &mut Calls) {
    let (Some(id), Some(start)) = (block.get("id").and_then(Value::as_str), ts) else {
        return;
    };
    let name = block
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string();
    let summary = summarize_input(&name, block.get("input"), calls.redact);
    calls.by_id.insert(id.to_string(), calls.order.len());
    calls.order.push(ToolInvocation {
        name,
        start,
        end: None,
        summary,
        error: false,
    });
}

/// A `tool_result` block closes its invocation. An unparseable result timestamp still CLOSES the
/// call (zero duration at start), only a genuinely missing result leaves `end = None`, so a
/// completed call is never mislabeled as cut short.
fn ingest_result(block: &Value, ts: Option<SystemTime>, calls: &mut Calls) {
    let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
        return;
    };
    let Some(call) = calls.by_id.get(id).and_then(|&i| calls.order.get_mut(i)) else {
        return;
    };
    call.end = Some(ts.unwrap_or(call.start));
    call.error = block
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
}

/// Which input field a tool's hint comes from, the closed set of arguments safe to surface (a
/// command, a path, a search pattern). Tools outside the set (every `mcp__*` included) get no hint,
/// so arbitrary argument payloads can't leak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HintField {
    Command,
    FilePath,
    Pattern,
}

impl HintField {
    /// The hint field for a tool name, or `None` for tools whose inputs are not surfaced.
    fn for_tool(name: &str) -> Option<HintField> {
        match name {
            // `terminal` is hermes's shell tool (claude's `Bash` equivalent).
            "Bash" | "BashOutput" | "terminal" => Some(HintField::Command),
            "Edit" | "MultiEdit" | "Write" | "Read" | "NotebookEdit" => Some(HintField::FilePath),
            "Glob" | "Grep" => Some(HintField::Pattern),
            _ => None,
        }
    }

    /// The JSON key this field reads from the tool's `input` object.
    fn key(self) -> &'static str {
        match self {
            HintField::Command => "command",
            HintField::FilePath => "file_path",
            HintField::Pattern => "pattern",
        }
    }
}

/// The sanitized one-line input hint for a tool: the [`HintField`] value, redacted (when `redact`)
/// then truncated (see [`redact`]). Empty when the tool has no hint field or the field is absent.
/// `pub(crate)` so the hermes reader synthesizes span hints through the same whitelist.
pub(crate) fn summarize_input(name: &str, input: Option<&Value>, redact: bool) -> String {
    let Some(field) = HintField::for_tool(name) else {
        return String::new();
    };
    let Some(raw) = input
        .and_then(|v| v.get(field.key()))
        .and_then(Value::as_str)
    else {
        return String::new();
    };
    let hint = raw.trim();
    let hint = if redact {
        self::redact(hint)
    } else {
        hint.to_string()
    };
    truncate(&hint, SUMMARY_CAP)
}

/// The key-name markers whose `NAME=value` assignments get their value scrubbed (case-insensitive
/// substring match on the trailing word before `=`).
const SECRET_MARKERS: [&str; 5] = ["TOKEN", "KEY", "SECRET", "PASSWORD", "PASSWD"];

/// Scrub credential-shaped fragments from an input hint, BEFORE truncation:
/// - `FOO_TOKEN=value` (any `\w` key word containing TOKEN/KEY/SECRET/PASSWORD/PASSWD) → `FOO_TOKEN=***`
/// - `Authorization: <value>` (case-insensitive) → `Authorization: ***`; a `Bearer`/`Basic` scheme
///   word masks the credential token after it too
/// - URL userinfo `scheme://user:pass@host` → `scheme://***@host`
///
/// Whitespace runs collapse to single spaces, this is a display hint, not a replayable command.
/// `pub(crate)` so the hermes reader scrubs its tool-result bodies through the same gate.
pub(crate) fn redact(s: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut mask_next = 0u8; // pending Authorization-value tokens to mask
    for token in s.split_whitespace() {
        if mask_next > 0 {
            mask_next -= 1;
            // A scheme word alone isn't the secret; the token after it is.
            if mask_next == 0 && ["bearer", "basic"].contains(&token.to_lowercase().as_str()) {
                out.push(token.to_string());
                mask_next = 1;
            } else {
                out.push("***".to_string());
            }
            continue;
        }
        // Match the header word behind a shell quote too (`-H "Authorization: …"`).
        let lower = token.to_lowercase();
        let bare = lower.trim_start_matches(['"', '\'']);
        if let Some(rest) = bare.strip_prefix("authorization:") {
            if rest.is_empty() {
                // `Authorization:` then whitespace: the value is the next token.
                out.push(token.to_string());
                mask_next = 1;
            } else {
                // `Authorization:value` glued together: mask the glued value.
                out.push(format!("{}: ***", &token[..token.len() - rest.len() - 1]));
            }
            continue;
        }
        out.push(redact_assignment(&redact_url_userinfo(token)));
    }
    out.join(" ")
}

/// Scrub the value of a `KEY=value` token when the `\w` word ending at `=` contains a secret
/// marker (`--api-token=x` → `--api-token=***`). Tokens without `=`, or with a non-secret key,
/// pass through.
fn redact_assignment(token: &str) -> String {
    let Some(eq) = token.find('=') else {
        return token.to_string();
    };
    if eq + 1 >= token.len() {
        return token.to_string(); // no value to scrub
    }
    // The word: the trailing run of word chars immediately before `=`.
    let key = &token[..eq];
    let word: String = key
        .chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let upper = word.to_uppercase();
    if !word.is_empty() && SECRET_MARKERS.iter().any(|m| upper.contains(m)) {
        format!("{key}=***")
    } else {
        token.to_string()
    }
}

/// Scrub URL userinfo inside a token: `scheme://user:pass@host/x` → `scheme://***@host/x`,
/// parsed by the `url` crate rather than hand-rolled authority slicing. A token that doesn't
/// parse as a URL (or carries no userinfo) passes through unchanged.
fn redact_url_userinfo(token: &str) -> String {
    if !token.contains("://") {
        return token.to_string();
    }
    let Ok(mut url) = url::Url::parse(token) else {
        return token.to_string();
    };
    if url.username().is_empty() && url.password().is_none() {
        return token.to_string();
    }
    if url.set_username("***").is_err() || url.set_password(None).is_err() {
        return token.to_string();
    }
    url.to_string()
}

/// Truncate to at most `cap` chars, appending an ellipsis when clipped. `char_indices` keeps the
/// cut on a char boundary (std's `String::truncate` is byte-indexed and panics off-boundary).
/// `pub(crate)` so the hermes reader caps its span hints identically.
pub(crate) fn truncate(s: &str, cap: usize) -> String {
    match s.char_indices().nth(cap) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Parse an RFC3339 transcript timestamp into a `SystemTime` via jiff, `None` when unparseable.
fn parse_ts(s: &str) -> Option<SystemTime> {
    s.parse::<Timestamp>().ok().map(SystemTime::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ts: &str) -> SystemTime {
        parse_ts(ts).expect("test timestamp parses")
    }

    /// A tool_use paired with its tool_result becomes one timed, non-errored span with the
    /// sanitized command hint.
    #[test]
    fn pairs_use_and_result_into_a_timed_span() {
        let jsonl = r#"
{"type":"assistant","timestamp":"2026-07-13T10:00:00Z","message":{"content":[{"type":"tool_use","id":"a","name":"Bash","input":{"command":"cargo test --lib"}}]}}
{"type":"user","timestamp":"2026-07-13T10:00:02Z","message":{"content":[{"type":"tool_result","tool_use_id":"a","is_error":false}]}}
"#;
        let calls = parse_transcript(jsonl);
        assert_eq!(calls.len(), 1);
        let c = &calls[0];
        assert_eq!(c.name, "Bash");
        assert_eq!(c.summary, "cargo test --lib");
        assert!(!c.error);
        assert_eq!(c.start, at("2026-07-13T10:00:00Z"));
        assert_eq!(c.end, Some(at("2026-07-13T10:00:02Z")));
    }

    /// Parallel calls: two tool_use blocks (in one assistant turn) before their results, resolved
    /// by tool_use_id, keeping first-seen order.
    #[test]
    fn resolves_parallel_calls_by_id() {
        let jsonl = concat!(
            r#"{"type":"assistant","timestamp":"2026-07-13T10:00:00Z","message":{"content":[{"type":"tool_use","id":"a","name":"Read","input":{"file_path":"/src/a.rs"}},{"type":"tool_use","id":"b","name":"Read","input":{"file_path":"/src/b.rs"}}]}}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-07-13T10:00:01Z","message":{"content":[{"type":"tool_result","tool_use_id":"b","is_error":true}]}}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-07-13T10:00:03Z","message":{"content":[{"type":"tool_result","tool_use_id":"a"}]}}"#,
        );
        let calls = parse_transcript(jsonl);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].summary, "/src/a.rs");
        assert_eq!(calls[0].end, Some(at("2026-07-13T10:00:03Z")));
        assert!(!calls[0].error);
        // The second call errored; is_error rides through.
        assert_eq!(calls[1].summary, "/src/b.rs");
        assert!(calls[1].error);
    }

    /// A tool_use with no matching tool_result (the turn ended first) has no end; the engine closes
    /// it at turn end and marks it errored.
    #[test]
    fn missing_result_leaves_end_none() {
        let jsonl = r#"{"type":"assistant","timestamp":"2026-07-13T10:00:00Z","message":{"content":[{"type":"tool_use","id":"a","name":"Bash","input":{"command":"sleep 999"}}]}}"#;
        let calls = parse_transcript(jsonl);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].end, None);
        assert!(!calls[0].error);
    }

    /// A result whose OWN line has an unparseable timestamp still closes the call (zero duration
    /// at start) and keeps is_error, never mislabeled as cut short.
    #[test]
    fn result_with_unparseable_timestamp_still_closes_the_call() {
        let jsonl = concat!(
            r#"{"type":"assistant","timestamp":"2026-07-13T10:00:00Z","message":{"content":[{"type":"tool_use","id":"a","name":"Bash","input":{"command":"true"}}]}}"#,
            "\n",
            r#"{"type":"user","timestamp":"not-a-time","message":{"content":[{"type":"tool_result","tool_use_id":"a","is_error":false}]}}"#,
        );
        let calls = parse_transcript(jsonl);
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].end,
            Some(at("2026-07-13T10:00:00Z")),
            "closed at start, not left open"
        );
        assert!(!calls[0].error);
    }

    /// Malformed and irrelevant lines are skipped without derailing the surrounding calls.
    #[test]
    fn skips_malformed_and_irrelevant_lines() {
        let jsonl = concat!(
            "not json at all\n",
            r#"{"type":"assistant","timestamp":"2026-07-13T10:00:00Z"}"#,
            "\n",
            r#"{"type":"summary","summary":"a recap line"}"#,
            "\n",
            "{\"torn\": \n",
            r#"{"type":"assistant","timestamp":"2026-07-13T10:00:00Z","message":{"content":[{"type":"tool_use","id":"a","name":"Write","input":{"file_path":"/x"}}]}}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-07-13T10:00:01Z","message":{"content":[{"type":"tool_result","tool_use_id":"a","is_error":false}]}}"#,
        );
        let calls = parse_transcript(jsonl);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "Write");
        assert_eq!(calls[0].summary, "/x");
    }

    /// A long Bash command is truncated to the cap with an ellipsis; nothing longer leaks.
    #[test]
    fn long_command_is_truncated() {
        let long = "echo ".to_string() + &"x".repeat(500);
        let jsonl = format!(
            r#"{{"type":"assistant","timestamp":"2026-07-13T10:00:00Z","message":{{"content":[{{"type":"tool_use","id":"a","name":"Bash","input":{{"command":{}}}}}]}}}}"#,
            serde_json::to_string(&long).unwrap()
        );
        let calls = parse_transcript(&jsonl);
        assert_eq!(calls.len(), 1);
        let s = &calls[0].summary;
        assert!(s.ends_with('…'));
        assert_eq!(
            s.chars().count(),
            SUMMARY_CAP + 1,
            "cap chars plus the ellipsis"
        );
    }

    /// An mcp tool (and any unknown tool) carries the span name but never an input hint, so
    /// argument payloads can't leak through the summary.
    #[test]
    fn unknown_and_mcp_tools_get_no_input_hint() {
        let jsonl = concat!(
            r#"{"type":"assistant","timestamp":"2026-07-13T10:00:00Z","message":{"content":[{"type":"tool_use","id":"a","name":"mcp__epp-broker__request_trace","input":{"secret":"do-not-leak","payload":"private"}}]}}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-07-13T10:00:01Z","message":{"content":[{"type":"tool_result","tool_use_id":"a","is_error":false}]}}"#,
        );
        let calls = parse_transcript(jsonl);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "mcp__epp-broker__request_trace");
        assert_eq!(
            calls[0].summary, "",
            "no input echoed for mcp/unknown tools"
        );
    }

    /// The whitelisted file tools surface only the path, never the written contents.
    #[test]
    fn write_summary_is_path_only_never_contents() {
        let jsonl = r#"{"type":"assistant","timestamp":"2026-07-13T10:00:00Z","message":{"content":[{"type":"tool_use","id":"a","name":"Write","input":{"file_path":"/etc/app.conf","content":"TOP SECRET CONTENTS"}}]}}"#;
        let calls = parse_transcript(jsonl);
        assert_eq!(calls[0].summary, "/etc/app.conf");
        assert!(!calls[0].summary.contains("SECRET"));
    }

    #[test]
    fn empty_transcript_yields_no_calls() {
        assert!(parse_transcript("").is_empty());
        assert!(parse_transcript("\n  \n").is_empty());
    }

    // --- redaction -------------------------------------------------------------------------

    /// Env-prefixed secret assignments are scrubbed, key kept; a truncated prefix carries no value.
    #[test]
    fn redact_scrubs_secret_assignments() {
        assert_eq!(
            redact("FOO_TOKEN=abc123 ./deploy.sh"),
            "FOO_TOKEN=*** ./deploy.sh"
        );
        assert_eq!(
            redact("env api_key=hunter2 AWS_SECRET_ACCESS_KEY=zzz aws s3 ls"),
            "env api_key=*** AWS_SECRET_ACCESS_KEY=*** aws s3 ls"
        );
        // The marker word can sit behind non-word chars (a long flag) and still scrub.
        assert_eq!(
            redact("curl --api-token=abcd http://x"),
            "curl --api-token=*** http://x"
        );
        // Case-insensitive.
        assert_eq!(redact("Password=nope run"), "Password=*** run");
        // Non-secret assignments pass through untouched.
        assert_eq!(
            redact("RUST_LOG=debug cargo test"),
            "RUST_LOG=debug cargo test"
        );
    }

    /// Authorization headers are masked: the glued form, the spaced form, and the value after a
    /// Bearer/Basic scheme word.
    #[test]
    fn redact_masks_authorization_values() {
        assert_eq!(
            redact("curl -H Authorization:sekret http://x"),
            "curl -H Authorization: *** http://x"
        );
        // The shell-quoted curl form: the quote glues to the header word, the credential after the
        // Bearer scheme word is still masked.
        assert_eq!(
            redact(r#"curl -H "Authorization: Bearer eyJabc" http://x"#),
            r#"curl -H "Authorization: Bearer *** http://x"#
        );
        assert_eq!(
            redact("curl -H Authorization: Bearer eyJabc http://x"),
            "curl -H Authorization: Bearer *** http://x"
        );
        assert_eq!(
            redact("wget --header authorization: tok8s"),
            "wget --header authorization: ***"
        );
    }

    /// URL userinfo is scrubbed; host/path survive.
    #[test]
    fn redact_scrubs_url_userinfo() {
        assert_eq!(
            redact("git clone https://user:pass@github.com/o/r.git"),
            "git clone https://***@github.com/o/r.git"
        );
        // No userinfo: untouched (an @ later in the path is not authority userinfo).
        assert_eq!(
            redact("curl https://example.com/a@b"),
            "curl https://example.com/a@b"
        );
    }

    /// Redaction runs BEFORE truncation: a secret sitting inside the first 200 chars is scrubbed,
    /// not carried by the prefix.
    #[test]
    fn summary_redacts_before_truncating() {
        let cmd = format!("MY_TOKEN=supersecret ./run.sh {}", "x".repeat(400));
        let jsonl = format!(
            r#"{{"type":"assistant","timestamp":"2026-07-13T10:00:00Z","message":{{"content":[{{"type":"tool_use","id":"a","name":"Bash","input":{{"command":{}}}}}]}}}}"#,
            serde_json::to_string(&cmd).unwrap()
        );
        let calls = parse_transcript(&jsonl);
        let s = &calls[0].summary;
        assert!(s.starts_with("MY_TOKEN=*** ./run.sh"), "scrubbed: {s}");
        assert!(!s.contains("supersecret"));
        assert!(s.ends_with('…'), "still truncated");
    }

    /// With redaction toggled off, the hint passes through verbatim (the harness is trusted to have
    /// sanitized it), still whitelisted-field-only and truncated, just not scrubbed.
    #[test]
    fn redaction_disabled_passes_hint_through() {
        let jsonl = r#"{"type":"assistant","timestamp":"2026-07-13T10:00:00Z","message":{"content":[{"type":"tool_use","id":"a","name":"Bash","input":{"command":"FOO_TOKEN=abc123 ./deploy.sh"}}]}}"#;
        let calls = parse_transcript_with(jsonl, false);
        assert_eq!(calls[0].summary, "FOO_TOKEN=abc123 ./deploy.sh");
    }

    // --- full-conversation (gen_ai) message parse ------------------------------------------------

    /// A user prompt, an assistant turn mixing thinking + text + a tool_use, and the tool_result map
    /// to the four gen_ai record kinds, in order, carrying the FULL bodies (not the one-line hint).
    #[test]
    fn parse_messages_maps_the_four_record_kinds() {
        let jsonl = concat!(
            r#"{"type":"user","timestamp":"2026-07-13T10:00:00Z","message":{"role":"user","content":"please build the project"}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-07-13T10:00:01Z","message":{"role":"assistant","model":"claude-opus-4-8","content":[{"type":"thinking","thinking":"I should run the build"},{"type":"text","text":"Running the build now"},{"type":"tool_use","id":"a","name":"Write","input":{"file_path":"/x","content":"SECRET CODE"}}]}}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-07-13T10:00:02Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"a","is_error":false,"content":[{"type":"text","text":"wrote 2 lines"}]}]}}"#,
        );
        let recs = parse_messages(jsonl);
        assert_eq!(recs.len(), 3);
        assert_eq!(
            recs[0],
            GenAiRecord::User {
                content: "please build the project".into()
            }
        );
        match &recs[1] {
            GenAiRecord::Assistant {
                text,
                reasoning,
                tool_calls,
                model,
            } => {
                assert_eq!(text.as_deref(), Some("Running the build now"));
                assert_eq!(reasoning.as_deref(), Some("I should run the build"));
                assert_eq!(model.as_deref(), Some("claude-opus-4-8"));
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].name, "Write");
                // The FULL input args ride here, including the content the span hint strips.
                assert!(tool_calls[0].arguments.contains("SECRET CODE"));
                assert!(tool_calls[0].arguments.contains("/x"));
            }
            other => panic!("expected assistant, got {other:?}"),
        }
        assert_eq!(
            recs[2],
            GenAiRecord::Tool {
                id: "a".into(),
                content: "wrote 2 lines".into(),
                is_error: false,
            }
        );
    }

    /// An errored tool_result carries `is_error` through, and a string tool_result body is read.
    #[test]
    fn parse_messages_reads_errored_and_string_tool_results() {
        let jsonl = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"z","is_error":true,"content":"command not found"}]}}"#;
        let recs = parse_messages(jsonl);
        assert_eq!(
            recs,
            vec![GenAiRecord::Tool {
                id: "z".into(),
                content: "command not found".into(),
                is_error: true,
            }]
        );
    }

    /// Redaction scrubs credential-shaped fragments from message BODIES (assistant text, tool args,
    /// user content), while newlines survive (per-line scrub, not a one-line collapse).
    #[test]
    fn parse_messages_redacts_bodies_keeping_newlines() {
        let jsonl = concat!(
            r#"{"type":"user","message":{"role":"user","content":"run with API_TOKEN=hunter2 now"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"line one\nAWS_SECRET_ACCESS_KEY=zzz\nline three"},{"type":"tool_use","id":"a","name":"Bash","input":{"command":"curl -H Authorization:sekret http://x"}}]}}"#,
        );
        let recs = parse_messages(jsonl);
        assert_eq!(
            recs[0],
            GenAiRecord::User {
                content: "run with API_TOKEN=*** now".into()
            }
        );
        match &recs[1] {
            GenAiRecord::Assistant {
                text, tool_calls, ..
            } => {
                let t = text.as_deref().unwrap();
                assert!(t.contains("line one"), "text body preserved: {t}");
                assert!(
                    t.contains("AWS_SECRET_ACCESS_KEY=***"),
                    "secret scrubbed: {t}"
                );
                assert!(t.contains("\nline three"), "newlines survive: {t:?}");
                assert!(!t.contains("zzz"));
                assert!(
                    !tool_calls[0].arguments.contains("sekret"),
                    "tool args scrubbed: {}",
                    tool_calls[0].arguments
                );
            }
            other => panic!("expected assistant, got {other:?}"),
        }
    }

    /// With redaction off, bodies pass through verbatim (the store is trusted / access-controlled).
    #[test]
    fn parse_messages_disabled_passes_bodies_through() {
        let jsonl = r#"{"type":"user","message":{"role":"user","content":"API_TOKEN=hunter2"}}"#;
        let recs = parse_messages_with(jsonl, false);
        assert_eq!(
            recs,
            vec![GenAiRecord::User {
                content: "API_TOKEN=hunter2".into()
            }]
        );
    }

    /// Torn/irrelevant lines are skipped; an assistant with only tool_use (no text) still parses.
    #[test]
    fn parse_messages_skips_noise_and_handles_tool_only_assistant() {
        let jsonl = concat!(
            "not json\n",
            r#"{"type":"summary","summary":"recap"}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"a","name":"Read","input":{"file_path":"/a"}}]}}"#,
        );
        let recs = parse_messages(jsonl);
        assert_eq!(recs.len(), 1);
        match &recs[0] {
            GenAiRecord::Assistant {
                text, tool_calls, ..
            } => {
                assert!(text.is_none());
                assert_eq!(tool_calls[0].name, "Read");
            }
            other => panic!("expected assistant, got {other:?}"),
        }
    }

    #[test]
    fn parse_messages_empty_transcript_is_empty() {
        assert!(parse_messages("").is_empty());
        assert!(parse_messages("\n \n").is_empty());
    }
}
