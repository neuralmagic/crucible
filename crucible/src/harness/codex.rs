//! The codex arm of the harness boundary: OpenAI's Codex CLI as a third harness.
//!
//! `codex exec --json` emits a type-tagged JSONL event stream, so the live decoder carries the
//! turn's result and token usage and `backfill_required` stays false; the rollout file under
//! `$CODEX_HOME/sessions` is trace garnish, same posture as claude.
//!
//! Auth is not Vertex: codex talks to the ChatGPT backend with an OAuth access token the loop
//! process mints host-side ([`crate::openshell::provider::mint_codex_token`]) and seeds as
//! `$CODEX_HOME/auth.json`. Provider-delivered env reaches the sandbox as an
//! `openshell:resolve:env:` placeholder that only the L7 egress proxy resolves, and codex talks to
//! `wss://api.openai.com/v1/responses` over an L4 tunnel, so the token has to be in the file. The
//! sandbox holds a short-lived access token and never a refresh token, so exactly one process ever
//! performs the refresh grant.

use crate::Args;
use crate::agent::ReasoningEffort;
use crate::harness::{SeedFile, TurnArtifacts, append_manifest_env};
use crate::openshell::provider::CodexToken;
use crate::turn_trace::{self, GenAiRecord, ToolCall, ToolInvocation};
use jiff::Timestamp;
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

/// The default model when neither the CLI nor the manifest names one.
pub(crate) const DEFAULT_MODEL: &str = "gpt-5.6-sol";

/// The codex CLI's path in the sandbox image.
pub(crate) const DEFAULT_BINARIES: &[&str] = &["/usr/local/bin/codex"];

/// Codex has no skills discovery; the toolbox still lands where domain prompts reference it.
pub(crate) const SKILLS_DIR: &str = ".claude/skills";

/// `$CODEX_HOME` inside the sandbox: relocates config, auth, and the rollout store off `~/.codex`.
pub(crate) const CODEX_HOME: &str = "/sandbox/.codex";

/// The rollout store, `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl`.
pub(crate) const SESSIONS: &str = "/sandbox/.codex/sessions";

/// The rollout file's glob relative to [`SESSIONS`]: three date segments, then the file.
pub(crate) const TRANSCRIPT_GLOB: &str = "*/*/*/rollout-*.jsonl";

/// Codex's config file (model, approvals, MCP servers), rendered host-side and seeded.
pub(crate) const CONFIG: &str = "/sandbox/.codex/config.toml";

/// Where the seeded access token lands; codex reads it at startup.
pub(crate) const AUTH: &str = "/sandbox/.codex/auth.json";

/// The live `--json` stream carries result + usage, so the rollout fetch is telemetry only and
/// must never wedge the turn: claude's number.
pub(crate) const TRANSCRIPT_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Harness env defaults for a local codex spawn (manifest `[agent].env` still wins). `CODEX_HOME`
/// is deliberately absent: a local spawn uses the operator's own `~/.codex`.
pub(crate) const LOCAL_ENV_DEFAULTS: &[(&str, &str)] = &[("AGENT_TOOL", "codex")];

/// Egress hosts a codex turn needs on top of the shared defaults. `full` (raw L4 tunnel, like
/// github's default) rather than `read-write`: the proxy applies protocol handling to read-write
/// endpoints, and codex's streaming connection to the ChatGPT backend dies mid-stream through it.
pub(crate) const EXTRA_ENDPOINTS: &[&str] = &[
    "chatgpt.com:443:full",
    "auth.openai.com:443:full",
    "api.openai.com:443:full",
    "ab.chatgpt.com:443:full",
];

/// The model for this turn: `[agent.codex].model` overrides the shared `[agent].model`, and a
/// Claude name in the shared slot falls back to [`DEFAULT_MODEL`]. Both `--model` and the
/// manifest's `[agent].model` default to a Claude model (the default harness owns that default),
/// and the ChatGPT backend rejects an Anthropic model name with a 400.
pub(crate) fn model(args: &Args) -> &str {
    match args.codex.model.as_deref() {
        Some(m) => m,
        None if args.model.starts_with("claude") => DEFAULT_MODEL,
        None => &args.model,
    }
}

/// Crucible's five reasoning tiers onto codex's three (`model_reasoning_effort`).
pub(crate) fn reasoning_effort(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High | ReasoningEffort::Xhigh | ReasoningEffort::Max => "high",
    }
}

