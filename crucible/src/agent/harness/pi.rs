//! The pi arm of the harness boundary: Pi (pi.dev) as a harness for an endpoint that speaks only
//! OpenAI Chat Completions (or a direct Anthropic/OpenAI key).
//!
//! `pi --mode json --print` emits a complete type-tagged JSONL event stream from its own
//! process, so the live decoder carries the turn's result and token usage and
//! `backfill_required` stays false; the session file under `$PI_CODING_AGENT_DIR/sessions` is
//! trace garnish, same posture as claude and codex.
//!
//! Auth is a direct API key relayed into the sandbox env, or the gateway's ambient Vertex
//! credential when the env names none. The seeded `models.json` registers a keyed endpoint as
//! the `crucible` provider, reading the key back through `$OPENAI_API_KEY` or
//! `$ANTHROPIC_API_KEY`, so a custom endpoint and the vendors' own APIs take the same shape.
//! Pi speaks Anthropic on Vertex through none of its built-in providers, so a Vertex turn seeds
//! an extension instead ([`VERTEX_EXTENSION`]): it registers `crucible` on pi's Messages API
//! with a `fetch` that rewrites each request into Vertex's `rawPredict` shape and signs it with
//! the token the metadata emulator serves.
//!
//! Pi has no MCP client of its own; the sandbox image carries the `pi-mcp-adapter` extension,
//! which the turn loads by path when the broker is on ([`MCP_ADAPTER`]) and points at the
//! broker through a seeded `mcp.json` ([`MCP_CONFIG`]). The agent sees one proxy `mcp` tool
//! that discovers and calls the broker's tools on demand.

use crate::agent::harness::{
    ApiEndpoint, AuthProvider, Backend, Broker, CRUCIBLE_PROVIDER_ID, HarnessSpec, SandboxAuth,
    SeedFile, StreamDecoder, TranscriptLocator, TurnArtifacts, api_endpoint, json_str,
};
use crate::agent::inference::{InferenceEnv, VertexConfig, WireApi};
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
        // `pi` is a node script, so the process that opens the socket is node itself (the
        // sandbox policy matches the kernel-resolved binary); the UBI rpm and a tarball install
        // put it in different places.
        binaries: &["/usr/local/bin/pi", "/usr/bin/node", "/usr/local/bin/node"],
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

/// Where the Vertex extension is seeded: pi loads every extension under its agent dir.
pub(crate) const VERTEX_EXTENSION: &str = "/sandbox/.pi/agent/extensions/crucible-vertex.ts";

/// The MCP adapter extension's entry, where the sandbox image's global npm install puts it.
/// Loaded with `-e` only when the broker is on, so a brokerless turn carries no `mcp` tool.
pub(crate) const MCP_ADAPTER: &str = "/usr/local/lib/node_modules/pi-mcp-adapter/index.ts";

/// The adapter's Pi-owned config under the relocated agent dir: the broker as one remote
/// server, its bearer in a header (the per-run token, or the provider placeholder the egress
/// proxy resolves).
pub(crate) const MCP_CONFIG: &str = "/sandbox/.pi/agent/mcp.json";

fn mcp_json(broker: &Broker<'_>) -> String {
    let mut server = json!({ "url": broker.url });
    if let Some(token) = broker.token {
        server["headers"] = json!({ "Authorization": format!("Bearer {token}") });
    }
    json!({ "mcpServers": { broker.name: server } }).to_string()
}

/// The Vertex extension, with the turn's project, region, host, and model spliced in as JS
/// string literals. Vertex's Anthropic endpoint is the Messages API with the model moved into
/// the path (`…/publishers/anthropic/models/<model>:streamRawPredict`), `anthropic_version`
/// pinned in the body, and an OAuth bearer in place of `x-api-key`; the bearer is fetched from
/// the metadata emulator openshell points `GCE_METADATA_HOST` at, or read from the
/// `GCP_ADC_ACCESS_TOKEN` variable outside a sandbox.
const VERTEX_EXTENSION_TEMPLATE: &str = r#"import { streamSimpleAnthropic } from "@earendil-works/pi-ai";

const PROJECT = __PROJECT__;
const REGION = __REGION__;
const HOST = __HOST__;
const MODEL = __MODEL__;
const PREFIX = `/v1/projects/${PROJECT}/locations/${REGION}/publishers/anthropic/models/`;
const ANTHROPIC_VERSION = "vertex-2023-10-16";

async function token(): Promise<string> {
  const metadata = process.env.GCE_METADATA_HOST;
  if (metadata) {
    const res = await fetch(`http://${metadata}/computeMetadata/v1/instance/service-accounts/default/token`, {
      headers: { "Metadata-Flavor": "Google" },
    });
    if (res.ok) {
      const body = (await res.json()) as { access_token?: string };
      if (body.access_token) return body.access_token;
    }
  }
  const fromEnv = process.env.GCP_SA_ACCESS_TOKEN || process.env.GCP_ADC_ACCESS_TOKEN;
  if (fromEnv) return fromEnv;
  throw new Error("no Vertex access token: no metadata server and GCP_ADC_ACCESS_TOKEN unset");
}

