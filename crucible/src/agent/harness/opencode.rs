//! The opencode arm of the harness boundary: OpenCode as the harness for an endpoint that speaks
//! only OpenAI Chat Completions (or a direct Anthropic/OpenAI key).
//!
//! `opencode run --format json` mirrors its session's event stream to stdout and exits on the
//! session's idle signal, which in a container can overtake the last `text`/`step_finish` it
//! would have mirrored. The live stream is therefore display only: the sandbox wrapper runs
//! `opencode export` after `run` exits and leaves the session at [`EXPORT`], and that export is
//! the turn's transcript and its ONLY source of result + cost (`backfill_required`).
//!
//! Auth is a direct API key relayed into the sandbox env, or the gateway's ambient Vertex
//! credential when the env names none. The seeded `opencode.json` names the turn's model and
//! registers its endpoint: a keyed one as the `crucible` provider, reading the key back through
//! `{env:…}`, so a custom endpoint, api.openai.com, and api.anthropic.com all take the same
//! shape; Vertex as opencode's own `google-vertex-anthropic` provider, whose google-auth client
//! reaches the metadata emulator like claude's does.

use crate::agent::harness::{
    ApiEndpoint, AuthProvider, Backend, Broker, CRUCIBLE_PROVIDER_ID, HarnessSpec, StreamDecoder,
    TranscriptLocator, TurnArtifacts, api_endpoint, json_str,
};
use crate::agent::inference::{InferenceEnv, WireApi};
use crate::args::Args;
use crate::turn_trace::{self, GenAiRecord, ToolCall, ToolInvocation};
use crucible_harness::tool_summary::summarize;
use crucible_harness::{LiveMeters, OpenCodeJsonParser};
use serde_json::{Value, json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) struct OpenCode;

/// Where the sandbox wrapper leaves the session export; the transcript the turn is read from.
pub(crate) const EXPORT: &str = "/sandbox/.local/share/opencode/crucible-export.json";

/// The in-sandbox wrapper around `opencode run`: mirror the stream to stdout while keeping a
/// copy, then export the session it opened to `$1`. Exits with `run`'s status.
const WRAPPER: &str = r#"set -o pipefail
out="$1"
shift
mkdir -p "$(dirname "$out")"
opencode "$@" | tee "$out.events.jsonl"
rc=${PIPESTATUS[0]}
sid=$(sed -n 's/.*"sessionID":"\([^"]*\)".*/\1/p' "$out.events.jsonl" | head -n 1)
if [ -n "$sid" ]; then
  opencode export "$sid" > "$out"
fi
exit "$rc"
"#;

impl OpenCode {
    pub(crate) const SPEC: HarnessSpec = HarnessSpec {
        name: "opencode",
        binaries: &["/usr/local/bin/opencode"],
        // OpenCode discovers Claude Code's skills tree, so the toolbox lands where claude's does.
        skills_dir: ".claude/skills",
        // Relocates the session store (and the export) off `~/.local/share`.
        home_var: "XDG_DATA_HOME",
        home: "/sandbox/.local/share",
        config: "/sandbox/.config/opencode/opencode.json",
        sandbox_env: &[
            ("XDG_CONFIG_HOME", "/sandbox/.config"),
            ("OPENCODE_DISABLE_AUTOUPDATE", "1"),
            ("OPENCODE_DISABLE_MODELS_FETCH", "1"),
            ("OPENCODE_DISABLE_DEFAULT_PLUGINS", "1"),
            ("OPENCODE_DISABLE_LSP_DOWNLOAD", "1"),
            ("OPENCODE_DISABLE_SHARE", "1"),
            ("OPENCODE_DISABLE_PRUNE", "1"),
        ],
        local_env: &[
            ("OPENCODE_DISABLE_AUTOUPDATE", "1"),
            ("OPENCODE_DISABLE_SHARE", "1"),
        ],
        transcript: TranscriptLocator::File {
            sandbox_path: EXPORT,
        },
        transcript_fetch_timeout: Duration::from_secs(60),
        auth: AuthProvider::ApiKey,
        otel_capable: false,
        backfill_required: true,
    };

