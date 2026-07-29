//! The hermes arm of the harness boundary: Nous Research's hermes-agent as a second harness.
//!
//! Hermes has no machine-readable live event stream, so its result verdict, cost, tool spans, and
//! conversation records are reconstructed post-turn from its SQLite `state.db` (see
//! [`crate::hermes_trace`]). That reader is the turn's ONLY source of result + cost, which is why
//! `backfill_required` is true in `mod.rs`: a transcript-fetch failure surfaces loudly, never as a
//! silent $0 success.
//!
//! Auth is Vertex ADC via the openshell gateway's metadata emulator (`uses_vertex_provider` stays
//! true), key-free, exactly like claude; the env script sets no API key. MCP toward the
//! provisioning broker rides hermes's own `config.yaml` (remote HTTP MCP with a bearer header), not
//! argv.

use crate::Args;
use crate::harness::{SeedFile, TurnArtifacts, append_manifest_env};
use crate::turn_trace::GenAiRecord;
use std::time::Duration;

/// The default model when neither the CLI nor the manifest names one. A claude model (the harness
/// A/B holds the model fixed and swaps only the harness); a manifest carries the provider-prefixed
/// form its provider expects (e.g. `vertex_ai/claude-opus-4-6`).
pub(crate) const DEFAULT_MODEL: &str = "claude-opus-4-6";

/// The provider named in hermes's `config.yaml`: the `vertex-anthropic` provider from the
/// hermes-agent fork (github.com/wseaton/hermes-agent, branch `anthropic-vertex`). It speaks the
/// Anthropic API surface over Vertex, keeping the harness A/B model-identical with claude. Auth is
/// Vertex ADC (key-free).
pub(crate) const PROVIDER: &str = "vertex-anthropic";

/// The hermes CLI's symlink in the sandbox image.
pub(crate) const DEFAULT_BINARIES: &[&str] = &["/usr/local/bin/hermes"];

/// Hermes has no skills discovery; the toolbox still lands where domain prompts reference it.
pub(crate) const SKILLS_DIR: &str = ".claude/skills";

/// `$HERMES_HOME` inside the sandbox: relocates the session store off the default `~/.hermes`.
pub(crate) const HERMES_HOME: &str = "/sandbox/.hermes";

/// Hermes's session store, `$HERMES_HOME/state.db`. The transcript locator downloads this one file.
pub(crate) const STATE_DB: &str = "/sandbox/.hermes/state.db";

/// Hermes's config file (model, tool-progress display, MCP servers), rendered host-side and seeded.
pub(crate) const CONFIG: &str = "/sandbox/.hermes/config.yaml";

/// The state.db carries the turn's result + cost, so the fetch gets more headroom than claude's
/// telemetry-only 30s.
pub(crate) const TRANSCRIPT_FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Harness env defaults for a local hermes spawn (manifest `[agent].env` still wins).
pub(crate) const LOCAL_ENV_DEFAULTS: &[(&str, &str)] = &[("AGENT_TOOL", "hermes")];

/// The shared invocation prefix: `hermes chat --yolo --model <model>`; prompt delivery is appended
/// by the caller. `--yolo` bypasses approval prompts (headless); `[agent.hermes].model` overrides
/// the shared `[agent].model`.
fn base_args(args: &Args) -> Vec<String> {
    let model = args.hermes.model.as_deref().unwrap_or(&args.model);
    vec![
        "hermes".to_string(),
        "chat".to_string(),
        "--yolo".to_string(),
        "--model".to_string(),
        model.to_string(),
    ]
}

/// The full local-spawn argv: the prompt rides inline as the `-q` value (a local spawn feeds argv
/// directly, no stdin redirect).
pub(crate) fn local_argv(args: &Args, prompt: &str) -> Vec<String> {
    let mut a = base_args(args);
    a.push("-q".to_string());
    a.push(prompt.to_string());
    a
}

/// The sandbox argv: `-q -` reads the prompt from stdin, which the shared exec wrapper redirects
/// from the uploaded prompt file (same pattern as claude's print mode). Hermes reads MCP servers
/// from its `config.yaml`, not argv, so `mcp_seeded` is accepted and unused by design.
pub(crate) fn sandbox_argv(args: &Args, _mcp_seeded: bool) -> Vec<String> {
    let mut a = base_args(args);
    a.push("-q".to_string());
    a.push("-".to_string());
    a
}