const vertexFetch: typeof fetch = async (input, init) => {
  const url = new URL(typeof input === "string" ? input : input instanceof URL ? input.href : input.url);
  const headers = new Headers(init?.headers ?? (input instanceof Request ? input.headers : undefined));
  headers.delete("x-api-key");
  headers.set("authorization", `Bearer ${await token()}`);
  let body = init?.body;
  if (url.pathname === "/v1/messages" && typeof body === "string") {
    const parsed = JSON.parse(body) as { model?: string; stream?: boolean; anthropic_version?: string };
    const model = parsed.model ?? MODEL;
    delete parsed.model;
    parsed.anthropic_version = ANTHROPIC_VERSION;
    url.pathname = `${PREFIX}${model}:${parsed.stream ? "streamRawPredict" : "rawPredict"}`;
    body = JSON.stringify(parsed);
  } else if (url.pathname === "/v1/messages/count_tokens" && typeof body === "string") {
    const parsed = JSON.parse(body) as { anthropic_version?: string };
    parsed.anthropic_version = ANTHROPIC_VERSION;
    url.pathname = `${PREFIX}count-tokens:rawPredict`;
    body = JSON.stringify(parsed);
  }
  return fetch(url, { ...init, headers, body });
};

export default function (pi: any) {
  pi.registerProvider("crucible", {
    name: "Crucible inference endpoint (Vertex)",
    baseUrl: HOST,
    apiKey: "vertex",
    api: "anthropic-messages",
    models: [
      {
        id: MODEL,
        name: MODEL,
        reasoning: false,
        input: ["text"],
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
        contextWindow: 200000,
        maxTokens: 16384,
      },
    ],
    streamSimple: (model: any, context: any, options: any) =>
      streamSimpleAnthropic(model, context, { ...options, fetch: vertexFetch }),
  });
}
"#;

/// Render the Vertex extension for a project, region, and model.
fn vertex_extension(model: &str, vertex: &VertexConfig) -> String {
    let literal = |s: &str| serde_json::Value::String(s.to_string()).to_string();
    VERTEX_EXTENSION_TEMPLATE
        .replace("__PROJECT__", &literal(&vertex.project))
        .replace("__REGION__", &literal(&vertex.region))
        .replace("__HOST__", &literal(&format!("https://{}", vertex.host())))
        .replace("__MODEL__", &literal(model))
}

