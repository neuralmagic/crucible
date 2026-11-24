//! Parse Claude Code's `--output-format stream-json` NDJSON into [`AgentEvent`]s.
//!
//! The parser is stateful: token totals accrue across messages, a `tool_use` block's JSON input
//! arrives in `input_json_delta` chunks and is only complete at `content_block_stop`, and
//! text/thinking deltas buffer to line boundaries so one event is one line. Lines with an
//! unmodeled top-level `type`, and non-JSON lines, pass through as [`AgentEvent::Raw`] so a
//! viewer never loses output to schema drift, and so a startup failure claude prints as bare
//! text reaches the turn's log instead of being dropped.

use crate::line::{LineFormat, parse_line, raw};
use crate::otel::{CostHandle, LiveMeters, RateHandle};
use crucible_contract::event::{AgentEvent, Tokens};
use serde_json::Value;
use std::collections::HashMap;

/// Emit a [`Tokens`] sample once the running total has grown by at least this much
/// (or on the first sample).
const TOKEN_EMIT_STEP: u64 = 5_000;

/// Byte bound on each verbose tool input / result excerpt, so one giant Write or
/// Read cannot balloon the session log.
pub(crate) const TOOL_IO_LIMIT: usize = 2_048;

/// Char bound on the excerpt a failed tool result leaves in the compact log.
const TOOL_ERROR_LIMIT: usize = 300;

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
    // Cumulative token counts: input/cache from `message_start`, output from `message_delta`,
    // and input/cache again from `message_delta` when it carries them.
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

    // Live token rate and running cost from the OTLP collector; `None` when no collector is
    // running.
    meters: Option<LiveMeters>,
    // The stream's own cumulative cost, which only arrives with the turn-end `result` message.
    // Kept so a live cost sample can never report LESS than the agent has already declared.
    stream_cost: f64,

    // Verbose tool IO (on unless CRUCIBLE_SESSION_TOOL_IO=compact): tool events carry bounded
    // inputs, and every tool result claude echoes back as a `user` message becomes a
    // result-excerpt event. Off, only failed results become events.
    tool_io: bool,
    // The open `tool_use` block's id, and id -> name for labeling result events.
    tool_id: Option<String>,
    tool_names: HashMap<String, String>,
}

impl StreamJsonParser {
    /// A parser that stamps each `tokens` sample with the collector's live meters: the 60 s-window
    /// token rate and the turn's cost so far.
    pub fn with_meters(meters: LiveMeters) -> Self {
        StreamJsonParser {
            meters: Some(meters),
            ..Default::default()
        }
    }

    /// Opt into verbose tool IO: tool events carry bounded inputs and result excerpts.
    pub fn with_tool_io(mut self, on: bool) -> Self {
        self.tool_io = on;
        self
    }
}

/// The borrowed wire model of claude's `stream-json`: one streaming deserialize per line, no
/// `Value` tree. Protocol constants stay borrowed `&str` — an escaped one fails the line, which
/// routes it to the `Value` walk, never a wrong decode. Free-form text (deltas, results, error
/// bodies) is `Cow`, so escapes cost an allocation instead of the fallback.
mod wire {
    use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
    use serde::{Deserialize, Deserializer};
    use std::borrow::Cow;
    use std::collections::BTreeMap;
    use std::fmt;

