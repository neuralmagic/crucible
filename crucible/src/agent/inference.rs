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

#[derive(Debug, thiserror::Error)]
#[error("{WIRE_API}={raw:?} is neither chat nor responses")]
pub struct UnknownWireApi {
    raw: String,
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

/// What the environment says about reaching the model. Read once per turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InferenceEnv {
    pub anthropic_key: Option<String>,
    pub anthropic_base_url: Option<String>,
    /// The OpenAI-speaking key a key-authenticated harness (opencode, pi) relays into the sandbox;
    /// codex selects its own key by name through `[agent.codex]` instead.
    pub openai_key: Option<String>,
    pub openai_base_url: Option<String>,
    pub wire_api: Option<WireApi>,
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
        let env = InferenceEnv {
            anthropic_key: non_empty(ANTHROPIC_API_KEY),
            anthropic_base_url: non_empty(ANTHROPIC_BASE_URL),
            openai_key: non_empty(OPENAI_API_KEY_ENV),
            openai_base_url: non_empty(OPENAI_BASE_URL),
            wire_api: non_empty(WIRE_API)
                .map(|w| WireApi::parse(&w))
                .transpose()?,
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
