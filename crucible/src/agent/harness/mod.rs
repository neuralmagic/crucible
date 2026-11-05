//! The agent-harness boundary: everything that knows which agent CLI runs a turn.
//!
//! A harness is the program that turns a prompt into edits + a stream of [`AgentEvent`]s:
//! claude, codex, opencode, or pi. Everything downstream of the decoder (session log, SSE, cost,
//! keep/discard) consumes the harness-neutral `AgentEvent`, so the swappable surface is exactly
//! what lives here: argv construction, env defaults, the sandbox seed files, the stream decoder,
//! and the post-turn transcript backfill. A backend is the [`HarnessSpec`] table entry plus the
//! few [`Backend`] methods that genuinely differ per CLI; the manifest's `Harness` token
//! (`[agent].harness` or `--harness`, riding on [`TurnAgent`]) selects one through [`backend`].

pub(crate) mod claude;
pub(crate) mod codex;
pub(crate) mod opencode;
pub(crate) mod pi;

use crate::agent::event::AgentEvent;
use crate::agent::inference::{InferenceEnv, WireApi};
use crate::agent::sandbox_env::sh_quote;
use crate::args::TurnAgent;
use crate::manifest::Harness;
use crate::openshell::provider::CodexAuth;
use crate::turn_trace::{GenAiRecord, ToolInvocation};
use crucible::harness::{AuthProvider, HarnessSpec};
use crucible_harness::line::LineFormat;
use crucible_harness::otel::LiveMeters;

/// Which credential this turn runs on: the harness's default, unless the environment carries a
/// direct Anthropic key for a Claude-speaking harness, or nothing at all for a key-authenticated
/// one, which then runs on the ambient Vertex credential the way claude does.
pub(crate) fn resolve_auth(
    harness: Harness,
    inference: &crate::agent::inference::InferenceEnv,
) -> AuthProvider {
    match (harness.spec().auth, inference.anthropic_key.as_deref()) {
        (AuthProvider::Vertex, Some(_)) => AuthProvider::AnthropicKey,
        (auth, _) if auth.runs_on_ambient_vertex() && inference.is_ambient() => {
            AuthProvider::Vertex
        }
        (auth, _) => auth,
    }
}

/// The GenAI provider name (`gen_ai.system`) of the API a turn on `auth` reaches. A
/// key-authenticated harness names the API its endpoint speaks.
pub(crate) fn gen_ai_system(
    auth: AuthProvider,
    model: &str,
    inference: &InferenceEnv,
    manifest_env: &[(String, String)],
) -> &'static str {
    match auth {
        AuthProvider::Vertex => "gcp.vertex_ai",
        AuthProvider::AnthropicKey => "anthropic",
        AuthProvider::Codex => "openai",
        AuthProvider::ApiKey => match api_endpoint(model, inference, manifest_env) {
            ApiEndpoint::OpenAi { .. } => "openai",
            ApiEndpoint::Anthropic { .. } => "anthropic",
            ApiEndpoint::Vertex(_) => "gcp.vertex_ai",
        },
    }
}

/// The API family a provider-table harness (opencode, pi) talks to, with the base URL it is
/// reached at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ApiEndpoint {
    /// An OpenAI-speaking service: a custom endpoint, or api.openai.com itself.
    OpenAi { base_url: String, wire_api: WireApi },
    /// An Anthropic Messages service: a custom endpoint, or api.anthropic.com itself.
    Anthropic { base_url: String },
    /// Anthropic on Vertex AI, on the gateway's ambient credential rather than a key.
    Vertex(crate::agent::inference::VertexConfig),
}