    #[derive(Deserialize)]
    pub(super) struct Line<'a> {
        #[serde(rename = "type", default)]
        pub kind: Option<&'a str>,
        #[serde(default)]
        pub subtype: Option<&'a str>,
        #[serde(default)]
        pub model: Option<&'a str>,
        #[serde(default)]
        pub tools: Vec<IgnoredAny>,
        #[serde(default)]
        pub agents: Vec<IgnoredAny>,
        #[serde(default)]
        pub attempt: Option<u64>,
        #[serde(rename = "max_retries", default)]
        pub max_retries: Option<u64>,
        #[serde(default)]
        pub error: RetryError<'a>,
        #[serde(default)]
        pub message: Message<'a>,
        #[serde(default)]
        pub status: Option<&'a str>,
        #[serde(default)]
        pub reason: Option<&'a str>,
        #[serde(default)]
        pub is_error: Option<bool>,
        #[serde(default)]
        pub result: Option<Cow<'a, str>>,
        #[serde(rename = "num_turns", default)]
        pub num_turns: Option<u64>,
        #[serde(rename = "total_cost_usd", default)]
        pub total_cost_usd: Option<f64>,
        #[serde(rename = "modelUsage", default)]
        pub model_usage: Option<BTreeMap<&'a str, ModelUsage>>,
        #[serde(default)]
        pub event: Option<Event<'a>>,
    }

    /// The `error` field, which `api_retry` writes as a string or an object and other lines
    /// write as a plain string.
    #[derive(Default)]
    pub(super) enum RetryError<'a> {
        Str(Cow<'a, str>),
        Obj {
            message: Option<Cow<'a, str>>,
            kind: Option<Cow<'a, str>>,
        },
        #[default]
        Absent,
    }

    impl RetryError<'_> {
        /// What `retry_error`'s direct arm reads: the string, or the object's message else its
        /// type; empty for anything else, as `str_field` reads the Value walk.
        pub fn retry_direct(&self) -> &str {
            match self {
                RetryError::Str(s) => s,
                RetryError::Obj { message, kind } => {
                    let message = message.as_deref().unwrap_or("");
                    if message.is_empty() {
                        kind.as_deref().unwrap_or("")
                    } else {
                        message
                    }
                }
                RetryError::Absent => "",
            }
        }

        /// What `str_field(msg, "error")` reads: the string spelling only.
        pub fn as_plain_str(&self) -> &str {
            match self {
                RetryError::Str(s) => s,
                _ => "",
            }
        }
    }

    impl<'de: 'a, 'a> Deserialize<'de> for RetryError<'a> {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            struct V<'a>(std::marker::PhantomData<&'a ()>);
            impl<'de: 'a, 'a> Visitor<'de> for V<'a> {
                type Value = RetryError<'a>;
                fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                    f.write_str("a string, an object, or nothing")
                }
                fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E> {
                    Ok(RetryError::Str(Cow::Borrowed(v)))
                }
                fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                    Ok(RetryError::Str(Cow::Owned(v.to_string())))
                }
                fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
                where
                    A: MapAccess<'de>,
                {
                    let mut message = None;
                    let mut kind = None;
                    while let Some(key) = map.next_key::<&'de str>()? {
                        match key {
                            "message" => message = map.next_value()?,
                            "type" => kind = map.next_value()?,
                            _ => {
                                map.next_value::<IgnoredAny>()?;
                            }
                        }
                    }
                    Ok(RetryError::Obj { message, kind })
                }
                fn visit_none<E>(self) -> Result<Self::Value, E> {
                    Ok(RetryError::Absent)
                }
                fn visit_unit<E>(self) -> Result<Self::Value, E> {
                    Ok(RetryError::Absent)
                }
            }
            deserializer.deserialize_any(V(std::marker::PhantomData))
        }
    }

    /// The `message` field: a user line's message object (only its content array is read), or
    /// an `api_retry` fallback string.
    #[derive(Default)]
    pub(super) enum Message<'a> {
        User {
            content: Vec<ToolResultBlock<'a>>,
        },
        Str(Cow<'a, str>),
        #[default]
        Absent,
    }

    impl Message<'_> {
        /// What the `api_retry` fallback loop reads: the string spelling only.
        pub fn as_str(&self) -> &str {
            match self {
                Message::Str(s) => s,
                _ => "",
            }
        }

        /// The user message's content blocks; `None` for every other shape, as the Value walk's
        /// `as_array` gate reads it.
        pub fn user_content(&self) -> Option<&[ToolResultBlock<'_>]> {
            match self {
                Message::User { content } => Some(content),
                _ => None,
            }
        }
    }

    impl<'de: 'a, 'a> Deserialize<'de> for Message<'a> {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            struct V<'a>(std::marker::PhantomData<&'a ()>);
            impl<'de: 'a, 'a> Visitor<'de> for V<'a> {
                type Value = Message<'a>;
                fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                    f.write_str("a message object, a string, or nothing")
                }
                fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E> {
                    Ok(Message::Str(Cow::Borrowed(v)))
                }
                fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                    Ok(Message::Str(Cow::Owned(v.to_string())))
                }
                fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
                where
                    A: MapAccess<'de>,
                {
                    let mut content = Vec::new();
                    while let Some(key) = map.next_key::<&'de str>()? {
                        if key == "content" {
                            content = map.next_value()?;
                        } else {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                    Ok(Message::User { content })
                }
                fn visit_none<E>(self) -> Result<Self::Value, E> {
                    Ok(Message::Absent)
                }
                fn visit_unit<E>(self) -> Result<Self::Value, E> {
                    Ok(Message::Absent)
                }
            }
            deserializer.deserialize_any(V(std::marker::PhantomData))
        }
    }

    #[derive(Deserialize)]
    pub(super) struct Event<'a> {
        #[serde(rename = "type", default)]
        pub kind: Option<&'a str>,
        #[serde(default)]
        pub content_block: Option<ContentBlock<'a>>,
        #[serde(default)]
        pub delta: Option<Delta<'a>>,
        #[serde(default)]
        pub message: Option<StartMessage>,
        #[serde(default)]
        pub usage: Option<Usage>,
        #[serde(default)]
        pub error: Option<StreamError<'a>>,
    }

    #[derive(Deserialize)]
    pub(super) struct ContentBlock<'a> {
        #[serde(rename = "type", default)]
        pub kind: Option<&'a str>,
        #[serde(default)]
        pub name: Option<&'a str>,
        #[serde(default)]
        pub id: Option<&'a str>,
    }

    #[derive(Deserialize)]
    pub(super) struct Delta<'a> {
        #[serde(rename = "type", default)]
        pub kind: Option<&'a str>,
        #[serde(default)]
        pub text: Option<Cow<'a, str>>,
        #[serde(default)]
        pub thinking: Option<Cow<'a, str>>,
        #[serde(default)]
        pub partial_json: Option<Cow<'a, str>>,
    }

    #[derive(Deserialize)]
    pub(super) struct StartMessage {
        #[serde(default)]
        pub usage: Option<Usage>,
    }

    #[derive(Deserialize, Default)]
    pub(super) struct Usage {
        #[serde(rename = "input_tokens", default)]
        pub input_tokens: Option<u64>,
        #[serde(rename = "cache_read_input_tokens", default)]
        pub cache_read: Option<u64>,
        #[serde(rename = "cache_creation_input_tokens", default)]
        pub cache_write: Option<u64>,
        #[serde(rename = "output_tokens", default)]
        pub output_tokens: Option<u64>,
    }

    #[derive(Deserialize)]
    pub(super) struct StreamError<'a> {
        #[serde(rename = "type", default)]
        pub kind: Option<Cow<'a, str>>,
        #[serde(default)]
        pub message: Option<Cow<'a, str>>,
    }

    /// A `modelUsage` entry, camelCased unlike the streamed usage samples.
    #[derive(Deserialize)]
    pub(super) struct ModelUsage {
        #[serde(rename = "inputTokens", default)]
        pub input: Option<u64>,
        #[serde(rename = "outputTokens", default)]
        pub output: Option<u64>,
        #[serde(rename = "cacheReadInputTokens", default)]
        pub cache_read: Option<u64>,
        #[serde(rename = "cacheCreationInputTokens", default)]
        pub cache_write: Option<u64>,
    }

    #[derive(Deserialize)]
    pub(super) struct ToolResultBlock<'a> {
        #[serde(rename = "type", default)]
        pub kind: Option<&'a str>,
        #[serde(rename = "tool_use_id", default)]
        pub tool_use_id: Option<&'a str>,
        #[serde(rename = "is_error", default)]
        pub is_error: Option<bool>,
        #[serde(default)]
        pub content: ResultContent<'a>,
    }

    /// A tool result's `content`: a plain string, or an array of text blocks.
    #[derive(Default)]
    pub(super) enum ResultContent<'a> {
        Str(Cow<'a, str>),
        Blocks(Vec<TextBlock<'a>>),
        #[default]
        Absent,
    }

    impl ResultContent<'_> {
        /// What `result_text` reads: the string, or the text blocks joined on newlines.
        pub fn text_joined(&self) -> String {
            match self {
                ResultContent::Str(s) => s.to_string(),
                ResultContent::Blocks(blocks) => blocks
                    .iter()
                    .filter(|b| b.kind == Some("text"))
                    .map(|b| b.text.as_deref().unwrap_or(""))
                    .collect::<Vec<_>>()
                    .join(
                        "
",
                    ),
                ResultContent::Absent => String::new(),
            }
        }
    }

    impl<'de: 'a, 'a> Deserialize<'de> for ResultContent<'a> {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            struct V<'a>(std::marker::PhantomData<&'a ()>);
            impl<'de: 'a, 'a> Visitor<'de> for V<'a> {
                type Value = ResultContent<'a>;
                fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                    f.write_str("a string, an array of blocks, or nothing")
                }
                fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E> {
                    Ok(ResultContent::Str(Cow::Borrowed(v)))
                }
                fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                    Ok(ResultContent::Str(Cow::Owned(v.to_string())))
                }
                fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
                where
                    A: SeqAccess<'de>,
                {
                    let mut blocks = Vec::new();
                    while let Some(block) = seq.next_element()? {
                        blocks.push(block);
                    }
                    Ok(ResultContent::Blocks(blocks))
                }
                fn visit_none<E>(self) -> Result<Self::Value, E> {
                    Ok(ResultContent::Absent)
                }
                fn visit_unit<E>(self) -> Result<Self::Value, E> {
                    Ok(ResultContent::Absent)
                }
            }
            deserializer.deserialize_any(V(std::marker::PhantomData))
        }
    }

    #[derive(Deserialize)]
    pub(super) struct TextBlock<'a> {
        #[serde(rename = "type", default)]
        pub kind: Option<&'a str>,
        #[serde(default)]
        pub text: Option<Cow<'a, str>>,
    }
}