/// The shared invocation prefix: `codex exec --json --dangerously-bypass-approvals-and-sandbox
/// --skip-git-repo-check --color never -m <model> [-c model_reasoning_effort=<tier>]`. Prompt
/// delivery is appended by the caller. The bypass flag is what makes the run headless; the
/// sandboxing crucible cares about is openshell's, not codex's own. `--skip-git-repo-check` keeps
/// a workdir that is not a repo runnable, and `--color never` keeps ANSI escapes out of the
/// captured stream.
fn base_args(args: &Args) -> Vec<String> {
    let mut a = vec![
        "codex".to_string(),
        "exec".to_string(),
        "--json".to_string(),
        "--dangerously-bypass-approvals-and-sandbox".to_string(),
        "--skip-git-repo-check".to_string(),
        "--color".to_string(),
        "never".to_string(),
        "-m".to_string(),
        model(args).to_string(),
    ];
    if let Some(effort) = args.reasoning_effort {
        a.push("-c".to_string());
        a.push(format!(
            "model_reasoning_effort={}",
            reasoning_effort(effort)
        ));
    }
    a
}

/// The full local-spawn argv: the prompt rides inline as the trailing positional.
pub(crate) fn local_argv(args: &Args, prompt: &str) -> Vec<String> {
    let mut a = base_args(args);
    a.push(prompt.to_string());
    a
}

/// The sandbox argv: a trailing `-` reads the prompt from stdin, which the shared exec wrapper
/// redirects from the uploaded prompt file. Codex reads MCP servers from its `config.toml`, not
/// argv, so `mcp_seeded` is accepted and unused by design.
pub(crate) fn sandbox_argv(args: &Args, _mcp_seeded: bool) -> Vec<String> {
    let mut a = base_args(args);
    a.push("-".to_string());
    a
}

/// The agent's env script (sourced before codex runs in the sandbox): `AGENT_TOOL`, `CODEX_HOME`,
/// then the manifest's `[agent].env`. No credential is ever exported: the access token reaches the
/// sandbox as the seeded `auth.json` ([`auth_json`]).
pub(crate) fn env_script(env: &[(String, String)]) -> String {
    let lines = vec![
        "export AGENT_TOOL=codex".to_string(),
        format!("export CODEX_HOME={CODEX_HOME}"),
    ];
    append_manifest_env(lines, env)
}

/// Stands in for the refresh token codex requires as a field but must never hold: the loop process
/// is the single refresher, so any refresh the sandbox attempts has to fail, and it fails naming
/// this instead of an empty string.
pub(crate) const WITHHELD_REFRESH_TOKEN: &str = "withheld-crucible-refreshes-host-side";

/// `$CODEX_HOME/auth.json` as codex's `AuthDotJson` parses it. All four `tokens` fields must be
/// present and `id_token` must be a real JWT: a missing or unparseable field makes serde drop
/// `tokens` wholesale and codex then runs unauthenticated into a 401 reconnect loop rather than
/// erroring. `last_refresh` is the mint time, or codex's 8-day staleness rule triggers a refresh
/// the sandbox cannot perform.
fn auth_json(token: &CodexToken, last_refresh: &str) -> String {
    serde_json::json!({
        "OPENAI_API_KEY": serde_json::Value::Null,
        "auth_mode": "chatgpt",
        "tokens": {
            "access_token": token.access_token,
            "account_id": token.account_id,
            "id_token": token.id_token,
            "refresh_token": WITHHELD_REFRESH_TOKEN,
        },
        "last_refresh": last_refresh,
    })
    .to_string()
}

/// Files seeded into the sandbox before exec: codex's `config.toml`, ALWAYS (it carries the model
/// and the approval/sandbox posture), plus `auth.json` whenever the turn minted a token. When the
/// broker is on, its streamable-HTTP MCP server is merged into the config with a bearer token,
/// since the broker binds `0.0.0.0` and the token is what makes the sandbox its only caller.
pub(crate) fn seed_files(
    args: &Args,
    broker_url: Option<&str>,
    broker_token: Option<&str>,
    auth: Option<&CodexToken>,
) -> Vec<SeedFile> {
    let mut seeds = vec![SeedFile {
        content: config_toml(model(args), &args.broker.name, broker_url, broker_token),
        dest: CONFIG,
    }];
    if let Some(token) = auth {
        seeds.push(SeedFile {
            content: auth_json(token, &Timestamp::now().to_string()),
            dest: AUTH,
        });
    }
    seeds
}

