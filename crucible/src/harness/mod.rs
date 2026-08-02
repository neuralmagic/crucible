//! The agent-harness boundary: everything that knows which agent CLI runs a turn.
//!
//! A harness is the program that turns a prompt into edits + a stream of [`AgentEvent`]s,
//! claude today, hermes next. Everything downstream of the decoder (session log, SSE, cost,
//! keep/discard) consumes the harness-neutral `AgentEvent`, so the swappable surface is exactly
//! what lives here: argv construction, env defaults, the sandbox seed files, the stream decoder,
//! and the post-turn transcript backfill. House style is enum-based static dispatch
//! (`AgentSource`, `AgentBackend`, `ComputeDriver`); the harness is selected at runtime from the
//! manifest (`[agent].harness`) or the CLI (`--harness`), riding on [`Args`], no turn-path
//! signature changes.

pub(crate) mod claude;
pub(crate) mod hermes;

use crate::Args;
use crate::event::{AgentEvent, RawStream};
use crate::stream_json::StreamJsonParser;
use crate::turn_trace::{GenAiRecord, ToolInvocation};
use crucible_harness::RateHandle;
use std::time::Duration;

/// Which agent harness runs the turn. `claude` is the default everywhere; `hermes` is the
/// second harness (Nous Research's hermes-agent), selected per-domain for harness ablations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    #[default]
    Claude,
    Hermes,
}

/// The filesystem contract between crucible and the agent harness inside the sandbox: where
/// crucible drops this turn's inputs. Harness-neutral, the harness-specific paths (claude's
/// `.mcp.json` and transcript dir, hermes's state db) live in their modules.
pub(crate) struct SandboxLayout;

impl SandboxLayout {
    /// Where the env script and prompt land (absolute `/tmp` uploads, validated).
    pub(crate) const ENV_SCRIPT: &'static str = "/tmp/.crucible-env.sh";
    pub(crate) const PROMPT: &'static str = "/tmp/.crucible-prompt";
    /// The sandbox home/workdir base (the workdir uploads to `<HOME>/<basename>`).
    pub(crate) const HOME: &'static str = "/sandbox";
}

/// One file uploaded into the sandbox before the agent execs (claude: `.mcp.json` when the
/// broker is on; hermes: its `config.yaml`). Rendered host-side, targeted-uploaded to `dest`.
pub(crate) struct SeedFile {
    pub content: String,
    pub dest: &'static str,
}

/// Where a harness leaves its native session transcript inside the sandbox, for the post-turn
/// fetch. Claude writes one jsonl per session under a projects tree (newest wins); hermes keeps
/// a single SQLite db at a fixed path.
pub(crate) enum TranscriptLocator {
    /// The newest `*.jsonl` under `<sandbox_root>/*/` (claude's `projects/<slug>/<session>.jsonl`).
    NewestJsonl { sandbox_root: &'static str },
    /// One fixed file (hermes's `state.db`).
    File { sandbox_path: &'static str },
}

/// What a harness recovers from its transcript after the turn. For claude this is trace garnish
/// (tool spans; empty events, no cost, the live stream already carried both); for a backfill
/// harness (hermes) it is the turn's ONLY source of result events + cost.
#[derive(Debug, Default)]
pub(crate) struct TurnArtifacts {
    pub events: Vec<AgentEvent>,
    pub cost_usd: Option<f64>,
    pub tool_calls: Vec<ToolInvocation>,
}

/// Decode the harness's stdout lines into [`AgentEvent`]s. Claude speaks `stream-json` (the
/// full structured live feed); a harness with no machine-readable stream degrades to
/// `RawLines`, one `Raw` event per non-empty line, structure arriving post-hoc via the
/// transcript backfill.
pub(crate) enum StreamDecoder {
    StreamJson(StreamJsonParser),
    RawLines,
}

impl StreamDecoder {
    /// Decode one complete stdout line into zero or more events.
    pub(crate) fn push(&mut self, line: &str) -> Vec<AgentEvent> {
        match self {
            StreamDecoder::StreamJson(parser) => parser.push(line),
            StreamDecoder::RawLines => {
                let text = line.trim();
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![AgentEvent::Raw {
                        text: text.to_string(),
                        stream: RawStream::Stdout,
                    }]
                }
            }
        }
    }
}

impl Harness {
    /// The full local-spawn argv (program name first): flags + the prompt as an argument.
    pub(crate) fn local_argv(self, args: &Args, prompt: &str) -> Vec<String> {
        match self {
            Harness::Claude => claude::local_argv(args, prompt),
            Harness::Hermes => hermes::local_argv(args, prompt),
        }
    }