    /// The wrapper invocation up to and including `opencode run`'s flags: `bash -c <wrapper>
    /// crucible-opencode <export> run --format json --auto`. The model comes from the seeded
    /// config (which provider it rides on depends on the endpoint) and prompt delivery is
    /// appended by the caller. `--auto` answers every permission ask, the sandboxing crucible
    /// cares about is openshell's.
    fn base_args(export: &str) -> Vec<String> {
        vec![
            "bash".to_string(),
            "-c".to_string(),
            WRAPPER.to_string(),
            "crucible-opencode".to_string(),
            export.to_string(),
            "run".to_string(),
            "--format".to_string(),
            "json".to_string(),
            "--auto".to_string(),
        ]
    }
}

/// OpenCode's built-in Anthropic-on-Vertex provider, the one whose google-auth client the
/// gateway's metadata emulator serves.
const VERTEX_PROVIDER_ID: &str = "google-vertex-anthropic";

/// The model as opencode names it: `<provider>/<model>`, under the provider the endpoint lands on.
fn model_ref(model: &str, endpoint: &ApiEndpoint) -> String {
    let provider = match endpoint {
        ApiEndpoint::OpenAi { .. } | ApiEndpoint::Anthropic { .. } => CRUCIBLE_PROVIDER_ID,
        ApiEndpoint::Vertex(_) => VERTEX_PROVIDER_ID,
    };
    format!("{provider}/{model}")
}

/// Where a local turn's wrapper leaves the export: the same place under this machine's data home.
fn local_export_path() -> std::path::PathBuf {
    let data_home = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| std::path::PathBuf::from(h).join(".local/share"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from(".local/share"));
    data_home.join("opencode/crucible-export.json")
}

/// Render `opencode.json`: every permission allowed, the turn's model under its provider (a
/// keyed endpoint as `crucible`: the URL, its API family, and the env var the key is read from;
/// Vertex as opencode's own provider carrying the project and region), and the broker as a
/// remote MCP server when it is on.
fn config_json(model: &str, endpoint: &ApiEndpoint, broker: Option<&Broker<'_>>) -> String {
    let keyed = |npm: &str, base_url: &str, key_env: &str| {
        json!({
            "npm": npm,
            "name": "Crucible inference endpoint",
            "options": {
                "baseURL": base_url,
                "apiKey": format!("{{env:{key_env}}}"),
            },
        })
    };
    let (provider_id, mut provider) = match endpoint {
        ApiEndpoint::OpenAi { base_url, wire_api } => (
            CRUCIBLE_PROVIDER_ID,
            keyed(
                match wire_api {
                    WireApi::Chat => "@ai-sdk/openai-compatible",
                    WireApi::Responses => "@ai-sdk/openai",
                },
                base_url,
                crate::agent::inference::OPENAI_API_KEY_ENV,
            ),
        ),
        ApiEndpoint::Anthropic { base_url } => (
            CRUCIBLE_PROVIDER_ID,
            keyed(
                "@ai-sdk/anthropic",
                base_url,
                crate::agent::inference::ANTHROPIC_API_KEY,
            ),
        ),
        ApiEndpoint::Vertex(vertex) => (
            VERTEX_PROVIDER_ID,
            json!({
                "options": { "project": vertex.project, "location": vertex.region },
            }),
        ),
    };
    provider["models"] = json!({ model: { "name": model } });
    let mut cfg = json!({
        "$schema": "https://opencode.ai/config.json",
        "model": model_ref(model, endpoint),
        "permission": { "*": "allow" },
        "provider": { provider_id: provider },
    });
    if let Some(b) = broker {
        let mut server = json!({ "type": "remote", "url": b.url, "enabled": true });
        if let Some(t) = b.token {
            server["headers"] = json!({ "Authorization": format!("Bearer {t}") });
        }
        cfg["mcp"] = json!({ b.name: server });
    }
    cfg.to_string()
}