/// Render codex's `config.toml`. Serialized through the `toml` crate rather than formatted, so a
/// token or server name carrying a quote cannot break out of its value.
fn config_toml(
    model: &str,
    broker_name: &str,
    broker_url: Option<&str>,
    token: Option<&str>,
) -> String {
    let mut cfg = toml::Table::new();
    cfg.insert("model".into(), model.into());
    cfg.insert("approval_policy".into(), "never".into());
    cfg.insert("sandbox_mode".into(), "danger-full-access".into());
    if let Some(url) = broker_url {
        let mut server = toml::Table::new();
        server.insert("url".into(), url.into());
        if let Some(t) = token {
            // Codex's schema has no inline `bearer_token`; the choices are `bearer_token_env_var`
            // (an env name) or static `http_headers`. The header keeps the token in the seeded
            // file rather than the sandbox env, same posture as hermes's config.yaml.
            let mut headers = toml::Table::new();
            headers.insert("Authorization".into(), format!("Bearer {t}").into());
            server.insert("http_headers".into(), headers.into());
        }
        let mut servers = toml::Table::new();
        servers.insert(broker_name.into(), server.into());
        cfg.insert("mcp_servers".into(), servers.into());
    }
    toml::to_string(&cfg).unwrap_or_default()
}

/// The longest tool hint kept on a span, matched to `turn_trace`'s cap (identical span behavior).
const SUMMARY_CAP: usize = 200;

/// One rollout line: the envelope's `timestamp` and `type`, and the `payload` it wraps.
struct Line {
    at: SystemTime,
    kind: String,
    payload: Value,
}

/// Every parseable rollout line, in file order. A line that is not JSON, or carries no envelope
/// `type`, is skipped: the rollout is telemetry and a torn tail must not cost the whole trace.
fn rollout_lines(text: &str) -> Vec<Line> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let mut v: Value = serde_json::from_str(l).ok()?;
            let at = v
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(turn_trace::parse_ts)
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let kind = v.get("type")?.as_str()?.to_string();
            let payload = v.get_mut("payload").map(Value::take).unwrap_or(Value::Null);
            Some(Line { at, kind, payload })
        })
        .collect()
}

/// The payload's own `type` discriminant (`message`, `custom_tool_call`, …).
fn payload_type(payload: &Value) -> &str {
    payload.get("type").and_then(Value::as_str).unwrap_or("")
}

