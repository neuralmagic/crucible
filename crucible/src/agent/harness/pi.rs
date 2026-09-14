//! The pi arm of the harness boundary: Pi (Mario Zechner's coding agent) as a harness for an
//! endpoint that speaks only OpenAI Chat Completions (or a direct Anthropic/OpenAI key).
//!
//! `pi --mode json --print` emits a complete type-tagged JSONL event stream from its own
//! process, so the live decoder carries the turn's result and token usage and
//! `backfill_required` stays false; the session file under `$PI_CODING_AGENT_DIR/sessions` is
//! trace garnish, same posture as claude and codex.
//!
//! Auth is a direct API key relayed into the sandbox env. The seeded `models.json` registers the
//! endpoint as the `crucible` provider, reading the key back through `$OPENAI_API_KEY` or
//! `$ANTHROPIC_API_KEY`, so a custom endpoint and the vendors' own APIs take the same shape.
//!
//! Pi has no MCP client, so the provisioning broker is unreachable from a pi turn: the seeded
//! config ignores it.

use crate::agent::harness::{
    ApiEndpoint, AuthProvider, Backend, Broker, CRUCIBLE_PROVIDER_ID, HarnessSpec, StreamDecoder,
    TranscriptLocator, TurnArtifacts, api_endpoint, json_str,
};
use crate::agent::inference::{InferenceEnv, WireApi};
use crate::args::Args;
use crate::manifest::ReasoningEffort;
use crate::turn_trace::{self, GenAiRecord, ToolCall, ToolInvocation};
use crucible_harness::tool_summary::summarize;
use crucible_harness::{LiveMeters, PiJsonParser};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

pub(crate) struct Pi;

impl Pi {
    pub(crate) const SPEC: HarnessSpec = HarnessSpec {
        name: "pi",
        binaries: &["/usr/local/bin/pi"],
        // Pi discovers project skills under `.agents/skills` (the Agent Skills layout).
        skills_dir: ".agents/skills",
        // Relocates settings, models.json, and the session store off `~/.pi/agent`.
        home_var: "PI_CODING_AGENT_DIR",
        home: "/sandbox/.pi/agent",
        config: "/sandbox/.pi/agent/models.json",
        sandbox_env: &[("PI_OFFLINE", "1"), ("PI_SKIP_VERSION_CHECK", "1")],
        local_env: &[("PI_SKIP_VERSION_CHECK", "1")],
        // `$PI_CODING_AGENT_DIR/sessions/<cwd-slug>/<timestamp>_<id>.jsonl`: one slug segment.
        transcript: TranscriptLocator::NewestJsonl {
            sandbox_root: "/sandbox/.pi/agent/sessions",
            glob: "*/*.jsonl",
        },
        transcript_fetch_timeout: Duration::from_secs(30),
        auth: AuthProvider::ApiKey,
        otel_capable: false,
        backfill_required: false,
    };

    /// The shared invocation prefix: `pi --mode json --print --approve --provider crucible
    /// --model <model> [--thinking <level>]`. Prompt delivery is appended by the caller.
    /// `--approve` trusts the workspace's project-local files (the toolbox skills among them)
    /// without the interactive prompt.
    fn base_args(args: &Args) -> Vec<String> {
        let mut a = vec![
            "pi".to_string(),
            "--mode".to_string(),
            "json".to_string(),
            "--print".to_string(),
            "--approve".to_string(),
            "--provider".to_string(),
            CRUCIBLE_PROVIDER_ID.to_string(),
            "--model".to_string(),
            args.model().to_string(),
        ];
        if let Some(effort) = args.reasoning_effort {
            a.push("--thinking".to_string());
            a.push(thinking_level(effort).to_string());
        }
        a
    }
}

/// Crucible's five reasoning tiers onto pi's `--thinking` levels; `max` is pi's ceiling on
/// providers that have one and is not accepted everywhere, so the top two tiers map to `xhigh`.
fn thinking_level(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Xhigh | ReasoningEffort::Max => "xhigh",
    }
}

/// The here-doc delimiter a local turn feeds the prompt through, chosen so it never appears on
/// a prompt line of its own.
fn heredoc_delimiter(prompt: &str) -> String {
    let mut delimiter = "CRUCIBLE_PROMPT".to_string();
    while prompt.lines().any(|l| l == delimiter) {
        delimiter.push('_');
    }
    delimiter
}