/// Where a provider-table turn sends `model`: Vertex when the env names neither a key nor a base
/// URL (`manifest_env` carries the project and region); else an OpenAI-speaking endpoint when the
/// env names one (`OPENAI_BASE_URL`) or carries only an OpenAI key, else Anthropic. A model named
/// `claude-*` with both keys present goes to Anthropic. The wire API is the env's when it names
/// one; otherwise api.openai.com itself speaks Responses (its current models refuse tool calls
/// over chat completions) and a custom endpoint speaks chat completions.
pub(crate) fn api_endpoint(
    model: &str,
    inference: &InferenceEnv,
    manifest_env: &[(String, String)],
) -> ApiEndpoint {
    if inference.is_ambient() {
        return ApiEndpoint::Vertex(crate::agent::inference::VertexConfig::from_env(
            manifest_env,
        ));
    }
    let openai = inference.openai_base_url.is_some()
        || (inference.openai_key.is_some()
            && (inference.anthropic_key.is_none() || !model.starts_with("claude")));
    if openai {
        let (base_url, default_wire) = match inference.openai_base_url.clone() {
            Some(url) => (url, WireApi::Chat),
            None => (
                crate::agent::inference::OPENAI_API_URL.to_string(),
                WireApi::Responses,
            ),
        };
        ApiEndpoint::OpenAi {
            base_url,
            wire_api: inference.wire_api.unwrap_or(default_wire),
        }
    } else {
        ApiEndpoint::Anthropic {
            base_url: inference
                .anthropic_base_url
                .clone()
                .unwrap_or_else(|| crate::agent::inference::ANTHROPIC_API_URL.to_string()),
        }
    }
}

/// The provider id a key-authenticated harness's seeded provider table registers the turn's
/// endpoint under, and the prefix its model rides on (`crucible/<model>`).
pub(crate) const CRUCIBLE_PROVIDER_ID: &str = "crucible";

/// A JSON object's string field, empty when absent or not a string.
pub(crate) fn json_str(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string()
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

    /// The agent's workdir for a workspace uploaded under `basename`.
    pub(crate) fn workdir(basename: &str) -> String {
        format!("{}/{basename}", Self::HOME)
    }
}

/// One file uploaded into the sandbox before the agent execs (claude: `.mcp.json` when the turn
/// reaches an MCP server; hermes: its `config.yaml`). Rendered host-side, targeted-uploaded to `dest`.
pub(crate) struct SeedFile {
    pub content: String,
    pub dest: &'static str,
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

/// The credential a sandbox turn resolved for its [`AuthProvider`], handed to the backend so it
/// can seed whatever its CLI reads off disk.
#[derive(Clone)]
pub(crate) enum SandboxAuth {
    /// Served by the gateway's metadata emulator; nothing to seed.
    Gateway,
    /// Injected by the egress proxy; the sandbox env holds the gateway's placeholder.
    AnthropicKey,
    /// Injected by the egress proxy; the sandbox env holds the gateway's placeholder under the
    /// endpoint's own variable.
    ApiKey,
    Codex(CodexAuth),
}

impl SandboxAuth {
    pub(crate) fn codex(&self) -> Option<&CodexAuth> {
        match self {
            SandboxAuth::Codex(auth) => Some(auth),
            SandboxAuth::Gateway | SandboxAuth::AnthropicKey | SandboxAuth::ApiKey => None,
        }
    }

    /// What a seeded file may carry of this auth: a Codex API key only as `issued`, the
    /// placeholder the gateway gave the sandbox for it, never the key itself.
    pub(crate) fn seeded(&self, issued: Option<String>) -> SandboxAuth {
        match self {
            SandboxAuth::Codex(CodexAuth::ApiKey(_)) => {
                SandboxAuth::Codex(CodexAuth::ApiKey(issued.unwrap_or_default()))
            }
            other => other.clone(),
        }
    }
}

/// One MCP server a seeded config points the agent at. `token`, when set, rides as an
/// `Authorization: Bearer` header: the server's port sits on a `0.0.0.0` bind, so the header is
/// what makes the sandbox the only caller it answers.
pub(crate) struct McpServer<'a> {
    /// The agent-visible MCP server name (the `mcp__<name>__…` tool prefix).
    pub name: &'a str,
    pub url: &'a str,
    pub token: Option<&'a str>,
}