impl Backend for OpenCode {
    fn spec(&self) -> &'static HarnessSpec {
        &Self::SPEC
    }

    /// A local turn has no seeded config, so the model is named on the command line under the
    /// `crucible` provider this machine's own opencode config is expected to carry. The prompt
    /// rides as the message positional after `--`, so one starting with `-` is still a message.
    fn local_argv(&self, args: &Args, prompt: &str) -> Vec<String> {
        let mut a = Self::base_args(&local_export_path().to_string_lossy());
        a.push("--model".to_string());
        a.push(format!("{CRUCIBLE_PROVIDER_ID}/{}", args.model()));
        a.push("--".to_string());
        a.push(prompt.to_string());
        a
    }

    /// No message positional: `run` reads a piped stdin as the message, which the shared exec
    /// wrapper redirects from the uploaded prompt file. The model and the MCP servers come from
    /// the seeded config, not argv, so `args` and `mcp_seeded` are accepted and unused by design.
    fn sandbox_argv(&self, _args: &Args, _mcp_seeded: bool) -> Vec<String> {
        Self::base_args(EXPORT)
    }

    /// `opencode.json`, ALWAYS (it carries the model, the provider, and the permission posture).
    fn config(
        &self,
        args: &Args,
        broker: Option<&Broker<'_>>,
        inference: &InferenceEnv,
    ) -> Option<String> {
        Some(config_json(
            args.model(),
            &api_endpoint(args.model(), inference, &args.env),
            broker,
        ))
    }

    fn decoder(
        &self,
        args: &Args,
        _meters: Option<&LiveMeters>,
        tool_io: bool,
    ) -> Box<dyn StreamDecoder> {
        Box::new(
            OpenCodeJsonParser::new(args.model())
                .with_price(crate::agent::event::estimate_cost)
                .with_tool_io(tool_io),
        )
    }

    fn parse_transcript(&self, content: &[u8]) -> TurnArtifacts {
        export_artifacts(content)
    }

    fn content_records(&self, content: &[u8]) -> Vec<GenAiRecord> {
        export_records(content)
    }

    /// Read the export the local wrapper left under this machine's data home.
    fn local_backfill(&self, _paths: &crate::args::Paths) -> Option<TurnArtifacts> {
        let bytes = std::fs::read(local_export_path()).ok()?;
        let artifacts = export_artifacts(&bytes);
        (!artifacts.events.is_empty()).then_some(artifacts)
    }
}

/// The session export's messages: `{info, parts}` pairs in conversation order.
fn export_messages(content: &[u8]) -> Vec<Value> {
    serde_json::from_slice::<Value>(content)
        .ok()
        .and_then(|mut doc| doc.get_mut("messages").map(Value::take))
        .and_then(|m| match m {
            Value::Array(items) => Some(items),
            _ => None,
        })
        .unwrap_or_default()
}

fn millis(v: &Value) -> Option<SystemTime> {
    v.as_u64().map(|ms| UNIX_EPOCH + Duration::from_millis(ms))
}

/// The turn as the export tells it: one token sample and the result over every assistant message
/// (usage summed, cost summed, the first `error` latched), plus one span per tool part. An export
/// with no assistant message yields empty artifacts, which the fetch layer reports loudly.
fn export_artifacts(content: &[u8]) -> TurnArtifacts {
    let mut tokens = crucible_contract::event::Tokens {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        total: 0,
        rate: None,
        cost_usd: None,
    };
    let mut cost = 0.0;
    let mut turns = 0u32;
    let mut error: Option<String> = None;
    let mut tool_calls = Vec::new();
    for msg in export_messages(content) {
        let info = msg.get("info").unwrap_or(&Value::Null);
        if json_str(info, "role") != "assistant" {
            continue;
        }
        turns += 1;
        let t = info.get("tokens").unwrap_or(&Value::Null);
        let cache = t.get("cache").unwrap_or(&Value::Null);
        tokens.input += t.get("input").and_then(Value::as_u64).unwrap_or(0);
        tokens.output += t.get("output").and_then(Value::as_u64).unwrap_or(0);
        tokens.cache_read += cache.get("read").and_then(Value::as_u64).unwrap_or(0);
        tokens.cache_write += cache.get("write").and_then(Value::as_u64).unwrap_or(0);
        cost += info.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
        if error.is_none()
            && let Some(e) = info.get("error").filter(|e| !e.is_null())
        {
            let message = e
                .get("data")
                .map(|d| json_str(d, "message"))
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| {
                    let name = json_str(e, "name");
                    if name.is_empty() { e.to_string() } else { name }
                });
            error = Some(message);
        }
        for part in msg
            .get("parts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if json_str(part, "type") != "tool" {
                continue;
            }
            let state = part.get("state").unwrap_or(&Value::Null);
            let name = json_str(part, "tool");
            let input = state.get("input").unwrap_or(&Value::Null);
            let mut summary = summarize(&name, input);
            if summary.is_empty() {
                summary = json_str(state, "title");
            }
            let time = state.get("time").unwrap_or(&Value::Null);
            tool_calls.push(ToolInvocation {
                name,
                start: time.get("start").and_then(millis).unwrap_or(UNIX_EPOCH),
                end: time.get("end").and_then(millis),
                summary: turn_trace::truncate(&summary, 200),
                error: json_str(state, "status") == "error",
            });
        }
    }
    if turns == 0 {
        return TurnArtifacts {
            events: Vec::new(),
            cost_usd: None,
            tool_calls,
        };
    }
    tokens.total = tokens.input + tokens.output + tokens.cache_read + tokens.cache_write;
    let cost_usd = (cost > 0.0).then_some(cost);
    tokens.cost_usd = cost_usd;
    let events = vec![
        crucible_contract::event::AgentEvent::Tokens(tokens),
        crucible_contract::event::AgentEvent::Result {
            subtype: if error.is_some() { "error" } else { "success" }.to_string(),
            is_error: error.is_some(),
            turns,
            cost_usd: cost,
            error,
        },
    ];
    TurnArtifacts {
        events,
        cost_usd,
        tool_calls,
    }
}