/// The agent's env script (sourced before hermes runs in the sandbox): `HERMES_HOME` (relocates the
/// state db) + `AGENT_TOOL`, then the manifest's `[agent].env` (Vertex vars ride here; the metadata
/// emulator serves the token, no API key is ever set).
pub(crate) fn env_script(env: &[(String, String)]) -> String {
    let lines = vec![
        "export AGENT_TOOL=hermes".to_string(),
        format!("export HERMES_HOME={HERMES_HOME}"),
    ];
    append_manifest_env(lines, env)
}

/// Files seeded into the sandbox before exec: hermes's `config.yaml`, ALWAYS (it carries the model
/// and tool-progress display). When the broker is on, its remote HTTP MCP server is merged in with a
/// bearer header, since the broker binds `0.0.0.0` and the header is what makes the sandbox its only
/// caller.
pub(crate) fn seed_files(args: &Args, broker_url: Option<&str>) -> Vec<SeedFile> {
    let model = args.hermes.model.as_deref().unwrap_or(&args.model);
    vec![SeedFile {
        content: config_yaml(
            model,
            &args.broker.name,
            broker_url,
            args.broker_token.as_deref(),
        ),
        dest: CONFIG,
    }]
}

/// Render hermes's `config.yaml`: model + provider + `display.tool_progress: all`, plus a
/// `mcp_servers:` entry toward the broker when it is on. serde_norway (a serde_yaml fork, already a
/// dep) keeps this escape-safe versus a hand-rolled string.
fn config_yaml(
    model: &str,
    broker_name: &str,
    broker_url: Option<&str>,
    token: Option<&str>,
) -> String {
    let mut cfg = serde_json::Map::new();
    cfg.insert("model".into(), serde_json::json!(model));
    cfg.insert("provider".into(), serde_json::json!(PROVIDER));
    cfg.insert(
        "display".into(),
        serde_json::json!({ "tool_progress": "all" }),
    );
    if let Some(url) = broker_url {
        let mut server = serde_json::json!({ "url": url });
        if let Some(t) = token {
            server["headers"] = serde_json::json!({ "Authorization": format!("Bearer {t}") });
        }
        cfg.insert(
            "mcp_servers".into(),
            serde_json::json!({ broker_name: server }),
        );
    }
    serde_norway::to_string(&serde_json::Value::Object(cfg)).unwrap_or_default()
}

/// Read the downloaded state.db bytes into the turn's result + cost + tool spans. Empty artifacts on
/// any read failure, combined with `backfill_required`, that means the fetch layer surfaces the
/// failure loudly, never as a $0-quiet success.
pub(crate) fn parse_transcript(content: &[u8]) -> TurnArtifacts {
    match crate::hermes_trace::read_turn(content) {
        Some(t) => TurnArtifacts {
            events: t.events,
            cost_usd: t.cost_usd,
            tool_calls: t.tool_calls,
        },
        None => TurnArtifacts::default(),
    }
}

/// The turn's conversation records for content-log export, from the same state.db. Reads the db a
/// second time (cheap, telemetry path) to keep the two boundary methods independent and infallible.
pub(crate) fn content_records(content: &[u8]) -> Vec<GenAiRecord> {
    crate::hermes_trace::read_turn(content)
        .map(|t| t.records)
        .unwrap_or_default()
}