/// Render `models.json`: a keyed endpoint as the `crucible` provider (its API family, base URL,
/// and the env var the key is read from) carrying the one model the turn runs. A Vertex turn
/// seeds none: its provider comes from the extension.
fn models_json(model: &str, endpoint: &ApiEndpoint) -> Option<String> {
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
        ApiEndpoint::Vertex(_) => return None,
    };
    let rendered = json!({
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
    .to_string();
    Some(rendered)
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
    /// wrapper redirects from the uploaded prompt file. With the broker on, the MCP adapter is
    /// loaded by path; it reads the seeded [`MCP_CONFIG`] itself.
    fn sandbox_argv(&self, args: &Args, mcp_seeded: bool) -> Vec<String> {
        let mut a = Self::base_args(args);
        if mcp_seeded {
            a.push("-e".to_string());
            a.push(MCP_ADAPTER.to_string());
        }
        a
    }

    /// `models.json` for a keyed endpoint (it is where the `crucible` provider and the model come
    /// from); a Vertex turn seeds the extension instead. The broker rides [`Backend::mcp_config`].
    fn config(
        &self,
        args: &Args,
        _broker: Option<&Broker<'_>>,
        inference: &InferenceEnv,
    ) -> Option<String> {
        models_json(
            args.model(),
            &api_endpoint(args.model(), inference, &args.env),
        )
    }

    /// The broker as the adapter's one remote server.
    fn mcp_config(&self, broker: &Broker<'_>) -> Option<SeedFile> {
        Some(SeedFile {
            content: mcp_json(broker),
            dest: MCP_CONFIG,
        })
    }

    /// The Vertex extension, when the turn runs on the gateway's ambient credential.
    fn credential(&self, args: &Args, auth: &SandboxAuth) -> Option<SeedFile> {
        match auth {
            SandboxAuth::Gateway => Some(SeedFile {
                content: vertex_extension(args.model(), &VertexConfig::from_env(&args.env)),
                dest: VERTEX_EXTENSION,
            }),
            SandboxAuth::AnthropicKey | SandboxAuth::ApiKey | SandboxAuth::Codex(_) => None,
        }
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
        let sandbox = Pi.sandbox_argv(&a, false);
        assert_eq!(&sandbox[..PREFIX.len()], PREFIX);
        assert_eq!(sandbox[PREFIX.len()], "qwen-3-8-27b");
        assert_eq!(
            sandbox.len(),
            PREFIX.len() + 1,
            "no message positional: stdin carries it; no broker, no adapter"
        );

        // With the broker on, the MCP adapter is loaded by its image path.
        let with_broker = Pi.sandbox_argv(&a, true);
        assert_eq!(&with_broker[..sandbox.len()], &sandbox[..]);
        assert_eq!(&with_broker[sandbox.len()..], &["-e", MCP_ADAPTER]);

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
    /// (or responses, per the wire API), keyed off `OPENAI_API_KEY`. The broker rides the
    /// adapter's `mcp.json`, never `models.json`.
    #[test]
    fn models_json_registers_a_custom_endpoint_and_the_broker_rides_mcp_json() {
        let mut a = args();
        a.model = Some("qwen-3-8-27b".to_string());
        let seeds = seed_files(&a, Some("http://10.0.0.1:8000/mcp"), &custom_endpoint());
        assert_eq!(seeds.len(), 2, "models.json, then the adapter's mcp.json");
        assert_eq!(seeds[0].dest, CONFIG);
        let v: Value = serde_json::from_str(&seeds[0].content).expect("valid json");
        let p = &v["providers"]["crucible"];
        assert_eq!(p["baseUrl"], "http://vllm.internal:8000/v1");
        assert_eq!(p["api"], "openai-completions");
        assert_eq!(p["apiKey"], "$OPENAI_API_KEY");
        assert_eq!(p["models"][0]["id"], "qwen-3-8-27b");
        assert!(!seeds[0].content.contains("10.0.0.1"));
        assert_eq!(seeds[1].dest, MCP_CONFIG);
        let m: Value = serde_json::from_str(&seeds[1].content).expect("valid json");
        let server = &m["mcpServers"][a.broker.name.as_str()];
        assert_eq!(server["url"], "http://10.0.0.1:8000/mcp");
        assert_eq!(server["headers"]["Authorization"], "Bearer tok");
        assert_eq!(seeds[1].content.matches("Bearer").count(), 1);

        let tokenless = Pi.seed_files(
            &a,
            Some("http://10.0.0.1:8000/mcp"),
            None,
            &SandboxAuth::ApiKey,
            &custom_endpoint(),
        );
        let m: Value = serde_json::from_str(&tokenless[1].content).expect("valid json");
        assert!(
            m["mcpServers"][a.broker.name.as_str()]
                .get("headers")
                .is_none()
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

    /// No key and no base URL is the ambient Vertex credential: no `models.json`, and the
    /// extension is seeded instead, with the project, region, host, and model as JS literals.
    #[test]
    fn a_vertex_turn_seeds_the_extension_instead_of_models_json() {
        let mut a = args();
        a.model = Some("claude-sonnet-5".to_string());
        a.env = vec![
            ("ANTHROPIC_VERTEX_PROJECT_ID".into(), "proj-x".into()),
            ("CLOUD_ML_REGION".into(), "us-east5".into()),
        ];
        let seeds = Pi.seed_files(
            &a,
            None,
            None,
            &SandboxAuth::Gateway,
            &InferenceEnv::default(),
        );
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].dest, VERTEX_EXTENSION);
        assert!(VERTEX_EXTENSION.starts_with("/sandbox/.pi/agent/extensions/"));
        let ext = &seeds[0].content;
        assert!(ext.contains("const PROJECT = \"proj-x\";"), "{ext}");
        assert!(ext.contains("const REGION = \"us-east5\";"), "{ext}");
        assert!(
            ext.contains("const HOST = \"https://us-east5-aiplatform.googleapis.com\";"),
            "{ext}"
        );
        assert!(ext.contains("const MODEL = \"claude-sonnet-5\";"), "{ext}");
        assert!(!ext.contains("__"), "every placeholder is spliced: {ext}");
        assert!(ext.contains("pi.registerProvider(\"crucible\""));
        assert!(ext.contains("\"streamRawPredict\""));
        assert!(ext.contains("vertex-2023-10-16"));
        assert!(ext.contains("GCE_METADATA_HOST"));

        // A model name is a JS string literal, so a quote in it cannot break out.
        a.model = Some("odd\"name".to_string());
        let seeds = Pi.seed_files(
            &a,
            None,
            None,
            &SandboxAuth::Gateway,
            &InferenceEnv::default(),
        );
        assert!(seeds[0].content.contains("const MODEL = \"odd\\\"name\";"));

        // A keyed turn seeds no extension.
        let keyed = seed_files(&a, None, &custom_endpoint());
        assert_eq!(keyed.len(), 1);
        assert_eq!(keyed[0].dest, CONFIG);
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