    /// A continuing local turn. Only harnesses with an explicit, opaque resume cursor may
    /// implement this; callers fail closed instead of silently starting a fresh conversation.
    pub(crate) fn local_session_argv(
        self,
        args: &Args,
        prompt: &str,
        session: &crate::agent_session::SessionTurn,
    ) -> std::io::Result<Vec<String>> {
        match self {
            Harness::Claude => Ok(claude::local_session_argv(args, prompt, session)),
            Harness::Hermes => Err(std::io::Error::other(
                "hermes does not yet expose a Crucible-managed opaque session id",
            )),
        }
    }

    /// Harness env defaults for a local spawn, applied before the manifest `[agent].env`
    /// (so a manifest override wins).
    pub(crate) fn local_env_defaults(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Harness::Claude => claude::LOCAL_ENV_DEFAULTS,
            Harness::Hermes => hermes::LOCAL_ENV_DEFAULTS,
        }
    }

    /// The sandbox exec argv (program name first, no prompt, it arrives over stdin).
    /// `mcp_seeded` says whether [`Harness::seed_files`] delivered an MCP config this turn, so
    /// the argv can point the agent at it; the flag-ordering contract lives entirely in the
    /// harness arm (openshell/run.rs no longer knows the argv grammar).
    pub(crate) fn sandbox_argv(self, args: &Args, mcp_seeded: bool) -> Vec<String> {
        match self {
            Harness::Claude => claude::sandbox_argv(args, mcp_seeded),
            Harness::Hermes => hermes::sandbox_argv(args, mcp_seeded),
        }
    }

    /// The sandbox counterpart of [`Harness::local_session_argv`]. Claude's transcript is
    /// restored into its private config directory before this argv executes.
    pub(crate) fn sandbox_session_argv(
        self,
        args: &Args,
        mcp_seeded: bool,
        session: &crate::agent_session::SessionTurn,
    ) -> std::io::Result<Vec<String>> {
        match self {
            Harness::Claude => Ok(claude::sandbox_session_argv(args, mcp_seeded, session)),
            Harness::Hermes => Err(std::io::Error::other(
                "hermes does not yet expose a Crucible-managed opaque session id",
            )),
        }
    }

    /// The agent's env script (sourced before the agent runs in the sandbox): harness defaults
    /// plus the manifest's `[agent].env`.
    pub(crate) fn env_script(self, env: &[(String, String)]) -> String {
        match self {
            Harness::Claude => claude::env_script(env),
            Harness::Hermes => hermes::env_script(env),
        }
    }

    /// Whether the sandbox turn authenticates via the openshell Vertex provider (token mint +
    /// provider create/attach + the pod-env Vertex relay). Both harnesses use it: hermes resolves
    /// Vertex ADC through the gateway's metadata emulator (its config.yaml `provider: vertex-anthropic`),
    /// key-free like claude, so the same token-mint + provider machinery serves it.
    pub(crate) fn uses_vertex_provider(self) -> bool {
        match self {
            Harness::Claude => true,
            Harness::Hermes => true,
        }
    }

    /// Whether the harness exports the `claude_code.*` OTEL metrics the in-process collector
    /// captures. False means the collector never starts (no `otel_summary`; the pricing-table
    /// estimate stays the cost fallback).
    pub(crate) fn otel_capable(self) -> bool {
        match self {
            Harness::Claude => true,
            Harness::Hermes => false,
        }
    }

    /// Files uploaded into the sandbox before the agent execs (claude: `.mcp.json` when the
    /// broker is on).
    pub(crate) fn seed_files(self, args: &Args, broker_url: Option<&str>) -> Vec<SeedFile> {
        match self {
            Harness::Claude => claude::seed_files(args, broker_url),
            Harness::Hermes => hermes::seed_files(args, broker_url),
        }
    }