/// Read the local `state.db` after a local-backend turn: `$HERMES_HOME/state.db`, falling back to
/// `~/.hermes/state.db`. `None` when there is no readable db (nothing to backfill).
pub(crate) fn local_backfill(_paths: &crate::Paths) -> Option<TurnArtifacts> {
    let home = std::env::var("HERMES_HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| std::path::PathBuf::from(h).join(".hermes"))
        })?;
    let t = crate::hermes_trace::read_turn_path(&home.join("state.db"))?;
    Some(TurnArtifacts {
        events: t.events,
        cost_usd: t.cost_usd,
        tool_calls: t.tool_calls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn args() -> Args {
        crate::Cli::parse_from(["crucible"]).run
    }

    #[test]
    fn invocation_is_headless_chat_with_the_model() {
        let a = args();
        let local = local_argv(&a, "do the thing");
        assert_eq!(&local[0..3], &["hermes", "chat", "--yolo"]);
        assert!(local.windows(2).any(|w| w[0] == "--model"));
        // Local delivers the prompt inline as the `-q` value.
        assert_eq!(&local[local.len() - 2..], &["-q", "do the thing"]);

        let sandbox = sandbox_argv(&a, true);
        assert_eq!(&sandbox[0..3], &["hermes", "chat", "--yolo"]);
        // Sandbox reads the prompt from stdin (`-q -`), redirected by the shared exec wrapper.
        assert_eq!(&sandbox[sandbox.len() - 2..], &["-q", "-"]);
    }

    #[test]
    fn hermes_model_override_beats_the_shared_agent_model() {
        let mut a = args();
        a.hermes.model = Some("anthropic/claude-haiku-4-5".to_string());
        let v = sandbox_argv(&a, false);
        assert!(
            v.windows(2)
                .any(|w| w == ["--model", "anthropic/claude-haiku-4-5"])
        );
    }

    #[test]
    fn env_script_relocates_hermes_home_with_no_api_key() {
        let s = env_script(&[("CLAUDE_CODE_USE_VERTEX".into(), "1".into())]);
        assert!(s.contains("export AGENT_TOOL=hermes"));
        assert!(s.contains("export HERMES_HOME=/sandbox/.hermes"));
        assert!(s.contains("export CLAUDE_CODE_USE_VERTEX='1'"));
        // Vertex ADC via the metadata emulator, never an API key in the env.
        assert!(!s.to_uppercase().contains("API_KEY"), "no api key: {s}");
    }

    #[test]
    fn config_yaml_always_seeds_model_and_display() {
        let a = args();
        let seeds = seed_files(&a, None);
        assert_eq!(seeds.len(), 1, "config.yaml is always seeded");
        assert_eq!(seeds[0].dest, CONFIG);
        let v: serde_json::Value = serde_norway::from_str(&seeds[0].content).expect("valid yaml");
        assert_eq!(v["model"], a.model.as_str());
        assert_eq!(v["provider"], PROVIDER);
        assert_eq!(v["display"]["tool_progress"], "all");
        assert!(v.get("mcp_servers").is_none(), "no broker ⇒ no mcp_servers");
    }

    #[test]
    fn config_yaml_merges_the_broker_mcp_server_with_a_bearer_header() {
        let mut a = args();
        a.broker.name = "epp-broker".into();
        a.broker_token = Some("s3cr3t".into());
        let seeds = seed_files(&a, Some("http://host.containers.internal:8849/mcp"));
        let v: serde_json::Value = serde_norway::from_str(&seeds[0].content).expect("valid yaml");
        assert_eq!(
            v["mcp_servers"]["epp-broker"]["url"],
            "http://host.containers.internal:8849/mcp"
        );
        assert_eq!(
            v["mcp_servers"]["epp-broker"]["headers"]["Authorization"],
            "Bearer s3cr3t"
        );
    }

    #[test]
    fn config_yaml_omits_headers_without_a_token() {
        let mut a = args();
        a.broker.name = "b".into();
        let seeds = seed_files(&a, Some("http://x/mcp"));
        let v: serde_json::Value = serde_norway::from_str(&seeds[0].content).expect("valid yaml");
        assert_eq!(v["mcp_servers"]["b"]["url"], "http://x/mcp");
        assert!(v["mcp_servers"]["b"].get("headers").is_none());
    }

    #[test]
    fn transcript_reads_the_banked_fixture_end_to_end() {
        // The reader's own tests cover the shapes; here we just prove the arm is wired to it.
        let fixture: &[u8] = include_bytes!("../testdata/hermes_state_fixture.db");
        let art = parse_transcript(fixture);
        assert_eq!(art.tool_calls.len(), 1, "the tool-call turn's span");
        assert!(art.cost_usd.unwrap() > 0.0);
        assert!(
            art.events
                .iter()
                .any(|e| matches!(e, crate::event::AgentEvent::Result { .. }))
        );
        assert!(!content_records(fixture).is_empty());
    }

    #[test]
    fn garbage_transcript_is_empty_never_panics() {
        let art = parse_transcript(b"not a db");
        assert!(art.events.is_empty() && art.cost_usd.is_none() && art.tool_calls.is_empty());
        assert!(content_records(b"").is_empty());
        assert_eq!(DEFAULT_BINARIES, ["/usr/local/bin/hermes"]);
    }
}
