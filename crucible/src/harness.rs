//! What each agent harness is, as data. Every per-harness decision the engine or a controller
//! makes reads this one table through [`Harness::spec`]; the argv grammar, config file, stream
//! decoder, and transcript parser that genuinely differ per CLI live in the binary's backends.

use crate::manifest::Harness;
use crate::openshell::policy::{CLAUDE_ENDPOINTS, CODEX_ENDPOINTS, OPENAI_API_ENDPOINTS};
use crucible_contract::inference::InferenceProtocol;
use std::time::Duration;

/// How a sandbox turn gets its model credential. Vertex mints a `cloud-platform` access token from
/// ADC and serves it through the gateway's metadata emulator; Codex seeds either an API key or a
/// host-refreshed ChatGPT OAuth access token into the sandbox's `auth.json`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthProvider {
    Vertex,
    Codex,
    /// A direct Anthropic API key from the environment, relayed into the sandbox; no gateway
    /// provider, no metadata emulator.
    AnthropicKey,
    /// A direct API key from the environment for whichever API the turn's endpoint speaks:
    /// `OPENAI_API_KEY` or `ANTHROPIC_API_KEY`, relayed into the sandbox alongside its base URL.
    /// The harnesses that take a provider table (opencode, pi) read the key back from that env
    /// through their seeded config.
    ApiKey,
}

impl AuthProvider {
    /// Whether a turn with no key and no base URL in its environment runs on the deploy
    /// profile's ambient Vertex credential: the Vertex harnesses do by construction, and a
    /// key-authenticated one falls back to it through its provider table.
    pub fn runs_on_ambient_vertex(self) -> bool {
        matches!(self, AuthProvider::Vertex | AuthProvider::ApiKey)
    }
}

/// The API families a harness can reach a model through. A harness that speaks both takes an
/// OpenAI-speaking endpoint first when the environment names one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiFamily {
    Anthropic,
    OpenAi,
    Both,
}

/// Where a harness leaves its native session transcript inside the sandbox, for the post-turn
/// fetch. Claude writes one jsonl per session under a projects tree (newest wins); hermes keeps
/// a single SQLite db at a fixed path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptLocator {
    /// The newest file matching `<sandbox_root>/<glob>`: claude's `projects/<slug>/<session>.jsonl`
    /// sits one segment down, codex's `sessions/YYYY/MM/DD/rollout-*.jsonl` three.
    NewestJsonl {
        sandbox_root: &'static str,
        glob: &'static str,
    },
    /// One fixed file (hermes's `state.db`).
    File { sandbox_path: &'static str },
}

impl TranscriptLocator {
    /// The `(root, glob)` of a jsonl tree, `None` for a fixed file.
    pub fn jsonl_tree(self) -> Option<(&'static str, &'static str)> {
        match self {
            TranscriptLocator::NewestJsonl { sandbox_root, glob } => Some((sandbox_root, glob)),
            TranscriptLocator::File { .. } => None,
        }
    }
}

/// What a harness is, as data: the values every shared path reads.
#[derive(Debug)]
pub struct HarnessSpec {
    /// The `AGENT_TOOL` value, exported first in both the sandbox env script and the local env.
    pub name: &'static str,
    /// The model a turn runs when neither the CLI nor the manifest names one.
    pub default_model: &'static str,
    /// Binaries always allowed to open egress for this harness (the agent CLIs). OpenShell
    /// denies network to any binary not listed, but descendants inherit a parent's egress, so a
    /// tool the agent shells out to (e.g. `kubectl`) does not need its own entry, only the
    /// root agent does.
    pub binaries: &'static [&'static str],
    /// The workspace-relative dir the toolbox skills install into.
    pub skills_dir: &'static str,
    /// The env var that relocates the harness's state dir into the sandbox, and its value.
    pub home_var: &'static str,
    pub home: &'static str,
    /// Where the rendered config file lands in the sandbox.
    pub config: &'static str,
    /// Further sandbox exports, after `AGENT_TOOL` and the home var; unquoted, like them.
    pub sandbox_env: &'static [(&'static str, &'static str)],
    /// Local-spawn env defaults after `AGENT_TOOL`, applied before the manifest `[agent].env`.
    pub local_env: &'static [(&'static str, &'static str)],
    /// Where this harness's session transcript lives inside the sandbox.
    pub transcript: TranscriptLocator,
    /// Hard bound on the whole transcript fetch (find + download). For claude the fetch is pure
    /// telemetry and must never wedge the turn; for a backfill harness the transcript carries
    /// the turn's result, so it gets more headroom.
    pub transcript_fetch_timeout: Duration,
    /// Which credential the sandbox turn attaches (token mint + provider create/attach + the
    /// pod-env relay). Claude and hermes both resolve Vertex ADC through the gateway's metadata
    /// emulator (hermes via its config.yaml `provider: vertex-anthropic`), key-free; codex
    /// authenticates against the ChatGPT backend instead.
    pub auth: AuthProvider,
    /// The API families the harness reaches a model through.
    pub api: ApiFamily,
    /// The inference protocols the harness is a client of.
    pub protocols: &'static [InferenceProtocol],
    /// Egress hosts the harness's model backend needs on top of the shared defaults.
    pub model_endpoints: &'static [&'static str],
    /// The sandbox-image capability predicate that says the harness's CLI is installed, `None`
    /// for a harness no image feature declares.
    pub sandbox_capability: Option<&'static str>,
    /// Whether the harness can resume a Crucible-managed session across turns.
    pub persistent_sessions: bool,
    /// Whether the harness exports the `claude_code.*` OTEL metrics the in-process collector
    /// captures. False means the collector never starts (no `otel_summary`; the pricing-table
    /// estimate stays the cost fallback).
    pub otel_capable: bool,
    /// Whether the transcript is the turn's ONLY source of result events + cost. When true the
    /// post-turn fetch runs unconditionally (not export-gated) and a fetch failure surfaces as
    /// an error event, never a silent $0 success (keep/discard integrity).
    pub backfill_required: bool,
}