    /// The stream decoder for this harness's stdout. `rate`, when present, is the in-process
    /// OTLP collector's live 60 s-window token rate stamped onto each `tokens` sample.
    pub(crate) fn decoder(self, rate: Option<&RateHandle>) -> StreamDecoder {
        match self {
            Harness::Claude => StreamDecoder::StreamJson(match rate {
                Some(r) => StreamJsonParser::with_rate(r.clone()),
                None => StreamJsonParser::default(),
            }),
            Harness::Hermes => StreamDecoder::RawLines,
        }
    }

    /// Where this harness's session transcript lives inside the sandbox.
    pub(crate) fn transcript_locator(self) -> TranscriptLocator {
        match self {
            Harness::Claude => TranscriptLocator::NewestJsonl {
                sandbox_root: claude::CLAUDE_PROJECTS,
            },
            Harness::Hermes => TranscriptLocator::File {
                sandbox_path: hermes::STATE_DB,
            },
        }
    }

    /// Parse the downloaded transcript's in-memory bytes into [`TurnArtifacts`]. The caller reads
    /// the file ONCE (async, see the fetch path) and hands the bytes here; this method is pure and
    /// infallible: undecodable or unparseable bytes yield empty artifacts (the fetch layer decides
    /// whether that is loud, see [`Harness::backfill_required`]). Bytes, not `&str`, because a
    /// backfill harness's transcript is binary (hermes's SQLite `state.db`), while claude's is
    /// UTF-8 jsonl, each arm decodes its own format.
    pub(crate) fn parse_transcript(self, content: &[u8]) -> TurnArtifacts {
        match self {
            Harness::Claude => claude::parse_transcript(content),
            Harness::Hermes => hermes::parse_transcript(content),
        }
    }

    /// The turn's conversation as GenAI records for content-log export, parsed from the same
    /// in-memory transcript bytes (no re-read). Claude maps its jsonl messages; a harness whose
    /// content arrives via its own reader (hermes: Phase B) returns empty.
    pub(crate) fn content_records(self, content: &[u8]) -> Vec<GenAiRecord> {
        match self {
            Harness::Claude => claude::content_records(content),
            Harness::Hermes => hermes::content_records(content),
        }
    }

    /// Whether the transcript is the turn's ONLY source of result events + cost. When true the
    /// post-turn fetch runs unconditionally (not export-gated) and a fetch failure surfaces as
    /// an [`AgentEvent::Error`], never a silent $0 success (keep/discard integrity).
    pub(crate) fn backfill_required(self) -> bool {
        match self {
            Harness::Claude => false,
            Harness::Hermes => true,
        }
    }

    /// The local-backend counterpart of the sandbox transcript fetch: recover
    /// [`TurnArtifacts`] from this machine after a local turn. `None` when the harness has
    /// nothing to backfill (claude's live stream already carried everything).
    pub(crate) fn local_backfill(self, paths: &crate::Paths) -> Option<TurnArtifacts> {
        match self {
            Harness::Claude => None,
            Harness::Hermes => hermes::local_backfill(paths),
        }
    }

    /// Hard bound on the whole transcript fetch (find + download). For claude the fetch is pure
    /// telemetry and must never wedge the turn; for a backfill harness the transcript carries
    /// the turn's result, so it gets more headroom.
    pub(crate) fn transcript_fetch_timeout(self) -> Duration {
        match self {
            Harness::Claude => claude::TRANSCRIPT_FETCH_TIMEOUT,
            Harness::Hermes => hermes::TRANSCRIPT_FETCH_TIMEOUT,
        }
    }

    /// Binaries always allowed to open egress for this harness (the agent CLIs). OpenShell
    /// denies network to any binary not listed, but descendants inherit a parent's egress, so a
    /// tool the agent shells out to (e.g. `kubectl`) does not need its own entry, only the
    /// root agent does.
    pub(crate) fn default_binaries(self) -> &'static [&'static str] {
        match self {
            Harness::Claude => claude::DEFAULT_BINARIES,
            Harness::Hermes => hermes::DEFAULT_BINARIES,
        }
    }

    /// The workspace-relative dir the toolbox skills install into.
    pub(crate) fn skills_dir(self) -> &'static str {
        match self {
            Harness::Claude => claude::SKILLS_DIR,
            Harness::Hermes => hermes::SKILLS_DIR,
        }
    }

    /// The default model when neither the CLI nor the manifest names one.
    pub(crate) fn default_model(self) -> &'static str {
        match self {
            Harness::Claude => claude::DEFAULT_MODEL,
            Harness::Hermes => hermes::DEFAULT_MODEL,
        }
    }
}

