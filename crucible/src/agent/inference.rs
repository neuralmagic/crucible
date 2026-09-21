//! Where a turn's model is reached and how it is paid for, as the controller (or an operator's
//! shell) says it through the process environment. The variable names are OpenShell's provider
//! config and credential keys, so a provider registered on its gateway and one delivered here
//! spell the same thing.
//!
//! Absent, every field is `None` and a turn authenticates the way it always has: Vertex through
//! the gateway's metadata emulator for Claude (and for opencode and pi, which speak Vertex through
//! their own provider tables), the selected `[agent.codex]` auth for Codex.

use crate::manifest::Harness;
use anyhow::{Context, Result};
use crucible_contract::inference::{InferenceBinding, InferenceProtocol, InferenceRole};

#[derive(Debug, thiserror::Error)]
#[error("{WIRE_API}={raw:?} is neither chat nor responses")]
pub struct UnknownWireApi {
    raw: String,
}

#[derive(Debug, thiserror::Error)]
#[error("an agent binding cannot speak {protocol}")]
pub struct NotAnAgentProtocol {
    protocol: InferenceProtocol,
}

/// The variable carrying an Anthropic API key. Its presence selects direct Anthropic auth for
/// Claude over the Vertex default.
pub const ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";
/// The base URL a Messages-speaking service is reached at, in place of `api.anthropic.com`.
pub const ANTHROPIC_BASE_URL: &str = "ANTHROPIC_BASE_URL";
/// The base URL an OpenAI-speaking service is reached at, in place of `api.openai.com`.
pub const OPENAI_BASE_URL: &str = "OPENAI_BASE_URL";
/// The variable Codex reads a custom endpoint's key from (`env_key` in its provider config).
pub const OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";
/// Codex's wire API for a custom OpenAI-speaking endpoint: `chat` or `responses`.
pub const WIRE_API: &str = "CRUCIBLE_INFERENCE_WIRE_API";

/// The Vertex selectors a manifest sets for a Claude turn; a direct-key turn must not carry them,
/// or Claude Code ignores the key and asks the metadata emulator that is not there.
pub const VERTEX_SELECTORS: &[&str] = &[
    "CLAUDE_CODE_USE_VERTEX",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "CLOUD_ML_REGION",
    "VERTEX_LOCATION",
    "GCP_PROJECT_ID",
];

/// The API shape Codex speaks to a custom endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireApi {
    Chat,
    Responses,
}

impl WireApi {
    pub fn parse(raw: &str) -> Result<WireApi, UnknownWireApi> {
        match raw.trim() {
            "chat" => Ok(WireApi::Chat),
            "responses" => Ok(WireApi::Responses),
            other => Err(UnknownWireApi {
                raw: other.to_string(),
            }),
        }
    }

    /// Codex's own spelling in `config.toml`.
    pub fn as_str(self) -> &'static str {
        match self {
            WireApi::Chat => "chat",
            WireApi::Responses => "responses",
        }
    }
}

/// The protocol and model an injected agent binding fixes for the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSelection {
    pub protocol: InferenceProtocol,
    pub model: String,
}

/// Whether `harness` is a client of `protocol`.
pub fn speaks(harness: Harness, protocol: InferenceProtocol) -> bool {
    match protocol {
        InferenceProtocol::Messages => matches!(
            harness,
            Harness::Claude | Harness::Hermes | Harness::OpenCode | Harness::Pi
        ),
        InferenceProtocol::ChatCompletions | InferenceProtocol::Responses => {
            matches!(harness, Harness::Codex | Harness::OpenCode | Harness::Pi)
        }
        InferenceProtocol::SystemOne => false,
    }
}

impl AgentSelection {
    /// `preferred` when it speaks the binding's protocol, else the harness that does.
    pub fn harness(&self, preferred: Harness) -> Harness {
        if speaks(preferred, self.protocol) {
            return preferred;
        }
        match self.protocol {
            InferenceProtocol::ChatCompletions | InferenceProtocol::Responses => Harness::Codex,
            InferenceProtocol::Messages | InferenceProtocol::SystemOne => Harness::Claude,
        }
    }
}