/// Where a sandboxed opencode turn's wrapper leaves the session export.
pub const OPENCODE_EXPORT: &str = "/sandbox/.local/share/opencode/crucible-export.json";

static CLAUDE: HarnessSpec = HarnessSpec {
    name: "claude",
    default_model: "claude-opus-4-6",
    binaries: &[
        "/usr/local/bin/claude",
        "/usr/local/lib/node_modules/@anthropic-ai/claude-code/**",
        "/usr/local/bin/opencode",
        "/usr/local/lib/node_modules/opencode-ai/**",
    ],
    skills_dir: ".claude/skills",
    home_var: "CLAUDE_CONFIG_DIR",
    home: "/sandbox/.claude",
    config: "/tmp/.crucible-mcp.json",
    sandbox_env: &[
        ("CLAUDE_CODE_PLUGIN_SEED_DIR", "/sandbox/.claude-seed"),
        ("CLAUDE_CODE_SYNC_PLUGIN_INSTALL", "1"),
        ("DISABLE_AUTOUPDATER", "1"),
        ("CLAUDE_CODE_MAX_RETRIES", "20"),
    ],
    local_env: &[
        ("DISABLE_AUTOUPDATER", "1"),
        ("CLAUDE_CODE_ENTRYPOINT", "sdk-cli"),
    ],
    transcript: TranscriptLocator::NewestJsonl {
        sandbox_root: "/sandbox/.claude/projects",
        glob: "*/*.jsonl",
    },
    transcript_fetch_timeout: Duration::from_secs(30),
    auth: AuthProvider::Vertex,
    api: ApiFamily::Anthropic,
    protocols: &[InferenceProtocol::Messages],
    model_endpoints: CLAUDE_ENDPOINTS,
    sandbox_capability: Some("agent.claude-code"),
    persistent_sessions: true,
    otel_capable: true,
    backfill_required: false,
};

static CODEX: HarnessSpec = HarnessSpec {
    name: "codex",
    default_model: "gpt-5.6-sol",
    binaries: &[
        "/usr/local/bin/codex",
        "/usr/local/lib/node_modules/@openai/codex/**",
    ],
    skills_dir: ".claude/skills",
    home_var: "CODEX_HOME",
    home: "/sandbox/.codex",
    config: "/sandbox/.codex/config.toml",
    sandbox_env: &[],
    local_env: &[],
    transcript: TranscriptLocator::NewestJsonl {
        sandbox_root: "/sandbox/.codex/sessions",
        glob: "*/*/*/rollout-*.jsonl",
    },
    transcript_fetch_timeout: Duration::from_secs(30),
    auth: AuthProvider::Codex,
    api: ApiFamily::OpenAi,
    protocols: &[
        InferenceProtocol::ChatCompletions,
        InferenceProtocol::Responses,
    ],
    model_endpoints: CODEX_ENDPOINTS,
    sandbox_capability: Some("agent.codex"),
    persistent_sessions: false,
    otel_capable: false,
    backfill_required: false,
};

static OPENCODE: HarnessSpec = HarnessSpec {
    name: "opencode",
    default_model: "claude-opus-4-6",
    binaries: &[
        "/usr/local/bin/opencode",
        "/usr/local/lib/node_modules/opencode-ai/**",
    ],
    skills_dir: ".claude/skills",
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
        sandbox_path: OPENCODE_EXPORT,
    },
    transcript_fetch_timeout: Duration::from_secs(60),
    auth: AuthProvider::ApiKey,
    api: ApiFamily::Both,
    protocols: &[
        InferenceProtocol::Messages,
        InferenceProtocol::ChatCompletions,
        InferenceProtocol::Responses,
    ],
    model_endpoints: OPENAI_API_ENDPOINTS,
    sandbox_capability: Some("agent.opencode"),
    persistent_sessions: false,
    otel_capable: false,
    backfill_required: true,
};

