//! The claude arm of the harness boundary: Claude Code's argv grammar, env defaults, `.mcp.json`
//! seeding, and transcript layout. Pure extraction, every function here moved verbatim from
//! `agent.rs` / `openshell/run.rs`, and the tests moved with them (they are the byte-identity
//! guard for the harness refactor).

use crate::Args;
use crate::harness::{SeedFile, TurnArtifacts, append_manifest_env};
use crate::turn_trace::GenAiRecord;
use std::time::Duration;

/// The default Vertex Claude model (`[agent].model` / `--model` when unset).
pub(crate) const DEFAULT_MODEL: &str = "claude-opus-4-6";

/// Binaries always allowed to open egress for a claude sandbox turn.
pub(crate) const DEFAULT_BINARIES: &[&str] = &["/usr/local/bin/claude", "/usr/local/bin/opencode"];

/// Where the toolbox skills install in the workspace (claude discovers them there).
pub(crate) const SKILLS_DIR: &str = ".claude/skills";

/// Where the generated `.mcp.json` (the provisioning-broker endpoint) lands; claude loads it via
/// `--mcp-config`. Absolute `/tmp`, like the env/prompt uploads. Only seeded when the broker is on.
pub(crate) const MCP_CONFIG: &str = "/tmp/.crucible-mcp.json";

/// Where the agent's native session transcripts live: claude writes them under
/// `$CLAUDE_CONFIG_DIR/projects/<slug>/<session>.jsonl`, and the env script pins
/// `CLAUDE_CONFIG_DIR=/sandbox/.claude` (see [`env_script`]).
pub(crate) const CLAUDE_PROJECTS: &str = "/sandbox/.claude/projects";

/// The transcript fetch is pure telemetry for claude and must never wedge the turn:
/// elapsed = silent no-op, the turn's tree just stays at the root span.
pub(crate) const TRANSCRIPT_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Harness env defaults for a local claude spawn (manifest `[agent].env` still wins).
pub(crate) const LOCAL_ENV_DEFAULTS: &[(&str, &str)] = &[
    ("AGENT_TOOL", "claude"),
    ("DISABLE_AUTOUPDATER", "1"),
    // `-p` sets sessionKind, which breaks `--continue` lookup (claude-code#43013).
    ("CLAUDE_CODE_ENTRYPOINT", "sdk-cli"),
];