impl LineFormat for StreamJsonParser {
    /// The typed fast path: parse straight into the borrowed [`wire`] model, one pass with no
    /// per-line `Value` tree. Any line the model cannot mirror event-for-event — schema drift,
    /// wrong-typed fields, the retry fallback that re-renders the whole line — takes the
    /// [`Value`] walk below, so behavior is the fallback's, never a divergence.
    fn push(&mut self, line: &str) -> Vec<AgentEvent> {
        let text = line.trim();
        if text.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Ok(parsed) = serde_json::from_str::<wire::Line<'_>>(text)
            && self.typed_line(parsed, text, &mut out)
        {
            return out;
        }
        self.push_parsed(&parse_line(line))
    }

    fn line(&mut self, msg: &Value, text: &str, out: &mut Vec<AgentEvent>) {
        match msg.get("type").and_then(Value::as_str) {
            Some("system") => self.system(msg, out),
            Some("result") => {
                self.end_block(out);
                let is_error = bool_field(msg, "is_error");
                // Only capture the CLI's text on the error path (`result`, or `error` when the
                // CLI splits them out); a clean turn's final message would fatten every line.
                let error = is_error
                    .then(|| {
                        let r = str_field(msg, "result");
                        if r.is_empty() {
                            str_field(msg, "error")
                        } else {
                            r
                        }
                    })
                    .filter(|s| !s.is_empty());
                let cost_usd = f64_field(msg, "total_cost_usd");
                self.stream_cost = self.stream_cost.max(cost_usd);
                out.extend(turn_tokens(msg, cost_usd));
                out.push(AgentEvent::Result {
                    subtype: str_field(msg, "subtype"),
                    is_error,
                    turns: u64_field(msg, "num_turns") as u32,
                    cost_usd,
                    error,
                });
            }
            Some("stream_event") => self.stream_event(msg, out),
            // `user` echoes carry the tool results.
            Some("user") => self.tool_results(msg, out),
            // `assistant` echoes duplicate the text already streamed via `stream_event`, and
            // `rate_limit_event` carries nothing the turn accounts for.
            Some("assistant") | Some("rate_limit_event") => {}
            _ => out.push(raw(text)),
        }
    }
}

impl StreamJsonParser {
    /// The typed mirror of [`LineFormat::line`]. `false` means "this line needs the Value
    /// walk" — the one case being `api_retry`'s whole-line fallback, which re-renders the
    /// parsed object as text and only the Value walk holds it.
    fn typed_line(&mut self, line: wire::Line<'_>, text: &str, out: &mut Vec<AgentEvent>) -> bool {
        match line.kind {
            Some("system") => self.typed_system(line, out),
            Some("result") => {
                self.end_block(out);
                let is_error = line.is_error.unwrap_or(false);
                let error = is_error
                    .then(|| {
                        let declared = line.result.as_deref().unwrap_or("");
                        if declared.is_empty() {
                            line.error.as_plain_str()
                        } else {
                            declared
                        }
                    })
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                let cost_usd = line.total_cost_usd.unwrap_or(0.0);
                self.stream_cost = self.stream_cost.max(cost_usd);
                out.extend(self.typed_turn_tokens(&line.model_usage, cost_usd));
                out.push(AgentEvent::Result {
                    subtype: line.subtype.unwrap_or("").to_string(),
                    is_error,
                    turns: line.num_turns.unwrap_or(0) as u32,
                    cost_usd,
                    error,
                });
                true
            }
            Some("stream_event") => {
                if let Some(event) = line.event {
                    self.typed_stream_event(event, out);
                }
                true
            }
            Some("user") => {
                self.typed_tool_results(line.message, out);
                true
            }
            Some("assistant") | Some("rate_limit_event") => true,
            _ => {
                out.push(raw(text));
                true
            }
        }
    }