/// The `bash -c` exec wrapper: cd into the workdir, source the env, exec the agent with the
/// prompt redirected from its file (openshell exec rejects newlines in argv, so the prompt
/// can't be an argument). Validated in-pod. Shared by both harnesses (hermes uses the same
/// stdin-redirect pattern), so callers use it directly, no per-harness dispatch.
pub(crate) fn exec_wrapper(workdir_basename: &str, agent_args: &[String]) -> Vec<String> {
    let script = format!(
        "cd {} && . {} && exec \"$@\" < {}",
        sh_quote(&format!("{}/{workdir_basename}", SandboxLayout::HOME)),
        sh_quote(SandboxLayout::ENV_SCRIPT),
        sh_quote(SandboxLayout::PROMPT),
    );
    let mut v = vec![
        "bash".to_string(),
        "-c".to_string(),
        script,
        "--".to_string(),
    ];
    v.extend(agent_args.iter().cloned());
    v
}

/// Single-quote a value for `sh` (wrap in `'…'`, escaping embedded single quotes), like
/// `shlex.quote`.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Append the manifest's `[agent].env` as `export k='v'` lines onto a harness's env script, then a
/// trailing blank line, and join with newlines. Shared tail of every harness's `env_script`, the
/// harness supplies its own defaults, this stamps the manifest overrides on top.
pub(crate) fn append_manifest_env(mut lines: Vec<String>, env: &[(String, String)]) -> String {
    for (k, v) in env {
        lines.push(format!("export {k}={}", sh_quote(v)));
    }
    lines.push(String::new()); // trailing newline
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AgentEvent;

    #[test]
    fn exec_wrapper_cds_sources_and_redirects_stdin() {
        let v = exec_wrapper("ws", &["claude".into(), "-p".into()]);
        assert_eq!(&v[0..2], &["bash", "-c"]);
        assert!(v[2].contains("cd '/sandbox/ws'"));
        assert!(v[2].contains(". '/tmp/.crucible-env.sh'"));
        assert!(v[2].contains("exec \"$@\" < '/tmp/.crucible-prompt'"));
        let sep = v.iter().position(|s| s == "--").unwrap();
        assert_eq!(
            &v[sep + 1..],
            &["claude", "-p"],
            "agent args after the separator"
        );
    }

    #[test]
    fn sh_quote_wraps_and_escapes() {
        assert_eq!(sh_quote("plain"), "'plain'");
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn harness_defaults_to_claude() {
        assert_eq!(Harness::default(), Harness::Claude);
    }

    #[test]
    fn harness_deserializes_lowercase() {
        #[derive(serde::Deserialize)]
        struct T {
            harness: Harness,
        }
        let t: T = toml::from_str("harness = \"hermes\"").expect("parse");
        assert_eq!(t.harness, Harness::Hermes);
        let t: T = toml::from_str("harness = \"claude\"").expect("parse");
        assert_eq!(t.harness, Harness::Claude);
        assert!(toml::from_str::<T>("harness = \"Claude\"").is_err());
    }

    #[test]
    fn raw_lines_decoder_wraps_nonempty_lines_and_drops_blanks() {
        let mut d = StreamDecoder::RawLines;
        assert!(d.push("").is_empty());
        assert!(d.push("   ").is_empty());
        let evs = d.push("  installing deps  ");
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            AgentEvent::Raw { text, stream } => {
                assert_eq!(text, "installing deps");
                assert_eq!(*stream, crate::event::RawStream::Stdout);
            }
            other => panic!("expected Raw, got {other:?}"),
        }
    }

    #[test]
    fn capability_split_between_the_harnesses() {
        assert!(Harness::Claude.uses_vertex_provider());
        // Hermes also authenticates via Vertex ADC (metadata emulator), key-free.
        assert!(Harness::Hermes.uses_vertex_provider());
        assert!(Harness::Claude.otel_capable());
        assert!(!Harness::Hermes.otel_capable());
        assert!(!Harness::Claude.backfill_required());
        assert!(Harness::Hermes.backfill_required());
    }
}