/// What the run was told about reaching the agent's model: an injected agent binding when there
/// is one, else the process's ambient environment. Read once per turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InferenceEnv {
    pub anthropic_key: Option<String>,
    pub anthropic_base_url: Option<String>,
    /// The OpenAI-speaking key a key-authenticated harness (opencode, pi) relays into the sandbox;
    /// codex selects its own key by name through `[agent.codex]` instead.
    pub openai_key: Option<String>,
    pub openai_base_url: Option<String>,
    pub wire_api: Option<WireApi>,
    /// Set exactly when an agent binding was injected.
    pub selection: Option<AgentSelection>,
}

fn non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl InferenceEnv {
    /// Read the process environment. A base URL is checked here so a bad one fails the turn at
    /// its start rather than as a connection error inside the sandbox.
    pub fn from_process_env() -> Result<InferenceEnv> {
        let injected = crucible::inference::from_process_env()?;
        if let Some(binding) = injected.binding(InferenceRole::Agent) {
            return Self::from_binding(binding, |name| std::env::var(name).ok());
        }
        let env = InferenceEnv {
            anthropic_key: non_empty(ANTHROPIC_API_KEY),
            anthropic_base_url: non_empty(ANTHROPIC_BASE_URL),
            openai_key: non_empty(OPENAI_API_KEY_ENV),
            openai_base_url: non_empty(OPENAI_BASE_URL),
            wire_api: non_empty(WIRE_API)
                .map(|w| WireApi::parse(&w))
                .transpose()?,
            selection: None,
        };
        for (key, url) in [
            (ANTHROPIC_BASE_URL, &env.anthropic_base_url),
            (OPENAI_BASE_URL, &env.openai_base_url),
        ] {
            if let Some(url) = url {
                egress_endpoint(url).with_context(|| format!("{key}={url:?}"))?;
            }
        }
        Ok(env)
    }

    /// An agent binding is the whole answer: the ambient variables are not consulted. `lookup`
    /// reads an environment variable by name.
    pub fn from_binding(
        binding: &InferenceBinding,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<InferenceEnv> {
        let key = match &binding.key_env {
            None => None,
            Some(name) => Some(
                lookup(name.as_str())
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .with_context(|| {
                        format!("the agent binding's credential variable {name:?} is unset")
                    })?,
            ),
        };
        if let Some(url) = &binding.url {
            egress_endpoint(url).with_context(|| format!("the agent binding's url {url:?}"))?;
        }
        let mut env = InferenceEnv {
            selection: Some(AgentSelection {
                protocol: binding.protocol,
                model: binding.model.clone(),
            }),
            ..InferenceEnv::default()
        };
        match binding.protocol {
            InferenceProtocol::Messages => {
                env.anthropic_key = key;
                env.anthropic_base_url = binding.url.clone();
            }
            InferenceProtocol::ChatCompletions | InferenceProtocol::Responses => {
                env.openai_key = key;
                env.openai_base_url = binding.url.clone();
                env.wire_api = Some(match binding.protocol {
                    InferenceProtocol::Responses => WireApi::Responses,
                    _ => WireApi::Chat,
                });
            }
            InferenceProtocol::SystemOne => {
                return Err(NotAnAgentProtocol {
                    protocol: binding.protocol,
                }
                .into());
            }
        }
        Ok(env)
    }

    /// Nothing named: no key, no base URL. The turn runs on the deploy profile's ambient
    /// credentials, which for every harness that can speak it means Vertex.
    pub fn is_ambient(&self) -> bool {
        self.anthropic_key.is_none()
            && self.anthropic_base_url.is_none()
            && self.openai_key.is_none()
            && self.openai_base_url.is_none()
    }

    /// The custom base URL the harness will talk to, if any. A harness that speaks both API
    /// families (opencode, pi) takes the OpenAI one first, the same precedence
    /// `crate::agent::harness::api_endpoint` resolves its provider by.
    pub fn base_url_for(&self, harness: Harness) -> Option<&str> {
        match harness {
            Harness::Claude | Harness::Hermes => self.anthropic_base_url.as_deref(),
            Harness::Codex => self.openai_base_url.as_deref(),
            Harness::OpenCode | Harness::Pi => self
                .openai_base_url
                .as_deref()
                .or(self.anthropic_base_url.as_deref()),
        }
    }

    /// The egress entry the sandbox needs for the harness's custom base URL, if there is one.
    pub fn egress_endpoint_for(&self, harness: Harness) -> Result<Option<String>> {
        self.base_url_for(harness).map(egress_endpoint).transpose()
    }
}

/// The Vertex project and region a turn's manifest env names, with sane fallbacks: the gateway
/// provider is registered against them, and a harness that speaks Vertex through its own
/// provider table (opencode, pi) is seeded with them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VertexConfig {
    pub project: String,
    pub region: String,
}