/// The servers as a JSON object keyed by name: `base(server)` for each, plus a `headers` block
/// carrying the bearer when the server has a token.
pub(crate) fn json_servers(
    servers: &[McpServer<'_>],
    base: impl Fn(&McpServer<'_>) -> serde_json::Value,
) -> serde_json::Value {
    let entries: serde_json::Map<String, serde_json::Value> = servers
        .iter()
        .map(|s| {
            let mut server = base(s);
            if let Some(t) = s.token {
                server["headers"] = serde_json::json!({ "Authorization": format!("Bearer {t}") });
            }
            (s.name.to_string(), server)
        })
        .collect();
    serde_json::Value::Object(entries)
}

/// One agent harness: its [`HarnessSpec`] plus what genuinely differs per CLI (the argv grammar,
/// the config file, the credential file, the stdout decoder, the transcript format). The
/// provided methods assemble the sandbox env, the local env, and the seed-file set from those,
/// once for every backend.
pub(crate) trait Backend: Sync {
    fn spec(&self) -> &'static HarnessSpec;

    /// The model this backend hands its turn. Most take the shared `[agent].model` as-is; one that
    /// reads a `[agent.<harness>].model` of its own, or rejects a name from another vendor,
    /// overrides this and its argv builders call it.
    fn model<'a>(&self, agent: &TurnAgent<'a>) -> &'a str {
        agent.model
    }

    /// The full local-spawn argv (program name first): flags + the prompt as an argument.
    fn local_argv(&self, agent: &TurnAgent<'_>, prompt: &str) -> Vec<String>;

    /// The sandbox exec argv (program name first, no prompt, it arrives over stdin).
    /// `mcp_seeded` says whether the turn reaches any MCP server, and so whether
    /// [`Backend::seed_files`] delivered an MCP config, so a harness that takes it on argv
    /// (claude's `--mcp-config`, pi's `-e` for the adapter) can point at it.
    fn sandbox_argv(&self, agent: &TurnAgent<'_>, mcp_seeded: bool) -> Vec<String>;

    /// Start or resume a Crucible-managed session locally.
    fn local_session_argv(
        &self,
        _agent: &TurnAgent<'_>,
        _prompt: &str,
        _session: &crate::agent::agent_session::SessionTurn,
    ) -> std::io::Result<Vec<String>> {
        Err(no_sessions(self.spec()))
    }

    /// Sandbox counterpart of [`Backend::local_session_argv`].
    fn sandbox_session_argv(
        &self,
        _agent: &TurnAgent<'_>,
        _mcp_seeded: bool,
        _session: &crate::agent::agent_session::SessionTurn,
    ) -> std::io::Result<Vec<String>> {
        Err(no_sessions(self.spec()))
    }

    /// The config file seeded at [`HarnessSpec::config`] before the agent execs, rendered
    /// host-side; `None` seeds nothing this turn.
    fn config(
        &self,
        agent: &TurnAgent<'_>,
        servers: &[McpServer<'_>],
        inference: &InferenceEnv,
    ) -> Option<String>;

    /// The credential file a harness seeds when its credential cannot ride the gateway as-is:
    /// codex's `auth.json`, pi's Vertex extension.
    fn credential(&self, _agent: &TurnAgent<'_>, _auth: &SandboxAuth) -> Option<SeedFile> {
        None
    }

    /// The MCP config a harness seeds beside its own config when the turn reaches any server: pi's
    /// `mcp.json` for the adapter extension. The others carry the servers inside
    /// [`Backend::config`] instead.
    fn mcp_config(&self, _servers: &[McpServer<'_>]) -> Option<SeedFile> {
        None
    }

    /// The stream decoder for this harness's stdout. `meters`, when present, are the in-process
    /// OTLP collector's live readings (60 s-window token rate and running cost) stamped onto each
    /// `tokens` sample; `tool_io` opts tool events into carrying bounded inputs and result excerpts
    /// (see [`crate::agent::turn::tool_io_full`]).
    fn decoder(
        &self,
        agent: &TurnAgent<'_>,
        meters: Option<&LiveMeters>,
        tool_io: bool,
    ) -> Box<dyn LineFormat + Send>;

    /// Parse the downloaded transcript's in-memory bytes into [`TurnArtifacts`]. The caller reads
    /// the file ONCE (async, see the fetch path) and hands the bytes here; this method is pure and
    /// infallible: undecodable or unparseable bytes yield empty artifacts (the fetch layer decides
    /// whether that is loud, see [`HarnessSpec::backfill_required`]). Bytes, not `&str`, because a
    /// backfill harness's transcript is binary (hermes's SQLite `state.db`), while claude's is
    /// UTF-8 jsonl, each arm decodes its own format.
    fn parse_transcript(&self, content: &[u8]) -> TurnArtifacts;

    /// The turn's conversation as GenAI records for content-log export, parsed from the same
    /// in-memory transcript bytes (no re-read).
    fn content_records(&self, content: &[u8]) -> Vec<GenAiRecord>;

    /// The local-backend counterpart of the sandbox transcript fetch: recover
    /// [`TurnArtifacts`] from this machine after a local turn. `None` when the harness has
    /// nothing to backfill (claude's live stream already carried everything).
    fn local_backfill(&self, _paths: &crate::args::Paths) -> Option<TurnArtifacts> {
        None
    }

    /// Env defaults for a local spawn, applied before the manifest `[agent].env` (so a manifest
    /// override wins): `AGENT_TOOL`, then the spec's.
    fn local_env(&self) -> Vec<(&'static str, &'static str)> {
        let spec = self.spec();
        let mut env = vec![("AGENT_TOOL", spec.name)];
        env.extend_from_slice(spec.local_env);
        env
    }

    /// Files uploaded into the sandbox before the agent execs: the config at the spec's path when
    /// the backend renders one, then its credential file, then the MCP config. `servers` are
    /// exactly the ones in the turn's scope, each token the raw value or the provider placeholder
    /// the openshell egress proxy resolves.
    fn seed_files(
        &self,
        agent: &TurnAgent<'_>,
        servers: &[McpServer<'_>],
        auth: &SandboxAuth,
        inference: &InferenceEnv,
    ) -> Vec<SeedFile> {
        let mut seeds: Vec<SeedFile> = self
            .config(agent, servers, inference)
            .map(|content| SeedFile {
                content,
                dest: self.spec().config,
            })
            .into_iter()
            .collect();
        seeds.extend(self.credential(agent, auth));
        if !servers.is_empty() {
            seeds.extend(self.mcp_config(servers));
        }
        seeds
    }
}

fn no_sessions(spec: &HarnessSpec) -> std::io::Error {
    std::io::Error::other(format!(
        "{} does not support Crucible-managed sessions",
        spec.name
    ))
}

/// The manifest's harness token, resolved to its backend.
pub(crate) fn backend(harness: Harness) -> &'static dyn Backend {
    match harness {
        Harness::Claude => &claude::Claude,
        Harness::Codex => &codex::Codex,
        Harness::OpenCode => &opencode::OpenCode,
        Harness::Pi => &pi::Pi,
    }
}

/// The `bash -c` exec wrapper: cd into the workdir, source the env, exec the agent with the
/// prompt redirected from its file (openshell exec rejects newlines in argv, so the prompt
/// can't be an argument). Validated in-pod. Shared by both harnesses (hermes uses the same
/// stdin-redirect pattern), so callers use it directly, no per-harness dispatch.
pub(crate) fn exec_wrapper(workdir_basename: &str, agent_args: &[String]) -> Vec<String> {
    let script = format!(
        "cd {} && . {} && exec \"$@\" < {}",
        sh_quote(&SandboxLayout::workdir(workdir_basename)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::event::AgentEvent;
    use crate::agent::sandbox_env::SandboxEnv;
    use clap::Parser;
    use crucible::harness::TranscriptLocator;

    #[test]
    fn npm_installed_harnesses_allow_their_canonical_install_tree() {
        let features = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../images/features");
        for (harness, feature) in [
            (Harness::Claude, "claude-code"),
            (Harness::Codex, "codex"),
            (Harness::OpenCode, "opencode"),
            (Harness::Pi, "pi"),
        ] {
            let script = std::fs::read_to_string(features.join(feature).join("install.sh"))
                .unwrap_or_else(|e| panic!("{feature}/install.sh: {e}"));
            let package = script
                .lines()
                .find_map(|line| {
                    let rest = line.trim().strip_prefix("npm install -g ")?;
                    let quoted = rest.trim_matches('"');
                    let at = quoted.rfind('@').filter(|&i| i > 0)?;
                    Some(quoted[..at].to_string())
                })
                .unwrap_or_else(|| panic!("{feature}/install.sh installs no npm package"));
            let tree = format!("/usr/local/lib/node_modules/{package}/**");
            assert!(
                harness.spec().binaries.contains(&tree.as_str()),
                "{harness:?} does not allow {tree}"
            );
        }
    }

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
    fn harness_defaults_to_claude() {
        assert_eq!(Harness::default(), Harness::Claude);
    }

    #[test]
    fn harness_deserializes_lowercase() {
        #[derive(serde::Deserialize)]
        struct T {
            harness: Harness,
        }
        let t: T = toml::from_str("harness = \"claude\"").expect("parse");
        assert_eq!(t.harness, Harness::Claude);
        let t: T = toml::from_str("harness = \"codex\"").expect("parse");
        assert_eq!(t.harness, Harness::Codex);
        let t: T = toml::from_str("harness = \"opencode\"").expect("parse");
        assert_eq!(t.harness, Harness::OpenCode);
        let t: T = toml::from_str("harness = \"pi\"").expect("parse");
        assert_eq!(t.harness, Harness::Pi);
        assert!(toml::from_str::<T>("harness = \"Claude\"").is_err());
        assert!(toml::from_str::<T>("harness = \"open-code\"").is_err());
    }

    #[test]
    fn harness_value_enum_accepts_codex_on_the_cli() {
        use clap::ValueEnum;
        assert_eq!(
            Harness::from_str("codex", true).expect("--harness codex"),
            Harness::Codex
        );
        assert!(Harness::from_str("Codex", false).is_err());
    }

    /// The CLI spelling, the manifest spelling, and `as_str` agree for every harness, so a
    /// controller rendering `--harness=<as_str>` and a manifest naming the same token select the
    /// same backend; `OpenCode` in particular must not kebab-case to `open-code`.
    #[test]
    fn every_harness_spells_the_same_on_the_cli_and_in_the_manifest() {
        use clap::ValueEnum;
        for harness in Harness::value_variants() {
            let cli = harness
                .to_possible_value()
                .expect("every variant is selectable")
                .get_name()
                .to_string();
            assert_eq!(cli, harness.as_str(), "{harness:?}");
            assert_eq!(
                Harness::from_str(&cli, false).expect("round trip"),
                *harness
            );
            assert_eq!(
                cli,
                harness.spec().name,
                "the AGENT_TOOL name is the same token"
            );
        }
    }

    #[test]
    fn only_the_codex_harness_grows_the_openai_endpoints() {
        // A claude turn's allowlist is the shared defaults plus Claude Code's log intake.
        let endpoints = crate::openshell::policy::default_endpoints;
        let claude = endpoints(Harness::Claude);
        assert_eq!(
            &claude[..crate::openshell::policy::DEFAULT_ENDPOINTS.len()],
            crate::openshell::policy::DEFAULT_ENDPOINTS
        );
        assert_eq!(
            &claude[crate::openshell::policy::DEFAULT_ENDPOINTS.len()..],
            crate::openshell::policy::CLAUDE_ENDPOINTS
        );
        let codex = endpoints(Harness::Codex);
        assert_eq!(
            &codex[..crate::openshell::policy::DEFAULT_ENDPOINTS.len()],
            crate::openshell::policy::DEFAULT_ENDPOINTS,
            "the shared defaults come first, unchanged"
        );
        for host in [
            "chatgpt.com",
            "auth.openai.com",
            "api.openai.com",
            "ab.chatgpt.com",
        ] {
            assert!(
                codex.iter().any(|e| e.starts_with(&format!("{host}:443:"))),
                "codex needs {host}: {codex:?}"
            );
            assert!(
                !endpoints(Harness::Claude).iter().any(|e| e.contains(host)),
                "claude must not reach {host}"
            );
        }
        // L4 only: an entry carrying `protocol=rest` would break CONNECT tunneling.
        assert!(!codex.iter().any(|e| e.contains("protocol=rest")));
        // The key-authenticated harnesses reach api.openai.com and nothing of ChatGPT's.
        for harness in [Harness::OpenCode, Harness::Pi] {
            let got = endpoints(harness);
            assert_eq!(
                &got[..crate::openshell::policy::DEFAULT_ENDPOINTS.len()],
                crate::openshell::policy::DEFAULT_ENDPOINTS
            );
            assert_eq!(
                &got[crate::openshell::policy::DEFAULT_ENDPOINTS.len()..],
                crate::openshell::policy::OPENAI_API_ENDPOINTS,
                "{harness:?}"
            );
            assert!(!got.iter().any(|e| e.contains("chatgpt.com")));
        }
        for harness in [Harness::Codex, Harness::OpenCode, Harness::Pi] {
            assert!(
                !endpoints(harness)
                    .iter()
                    .any(|e| e.contains("datadoghq.com")),
                "only claude posts to its own log intake: {harness:?}"
            );
        }
    }

    /// A turn's content logs name the provider its credential reaches, not the vendor of the
    /// harness that ran it.
    #[test]
    fn gen_ai_system_names_the_provider_the_turn_reaches() {
        let ambient = crate::agent::inference::InferenceEnv::default();
        let anthropic = crate::agent::inference::InferenceEnv {
            anthropic_key: Some("k".into()),
            ..Default::default()
        };
        let openai = crate::agent::inference::InferenceEnv {
            openai_key: Some("k".into()),
            ..Default::default()
        };
        let system =
            |harness: Harness, model: &str, env: &crate::agent::inference::InferenceEnv| {
                gen_ai_system(resolve_auth(harness, env), model, env, &[])
            };
        assert_eq!(
            system(Harness::Claude, "claude-opus-4-6", &ambient),
            "gcp.vertex_ai"
        );
        assert_eq!(
            system(Harness::Claude, "claude-opus-4-6", &anthropic),
            "anthropic"
        );
        assert_eq!(system(Harness::Codex, "gpt-5.6-sol", &ambient), "openai");
        assert_eq!(system(Harness::OpenCode, "qwen", &openai), "openai");
        assert_eq!(
            system(Harness::Pi, "claude-opus-4-6", &anthropic),
            "anthropic"
        );
        assert_eq!(
            system(Harness::Pi, "claude-opus-4-6", &ambient),
            "gcp.vertex_ai"
        );
    }

    /// Where a key-authenticated turn goes: a named OpenAI base URL wins outright; otherwise the
    /// keys decide, with a `claude-*` model preferring Anthropic when both keys are present.
    #[test]
    fn api_endpoint_follows_the_base_url_then_the_keys() {
        let env = |openai_key: Option<&str>, anthropic_key: Option<&str>, url: Option<&str>| {
            crate::agent::inference::InferenceEnv {
                openai_key: openai_key.map(str::to_string),
                anthropic_key: anthropic_key.map(str::to_string),
                openai_base_url: url.map(str::to_string),
                ..Default::default()
            }
        };
        assert_eq!(
            api_endpoint(
                "qwen",
                &env(Some("k"), None, Some("http://vllm:8000/v1")),
                &[]
            ),
            ApiEndpoint::OpenAi {
                base_url: "http://vllm:8000/v1".into(),
                wire_api: WireApi::Chat
            }
        );
        assert_eq!(
            api_endpoint(
                "claude-opus-4-6",
                &env(None, Some("a"), Some("http://vllm:8000/v1")),
                &[]
            ),
            ApiEndpoint::OpenAi {
                base_url: "http://vllm:8000/v1".into(),
                wire_api: WireApi::Chat
            },
            "a named OpenAI endpoint wins even for a claude model name"
        );
        assert_eq!(
            api_endpoint("gpt-5.6-sol", &env(Some("k"), None, None), &[]),
            ApiEndpoint::OpenAi {
                base_url: "https://api.openai.com/v1".into(),
                wire_api: WireApi::Responses
            },
            "the vendor's own API speaks Responses"
        );
        assert_eq!(
            api_endpoint("claude-opus-4-6", &env(Some("k"), Some("a"), None), &[]),
            ApiEndpoint::Anthropic {
                base_url: "https://api.anthropic.com".into()
            }
        );
        assert_eq!(
            api_endpoint("gpt-5.6-sol", &env(Some("k"), Some("a"), None), &[]),
            ApiEndpoint::OpenAi {
                base_url: "https://api.openai.com/v1".into(),
                wire_api: WireApi::Responses
            }
        );
        assert_eq!(
            api_endpoint(
                "claude-opus-4-6",
                &env(None, None, None),
                &[
                    ("ANTHROPIC_VERTEX_PROJECT_ID".into(), "proj-x".into()),
                    ("CLOUD_ML_REGION".into(), "us-east5".into()),
                ]
            ),
            ApiEndpoint::Vertex(crate::agent::inference::VertexConfig {
                project: "proj-x".into(),
                region: "us-east5".into(),
            }),
            "no key and no base URL is the ambient Vertex credential"
        );
        assert_eq!(
            api_endpoint("claude-opus-4-6", &env(None, Some("a"), None), &[]),
            ApiEndpoint::Anthropic {
                base_url: "https://api.anthropic.com".into()
            }
        );
        let responses = crate::agent::inference::InferenceEnv {
            wire_api: Some(WireApi::Responses),
            ..env(Some("k"), None, Some("http://vllm:8000/v1"))
        };
        assert!(matches!(
            api_endpoint("m", &responses, &[]),
            ApiEndpoint::OpenAi {
                wire_api: WireApi::Responses,
                ..
            }
        ));
    }

    #[test]
    fn codex_sessions_are_stubbed_and_its_transcript_is_the_rollout_tree() {
        let args = crate::cli::Cli::parse_from(["crucible"]).run;
        let session = crate::agent::agent_session::SessionTurn {
            logical_name: "solver".to_string(),
            provider_id: "id".to_string(),
            completed_turns: 0,
        };
        assert!(
            crate::agent::harness::backend(Harness::Codex)
                .local_session_argv(&args.turn_agent(), "p", &session)
                .is_err()
        );
        assert!(
            crate::agent::harness::backend(Harness::Codex)
                .sandbox_session_argv(&args.turn_agent(), false, &session)
                .is_err()
        );
        assert_eq!(
            Harness::Codex.spec().transcript.jsonl_tree(),
            Some(("/sandbox/.codex/sessions", "*/*/*/rollout-*.jsonl"))
        );
    }

    /// Each harness gets the decoder its stdout speaks, and the codex parser is handed the model
    /// it will actually be invoked with (it keys the pricing estimate off that string).
    #[test]
    fn each_harness_decodes_its_own_stdout() {
        let mut args = crate::cli::Cli::parse_from(["crucible"]).run;
        let claude = crate::agent::harness::backend(Harness::Claude).decoder(&args.turn_agent(), None, false).push(
            r#"{"type":"system","subtype":"init","model":"claude-opus-4-8","tools":[],"agents":[]}"#,
        );
        assert!(
            matches!(&claude[0], AgentEvent::Init { model, .. } if model == "claude-opus-4-8"),
            "{claude:?}"
        );
        args.codex.model = Some("gpt-5.6-terra".to_string());
        let events = crate::agent::harness::backend(Harness::Codex)
            .decoder(&args.turn_agent(), None, false)
            .push("{\"type\":\"thread.started\",\"thread_id\":\"t1\"}");
        assert!(
            matches!(&events[0], AgentEvent::Init { model, .. } if model == "gpt-5.6-terra"),
            "{events:?}"
        );
        args.model = Some("qwen-3-8-27b".to_string());
        let events = crate::agent::harness::backend(Harness::OpenCode)
            .decoder(&args.turn_agent(), None, false)
            .push(r#"{"type":"step_start","sessionID":"ses_1","part":{"type":"step-start"}}"#);
        assert!(
            matches!(&events[0], AgentEvent::Init { model, .. } if model == "qwen-3-8-27b"),
            "{events:?}"
        );
        let events = crate::agent::harness::backend(Harness::Pi)
            .decoder(&args.turn_agent(), None, false)
            .push(r#"{"type":"session","version":3,"id":"u","timestamp":"t","cwd":"/w"}"#);
        assert!(
            matches!(&events[0], AgentEvent::Init { model, .. } if model == "qwen-3-8-27b"),
            "{events:?}"
        );
    }

    /// Each `NewestJsonl` harness names the glob its transcript tree actually needs: claude's
    /// project slug is one segment, codex's `YYYY/MM/DD` is three.
    #[test]
    fn the_transcript_globs_match_each_harness_tree() {
        for (harness, root, glob) in [
            (Harness::Claude, "/sandbox/.claude/projects", "*/*.jsonl"),
            (
                Harness::Codex,
                "/sandbox/.codex/sessions",
                "*/*/*/rollout-*.jsonl",
            ),
            (Harness::Pi, "/sandbox/.pi/agent/sessions", "*/*.jsonl"),
        ] {
            let (sandbox_root, g) = harness
                .spec()
                .transcript
                .jsonl_tree()
                .unwrap_or_else(|| panic!("{harness:?} reads a jsonl tree"));
            assert_eq!(sandbox_root, root);
            assert_eq!(g, glob);
            assert_eq!(g.matches('/').count() + 1, g.split('/').count());
        }
        assert_eq!(
            Harness::OpenCode.spec().transcript,
            TranscriptLocator::File {
                sandbox_path: "/sandbox/.local/share/opencode/crucible-export.json"
            }
        );
    }

    /// The env script and local env are the spec, rendered: `AGENT_TOOL` first, the home var
    /// second, then the spec's own exports, then the manifest's quoted overrides.
    #[test]
    fn env_assembly_reads_every_backend_off_its_spec() {
        for harness in Harness::all() {
            let spec = harness.spec();
            let script = SandboxEnv::from_manifest([("K", "v w")]).script(spec);
            let lines: Vec<&str> = script.lines().collect();
            assert_eq!(lines[0], format!("export AGENT_TOOL={}", spec.name));
            assert_eq!(lines[1], format!("export {}={}", spec.home_var, spec.home));
            assert_eq!(lines.len(), 3 + spec.sandbox_env.len(), "{script}");
            assert_eq!(lines[lines.len() - 1], "export K='v w'");
            let local = crate::agent::harness::backend(*harness).local_env();
            assert_eq!(local[0], ("AGENT_TOOL", spec.name));
            assert_eq!(&local[1..], spec.local_env);
            assert!(spec.home.starts_with(SandboxLayout::HOME), "{harness:?}");
        }
    }

    /// A direct Anthropic key in the environment moves a Claude-speaking harness off Vertex; Codex
    /// never reads it.
    #[test]
    fn a_direct_anthropic_key_selects_key_auth_for_claude_only() {
        let keyed = crate::agent::inference::InferenceEnv {
            anthropic_key: Some("sk-ant".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_auth(Harness::Claude, &keyed),
            AuthProvider::AnthropicKey
        );
        assert_eq!(resolve_auth(Harness::Codex, &keyed), AuthProvider::Codex);
        assert_eq!(
            resolve_auth(Harness::Claude, &Default::default()),
            AuthProvider::Vertex
        );
        // The provider-table harnesses take any key the env carries, and Vertex when it carries
        // nothing, the same ambient credential claude runs on.
        for harness in [Harness::OpenCode, Harness::Pi] {
            assert_eq!(resolve_auth(harness, &keyed), AuthProvider::ApiKey);
            assert_eq!(
                resolve_auth(harness, &Default::default()),
                AuthProvider::Vertex
            );
            let url_only = crate::agent::inference::InferenceEnv {
                openai_base_url: Some("http://vllm:8000/v1".into()),
                ..Default::default()
            };
            assert_eq!(resolve_auth(harness, &url_only), AuthProvider::ApiKey);
        }
    }

    #[test]
    fn capability_split_between_the_harnesses() {
        assert_eq!(Harness::Claude.spec().auth, AuthProvider::Vertex);
        // Codex talks to the ChatGPT backend on an OAuth access token instead.
        assert_eq!(Harness::Codex.spec().auth, AuthProvider::Codex);
        assert!(Harness::Claude.spec().otel_capable);
        assert!(!Harness::Codex.spec().otel_capable);
        assert!(!Harness::Claude.spec().backfill_required);
        // The codex `--json` stream carries result + usage, so the rollout is garnish.
        assert!(!Harness::Codex.spec().backfill_required);
        // OpenCode's mirrored stream can lose its tail in a container: the export closes the
        // turn. Pi's stream is its own process's and complete.
        assert_eq!(Harness::OpenCode.spec().auth, AuthProvider::ApiKey);
        assert_eq!(Harness::Pi.spec().auth, AuthProvider::ApiKey);
        assert!(!Harness::OpenCode.spec().otel_capable);
        assert!(!Harness::Pi.spec().otel_capable);
        assert!(Harness::OpenCode.spec().backfill_required);
        assert!(!Harness::Pi.spec().backfill_required);
    }
}