/// Render `models.json`: the turn's endpoint as the `crucible` provider (its API family, base
/// URL, and the env var the key is read from) carrying the one model the turn runs.
fn models_json(model: &str, endpoint: &ApiEndpoint) -> String {
    let (api, base_url, key_env) = match endpoint {
        ApiEndpoint::OpenAi { base_url, wire_api } => (
            match wire_api {
                WireApi::Chat => "openai-completions",
                WireApi::Responses => "openai-responses",
            },
            base_url.as_str(),
            crate::agent::inference::OPENAI_API_KEY_ENV,
        ),
        ApiEndpoint::Anthropic { base_url } => (
            "anthropic-messages",
            base_url.as_str(),
            crate::agent::inference::ANTHROPIC_API_KEY,
        ),
    };
    json!({
        "providers": {
            CRUCIBLE_PROVIDER_ID: {
                "baseUrl": base_url,
                "api": api,
                "apiKey": format!("${key_env}"),
                "models": [{
                    "id": model,
                    "name": model,
                    "reasoning": false,
                    "input": ["text"],
                    "contextWindow": 128_000,
                    "maxTokens": 16_384,
                }],
            }
        }
    })
    .to_string()
}

impl Backend for Pi {
    fn spec(&self) -> &'static HarnessSpec {
        &Self::SPEC
    }

    /// Pi's option parser reads any positional starting with `-` as a flag, so the prompt goes
    /// over stdin here too: a `bash -c` here-doc feeds it, the delimiter chosen to be absent from
    /// the prompt.
    fn local_argv(&self, args: &Args, prompt: &str) -> Vec<String> {
        let delimiter = heredoc_delimiter(prompt);
        let script = format!("exec \"$@\" <<'{delimiter}'\n{prompt}\n{delimiter}\n");
        let mut a = vec![
            "bash".to_string(),
            "-c".to_string(),
            script,
            "crucible-pi".to_string(),
        ];
        a.extend(Self::base_args(args));
        a
    }

    /// No message positional: `--print` reads a piped stdin as the prompt, which the shared exec
    /// wrapper redirects from the uploaded prompt file. Pi has no MCP client, so `mcp_seeded` is
    /// accepted and unused by design.
    fn sandbox_argv(&self, args: &Args, _mcp_seeded: bool) -> Vec<String> {
        Self::base_args(args)
    }

    /// `models.json`, ALWAYS (it is where the `crucible` provider and the model come from). The
    /// broker is ignored: pi speaks no MCP.
    fn config(
        &self,
        args: &Args,
        _broker: Option<&Broker<'_>>,
        inference: &InferenceEnv,
    ) -> Option<String> {
        Some(models_json(
            args.model(),
            &api_endpoint(args.model(), inference),
        ))
    }

    fn decoder(
        &self,
        args: &Args,
        _meters: Option<&LiveMeters>,
        tool_io: bool,
    ) -> Box<dyn StreamDecoder> {
        Box::new(
            PiJsonParser::new(args.model())
                .with_price(crate::agent::event::estimate_cost)
                .with_tool_io(tool_io),
        )
    }

    fn parse_transcript(&self, content: &[u8]) -> TurnArtifacts {
        session_spans(content)
    }

    fn content_records(&self, content: &[u8]) -> Vec<GenAiRecord> {
        session_records(content)
    }
}

/// One session entry: when it was written and the message it carries (`type: message` only).
struct SessionEntry {
    at: SystemTime,
    message: Value,
}

/// Every message entry of the session file, in file order. A line that is not JSON, or is not a
/// message, is skipped: the session is telemetry and a torn tail must not cost the whole trace.
fn session_entries(content: &[u8]) -> Vec<SessionEntry> {
    String::from_utf8_lossy(content)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let mut v: Value = serde_json::from_str(l).ok()?;
            if v.get("type").and_then(Value::as_str) != Some("message") {
                return None;
            }
            let at = v
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(turn_trace::parse_ts)
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let message = v.get_mut("message").map(Value::take)?;
            Some(SessionEntry { at, message })
        })
        .collect()
}