static PI: HarnessSpec = HarnessSpec {
    name: "pi",
    default_model: "claude-opus-4-6",
    binaries: &[
        "/usr/local/bin/pi",
        "/usr/local/lib/node_modules/@earendil-works/pi-coding-agent/**",
        "/usr/bin/node",
        "/usr/bin/node-*",
        "/usr/local/bin/node",
    ],
    skills_dir: ".agents/skills",
    home_var: "PI_CODING_AGENT_DIR",
    home: "/sandbox/.pi/agent",
    config: "/sandbox/.pi/agent/models.json",
    sandbox_env: &[("PI_OFFLINE", "1"), ("PI_SKIP_VERSION_CHECK", "1")],
    local_env: &[("PI_SKIP_VERSION_CHECK", "1")],
    transcript: TranscriptLocator::NewestJsonl {
        sandbox_root: "/sandbox/.pi/agent/sessions",
        glob: "*/*.jsonl",
    },
    transcript_fetch_timeout: Duration::from_secs(30),
    auth: AuthProvider::ApiKey,
    api: ApiFamily::Both,
    protocols: &[
        InferenceProtocol::Messages,
        InferenceProtocol::ChatCompletions,
        InferenceProtocol::Responses,
    ],
    model_endpoints: OPENAI_API_ENDPOINTS,
    sandbox_capability: Some("agent.pi"),
    persistent_sessions: false,
    otel_capable: false,
    backfill_required: false,
};

impl Harness {
    /// This harness's table entry.
    pub const fn spec(self) -> &'static HarnessSpec {
        match self {
            Harness::Claude => &CLAUDE,
            Harness::Codex => &CODEX,
            Harness::OpenCode => &OPENCODE,
            Harness::Pi => &PI,
        }
    }

    /// Whether this harness is a client of `protocol`.
    pub fn speaks(self, protocol: InferenceProtocol) -> bool {
        self.spec().protocols.contains(&protocol)
    }

    /// Every harness, in CLI order.
    pub fn all() -> &'static [Harness] {
        <Harness as clap::ValueEnum>::value_variants()
    }

    /// The first harness that speaks `protocol`, claude when none does.
    pub fn default_for(protocol: InferenceProtocol) -> Harness {
        Harness::all()
            .iter()
            .copied()
            .find(|h| h.speaks(protocol))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use crate::harness::*;

    #[test]
    fn every_harness_names_its_agent_tool_and_a_default_model() {
        for harness in Harness::all() {
            let spec = harness.spec();
            assert_eq!(spec.name, harness.as_str());
            assert!(!spec.default_model.is_empty(), "{harness:?}");
            assert!(!spec.binaries.is_empty(), "{harness:?}");
        }
    }

    #[test]
    fn a_harness_speaks_exactly_the_protocols_its_api_family_covers() {
        use InferenceProtocol::{ChatCompletions, Decisions, Messages, Responses, SystemOne};
        for harness in Harness::all() {
            let spec = harness.spec();
            let openai =
                spec.protocols.contains(&ChatCompletions) || spec.protocols.contains(&Responses);
            let anthropic = spec.protocols.contains(&Messages);
            let expected = match spec.api {
                ApiFamily::Anthropic => (true, false),
                ApiFamily::OpenAi => (false, true),
                ApiFamily::Both => (true, true),
            };
            assert_eq!((anthropic, openai), expected, "{harness:?}");
            assert!(!harness.speaks(SystemOne) && !harness.speaks(Decisions));
        }
    }

    #[test]
    fn the_default_harness_for_a_protocol_is_the_first_that_speaks_it() {
        use InferenceProtocol::{ChatCompletions, Decisions, Messages, Responses, SystemOne};
        assert_eq!(Harness::default_for(Messages), Harness::Claude);
        assert_eq!(Harness::default_for(ChatCompletions), Harness::Codex);
        assert_eq!(Harness::default_for(Responses), Harness::Codex);
        assert_eq!(Harness::default_for(SystemOne), Harness::Claude);
        assert_eq!(Harness::default_for(Decisions), Harness::Claude);
    }

    #[test]
    fn only_claude_resumes_sessions_and_only_vertex_harnesses_are_key_free() {
        for harness in Harness::all() {
            let spec = harness.spec();
            assert_eq!(spec.persistent_sessions, *harness == Harness::Claude);
            let key_free = matches!(spec.auth, AuthProvider::Vertex);
            assert_eq!(key_free, *harness == Harness::Claude, "{harness:?}");
        }
    }
}