/// The conversation as GenAI records: the user's text parts, each assistant message's text,
/// reasoning, and tool calls, then one tool record per tool part's output.
fn export_records(content: &[u8]) -> Vec<GenAiRecord> {
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
    for msg in export_messages(content) {
        let info = msg.get("info").unwrap_or(&Value::Null);
        let parts = msg.get("parts").and_then(Value::as_array);
        let parts = parts.into_iter().flatten();
        match json_str(info, "role").as_str() {
            "user" => {
                let text = parts
                    .filter(|p| json_str(p, "type") == "text")
                    .map(|p| json_str(p, "text"))
                    .collect::<Vec<_>>()
                    .join("\n");
                if let Some(content) = body(&text) {
                    out.push(GenAiRecord::User { content });
                }
            }
            "assistant" => {
                let mut text = Vec::new();
                let mut reasoning = Vec::new();
                let mut tool_calls = Vec::new();
                let mut results = Vec::new();
                for part in parts {
                    match json_str(part, "type").as_str() {
                        "text" => text.push(json_str(part, "text")),
                        "reasoning" => reasoning.push(json_str(part, "text")),
                        "tool" => {
                            let state = part.get("state").unwrap_or(&Value::Null);
                            let id = json_str(part, "callID");
                            let input = state.get("input").cloned().unwrap_or(Value::Null);
                            tool_calls.push(ToolCall {
                                id: id.clone(),
                                name: json_str(part, "tool"),
                                arguments: body(&input.to_string()).unwrap_or_default(),
                            });
                            let output = json_str(state, "output");
                            let is_error = json_str(state, "status") == "error";
                            let content = if is_error && output.is_empty() {
                                json_str(state, "error")
                            } else {
                                output
                            };
                            results.push(GenAiRecord::Tool {
                                id,
                                content: body(&content).unwrap_or_default(),
                                is_error,
                            });
                        }
                        _ => {}
                    }
                }
                let text = body(&text.join("\n"));
                let reasoning = body(&reasoning.join("\n"));
                if text.is_some() || reasoning.is_some() || !tool_calls.is_empty() {
                    let model = json_str(info, "modelID");
                    out.push(GenAiRecord::Assistant {
                        text,
                        reasoning,
                        tool_calls,
                        model: (!model.is_empty()).then_some(model),
                    });
                }
                out.extend(results);
            }
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
    use crucible_contract::event::AgentEvent;

    const CONFIG: &str = OpenCode::SPEC.config;

    fn args() -> Args {
        crate::cli::Cli::parse_from(["crucible"]).run
    }

    fn seed_files(
        args: &Args,
        broker_url: Option<&str>,
        broker_token: Option<&str>,
        inference: &InferenceEnv,
    ) -> Vec<SeedFile> {
        OpenCode.seed_files(
            args,
            broker_url,
            broker_token,
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

    const RUN_FLAGS: &[&str] = &["run", "--format", "json", "--auto"];

    /// The argv is the bash wrapper around `opencode run`, with the export path as its first
    /// argument, the run flags after it, and the prompt over stdin in the sandbox. The sandbox
    /// names no model (the seeded config does); a local turn names it under `crucible`.
    #[test]
    fn invocation_is_the_export_wrapper_around_headless_run() {
        let mut a = args();
        a.model = Some("qwen-3-8-27b".to_string());
        let sandbox = OpenCode.sandbox_argv(&a, true);
        assert_eq!(&sandbox[..2], &["bash", "-c"]);
        assert_eq!(sandbox[2], WRAPPER);
        assert_eq!(sandbox[3], "crucible-opencode");
        assert_eq!(sandbox[4], EXPORT);
        assert_eq!(&sandbox[5..], RUN_FLAGS, "no model, no message positional");

        let local = OpenCode.local_argv(&a, "--- looks like a flag");
        assert_eq!(&local[..4], &sandbox[..4]);
        assert!(
            local[4].ends_with("opencode/crucible-export.json"),
            "{local:?}"
        );
        assert_eq!(&local[5..9], RUN_FLAGS);
        assert_eq!(&local[9..11], &["--model", "crucible/qwen-3-8-27b"]);
        assert_eq!(&local[local.len() - 2..], &["--", "--- looks like a flag"]);
    }

    /// No key and no base URL is the ambient Vertex credential: the config rides opencode's own
    /// Vertex Anthropic provider with the manifest's project and region, no `crucible` provider
    /// and no key reference, and the model is named under it.
    #[test]
    fn config_rides_opencodes_vertex_provider_on_the_ambient_credential() {
        let mut a = args();
        a.model = Some("claude-sonnet-5".to_string());
        a.env = vec![
            ("ANTHROPIC_VERTEX_PROJECT_ID".into(), "proj-x".into()),
            ("CLOUD_ML_REGION".into(), "us-east5".into()),
        ];
        let seeds = OpenCode.seed_files(
            &a,
            None,
            None,
            &SandboxAuth::Gateway,
            &InferenceEnv::default(),
        );
        assert_eq!(seeds.len(), 1);
        let v: Value = serde_json::from_str(&seeds[0].content).expect("valid json");
        assert_eq!(v["model"], "google-vertex-anthropic/claude-sonnet-5");
        let p = &v["provider"]["google-vertex-anthropic"];
        assert_eq!(p["options"]["project"], "proj-x");
        assert_eq!(p["options"]["location"], "us-east5");
        assert_eq!(p["models"]["claude-sonnet-5"]["name"], "claude-sonnet-5");
        assert!(v["provider"].get("crucible").is_none());
        assert!(!seeds[0].content.contains("apiKey"), "{}", seeds[0].content);
    }

    /// The wrapper has to parse where it runs, and it must forward `run`'s exit status rather
    /// than `tee`'s.
    #[test]
    fn wrapper_is_valid_shell_and_forwards_the_run_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("bin dir");
        // A stand-in `opencode`: `run` prints one event and fails; `export` echoes its argument.
        std::fs::write(
            bin.join("opencode"),
            "#!/bin/bash\nif [ \"$1\" = run ]; then echo '{\"type\":\"step_start\",\"sessionID\":\"ses_1\"}'; exit 3; fi\nif [ \"$1\" = export ]; then echo \"{\\\"exported\\\":\\\"$2\\\"}\"; fi\n",
        )
        .expect("stub");
        std::fs::set_permissions(
            bin.join("opencode"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .expect("chmod");
        let export = dir.path().join("data/opencode/crucible-export.json");
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(WRAPPER)
            .arg("crucible-opencode")
            .arg(&export)
            .arg("run")
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .output()
            .expect("bash");
        assert_eq!(out.status.code(), Some(3), "run's status is forwarded");
        assert_eq!(
            std::fs::read_to_string(&export)
                .expect("export written")
                .trim(),
            r#"{"exported":"ses_1"}"#
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("step_start"),
            "the stream still reaches stdout"
        );
    }

    #[test]
    fn env_script_relocates_the_data_home_and_carries_no_credential() {
        let s = OpenCode.env_script(&[("FOO".into(), "bar".into())]);
        assert!(s.contains("export AGENT_TOOL=opencode"));
        assert!(s.contains("export XDG_DATA_HOME=/sandbox/.local/share"));
        assert!(s.contains("export XDG_CONFIG_HOME=/sandbox/.config"));
        assert!(s.contains("export OPENCODE_DISABLE_MODELS_FETCH=1"));
        assert!(s.contains("export FOO='bar'"));
        assert!(
            !s.contains("API_KEY"),
            "the key rides the inference env, not the spec: {s}"
        );
        assert!(CONFIG.starts_with("/sandbox/.config/opencode/"));
    }

    /// A custom OpenAI-speaking endpoint becomes the `crucible` provider on the openai-compatible
    /// SDK, keyed off `OPENAI_API_KEY`; every permission is allowed; the model rides under it.
    #[test]
    fn config_registers_a_custom_endpoint_as_the_crucible_provider() {
        let mut a = args();
        a.model = Some("qwen-3-8-27b".to_string());
        let seeds = seed_files(&a, None, None, &custom_endpoint());
        assert_eq!(
            seeds.len(),
            1,
            "opencode.json is always seeded, nothing else"
        );
        assert_eq!(seeds[0].dest, CONFIG);
        let v: Value = serde_json::from_str(&seeds[0].content).expect("valid json");
        assert_eq!(v["model"], "crucible/qwen-3-8-27b");
        assert_eq!(v["permission"]["*"], "allow");
        let p = &v["provider"]["crucible"];
        assert_eq!(p["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(p["options"]["baseURL"], "http://vllm.internal:8000/v1");
        assert_eq!(p["options"]["apiKey"], "{env:OPENAI_API_KEY}");
        assert_eq!(p["models"]["qwen-3-8-27b"]["name"], "qwen-3-8-27b");
        assert!(v.get("mcp").is_none());
    }

    /// A direct Anthropic key selects the anthropic SDK against Anthropic's API (or the base URL
    /// the env names), keyed off `ANTHROPIC_API_KEY`.
    #[test]
    fn config_rides_the_openai_sdk_on_the_vendors_own_api() {
        let mut a = args();
        a.model = Some("gpt-5.6-luna".to_string());
        let key_only = InferenceEnv {
            openai_key: Some("sk-oa".into()),
            ..Default::default()
        };
        let seeds = seed_files(&a, None, None, &key_only);
        let v: Value = serde_json::from_str(&seeds[0].content).expect("valid json");
        let p = &v["provider"]["crucible"];
        assert_eq!(
            p["npm"], "@ai-sdk/openai",
            "Responses, not chat completions"
        );
        assert_eq!(p["options"]["baseURL"], "https://api.openai.com/v1");
        assert_eq!(p["options"]["apiKey"], "{env:OPENAI_API_KEY}");
    }

    #[test]
    fn config_registers_anthropic_for_a_direct_anthropic_key() {
        let a = args();
        let inference = InferenceEnv {
            anthropic_key: Some("sk-ant".into()),
            ..Default::default()
        };
        let seeds = seed_files(&a, None, None, &inference);
        let v: Value = serde_json::from_str(&seeds[0].content).expect("valid json");
        let p = &v["provider"]["crucible"];
        assert_eq!(p["npm"], "@ai-sdk/anthropic");
        assert_eq!(p["options"]["baseURL"], "https://api.anthropic.com");
        assert_eq!(p["options"]["apiKey"], "{env:ANTHROPIC_API_KEY}");
        assert_eq!(v["model"], format!("crucible/{}", a.model()));
    }

    #[test]
    fn config_merges_the_broker_as_a_remote_mcp_server_with_a_bearer_header() {
        let a = args();
        let seeds = seed_files(
            &a,
            Some("http://10.0.0.1:8000/mcp"),
            Some("tok\"quoted"),
            &custom_endpoint(),
        );
        let v: Value = serde_json::from_str(&seeds[0].content).expect("valid json");
        let server = &v["mcp"][a.broker.name.as_str()];
        assert_eq!(server["type"], "remote");
        assert_eq!(server["url"], "http://10.0.0.1:8000/mcp");
        assert_eq!(server["enabled"], true);
        assert_eq!(server["headers"]["Authorization"], "Bearer tok\"quoted");

        let bare = seed_files(
            &a,
            Some("http://10.0.0.1:8000/mcp"),
            None,
            &custom_endpoint(),
        );
        let v: Value = serde_json::from_str(&bare[0].content).expect("valid json");
        assert!(v["mcp"][a.broker.name.as_str()].get("headers").is_none());
    }

    const EXPORT_FIXTURE: &[u8] = include_bytes!("../../testdata/opencode_export_fixture.json");

    /// The banked export (a real `opencode export` after a `run` against a scripted endpoint)
    /// closes the turn: usage summed over both assistant messages, one span for the write.
    #[test]
    fn the_export_yields_the_turn_result_usage_and_tool_spans() {
        let art = OpenCode.parse_transcript(EXPORT_FIXTURE);
        match &art.events[..] {
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
                assert_eq!(
                    (t.input, t.output, t.cache_read, t.cache_write),
                    (500, 50, 0, 0)
                );
                assert_eq!(t.total, 550);
                assert_eq!(t.cost_usd, None, "opencode priced the custom model at zero");
                assert_eq!(subtype, "success");
                assert!(!*is_error);
                assert_eq!(*turns, 2);
                assert_eq!(*cost_usd, 0.0);
                assert!(error.is_none());
            }
            other => panic!("expected Tokens + Result, got {other:?}"),
        }
        assert_eq!(art.cost_usd, None);
        assert_eq!(art.tool_calls.len(), 1);
        let call = &art.tool_calls[0];
        assert_eq!(call.name, "write");
        assert_eq!(call.summary, "hello.txt");
        assert!(!call.error);
        assert!(call.end.is_some_and(|e| e >= call.start));
    }

    #[test]
    fn the_export_yields_the_conversation_records() {
        let records = OpenCode.content_records(EXPORT_FIXTURE);
        assert!(
            matches!(&records[0], GenAiRecord::User { content } if content.starts_with("Run the shell command")),
            "{records:?}"
        );
        let assistants: Vec<&GenAiRecord> = records
            .iter()
            .filter(|r| matches!(r, GenAiRecord::Assistant { .. }))
            .collect();
        assert_eq!(assistants.len(), 2);
        match assistants[0] {
            GenAiRecord::Assistant {
                tool_calls,
                model,
                text,
                ..
            } => {
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].name, "write");
                assert_eq!(tool_calls[0].id, "call_2");
                assert!(tool_calls[0].arguments.contains("hello.txt"));
                assert_eq!(model.as_deref(), Some("qwen-3-8-27b"));
                assert!(text.is_none());
            }
            other => panic!("{other:?}"),
        }
        assert!(
            records
                .iter()
                .any(|r| matches!(r, GenAiRecord::Tool { id, content, is_error }
                if id == "call_2" && content.contains("Wrote file") && !is_error)),
            "{records:?}"
        );
        match assistants[1] {
            GenAiRecord::Assistant { text, .. } => {
                assert!(
                    text.as_deref()
                        .is_some_and(|t| t.starts_with("Ran echo and wrote hello.txt.")),
                    "{text:?}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_assistant_error_makes_the_result_an_error() {
        let export = json!({"info": {}, "messages": [
            {"info": {"role": "assistant", "cost": 0, "tokens": {"input": 5, "output": 0, "cache": {"read": 0, "write": 0}},
              "error": {"name": "ProviderAuthError", "data": {"providerID": "crucible", "message": "401 Unauthorized"}}},
             "parts": []}
        ]})
        .to_string();
        let art = OpenCode.parse_transcript(export.as_bytes());
        match &art.events[..] {
            [
                AgentEvent::Tokens(_),
                AgentEvent::Result {
                    is_error, error, ..
                },
            ] => {
                assert!(*is_error);
                assert_eq!(error.as_deref(), Some("401 Unauthorized"));
            }
            other => panic!("expected Tokens + Result, got {other:?}"),
        }
    }

    /// No assistant message means the model never answered: nothing to close the turn with, which
    /// the fetch layer reports as a missing result rather than a $0 success.
    #[test]
    fn garbage_or_empty_export_is_empty_never_panics() {
        for bytes in [
            &b"not json"[..],
            b"{}",
            br#"{"messages":[{"info":{"role":"user"},"parts":[]}]}"#,
            &[0xff, 0xfe],
        ] {
            let art = OpenCode.parse_transcript(bytes);
            assert!(art.events.is_empty(), "{bytes:?}");
            assert!(art.tool_calls.is_empty());
            assert!(
                OpenCode.content_records(bytes).is_empty() || bytes.starts_with(b"{\"messages\"")
            );
        }
    }
}