/// The text of a message's `content` blocks (or a bare string), text blocks joined by newlines.
fn content_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| json_str(b, "type") == "text")
            .map(|b| json_str(b, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Tool spans recovered from the session file: one per assistant `toolCall` block, opened at the
/// assistant entry and closed by the matching `toolResult` entry. The live stream owns result +
/// cost, so an unreadable session costs only trace detail.
fn session_spans(content: &[u8]) -> TurnArtifacts {
    let mut order: Vec<ToolInvocation> = Vec::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    for entry in session_entries(content) {
        match json_str(&entry.message, "role").as_str() {
            "assistant" => {
                for block in entry
                    .message
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|b| json_str(b, "type") == "toolCall")
                {
                    let name = json_str(block, "name");
                    let args = block.get("arguments").unwrap_or(&Value::Null);
                    by_id.insert(json_str(block, "id"), order.len());
                    order.push(ToolInvocation {
                        summary: turn_trace::truncate(&summarize(&name, args), 200),
                        name,
                        start: entry.at,
                        end: None,
                        error: false,
                    });
                }
            }
            "toolResult" => {
                if let Some(i) = by_id.get(&json_str(&entry.message, "toolCallId")).copied()
                    && let Some(call) = order.get_mut(i)
                {
                    call.end = Some(entry.at);
                    call.error = entry
                        .message
                        .get("isError")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                }
            }
            _ => {}
        }
    }
    TurnArtifacts {
        events: Vec::new(),
        cost_usd: None,
        tool_calls: order,
    }
}

/// The conversation as GenAI records: user text, each assistant message's text, thinking, and
/// tool calls, and one tool record per tool result.
fn session_records(content: &[u8]) -> Vec<GenAiRecord> {
    let redact = turn_trace::redact_enabled();
    let body = |text: &str| -> Option<String> {
        if text.trim().is_empty() {
            return None;
        }
        Some(if redact {
            turn_trace::redact(text)
        } else {
            text.to_string()
        })
    };
    let mut out = Vec::new();
    for entry in session_entries(content) {
        let m = &entry.message;
        match json_str(m, "role").as_str() {
            "user" => {
                if let Some(content) = body(&content_text(m)) {
                    out.push(GenAiRecord::User { content });
                }
            }
            "assistant" => {
                let mut reasoning = Vec::new();
                let mut tool_calls = Vec::new();
                for block in m
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    match json_str(block, "type").as_str() {
                        "thinking" => reasoning.push(json_str(block, "thinking")),
                        "toolCall" => tool_calls.push(ToolCall {
                            id: json_str(block, "id"),
                            name: json_str(block, "name"),
                            arguments: body(
                                &block
                                    .get("arguments")
                                    .map(Value::to_string)
                                    .unwrap_or_default(),
                            )
                            .unwrap_or_default(),
                        }),
                        _ => {}
                    }
                }
                let text = body(&content_text(m));
                let reasoning = body(&reasoning.join("\n"));
                if text.is_some() || reasoning.is_some() || !tool_calls.is_empty() {
                    let model = json_str(m, "model");
                    out.push(GenAiRecord::Assistant {
                        text,
                        reasoning,
                        tool_calls,
                        model: (!model.is_empty()).then_some(model),
                    });
                }
            }
            "toolResult" => out.push(GenAiRecord::Tool {
                id: json_str(m, "toolCallId"),
                content: body(&content_text(m)).unwrap_or_default(),
                is_error: m.get("isError").and_then(Value::as_bool).unwrap_or(false),
            }),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::harness::{SandboxAuth, SeedFile};
    use clap::Parser;

    const CONFIG: &str = Pi::SPEC.config;

    fn args() -> Args {
        crate::cli::Cli::parse_from(["crucible"]).run
    }

    fn seed_files(
        args: &Args,
        broker_url: Option<&str>,
        inference: &InferenceEnv,
    ) -> Vec<SeedFile> {
        Pi.seed_files(
            args,
            broker_url,
            Some("tok"),
            &SandboxAuth::ApiKey,
            inference,
        )
    }

    fn custom_endpoint() -> InferenceEnv {
        InferenceEnv {
            openai_key: Some("sk-oa".into()),
            openai_base_url: Some("http://vllm.internal:8000/v1".into()),
            ..Default::default()
        }
    }

    const PREFIX: &[&str] = &[
        "pi",
        "--mode",
        "json",
        "--print",
        "--approve",
        "--provider",
        "crucible",
        "--model",
    ];

    #[test]
    fn invocation_is_headless_json_print_on_the_crucible_provider() {
        let mut a = args();
        a.model = Some("qwen-3-8-27b".to_string());
        let sandbox = Pi.sandbox_argv(&a, true);
        assert_eq!(&sandbox[..PREFIX.len()], PREFIX);
        assert_eq!(sandbox[PREFIX.len()], "qwen-3-8-27b");
        assert_eq!(
            sandbox.len(),
            PREFIX.len() + 1,
            "no message positional: stdin carries it"
        );

        a.reasoning_effort = Some(ReasoningEffort::Max);
        let sandbox = Pi.sandbox_argv(&a, false);
        assert_eq!(&sandbox[sandbox.len() - 2..], &["--thinking", "xhigh"]);
    }

    #[test]
    fn effort_maps_five_tiers_onto_pi_levels() {
        assert_eq!(thinking_level(ReasoningEffort::Low), "low");
        assert_eq!(thinking_level(ReasoningEffort::Medium), "medium");
        assert_eq!(thinking_level(ReasoningEffort::High), "high");
        assert_eq!(thinking_level(ReasoningEffort::Xhigh), "xhigh");
        assert_eq!(thinking_level(ReasoningEffort::Max), "xhigh");
    }

    /// A local turn feeds the prompt through a here-doc, so a prompt starting with `-` (a
    /// SKILL.md's frontmatter) reaches pi as stdin rather than as a flag; the delimiter dodges a
    /// prompt line that spells it.
    #[test]
    fn local_prompt_rides_a_heredoc_with_a_delimiter_absent_from_the_prompt() {
        let a = args();
        let local = Pi.local_argv(&a, "---\nname: analyze\n---\n\nDo it.");
        assert_eq!(&local[..2], &["bash", "-c"]);
        assert!(local[2].starts_with("exec \"$@\" <<'CRUCIBLE_PROMPT'\n---\nname: analyze"));
        assert!(local[2].ends_with("\nDo it.\nCRUCIBLE_PROMPT\n"));
        assert_eq!(local[3], "crucible-pi");
        assert_eq!(&local[4..4 + PREFIX.len()], PREFIX);

        let tricky = Pi.local_argv(&a, "CRUCIBLE_PROMPT\nreal prompt");
        assert!(tricky[2].starts_with("exec \"$@\" <<'CRUCIBLE_PROMPT_'\n"));
        assert!(tricky[2].ends_with("\nreal prompt\nCRUCIBLE_PROMPT_\n"));
    }

    /// The here-doc actually delivers the prompt to the exec'd program's stdin.
    #[test]
    fn local_heredoc_delivers_the_prompt_over_stdin() {
        let a = args();
        let mut argv = Pi.local_argv(&a, "-p looks like a flag\nline two");
        // Replace `pi …` with `cat`, keeping the wrapper: what cat prints is what pi would read.
        argv.truncate(4);
        argv.push("cat".to_string());
        let out = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .expect("bash");
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "-p looks like a flag\nline two\n"
        );
    }

    #[test]
    fn env_script_relocates_the_agent_dir_offline_with_no_credential() {
        let s = Pi.env_script(&[("FOO".into(), "bar".into())]);
        assert!(s.contains("export AGENT_TOOL=pi"));
        assert!(s.contains("export PI_CODING_AGENT_DIR=/sandbox/.pi/agent"));
        assert!(s.contains("export PI_OFFLINE=1"));
        assert!(s.contains("export FOO='bar'"));
        assert!(
            !s.contains("API_KEY"),
            "the key rides the inference env, not the spec: {s}"
        );
        assert_eq!(CONFIG, "/sandbox/.pi/agent/models.json");
    }

    /// A custom OpenAI-speaking endpoint becomes the `crucible` provider on the completions API
    /// (or responses, per the wire API), keyed off `OPENAI_API_KEY`; the broker is not seeded.
    #[test]
    fn models_json_registers_a_custom_endpoint_and_ignores_the_broker() {
        let mut a = args();
        a.model = Some("qwen-3-8-27b".to_string());
        let seeds = seed_files(&a, Some("http://10.0.0.1:8000/mcp"), &custom_endpoint());
        assert_eq!(seeds.len(), 1, "models.json is always seeded, nothing else");
        assert_eq!(seeds[0].dest, CONFIG);
        let v: Value = serde_json::from_str(&seeds[0].content).expect("valid json");
        let p = &v["providers"]["crucible"];
        assert_eq!(p["baseUrl"], "http://vllm.internal:8000/v1");
        assert_eq!(p["api"], "openai-completions");
        assert_eq!(p["apiKey"], "$OPENAI_API_KEY");
        assert_eq!(p["models"][0]["id"], "qwen-3-8-27b");
        assert!(
            !seeds[0].content.contains("10.0.0.1"),
            "no MCP client, no broker"
        );

        let responses = InferenceEnv {
            wire_api: Some(WireApi::Responses),
            ..custom_endpoint()
        };
        let seeds = seed_files(&a, None, &responses);
        let v: Value = serde_json::from_str(&seeds[0].content).expect("valid json");
        assert_eq!(v["providers"]["crucible"]["api"], "openai-responses");
    }

    #[test]
    fn models_json_registers_anthropic_for_a_direct_anthropic_key() {
        let a = args();
        let inference = InferenceEnv {
            anthropic_key: Some("sk-ant".into()),
            anthropic_base_url: Some("https://claude.corp".into()),
            ..Default::default()
        };
        let seeds = seed_files(&a, None, &inference);
        let v: Value = serde_json::from_str(&seeds[0].content).expect("valid json");
        let p = &v["providers"]["crucible"];
        assert_eq!(p["api"], "anthropic-messages");
        assert_eq!(p["baseUrl"], "https://claude.corp");
        assert_eq!(p["apiKey"], "$ANTHROPIC_API_KEY");
        assert_eq!(p["models"][0]["id"], a.model());
    }

    const SESSION: &[u8] = include_bytes!("../../testdata/pi_session_fixture.jsonl");

    /// The banked session (a real `pi --mode json --print` run against a scripted endpoint)
    /// yields one span per tool call, closed by its result.
    #[test]
    fn the_session_yields_one_span_per_tool_call() {
        let art = Pi.parse_transcript(SESSION);
        assert!(art.events.is_empty(), "the live stream owns the result");
        assert_eq!(art.cost_usd, None);
        let names: Vec<(&str, &str, bool)> = art
            .tool_calls
            .iter()
            .map(|c| (c.name.as_str(), c.summary.as_str(), c.end.is_some()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("bash", "$ echo hi  # Print hi", true),
                ("write", "hello.txt", true)
            ]
        );
        assert!(art.tool_calls.iter().all(|c| !c.error));
        assert!(art.tool_calls.iter().all(|c| c.end.unwrap() >= c.start));
    }

    #[test]
    fn the_session_yields_the_conversation_records() {
        let records = Pi.content_records(SESSION);
        let kinds: Vec<&str> = records
            .iter()
            .map(|r| match r {
                GenAiRecord::System { .. } => "system",
                GenAiRecord::User { .. } => "user",
                GenAiRecord::Assistant { .. } => "assistant",
                GenAiRecord::Tool { .. } => "tool",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "user",
                "assistant",
                "tool",
                "assistant",
                "tool",
                "assistant"
            ]
        );
        match &records[1] {
            GenAiRecord::Assistant {
                tool_calls, model, ..
            } => {
                assert_eq!(tool_calls[0].id, "call_1");
                assert_eq!(tool_calls[0].name, "bash");
                assert!(tool_calls[0].arguments.contains("echo hi"));
                assert_eq!(model.as_deref(), Some("qwen-3-8-27b"));
            }
            other => panic!("{other:?}"),
        }
        assert!(
            matches!(&records[2], GenAiRecord::Tool { id, content, is_error }
            if id == "call_1" && content.trim() == "hi" && !is_error)
        );
        assert!(
            matches!(&records[5], GenAiRecord::Assistant { text: Some(t), .. }
            if t.starts_with("Ran echo and wrote hello.txt."))
        );
    }

    #[test]
    fn garbage_transcript_is_empty_never_panics() {
        for bytes in [&b"not json"[..], b"{}", b"", &[0xff, 0xfe]] {
            assert!(Pi.parse_transcript(bytes).tool_calls.is_empty());
            assert!(Pi.content_records(bytes).is_empty());
        }
    }
}