impl VertexConfig {
    pub fn from_env(env: &[(String, String)]) -> VertexConfig {
        let get = |keys: &[&str]| {
            keys.iter()
                .find_map(|k| env.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.clone()))
        };
        VertexConfig {
            project: get(&["ANTHROPIC_VERTEX_PROJECT_ID", "GCP_PROJECT_ID"]).unwrap_or_default(),
            region: get(&["CLOUD_ML_REGION", "VERTEX_LOCATION"]).unwrap_or_else(|| "global".into()),
        }
    }

    /// The Vertex AI host for the region: `global` is the apex, `us`/`eu` are the multi-region
    /// `rep` hosts, anything else is region-prefixed. The same hosts
    /// `crate::openshell::policy::VERTEX_CREDENTIAL_HOSTS` binds the credential to.
    pub fn host(&self) -> String {
        match self.region.as_str() {
            "global" => "aiplatform.googleapis.com".to_string(),
            "us" | "eu" => format!("aiplatform.{}.rep.googleapis.com", self.region),
            region => format!("{region}-aiplatform.googleapis.com"),
        }
    }
}

/// A base URL as the sandbox policy's `host:port:full` entry.
pub fn egress_endpoint(base_url: &str) -> Result<String> {
    crate::manifest::broker_endpoint_from_url(base_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(
        protocol: InferenceProtocol,
        url: Option<&str>,
        key_env: Option<&str>,
    ) -> InferenceBinding {
        InferenceBinding {
            role: InferenceRole::Agent,
            protocol,
            url: url.map(str::to_owned),
            model: "the-model".into(),
            key_env: key_env.map(|n| crucible_contract::inference::EnvName::new(n).unwrap()),
        }
    }

    fn keys(name: &str) -> Option<String> {
        (name == "AGENT_KEY").then(|| " sk-1 \n".to_owned())
    }

    #[test]
    fn a_messages_binding_carries_its_url_key_and_selection() {
        let env = InferenceEnv::from_binding(
            &binding(
                InferenceProtocol::Messages,
                Some("https://claude.corp/v1"),
                Some("AGENT_KEY"),
            ),
            keys,
        )
        .unwrap();
        assert_eq!(
            env,
            InferenceEnv {
                anthropic_key: Some("sk-1".into()),
                anthropic_base_url: Some("https://claude.corp/v1".into()),
                selection: Some(AgentSelection {
                    protocol: InferenceProtocol::Messages,
                    model: "the-model".into(),
                }),
                ..InferenceEnv::default()
            }
        );
    }

    #[test]
    fn a_stock_messages_binding_with_no_key_leaves_auth_ambient() {
        let env =
            InferenceEnv::from_binding(&binding(InferenceProtocol::Messages, None, None), |_| {
                panic!("no key to look up")
            })
            .unwrap();
        assert_eq!(env.anthropic_key, None);
        assert_eq!(env.anthropic_base_url, None);
        assert_eq!(
            crate::agent::harness::resolve_auth(Harness::Claude, &env),
            crate::agent::harness::AuthProvider::Vertex
        );
        assert_eq!(env.selection.unwrap().model, "the-model");
    }

    #[test]
    fn a_keyed_messages_binding_selects_direct_key_auth() {
        let env = InferenceEnv::from_binding(
            &binding(InferenceProtocol::Messages, None, Some("AGENT_KEY")),
            keys,
        )
        .unwrap();
        assert_eq!(
            crate::agent::harness::resolve_auth(Harness::Claude, &env),
            crate::agent::harness::AuthProvider::AnthropicKey
        );
    }

    #[test]
    fn an_openai_binding_carries_its_wire_api_and_a_codex_key() {
        for (protocol, wire) in [
            (InferenceProtocol::ChatCompletions, WireApi::Chat),
            (InferenceProtocol::Responses, WireApi::Responses),
        ] {
            let env = InferenceEnv::from_binding(
                &binding(
                    protocol,
                    Some("http://vllm.internal:8000/v1"),
                    Some("AGENT_KEY"),
                ),
                keys,
            )
            .unwrap();
            assert_eq!(
                env.openai_base_url.as_deref(),
                Some("http://vllm.internal:8000/v1")
            );
            assert_eq!(env.wire_api, Some(wire));
            assert_eq!(env.openai_key.as_deref(), Some("sk-1"));
            assert_eq!(
                env.anthropic_key, None,
                "an OpenAI key is not an Anthropic key"
            );
            assert_eq!(
                env.egress_endpoint_for(Harness::Codex).unwrap().as_deref(),
                Some("vllm.internal:8000:full")
            );
        }
    }

    #[test]
    fn a_named_key_that_is_unset_or_blank_fails_by_name() {
        for value in [None, Some("  ".to_owned())] {
            let err = InferenceEnv::from_binding(
                &binding(InferenceProtocol::Messages, None, Some("AGENT_KEY")),
                |_| value.clone(),
            )
            .unwrap_err();
            assert!(
                format!("{err:#}").contains("\"AGENT_KEY\" is unset"),
                "{err:#}"
            );
        }
    }

    #[test]
    fn an_injected_binding_is_read_and_the_ambient_variables_are_not() {
        use crucible_contract::inference::ENV_INFERENCE;
        let _guard = crucible::test_support::env_lock();
        let set = |key: &str, value: &str| unsafe { std::env::set_var(key, value) };
        let clear = |key: &str| unsafe { std::env::remove_var(key) };
        set(ANTHROPIC_API_KEY, "sk-ambient");
        set(ANTHROPIC_BASE_URL, "https://ambient.example/v1");
        set("CRUCIBLE_TEST_AGENT_KEY", "sk-bound");

        clear(ENV_INFERENCE);
        let ambient = InferenceEnv::from_process_env().unwrap();
        assert_eq!(ambient.anthropic_key.as_deref(), Some("sk-ambient"));
        assert_eq!(ambient.selection, None);

        set(
            ENV_INFERENCE,
            r#"{"version":1,"bindings":[{"role":"agent","protocol":"messages","url":"https://bound.example/v1","model":"bound-model","key_env":"CRUCIBLE_TEST_AGENT_KEY"}]}"#,
        );
        let bound = InferenceEnv::from_process_env().unwrap();
        assert_eq!(bound.anthropic_key.as_deref(), Some("sk-bound"));
        assert_eq!(
            bound.anthropic_base_url.as_deref(),
            Some("https://bound.example/v1")
        );
        assert_eq!(bound.selection.unwrap().model, "bound-model");

        set(
            ENV_INFERENCE,
            r#"{"version":1,"bindings":[{"role":"agent","protocol":"messages","model":"bound-model"}]}"#,
        );
        let keyless = InferenceEnv::from_process_env().unwrap();
        assert_eq!(
            keyless.anthropic_key, None,
            "an ambient key must not leak into a binding"
        );
        assert_eq!(keyless.anthropic_base_url, None);

        set(
            ENV_INFERENCE,
            r#"{"version":1,"bindings":[{"role":"decision","protocol":"system_one","url":"http://h/v1/systemone","model":"m"}]}"#,
        );
        let decision_only = InferenceEnv::from_process_env().unwrap();
        assert_eq!(
            decision_only, ambient,
            "a decision binding says nothing about the agent"
        );

        for key in [
            ENV_INFERENCE,
            ANTHROPIC_API_KEY,
            ANTHROPIC_BASE_URL,
            "CRUCIBLE_TEST_AGENT_KEY",
        ] {
            clear(key);
        }
    }

    #[test]
    fn a_system_one_binding_cannot_serve_an_agent() {
        let err = InferenceEnv::from_binding(
            &binding(
                InferenceProtocol::SystemOne,
                Some("http://h/v1/systemone"),
                None,
            ),
            |_| None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("system_one"), "{err}");
    }

    #[test]
    fn a_harness_is_kept_when_it_speaks_the_protocol_and_replaced_when_it_does_not() {
        let select = |protocol| AgentSelection {
            protocol,
            model: "m".into(),
        };
        let messages = select(InferenceProtocol::Messages);
        assert_eq!(messages.harness(Harness::Claude), Harness::Claude);
        assert_eq!(messages.harness(Harness::Hermes), Harness::Hermes);
        assert_eq!(messages.harness(Harness::Codex), Harness::Claude);
        for protocol in [
            InferenceProtocol::ChatCompletions,
            InferenceProtocol::Responses,
        ] {
            assert_eq!(select(protocol).harness(Harness::Codex), Harness::Codex);
            assert_eq!(select(protocol).harness(Harness::Claude), Harness::Codex);
            assert_eq!(select(protocol).harness(Harness::Hermes), Harness::Codex);
        }
        for both in [Harness::OpenCode, Harness::Pi] {
            assert_eq!(messages.harness(both), both);
            assert_eq!(select(InferenceProtocol::Responses).harness(both), both);
        }
        for harness in [
            Harness::Claude,
            Harness::Hermes,
            Harness::Codex,
            Harness::OpenCode,
            Harness::Pi,
        ] {
            assert!(!speaks(harness, InferenceProtocol::SystemOne));
        }
    }

    #[test]
    fn vertex_config_reads_manifest_keys_with_fallback() {
        let vertex = VertexConfig::from_env(&[
            ("ANTHROPIC_VERTEX_PROJECT_ID".into(), "proj-x".into()),
            ("CLOUD_ML_REGION".into(), "us-east5".into()),
        ]);
        assert_eq!(vertex.project, "proj-x");
        assert_eq!(vertex.region, "us-east5");
        assert_eq!(vertex.host(), "us-east5-aiplatform.googleapis.com");
        let unset = VertexConfig::from_env(&[]);
        assert_eq!(unset.region, "global");
        assert_eq!(unset.host(), "aiplatform.googleapis.com");
        let multi = VertexConfig::from_env(&[("VERTEX_LOCATION".into(), "eu".into())]);
        assert_eq!(multi.host(), "aiplatform.eu.rep.googleapis.com");
    }

    #[test]
    fn ambient_means_no_key_and_no_base_url() {
        assert!(InferenceEnv::default().is_ambient());
        for env in [
            InferenceEnv {
                anthropic_key: Some("k".into()),
                ..Default::default()
            },
            InferenceEnv {
                openai_base_url: Some("http://vllm:8000/v1".into()),
                ..Default::default()
            },
        ] {
            assert!(!env.is_ambient(), "{env:?}");
        }
    }

    #[test]
    fn a_base_url_becomes_an_egress_entry_on_its_own_port() {
        assert_eq!(
            egress_endpoint("http://vllm.internal:8000/v1").unwrap(),
            "vllm.internal:8000:full"
        );
        assert_eq!(
            egress_endpoint("https://proxy.corp/v1").unwrap(),
            "proxy.corp:443:full"
        );
        assert!(egress_endpoint("vllm.internal/v1").is_err());
    }

    #[test]
    fn the_wire_api_is_a_closed_pair() {
        assert_eq!(WireApi::parse(" chat ").unwrap(), WireApi::Chat);
        assert_eq!(WireApi::parse("responses").unwrap(), WireApi::Responses);
        assert!(WireApi::parse("completions").is_err());
    }

    #[test]
    fn each_harness_reads_its_own_base_url() {
        let env = InferenceEnv {
            anthropic_base_url: Some("https://claude.corp/v1".into()),
            openai_base_url: Some("http://vllm.internal:8000/v1".into()),
            ..InferenceEnv::default()
        };
        assert_eq!(
            env.base_url_for(Harness::Claude),
            Some("https://claude.corp/v1")
        );
        assert_eq!(
            env.base_url_for(Harness::Codex),
            Some("http://vllm.internal:8000/v1")
        );
        assert_eq!(
            env.egress_endpoint_for(Harness::Codex).unwrap().as_deref(),
            Some("vllm.internal:8000:full")
        );
        assert_eq!(
            InferenceEnv::default()
                .egress_endpoint_for(Harness::Claude)
                .unwrap(),
            None
        );
    }
}