    fn typed_system(&mut self, line: wire::Line<'_>, out: &mut Vec<AgentEvent>) -> bool {
        match line.subtype {
            Some("init") => out.push(AgentEvent::Init {
                model: line.model.unwrap_or("").to_string(),
                tools: line.tools.len() as u32,
                agents: line.agents.len() as u32,
            }),
            Some("api_retry") => {
                let direct = line.error.retry_direct();
                let mut error = None;
                if !direct.is_empty() && direct != "unknown" {
                    error = Some(truncate_chars(direct, TOOL_ERROR_LIMIT));
                } else {
                    for candidate in [Some(line.message.as_str()), line.status, line.reason] {
                        if let Some(found) = candidate.filter(|s| !s.is_empty()) {
                            error = Some(truncate_chars(found, TOOL_ERROR_LIMIT));
                            break;
                        }
                    }
                }
                let Some(error) = error else {
                    return false;
                };
                out.push(AgentEvent::Retry {
                    attempt: line.attempt.unwrap_or(0) as u32,
                    max: line.max_retries.unwrap_or(0) as u32,
                    error,
                });
            }
            _ => {}
        }
        true
    }

    fn typed_stream_event(&mut self, event: wire::Event<'_>, out: &mut Vec<AgentEvent>) {
        match event.kind {
            Some("content_block_start") => match event.content_block {
                Some(wire::ContentBlock {
                    kind: Some("text"), ..
                }) => self.block = Some(TextKind::Text),
                Some(wire::ContentBlock {
                    kind: Some("thinking"),
                    ..
                }) => self.block = Some(TextKind::Thinking),
                Some(wire::ContentBlock {
                    kind: Some("tool_use" | "server_tool_use"),
                    name,
                    id,
                }) => {
                    self.tool_name = Some(name.unwrap_or("").to_string());
                    self.tool_json.clear();
                    let id = id.unwrap_or("");
                    self.tool_id = (!id.is_empty()).then(|| id.to_string());
                }
                _ => {}
            },
            Some("content_block_delta") => {
                if let Some(delta) = event.delta {
                    match delta.kind {
                        Some("text_delta") => {
                            self.buffer(TextKind::Text, delta.text.as_deref().unwrap_or(""), out)
                        }
                        Some("thinking_delta") => self.buffer(
                            TextKind::Thinking,
                            delta.thinking.as_deref().unwrap_or(""),
                            out,
                        ),
                        Some("input_json_delta") => self
                            .tool_json
                            .push_str(delta.partial_json.as_deref().unwrap_or("")),
                        _ => {}
                    }
                }
            }
            Some("content_block_stop") => self.end_block(out),
            Some("message_start") => match event.message.and_then(|m| m.usage) {
                Some(usage) => {
                    self.input = usage.input_tokens.unwrap_or(0);
                    self.cache_read = usage.cache_read.unwrap_or(0);
                    self.cache_write = usage.cache_write.unwrap_or(0);
                }
                None => {
                    self.input = 0;
                    self.cache_read = 0;
                    self.cache_write = 0;
                }
            },
            Some("message_delta") => {
                if let Some(usage) = event.usage {
                    if let Some(input) = usage.input_tokens {
                        self.input = input;
                    }
                    if let Some(read) = usage.cache_read {
                        self.cache_read = read;
                    }
                    if let Some(write) = usage.cache_write {
                        self.cache_write = write;
                    }
                    let out_tokens = usage.output_tokens.unwrap_or(0);
                    if out_tokens > 0 {
                        self.output = out_tokens;
                        self.maybe_emit_tokens(out);
                    }
                }
            }
            Some("error") => {
                let (error_type, message) = match event.error {
                    Some(e) => (
                        e.kind.as_deref().unwrap_or("").to_string(),
                        e.message.as_deref().unwrap_or("").to_string(),
                    ),
                    None => (String::new(), String::new()),
                };
                out.push(AgentEvent::Error {
                    error_type,
                    message,
                });
            }
            _ => {}
        }
    }

    fn typed_tool_results(&mut self, message: wire::Message<'_>, out: &mut Vec<AgentEvent>) {
        let Some(blocks) = message.user_content() else {
            return;
        };
        for block in blocks {
            if block.kind != Some("tool_result") {
                continue;
            }
            let id = block.tool_use_id.unwrap_or("");
            let name = self
                .tool_names
                .remove(id)
                .unwrap_or_else(|| "tool".to_string());
            let failed = block.is_error.unwrap_or(false);
            let text = block.content.text_joined();
            if self.tool_io {
                out.push(AgentEvent::Tool {
                    name,
                    summary: if failed { "failed" } else { "result" }.to_string(),
                    subagent: false,
                    input: None,
                    result: Some(truncate_chars(&text, TOOL_IO_LIMIT)),
                    failed,
                });
            } else if failed {
                let reason = text.split_whitespace().collect::<Vec<_>>().join(" ");
                out.push(AgentEvent::Tool {
                    name,
                    summary: format!("failed: {}", truncate_chars(&reason, TOOL_ERROR_LIMIT)),
                    subagent: false,
                    input: None,
                    result: None,
                    failed: true,
                });
            }
        }
    }