/// The `claude` args up to and including `-p`, without the prompt. Pure and unit-testable.
/// The local path appends the prompt as an argument; the openshell path leaves it off and
/// feeds the prompt over stdin (openshell exec rejects newlines in argv), which claude's
/// print mode reads when no prompt argument is given.
pub(crate) fn claude_base_args(args: &Args) -> Vec<String> {
    let mut a = vec![
        "claude".to_string(),
        "--permission-mode".to_string(),
        "bypassPermissions".to_string(),
        "--model".to_string(),
        args.model.clone(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--include-partial-messages".to_string(),
        "--verbose".to_string(),
    ];
    // Per-domain reasoning-effort tier; unset = Claude Code's default (no flag). Pushed before
    // `-p` so `-p` stays last, the sandbox path inserts `--mcp-config` before the trailing `-p`.
    if let Some(effort) = args.reasoning_effort {
        a.push("--effort".to_string());
        a.push(effort.as_flag().to_string());
    }
    a.push("-p".to_string());
    a
}

/// The full `claude` argument vector (program name first) for a direct local turn:
/// the base args plus the prompt.
pub(crate) fn local_argv(args: &Args, prompt: &str) -> Vec<String> {
    let mut a = claude_base_args(args);
    a.push(prompt.to_string());
    a
}

/// Start or resume a local Claude session with an opaque UUID.
pub(crate) fn local_session_argv(
    args: &Args,
    prompt: &str,
    session: &crate::agent_session::SessionTurn,
) -> Vec<String> {
    let mut a = claude_base_args(args);
    let at = a.len().saturating_sub(1); // before `-p`
    if session.is_resume() {
        a.insert(at, session.provider_id.clone());
        a.insert(at, "--resume".to_string());
    } else {
        a.insert(at, session.provider_id.clone());
        a.insert(at, "--session-id".to_string());
    }
    a.push(prompt.to_string());
    a
}

/// The claude argv (program name first) for a sandbox turn: the shared base args, plus
/// `--mcp-config <path>` when an MCP config was seeded, so the agent sees the broker's
/// `request_trace` / `check_capture` tools. The flag goes just before the trailing `-p`, the
/// boundary [`claude_base_args`] guarantees (it ends with `-p`; the prompt arrives over stdin).
pub(crate) fn sandbox_argv(args: &Args, mcp_seeded: bool) -> Vec<String> {
    let mut a = claude_base_args(args);
    if mcp_seeded {
        let at = a.len().saturating_sub(1); // before the trailing `-p`
        a.insert(at, MCP_CONFIG.to_string());
        a.insert(at, "--mcp-config".to_string());
    }
    a
}

/// Start or resume a sandbox session; the prompt still arrives over stdin.
pub(crate) fn sandbox_session_argv(
    args: &Args,
    mcp_seeded: bool,
    session: &crate::agent_session::SessionTurn,
) -> Vec<String> {
    let mut a = sandbox_argv(args, mcp_seeded);
    let at = a.len().saturating_sub(1);
    if session.is_resume() {
        a.insert(at, session.provider_id.clone());
        a.insert(at, "--resume".to_string());
    } else {
        a.insert(at, session.provider_id.clone());
        a.insert(at, "--session-id".to_string());
    }
    a
}

/// The agent's env script (sourced before claude runs in the sandbox). Claude defaults +
/// the manifest's `[agent].env` (which already carries the Vertex vars); no token, the
/// metadata emulator serves it.
pub(crate) fn env_script(env: &[(String, String)]) -> String {
    let lines: Vec<String> = [
        "export AGENT_TOOL=claude",
        "export CLAUDE_CONFIG_DIR=/sandbox/.claude",
        "export CLAUDE_CODE_PLUGIN_SEED_DIR=/sandbox/.claude-seed",
        "export CLAUDE_CODE_SYNC_PLUGIN_INSTALL=1",
        "export DISABLE_AUTOUPDATER=1",
        "export CLAUDE_CODE_MAX_RETRIES=20",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    append_manifest_env(lines, env)
}

/// Files seeded into the sandbox before exec: the `.mcp.json` pointing claude at the
/// provisioning broker, only when the broker is on.
pub(crate) fn seed_files(args: &Args, broker_url: Option<&str>) -> Vec<SeedFile> {
    let Some(url) = broker_url else {
        return Vec::new();
    };
    vec![SeedFile {
        content: mcp_config_json(&args.broker.name, url, args.broker_token.as_deref()),
        dest: MCP_CONFIG,
    }]
}

/// The `.mcp.json` seeded into the sandbox: one streamable-http server pointing at the broker.
/// `name` is the agent-visible MCP server name (the `mcp__<name>__…` tool prefix), domain-owned.
/// `token`, when set, rides as an `Authorization: Bearer` header, the broker's port sits on a
/// `0.0.0.0` bind, so the header is what makes the sandbox the only caller it answers.
fn mcp_config_json(name: &str, url: &str, token: Option<&str>) -> String {
    let mut server = serde_json::json!({ "type": "http", "url": url });
    if let Some(t) = token {
        server["headers"] = serde_json::json!({ "Authorization": format!("Bearer {t}") });
    }
    serde_json::json!({ "mcpServers": { name: server } }).to_string()
}

/// Parse a claude session transcript's in-memory bytes into [`TurnArtifacts`]. For claude this is
/// trace garnish: tool spans only, the live `stream-json` feed already carried the events and
/// cost, so `events` stays empty and `cost_usd` stays `None` (byte-identical turn behavior).
/// Non-UTF-8 bytes yield empty artifacts, matching the old `read_to_string` failure path.
pub(crate) fn parse_transcript(content: &[u8]) -> TurnArtifacts {
    let Ok(text) = std::str::from_utf8(content) else {
        return TurnArtifacts::default();
    };
    TurnArtifacts {
        events: Vec::new(),
        cost_usd: None,
        tool_calls: crate::turn_trace::parse_transcript(text),
    }
}

/// The turn's conversation as GenAI records, mapped from claude's jsonl message format for
/// content-log export. Same in-memory bytes as [`parse_transcript`] (no re-read); non-UTF-8 or
/// empty transcript yields no records.
pub(crate) fn content_records(content: &[u8]) -> Vec<GenAiRecord> {
    match std::str::from_utf8(content) {
        Ok(text) => crate::turn_trace::parse_messages(text),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Parse a bare arg list into [`Args`] via the top-level CLI (the loop is the
    /// default subcommand), so we exercise the real clap wiring.
    fn args(extra: &[&str]) -> Args {
        let mut argv = vec!["crucible"];
        argv.extend_from_slice(extra);
        crate::Cli::parse_from(argv).run
    }

    #[test]
    fn claude_args_are_stream_json_print_mode() {
        let a = args(&[]);
        let v = local_argv(&a, "do the thing");
        assert_eq!(v.last().unwrap(), "do the thing");
        assert!(v.windows(2).any(|w| w == ["-p", "do the thing"]));
        assert!(
            v.windows(2)
                .any(|w| w == ["--output-format", "stream-json"])
        );
        assert!(
            v.windows(2)
                .any(|w| w == ["--permission-mode", "bypassPermissions"])
        );
        assert!(v.iter().any(|s| s == "--include-partial-messages"));
    }

    #[test]
    fn effort_flag_absent_by_default_present_when_set() {
        // Unset: no `--effort`, `-p` last (Claude Code picks its own default).
        let base = claude_base_args(&args(&[]));
        assert!(!base.iter().any(|s| s == "--effort"));
        assert_eq!(base.last().unwrap(), "-p");

        // Set: `--effort <level>` right before the trailing `-p` (which must stay last so the
        // sandbox path can still insert `--mcp-config` before it).
        let v = claude_base_args(&args(&["--effort", "xhigh"]));
        assert_eq!(&v[v.len() - 3..], &["--effort", "xhigh", "-p"]);
    }

    #[test]
    fn logical_sessions_start_then_resume_by_opaque_id() {
        let first = crate::agent_session::SessionTurn {
            logical_name: "solver".into(),
            provider_id: "018f47a0-0000-7000-8000-000000000000".into(),
            completed_turns: 0,
        };
        let started = local_session_argv(&args(&[]), "try one", &first);
        assert!(
            started
                .windows(2)
                .any(|w| w == ["--session-id", first.provider_id.as_str()])
        );
        assert!(!started.iter().any(|arg| arg == "--resume"));

        let resumed = crate::agent_session::SessionTurn {
            completed_turns: 1,
            ..first
        };
        let continued = local_session_argv(&args(&[]), "try two", &resumed);
        assert!(
            continued
                .windows(2)
                .any(|w| w == ["--resume", resumed.provider_id.as_str()])
        );
        assert!(!continued.iter().any(|arg| arg == "--session-id"));

        let sandbox = sandbox_session_argv(&args(&[]), true, &resumed);
        assert!(
            sandbox
                .windows(2)
                .any(|w| w == ["--resume", resumed.provider_id.as_str()])
        );
        assert!(sandbox.iter().any(|arg| arg == "--mcp-config"));
        assert_eq!(sandbox.last().map(String::as_str), Some("-p"));
    }

    #[test]
    fn env_script_has_claude_defaults_and_manifest_env() {
        let s = env_script(&[
            ("CLAUDE_CODE_USE_VERTEX".into(), "1".into()),
            (
                "ANTHROPIC_VERTEX_PROJECT_ID".into(),
                "example-project".into(),
            ),
        ]);
        assert!(s.contains("export AGENT_TOOL=claude"));
        assert!(s.contains("export CLAUDE_CODE_MAX_RETRIES=20"));
        assert!(s.contains("export CLAUDE_CONFIG_DIR=/sandbox/.claude"));
        assert!(s.contains("export CLAUDE_CODE_USE_VERTEX='1'"));
        assert!(s.contains("export ANTHROPIC_VERTEX_PROJECT_ID='example-project'"));
        // No token is ever injected into the env (the provider/emulator serves it).
        assert!(
            !s.contains("ACCESS_TOKEN"),
            "no token in the env script: {s}"
        );
    }

    #[test]
    fn sandbox_args_inject_mcp_config_only_when_broker_on() {
        let a = args(&[]);
        // Not seeded: identical to the base args (ends with `-p`, no `--mcp-config`).
        let off = sandbox_argv(&a, false);
        assert_eq!(off, claude_base_args(&a));
        assert!(!off.iter().any(|s| s == "--mcp-config"));
        assert_eq!(off.last().unwrap(), "-p");

        // Seeded: `--mcp-config <path>` inserted right before the trailing `-p`.
        let on = sandbox_argv(&a, true);
        let i = on
            .iter()
            .position(|s| s == "--mcp-config")
            .expect("flag present");
        assert_eq!(&on[i..], &["--mcp-config", MCP_CONFIG, "-p"]);
        assert_eq!(on.last().unwrap(), "-p", "prompt flag stays last");
    }

    #[test]
    fn sandbox_args_keep_effort_and_mcp_config_before_prompt() {
        let a = args(&["--effort", "max"]);
        let v = sandbox_argv(&a, true);
        // Both flags land before the trailing `-p`; the prompt flag stays last.
        assert!(v.windows(2).any(|w| w == ["--effort", "max"]));
        assert!(v.iter().any(|s| s == "--mcp-config"));
        assert_eq!(v.last().unwrap(), "-p");
    }

    #[test]
    fn mcp_config_json_points_one_http_server_at_the_url() {
        let url = "http://host.containers.internal:8849/mcp";
        let v: serde_json::Value =
            serde_json::from_str(&mcp_config_json("epp-broker", url, None)).expect("valid json");
        assert_eq!(v["mcpServers"]["epp-broker"]["type"], "http");
        assert_eq!(v["mcpServers"]["epp-broker"]["url"], url);
        assert!(
            v["mcpServers"]["epp-broker"].get("headers").is_none(),
            "no token, no headers block"
        );
    }

    #[test]
    fn mcp_config_json_carries_the_bearer_header_when_token_set() {
        let v: serde_json::Value = serde_json::from_str(&mcp_config_json(
            "broker",
            "http://host.containers.internal:8849/mcp",
            Some("s3cr3t"),
        ))
        .expect("valid json");
        assert_eq!(
            v["mcpServers"]["broker"]["headers"]["Authorization"],
            "Bearer s3cr3t"
        );
    }

    /// A domain-owned name/url with JSON-hostile characters must not break the config (the old
    /// `format!` builder would have).
    #[test]
    fn mcp_config_json_escapes_hostile_values() {
        let v: serde_json::Value =
            serde_json::from_str(&mcp_config_json(r#"we"ird"#, "http://x/mcp", None))
                .expect("valid json even with a quote in the name");
        assert_eq!(v["mcpServers"][r#"we"ird"#]["url"], "http://x/mcp");
    }

    #[test]
    fn seed_files_only_when_broker_url_present() {
        let mut a = args(&[]);
        assert!(seed_files(&a, None).is_empty());
        a.broker.enabled = true;
        let seeds = seed_files(&a, Some("http://host.containers.internal:8849/mcp"));
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].dest, MCP_CONFIG);
        let v: serde_json::Value = serde_json::from_str(&seeds[0].content).expect("valid json");
        assert!(v["mcpServers"].is_object());
    }
}