/// A payload's string field, empty when absent.
fn field(payload: &Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Tool spans recovered from the downloaded rollout JSONL. The live stream is the turn's source of
/// result + cost (`backfill_required` is false), so an unreadable rollout costs only trace detail.
pub(crate) fn parse_transcript(content: &[u8]) -> TurnArtifacts {
    let text = String::from_utf8_lossy(content);
    let redact = turn_trace::redact_enabled();
    let mut order: Vec<ToolInvocation> = Vec::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    for line in rollout_lines(&text) {
        if line.kind != "response_item" {
            continue;
        }
        match payload_type(&line.payload) {
            "custom_tool_call" => {
                let input = field(&line.payload, "input");
                let name = tool_name(&input, &field(&line.payload, "name"));
                let status = field(&line.payload, "status");
                by_id.insert(field(&line.payload, "call_id"), order.len());
                order.push(ToolInvocation {
                    name: name.clone(),
                    start: line.at,
                    end: None,
                    summary: call_summary(&name, &input, redact),
                    error: !status.is_empty() && status != "completed",
                });
            }
            "custom_tool_call_output" => {
                if let Some(i) = by_id.get(&field(&line.payload, "call_id")).copied()
                    && let Some(call) = order.get_mut(i)
                {
                    call.end = Some(line.at);
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

/// The real tool behind a rollout call. Codex's sol models route everything through one `exec`
/// custom tool whose `input` is a JavaScript snippet, so the tool is the `tools.<fn>` it calls;
/// `declared` (the payload's own `name`) is the fallback for a model that names its tools directly.
/// `exec_command` maps to `shell` so a span matches what the live decoder emitted for it.
fn tool_name(input: &str, declared: &str) -> String {
    match js_tool(input) {
        Some("exec_command") => "shell".to_string(),
        Some(f) => f.to_string(),
        None => declared.to_string(),
    }
}

/// The identifier after the first `tools.` in a JavaScript tool snippet.
fn js_tool(input: &str) -> Option<&str> {
    let rest = input.split_once("tools.")?.1;
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

/// The sanitized one-line hint for a rollout call: the shell command, the patched paths, or the
/// snippet's first line. Redacted then capped, exactly like a claude span's hint.
fn call_summary(name: &str, input: &str, redact: bool) -> String {
    let hint = match name {
        "shell" => first_json_object(input)
            .as_ref()
            .map(|o| field(o, "cmd"))
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| first_line(input)),
        "apply_patch" => {
            let paths = patch_paths(input);
            if paths.is_empty() {
                first_line(input)
            } else {
                paths.join(", ")
            }
        }
        _ => first_line(input),
    };
    let hint = if redact {
        turn_trace::redact(&hint)
    } else {
        hint
    };
    turn_trace::truncate(&hint, SUMMARY_CAP)
}

/// The first brace-balanced JSON object embedded in a JavaScript snippet (the tool's arguments),
/// string-aware so a brace inside a quoted value does not close it early.
fn first_json_object(input: &str) -> Option<Value> {
    let start = input.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in input[start..].char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&input[start..start + i + c.len_utf8()]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

/// The files an `apply_patch` snippet touches, read off the patch envelope's file headers (the
/// patch text is a JS string literal, so its newlines are still escaped).
fn patch_paths(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    for marker in ["Add File: ", "Update File: ", "Delete File: ", "Move to: "] {
        let mut rest = input;
        while let Some((_, tail)) = rest.split_once(marker) {
            let end = tail
                .find("\\n")
                .or_else(|| tail.find('"'))
                .unwrap_or(tail.len());
            let path = tail[..end].trim();
            if !path.is_empty() {
                out.push(path.to_string());
            }
            rest = &tail[end..];
        }
    }
    out
}

/// The snippet's first non-empty line, the last-resort hint.
fn first_line(input: &str) -> String {
    input
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// The turn's conversation records for content-log export, from the same rollout bytes. Codex
/// keeps the conversation in `response_item` envelopes: `message` per role, `custom_tool_call` for
/// each tool the assistant invoked, `custom_tool_call_output` for what came back.
pub(crate) fn content_records(content: &[u8]) -> Vec<GenAiRecord> {
    let text = String::from_utf8_lossy(content);
    let redact = turn_trace::redact_enabled();
    let mut model: Option<String> = None;
    let mut out = Vec::new();
    for line in rollout_lines(&text) {
        if line.kind == "turn_context" {
            let m = field(&line.payload, "model");
            if !m.is_empty() {
                model = Some(m);
            }
            continue;
        }
        if line.kind != "response_item" {
            continue;
        }
        match payload_type(&line.payload) {
            "message" => {
                let Some(content) = body(&message_text(&line.payload), redact) else {
                    continue;
                };
                match field(&line.payload, "role").as_str() {
                    "developer" | "system" => out.push(GenAiRecord::System { content }),
                    "user" => out.push(GenAiRecord::User { content }),
                    "assistant" => out.push(GenAiRecord::Assistant {
                        text: Some(content),
                        reasoning: None,
                        tool_calls: Vec::new(),
                        model: model.clone(),
                    }),
                    _ => {}
                }
            }
            "custom_tool_call" => {
                let input = field(&line.payload, "input");
                out.push(GenAiRecord::Assistant {
                    text: None,
                    reasoning: None,
                    tool_calls: vec![ToolCall {
                        id: field(&line.payload, "call_id"),
                        name: tool_name(&input, &field(&line.payload, "name")),
                        arguments: body(&input, redact).unwrap_or_default(),
                    }],
                    model: model.clone(),
                });
            }
            "custom_tool_call_output" => out.push(GenAiRecord::Tool {
                id: field(&line.payload, "call_id"),
                content: body(&message_text(&line.payload), redact).unwrap_or_default(),
                is_error: false,
            }),
            _ => {}
        }
    }
    out
}

/// The text of a `content`/`output` chunk array, chunks joined by newlines.
fn message_text(payload: &Value) -> String {
    let chunks = payload
        .get("content")
        .or_else(|| payload.get("output"))
        .and_then(Value::as_array);
    let Some(chunks) = chunks else {
        return String::new();
    };
    chunks
        .iter()
        .filter_map(|c| c.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A record body: trimmed-empty becomes `None`, and redaction rides the shared toggle.
fn body(text: &str, redact: bool) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    Some(if redact {
        turn_trace::redact(text)
    } else {
        text.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn args() -> Args {
        crate::Cli::parse_from(["crucible"]).run
    }

    const PREFIX: &[&str] = &[
        "codex",
        "exec",
        "--json",
        "--dangerously-bypass-approvals-and-sandbox",
        "--skip-git-repo-check",
        "--color",
        "never",
    ];

    #[test]
    fn invocation_is_headless_exec_with_the_model() {
        let mut a = args();
        a.codex.model = Some("gpt-5.6-sol".to_string());
        let local = local_argv(&a, "do the thing");
        assert_eq!(&local[..PREFIX.len()], PREFIX);
        assert_eq!(
            &local[PREFIX.len()..PREFIX.len() + 2],
            &["-m", "gpt-5.6-sol"]
        );
        assert_eq!(local.last().unwrap(), "do the thing");

        let sandbox = sandbox_argv(&a, true);
        assert_eq!(&sandbox[..PREFIX.len()], PREFIX);
        assert_eq!(sandbox.last().unwrap(), "-", "stdin marker stays last");
    }

    /// The default model is what the ChatGPT backend actually accepts, and the shared Claude
    /// default never reaches it: `gpt-5.2-codex` and `claude-*` are both 400s on a ChatGPT account.
    #[test]
    fn a_claude_model_never_reaches_the_chatgpt_backend() {
        assert_eq!(DEFAULT_MODEL, "gpt-5.6-sol");
        let mut a = args();
        a.model = "claude-opus-4-6".to_string();
        let v = sandbox_argv(&a, false);
        assert!(v.windows(2).any(|w| w == ["-m", DEFAULT_MODEL]), "{v:?}");
        a.model = "gpt-5.6-terra".to_string();
        assert!(
            sandbox_argv(&a, false)
                .windows(2)
                .any(|w| w == ["-m", "gpt-5.6-terra"]),
            "an OpenAI name in the shared slot is honored"
        );
    }

    #[test]
    fn codex_model_override_beats_the_shared_agent_model() {
        let mut a = args();
        a.model = "claude-opus-4-6".to_string();
        a.codex.model = Some("gpt-5.6-terra".to_string());
        let v = sandbox_argv(&a, false);
        assert!(v.windows(2).any(|w| w == ["-m", "gpt-5.6-terra"]));
        assert!(!v.iter().any(|s| s == "claude-opus-4-6"));
    }

    #[test]
    fn effort_maps_five_tiers_onto_three() {
        assert_eq!(reasoning_effort(ReasoningEffort::Low), "low");
        assert_eq!(reasoning_effort(ReasoningEffort::Medium), "medium");
        assert_eq!(reasoning_effort(ReasoningEffort::High), "high");
        assert_eq!(reasoning_effort(ReasoningEffort::Xhigh), "high");
        assert_eq!(reasoning_effort(ReasoningEffort::Max), "high");
    }

    #[test]
    fn effort_rides_a_config_override_before_the_stdin_marker() {
        let mut a = args();
        a.reasoning_effort = Some(ReasoningEffort::Xhigh);
        let v = sandbox_argv(&a, false);
        assert!(
            v.windows(2)
                .any(|w| w == ["-c", "model_reasoning_effort=high"]),
            "{v:?}"
        );
        assert_eq!(v.last().unwrap(), "-");

        let mut a = args();
        a.reasoning_effort = None;
        assert!(
            !sandbox_argv(&a, false).iter().any(|s| s == "-c"),
            "no tier ⇒ no override"
        );
    }

    #[test]
    fn env_script_pins_codex_home_and_carries_no_credential() {
        let s = env_script(&[("FOO".into(), "bar".into())]);
        assert!(s.contains("export AGENT_TOOL=codex"));
        assert!(s.contains("export CODEX_HOME=/sandbox/.codex"));
        assert!(s.contains("export FOO='bar'"));
        assert!(
            !s.to_uppercase().contains("TOKEN"),
            "credentials ride the seeded auth.json, not the env: {s}"
        );
        assert!(!s.to_uppercase().contains("API_KEY"), "no api key: {s}");
    }

    /// The env script is sourced by the sandbox's `bash -c` wrapper, so it has to parse there.
    #[test]
    fn env_script_is_valid_shell() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("env.sh");
        std::fs::write(&path, env_script(&[("A".into(), "a b'c".into())])).expect("write");
        let out = std::process::Command::new("bash")
            .arg("-n")
            .arg(&path)
            .output()
            .expect("bash -n");
        assert!(
            out.status.success(),
            "bash -n: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn config_toml_always_seeds_model_and_approval_posture() {
        let mut a = args();
        a.codex.model = Some("gpt-5.6-sol".to_string());
        let seeds = seed_files(&a, None, None, None);
        assert_eq!(seeds.len(), 1, "config.toml is always seeded");
        assert_eq!(seeds[0].dest, CONFIG);
        let v: toml::Table = toml::from_str(&seeds[0].content).expect("valid toml");
        assert_eq!(v["model"].as_str(), Some("gpt-5.6-sol"));
        assert_eq!(v["approval_policy"].as_str(), Some("never"));
        assert_eq!(v["sandbox_mode"].as_str(), Some("danger-full-access"));
        assert!(v.get("mcp_servers").is_none(), "no broker ⇒ no mcp_servers");
    }

    #[test]
    fn config_toml_merges_the_broker_mcp_server_with_a_bearer_token() {
        let mut a = args();
        a.broker.name = "epp-broker".into();
        a.broker_token = Some("s3cr3t".into());
        let seeds = seed_files(
            &a,
            Some("http://host.containers.internal:8849/mcp"),
            a.broker_token.as_deref(),
            None,
        );
        let v: toml::Table = toml::from_str(&seeds[0].content).expect("valid toml");
        let server = &v["mcp_servers"]["epp-broker"];
        assert_eq!(
            server["url"].as_str(),
            Some("http://host.containers.internal:8849/mcp")
        );
        assert_eq!(
            server["http_headers"]["Authorization"].as_str(),
            Some("Bearer s3cr3t")
        );
    }

    #[test]
    fn config_toml_omits_the_bearer_token_when_there_is_none() {
        let mut a = args();
        a.broker.name = "b".into();
        let seeds = seed_files(&a, Some("http://x/mcp"), None, None);
        let v: toml::Table = toml::from_str(&seeds[0].content).expect("valid toml");
        assert_eq!(v["mcp_servers"]["b"]["url"].as_str(), Some("http://x/mcp"));
        assert!(v["mcp_servers"]["b"].get("http_headers").is_none());
    }

    #[test]
    fn a_quote_in_a_broker_token_cannot_break_the_config() {
        let mut a = args();
        a.broker.name = "b".into();
        let seeds = seed_files(&a, Some("http://x/mcp"), Some("a\"b\nc"), None);
        let v: toml::Table = toml::from_str(&seeds[0].content).expect("valid toml");
        assert_eq!(
            v["mcp_servers"]["b"]["http_headers"]["Authorization"].as_str(),
            Some("Bearer a\"b\nc")
        );
    }

    fn token() -> CodexToken {
        CodexToken {
            access_token: "access-jwt".to_string(),
            account_id: "acct-1".to_string(),
            id_token: "id-jwt".to_string(),
        }
    }

    /// Codex drops the whole `tokens` object when a field is missing, then runs unauthenticated
    /// into a 401 loop, so every field has to be there and the withheld refresh token has to be
    /// non-empty (an empty one 400s at the auth service instead of naming the withholding).
    #[test]
    fn the_seeded_auth_json_carries_all_four_token_fields() {
        let v: serde_json::Value =
            serde_json::from_str(&auth_json(&token(), "2026-08-19T20:00:00Z")).expect("valid json");
        assert_eq!(v["auth_mode"], "chatgpt");
        assert!(v["OPENAI_API_KEY"].is_null());
        assert_eq!(v["tokens"]["access_token"], "access-jwt");
        assert_eq!(v["tokens"]["account_id"], "acct-1");
        assert_eq!(v["tokens"]["id_token"], "id-jwt");
        assert_eq!(v["tokens"]["refresh_token"], WITHHELD_REFRESH_TOKEN);
        assert!(!WITHHELD_REFRESH_TOKEN.is_empty());
        assert_eq!(v["last_refresh"], "2026-08-19T20:00:00Z");
    }

    #[test]
    fn auth_json_is_seeded_only_when_the_turn_minted_a_token() {
        let a = args();
        let minted = token();
        let seeds = seed_files(&a, None, None, Some(&minted));
        assert_eq!(seeds.len(), 2);
        assert_eq!(seeds[1].dest, AUTH);
        assert!(seeds[1].content.contains("access-jwt"));
        assert!(
            seed_files(&a, None, None, None)
                .iter()
                .all(|s| s.dest != AUTH)
        );
    }

    const ROLLOUT: &[u8] = include_bytes!("../testdata/codex_rollout_fixture.jsonl");

    #[test]
    fn the_rollout_yields_one_span_per_tool_call() {
        let art = parse_transcript(ROLLOUT);
        assert!(art.events.is_empty(), "the live stream owns the result");
        assert!(art.cost_usd.is_none(), "and the cost");
        let names: Vec<&str> = art.tool_calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["shell", "apply_patch"], "{art:?}");

        let shell = &art.tool_calls[0];
        assert_eq!(shell.summary, "echo hi", "the cmd, not the JS wrapper");
        assert!(!shell.error);
        let end = shell.end.expect("the output line closes the span");
        assert!(end > shell.start, "the output arrives after the call");

        let patch = &art.tool_calls[1];
        assert_eq!(patch.summary, "hello.txt");
        assert!(patch.end.is_some());
    }

    #[test]
    fn the_rollout_yields_the_conversation_records() {
        let records = content_records(ROLLOUT);
        assert!(
            records
                .iter()
                .any(|r| matches!(r, GenAiRecord::System { .. })),
            "the developer preamble is a system record"
        );
        let prompt = records
            .iter()
            .filter_map(|r| match r {
                GenAiRecord::User { content } => Some(content.as_str()),
                _ => None,
            })
            .find(|c| c.contains("echo hi"))
            .expect("the user prompt");
        assert!(prompt.contains("hello.txt"));

        let assistant_text = records
            .iter()
            .filter_map(|r| match r {
                GenAiRecord::Assistant { text, model, .. } => Some((text.clone()?, model.clone())),
                _ => None,
            })
            .next()
            .expect("the assistant's commentary");
        assert!(assistant_text.0.contains("run the command"));
        assert_eq!(assistant_text.1.as_deref(), Some("gpt-5.6-sol"));

        let calls: Vec<&ToolCall> = records
            .iter()
            .filter_map(|r| match r {
                GenAiRecord::Assistant { tool_calls, .. } => tool_calls.first(),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "shell");
        assert!(calls[0].arguments.contains("exec_command"));
        assert_eq!(calls[1].name, "apply_patch");

        let outputs: Vec<&GenAiRecord> = records
            .iter()
            .filter(|r| matches!(r, GenAiRecord::Tool { .. }))
            .collect();
        assert_eq!(outputs.len(), 2);
        match outputs[0] {
            GenAiRecord::Tool { id, content, .. } => {
                assert_eq!(id, &calls[0].id, "the output pairs with its call");
                assert!(content.contains("hi"));
            }
            other => panic!("expected a tool record, got {other:?}"),
        }
    }

    /// The `exec` custom tool names nothing useful; the JS body does.
    #[test]
    fn the_tool_name_comes_from_the_javascript_body() {
        assert_eq!(tool_name("await tools.exec_command({})", "exec"), "shell");
        assert_eq!(
            tool_name("text(await tools.apply_patch(p))", "exec"),
            "apply_patch"
        );
        assert_eq!(tool_name("no call here", "web_search"), "web_search");
    }

    #[test]
    fn garbage_transcript_is_empty_never_panics() {
        let art = parse_transcript(b"not jsonl");
        assert!(art.events.is_empty() && art.cost_usd.is_none() && art.tool_calls.is_empty());
        assert!(content_records(b"").is_empty());
        // Invalid UTF-8, a torn line, and an envelope with no payload all degrade, never panic.
        let torn = [
            &b"{\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call\""[..],
            &[0xff, 0xfe, b'\n'][..],
            &b"{\"type\":\"response_item\"}\n"[..],
            &b"{}\n"[..],
        ]
        .concat();
        assert!(parse_transcript(&torn).tool_calls.is_empty());
        assert!(content_records(&torn).is_empty());
        assert_eq!(DEFAULT_BINARIES, ["/usr/local/bin/codex"]);
    }
}