    fn typed_turn_tokens(
        &self,
        models: &Option<std::collections::BTreeMap<&str, wire::ModelUsage>>,
        cost_usd: f64,
    ) -> Option<AgentEvent> {
        let models = models.as_ref()?;
        let mut tokens = Tokens::default();
        for usage in models.values() {
            tokens.input += usage.input.unwrap_or(0);
            tokens.output += usage.output.unwrap_or(0);
            tokens.cache_read += usage.cache_read.unwrap_or(0);
            tokens.cache_write += usage.cache_write.unwrap_or(0);
        }
        tokens.total = tokens.input + tokens.output + tokens.cache_read + tokens.cache_write;
        tokens.cost_usd = (cost_usd > 0.0).then_some(cost_usd);
        Some(AgentEvent::Tokens(tokens))
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
                error: retry_error(msg),
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
                        let id = str_field(block, "id");
                        self.tool_id = (!id.is_empty()).then_some(id);
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
                if let Some(input) = usage.get("input_tokens").and_then(Value::as_u64) {
                    self.input = input;
                }
                if let Some(read) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
                    self.cache_read = read;
                }
                if let Some(write) = usage
                    .get("cache_creation_input_tokens")
                    .and_then(Value::as_u64)
                {
                    self.cache_write = write;
                }
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
            if let Some(id) = self.tool_id.take() {
                self.tool_names.insert(id, name.clone());
            }
            let input = if self.tool_io {
                parsed.as_ref().map(bounded_input)
            } else {
                None
            };
            out.push(AgentEvent::Tool {
                name,
                summary,
                subagent,
                input,
                result: None,
                failed: false,
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

    /// Turn the `tool_result` blocks claude echoes back in a `user` message into Tool events
    /// labeled with the originating tool's name. Under verbose tool IO every result carries a
    /// bounded excerpt; without it only a failed result shows, as a one-line `failed:` summary.
    fn tool_results(&mut self, msg: &Value, out: &mut Vec<AgentEvent>) {
        let Some(content) = msg
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        else {
            return;
        };
        for block in content {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let id = str_field(block, "tool_use_id");
            let name = self
                .tool_names
                .remove(&id)
                .unwrap_or_else(|| "tool".to_string());
            let failed = bool_field(block, "is_error");
            let text = result_text(block);
            if self.tool_io {
                out.push(AgentEvent::Tool {
                    name,
                    summary: if failed { "failed" } else { "result" }.to_string(),
                    subagent: false,
                    input: None,
                    result: Some(truncate_chars(&text, TOOL_IO_LIMIT)),
                    failed,
                });
            } else if failed {
                let reason = text.split_whitespace().collect::<Vec<_>>().join(" ");
                out.push(AgentEvent::Tool {
                    name,
                    summary: format!("failed: {}", truncate_chars(&reason, TOOL_ERROR_LIMIT)),
                    subagent: false,
                    input: None,
                    result: None,
                    failed: true,
                });
            }
        }
    }

    /// The collector's running cost, when it beats what the stream has declared for itself. The
    /// agent exports its cost metric every 10 s while `total_cost_usd` lands only at turn end, so
    /// mid-turn this is the only honest number; once the stream reports its own (larger, final)
    /// figure, that one wins.
    fn live_cost(&self) -> Option<f64> {
        self.meters
            .as_ref()
            .map(|m| &m.cost)
            .and_then(CostHandle::get)
            .filter(|live| *live > self.stream_cost)
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
            rate: self
                .meters
                .as_ref()
                .map(|m| &m.rate)
                .and_then(RateHandle::get),
            cost_usd: self.live_cost(),
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

/// The underlying error of an `api_retry` line. The CLI often reports the literal
/// string "unknown" in `error` while the useful detail sits elsewhere (an error
/// object, or sibling fields), so dig before settling — and when nothing classifies,
/// carry the raw line so the log never says just "unknown".
fn retry_error(msg: &Value) -> String {
    let direct = match msg.get("error") {
        Some(Value::String(s)) => s.clone(),
        Some(obj @ Value::Object(_)) => {
            let m = str_field(obj, "message");
            if m.is_empty() {
                str_field(obj, "type")
            } else {
                m
            }
        }
        _ => String::new(),
    };
    if !direct.is_empty() && direct != "unknown" {
        return truncate_chars(&direct, 300);
    }
    for key in ["message", "status", "reason"] {
        let v = str_field(msg, key);
        if !v.is_empty() {
            return truncate_chars(&v, 300);
        }
    }
    truncate_chars(&msg.to_string(), 300)
}

/// Char-truncate with an ellipsis marker; identity when already within `limit`.
pub(crate) fn truncate_chars(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(limit).collect();
        t.push('…');
        t
    }
}

/// Bound a verbose tool input: pass small inputs through verbatim; large ones become
/// a truncated string of their serialization (truncated JSON is not valid JSON, so it
/// cannot stay a structured value).
pub(crate) fn bounded_input(v: &Value) -> Value {
    let s = v.to_string();
    if s.len() <= TOOL_IO_LIMIT {
        v.clone()
    } else {
        Value::String(truncate_chars(&s, TOOL_IO_LIMIT))
    }
}

/// A `tool_result` block's text: `content` is either a plain string or an array of
/// blocks whose text-typed entries we join.
fn result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .map(|b| str_field(b, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub(crate) fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// The whole turn's usage from `result.modelUsage`, summed across every model the turn called.
/// The per-request `message_start` samples only gauge the latest request.
fn turn_tokens(msg: &Value, cost_usd: f64) -> Option<AgentEvent> {
    let models = msg.get("modelUsage")?.as_object()?;
    let mut tokens = Tokens::default();
    for usage in models.values() {
        tokens.input += u64_field(usage, "inputTokens");
        tokens.output += u64_field(usage, "outputTokens");
        tokens.cache_read += u64_field(usage, "cacheReadInputTokens");
        tokens.cache_write += u64_field(usage, "cacheCreationInputTokens");
    }
    tokens.total = tokens.input + tokens.output + tokens.cache_read + tokens.cache_write;
    tokens.cost_usd = (cost_usd > 0.0).then_some(cost_usd);
    Some(AgentEvent::Tokens(tokens))
}

pub(crate) fn u64_field(v: &Value, key: &str) -> u64 {
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
    use crucible_contract::event::RawStream;

    /// Feed a sequence of lines and collect every event produced, in order.
    fn run(lines: &[&str]) -> Vec<AgentEvent> {
        let mut p = StreamJsonParser::default();
        lines.iter().flat_map(|l| p.push(l)).collect()
    }

    /// The outage shape: claude prints a bare-text failure to stdout and exits non-zero. The
    /// line must reach the caller, not vanish for failing to parse as JSON.
    /// The typed fast path must emit exactly what the Value walk emits, over the golden
    /// captures plus shapes that force the fallback (wrong-typed fields, unknown types, junk)
    /// and shapes that must not (escaped text, nulls, empty arrays).
    #[test]
    fn the_typed_path_emits_what_the_value_walk_emits() {
        let mut lines: Vec<&str> = include_str!("testdata/claude_stream_hello.jsonl")
            .lines()
            .chain(include_str!("testdata/claude_stream_tools.jsonl").lines())
            .collect();
        lines.extend([
            r#"{"type":"some_future_kind","detail":"x"}"#,
            "Error: connection refused (os error 111)",
            r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":10,"error":"overloaded_error"}"#,
            r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":10,"error":"unknown","message":"upstream connect error"}"#,
            r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":10,"error":"unknown"}"#,
            r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":10,"error":{"type":"overloaded_error","message":"busy"}}"#,
            r#"{"type":"result","subtype":"success","num_turns":"x","total_cost_usd":0.1}"#,
            r#"{"type":"system","subtype":"init","model":"m","tools":"not-an-array"}"#,
            r#"{"type":"stream_event","event":{"type":"error","error":null}}"#,
            r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"output_tokens":4}}}"#,
            r#"{"type":"user","message":{"content":[]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"a"},{"type":"other"}]},{"junk":"element"}]}}"#,
            r#"{"type":"user","message":"just a string"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","id":"toolu_1","name":"Bash"}}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"done"}]}}"#,
            "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"quoted \\\"text\\\" here\\n\"}}}",
        ]);
        let mut typed = StreamJsonParser::default();
        let mut value = StreamJsonParser::default();
        let mut parsed_typed = 0usize;
        for line in &lines {
            if serde_json::from_str::<wire::Line<'_>>(line.trim()).is_ok() {
                parsed_typed += 1;
            }
        }
        let events_typed: Vec<_> = lines.iter().flat_map(|l| typed.push(l)).collect();
        let events_value: Vec<_> = lines
            .iter()
            .flat_map(|l| value.push_parsed(&parse_line(l)))
            .collect();
        assert_eq!(events_typed, events_value, "the fast path diverged");
        assert!(
            parsed_typed * 10 >= lines.len() * 9,
            "the fast path must carry the corpus: {parsed_typed}/{}",
            lines.len()
        );
    }

    #[test]
    fn non_json_stdout_passes_through_as_raw() {
        let evs = run(&["Error: connection refused (os error 111)"]);
        assert!(
            matches!(
                evs.as_slice(),
                [AgentEvent::Raw { text, stream: RawStream::Stdout }]
                    if text == "Error: connection refused (os error 111)"
            ),
            "{evs:?}"
        );
    }

    #[test]
    fn blank_stdout_lines_stay_silent() {
        assert!(run(&["", "   "]).is_empty());
    }

    #[test]
    fn unmodeled_json_type_passes_through_as_raw() {
        let line = r#"{"type":"some_future_kind","detail":"x"}"#;
        let evs = run(&[line]);
        assert!(
            matches!(
                evs.as_slice(),
                [AgentEvent::Raw { text, stream: RawStream::Stdout }] if text == line
            ),
            "{evs:?}"
        );
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
    fn known_ignored_types_emit_nothing() {
        // Real capture shapes the parser deliberately ignores.
        let ev = run(&[
            r#"{"type":"system","subtype":"status","model":"","tools":[]}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{},"uuid":"u","session_id":"s"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text"}]}}"#,
        ]);
        assert!(ev.is_empty(), "no events for ignored types, got {ev:?}");
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
                    input,
                    result,
                    failed,
                },
            ] => {
                assert!(!failed, "a call is not a failure until its result says so");
                assert_eq!(name, "Edit");
                assert_eq!(summary, "p.go: snap := x");
                assert!(!subagent);
                assert!(input.is_none(), "compact by default: no input recorded");
                assert!(result.is_none());
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
    fn a_delta_that_splits_out_the_cache_replaces_the_start_counts() {
        let ev = run(&[
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"usage":{"input_tokens":9017,"output_tokens":0}}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"cache_creation_input_tokens":0,"cache_read_input_tokens":8704,"input_tokens":313,"output_tokens":3}}}"#,
        ]);
        match &ev[..] {
            [AgentEvent::Tokens(t)] => {
                assert_eq!(t.input, 313);
                assert_eq!(t.output, 3);
                assert_eq!(t.cache_read, 8704);
                assert_eq!(t.cache_write, 0);
                assert_eq!(t.total, 313 + 3 + 8704);
            }
            other => panic!("expected one Tokens, got {other:?}"),
        }
    }

    #[test]
    fn tokens_carry_the_live_rate_when_a_handle_is_attached() {
        let meters = LiveMeters::default();
        let mut p = StreamJsonParser::with_meters(meters.clone());
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
        meters.rate.store(318.4);
        let mut p2 = StreamJsonParser::with_meters(meters);
        match drive(&mut p2).into_iter().find_map(|e| match e {
            AgentEvent::Tokens(t) => Some(t),
            _ => None,
        }) {
            Some(t) => assert_eq!(t.rate, Some(318.4)),
            None => panic!("expected a tokens sample"),
        }
    }

    #[test]
    fn tokens_carry_the_collectors_live_cost() {
        let meters = LiveMeters::default();
        let drive = |p: &mut StreamJsonParser| -> Option<Tokens> {
            let mut evs = p.push(
                r#"{"type":"stream_event","event":{"type":"message_start","message":{"usage":{"input_tokens":6000,"output_tokens":1}}}}"#,
            );
            evs.extend(p.push(
                r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"output_tokens":4}}}"#,
            ));
            evs.into_iter().find_map(|e| match e {
                AgentEvent::Tokens(t) => Some(t),
                _ => None,
            })
        };

        // No cost export yet -> nothing to stamp, and the budget falls back to the estimate.
        let mut p = StreamJsonParser::with_meters(meters.clone());
        assert_eq!(drive(&mut p).expect("a tokens sample").cost_usd, None);

        // A live cost the stream has not declared yet wins.
        meters.cost.add(23.4567);
        let mut p2 = StreamJsonParser::with_meters(meters.clone());
        assert_eq!(
            drive(&mut p2).expect("a tokens sample").cost_usd,
            Some(23.4567)
        );

        // Once the stream declares a LARGER figure of its own, that one wins.
        let mut p3 = StreamJsonParser::with_meters(meters);
        p3.push(r#"{"type":"result","subtype":"success","num_turns":1,"total_cost_usd":25.0}"#);
        assert_eq!(drive(&mut p3).expect("a tokens sample").cost_usd, None);
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

    #[test]
    fn api_retry_digs_past_unknown_for_the_real_error() {
        // error:"unknown" with a sibling message: the message wins.
        let ev = run(&[
            r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":15,"error":"unknown","message":"upstream connect error"}"#,
        ]);
        assert!(
            matches!(&ev[..], [AgentEvent::Retry { error, .. }] if error == "upstream connect error")
        );

        // error as an object: its message wins.
        let ev = run(&[
            r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":15,"error":{"type":"overloaded_error","message":"busy"}}"#,
        ]);
        assert!(matches!(&ev[..], [AgentEvent::Retry { error, .. }] if error == "busy"));

        // Nothing classifiable: the whole raw line rides along instead of "unknown".
        let ev = run(&[
            r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":15,"error":"unknown","delay_ms":2000}"#,
        ]);
        match &ev[..] {
            [AgentEvent::Retry { error, .. }] => {
                assert!(error.contains("delay_ms"), "raw payload carried: {error}");
                assert_ne!(error, "unknown");
            }
            other => panic!("expected one Retry, got {other:?}"),
        }
    }

    #[test]
    fn verbose_tool_io_carries_input_and_result_excerpt() {
        let mut p = StreamJsonParser::default().with_tool_io(true);
        let lines = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","id":"toolu_1","name":"Bash"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ls\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop"}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"a.txt\nb.txt"}]}]}}"#,
        ];
        let ev: Vec<AgentEvent> = lines.iter().flat_map(|l| p.push(l)).collect();
        match &ev[..] {
            [
                AgentEvent::Tool {
                    name: call_name,
                    input,
                    result: call_result,
                    ..
                },
                AgentEvent::Tool {
                    name: res_name,
                    result,
                    ..
                },
            ] => {
                assert_eq!(call_name, "Bash");
                assert_eq!(
                    input
                        .as_ref()
                        .and_then(|i| i.get("command"))
                        .and_then(|c| c.as_str()),
                    Some("ls")
                );
                assert!(call_result.is_none());
                assert_eq!(res_name, "Bash", "result labeled via the id map");
                assert_eq!(result.as_deref(), Some("a.txt\nb.txt"));
            }
            other => panic!("expected call + result Tool events, got {other:?}"),
        }
    }

    #[test]
    fn verbose_tool_io_bounds_oversized_input_and_result() {
        let big = "x".repeat(TOOL_IO_LIMIT * 2);
        let mut p = StreamJsonParser::default().with_tool_io(true);
        let lines = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","id":"toolu_2","name":"Write"}}}"#.to_string(),
            format!(
                r#"{{"type":"stream_event","event":{{"type":"content_block_delta","delta":{{"type":"input_json_delta","partial_json":"{{\"content\":\"{big}\"}}"}}}}}}"#
            ),
            r#"{"type":"stream_event","event":{"type":"content_block_stop"}}"#.to_string(),
            format!(
                r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_2","content":"{big}"}}]}}}}"#
            ),
        ];
        let ev: Vec<AgentEvent> = lines.iter().flat_map(|l| p.push(l)).collect();
        match &ev[..] {
            [
                AgentEvent::Tool { input, .. },
                AgentEvent::Tool { result, .. },
            ] => {
                // An oversized input degrades to a truncated string of its serialization.
                let stored = input
                    .as_ref()
                    .and_then(|i| i.as_str())
                    .expect("truncated to string");
                assert!(stored.chars().count() <= TOOL_IO_LIMIT + 1);
                assert!(stored.ends_with('…'));
                let excerpt = result.as_deref().expect("result excerpt");
                assert!(excerpt.chars().count() <= TOOL_IO_LIMIT + 1);
                assert!(excerpt.ends_with('…'));
            }
            other => panic!("expected call + result Tool events, got {other:?}"),
        }
    }

    #[test]
    fn default_mode_ignores_tool_results() {
        let ev = run(&[
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"secret-ish output"}]}}"#,
        ]);
        assert!(ev.is_empty(), "compact mode drops tool results, got {ev:?}");
    }

    fn mcp_call_then(result: &str) -> [String; 4] {
        [
            r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","id":"toolu_9","name":"mcp__buildit__run"}}}"#.to_string(),
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"build_id\":\"buildit-1\"}"}}}"#.to_string(),
            r#"{"type":"stream_event","event":{"type":"content_block_stop"}}"#.to_string(),
            format!(
                r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_9","is_error":true,"content":{result}}}]}}}}"#
            ),
        ]
    }

    #[test]
    fn default_mode_shows_a_failed_tool_result_as_one_line() {
        let lines = mcp_call_then(r#""`sh -c alloc` failed in pod buildit-1:\n  exit code 1""#);
        let ev = run(&lines.iter().map(String::as_str).collect::<Vec<_>>());
        match &ev[..] {
            [
                AgentEvent::Tool { name: call, .. },
                AgentEvent::Tool {
                    name,
                    summary,
                    input,
                    result,
                    ..
                },
            ] => {
                assert_eq!(call, "mcp__buildit__run");
                assert_eq!(name, "mcp__buildit__run", "labeled by the call's id");
                assert_eq!(
                    summary,
                    "failed: `sh -c alloc` failed in pod buildit-1: exit code 1"
                );
                assert!(
                    input.is_none() && result.is_none(),
                    "compact mode stays compact"
                );
            }
            other => panic!("expected the call and its failure, got {other:?}"),
        }
    }

    #[test]
    fn default_mode_bounds_a_long_failure() {
        let long = format!(r#"[{{"type":"text","text":"{}"}}]"#, "e".repeat(5_000));
        let lines = mcp_call_then(&long);
        let ev = run(&lines.iter().map(String::as_str).collect::<Vec<_>>());
        match ev.last() {
            Some(AgentEvent::Tool { summary, .. }) => {
                let reason = summary.strip_prefix("failed: ").unwrap();
                assert_eq!(reason.chars().count(), TOOL_ERROR_LIMIT + 1);
                assert!(reason.ends_with('…'));
            }
            other => panic!("expected a failure event, got {other:?}"),
        }
    }

    #[test]
    fn verbose_tool_io_marks_a_failed_result() {
        let mut p = StreamJsonParser::default().with_tool_io(true);
        let lines = mcp_call_then(r#""no live build""#);
        let ev: Vec<AgentEvent> = lines.iter().flat_map(|l| p.push(l)).collect();
        match ev.last() {
            Some(AgentEvent::Tool {
                name,
                summary,
                result,
                ..
            }) => {
                assert_eq!(name, "mcp__buildit__run");
                assert_eq!(summary, "failed");
                assert_eq!(result.as_deref(), Some("no live build"));
            }
            other => panic!("expected a failure excerpt, got {other:?}"),
        }
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
                AgentEvent::Tokens(turn),
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
                assert_eq!(turn.input, 3968 + 507, "every model the turn called");
                assert_eq!(turn.output, 4 + 11);
                assert_eq!(turn.cache_read, 14376);
                assert_eq!(turn.cache_write, 6439);
                assert_eq!(turn.total, 4475 + 15 + 14376 + 6439);
                assert_eq!(turn.cost_usd, Some(0.09208));
                assert_eq!(subtype, "success");
                assert_eq!(*turns, 1);
                assert!((cost_usd - 0.09208).abs() < 1e-9, "real total_cost_usd");
            }
            other => panic!("unexpected event sequence from real capture: {other:?}"),
        }
    }

    /// Golden test against a real Claude Code 2.1.306 capture on the sandbox argv: two parallel
    /// tool calls whose inputs stream as split `input_json_delta` chunks, their `user` result
    /// echoes, and system subtypes the parser does not model.
    #[test]
    fn golden_real_tool_capture() {
        let lines: Vec<&str> = include_str!("testdata/claude_stream_tools.jsonl")
            .lines()
            .collect();
        let decode = |tool_io: bool| {
            let mut p = StreamJsonParser::default().with_tool_io(tool_io);
            let mut ev: Vec<AgentEvent> = lines.iter().flat_map(|l| p.push(l)).collect();
            ev.extend(p.flush());
            ev
        };

        match &decode(false)[..] {
            [
                AgentEvent::Init {
                    model,
                    tools,
                    agents,
                },
                AgentEvent::Tool {
                    name: bash,
                    summary: bash_summary,
                    input: None,
                    result: None,
                    failed: false,
                    ..
                },
                AgentEvent::Tool {
                    name: read,
                    summary: read_summary,
                    input: None,
                    result: None,
                    failed: false,
                    ..
                },
                AgentEvent::Tokens(first),
                AgentEvent::Text { delta: count },
                AgentEvent::Text { delta: line },
                AgentEvent::Tokens(turn),
                AgentEvent::Result {
                    subtype,
                    is_error: false,
                    turns: 3,
                    cost_usd,
                    error: None,
                },
            ] => {
                assert_eq!(model, "claude-opus-5-5");
                assert_eq!((*tools, *agents), (25, 6));
                assert_eq!(bash, "Bash");
                assert_eq!(bash_summary, "$ wc -l note.txt  # Count lines in note file");
                assert_eq!(read, "Read");
                assert_eq!(read_summary, "/sandbox/work/note.txt");
                assert_eq!(
                    (
                        first.input,
                        first.output,
                        first.cache_read,
                        first.cache_write
                    ),
                    (2, 214, 0, 27996)
                );
                assert_eq!(first.cost_usd, None);
                assert_eq!((count.as_str(), line.as_str()), ("2", "alpha"));
                assert_eq!(
                    (turn.input, turn.output, turn.cache_read, turn.cache_write),
                    (4, 221, 21039, 35242)
                );
                assert_eq!(turn.cost_usd, Some(0.1848538));
                assert_eq!(subtype, "success");
                assert!((cost_usd - 0.1848538).abs() < 1e-9);
            }
            other => panic!("unexpected event sequence from real capture: {other:?}"),
        }

        let verbose = decode(true);
        let tool_io: Vec<(&str, &str, Option<&serde_json::Value>, Option<&str>)> = verbose
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Tool {
                    name,
                    summary,
                    input,
                    result,
                    ..
                } => Some((
                    name.as_str(),
                    summary.as_str(),
                    input.as_ref(),
                    result.as_deref(),
                )),
                _ => None,
            })
            .collect();
        let bash_input = serde_json::json!({
            "command": "wc -l note.txt",
            "description": "Count lines in note file",
        });
        let read_input = serde_json::json!({ "file_path": "/sandbox/work/note.txt" });
        assert_eq!(
            tool_io,
            [
                (
                    "Bash",
                    "$ wc -l note.txt  # Count lines in note file",
                    Some(&bash_input),
                    None
                ),
                ("Read", "/sandbox/work/note.txt", Some(&read_input), None),
                ("Read", "result", None, Some("1\talpha\n2\tbeta\n3\t")),
                ("Bash", "result", None, Some("       2 note.txt")),
            ]
        );
        assert!(
            !verbose.iter().any(|e| matches!(e, AgentEvent::Raw { .. })),
            "every line of the capture is modeled or deliberately dropped: {verbose:?}"
        );
    }

    #[test]
    fn a_result_without_model_usage_adds_no_turn_total() {
        let ev =
            run(&[r#"{"type":"result","subtype":"success","num_turns":1,"total_cost_usd":0.5}"#]);
        assert!(matches!(&ev[..], [AgentEvent::Result { .. }]), "{ev:?}");
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
