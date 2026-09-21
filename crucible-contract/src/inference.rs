//! The resolved inference bindings an orchestrator injects and the engine reads.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Env var carrying the run's [`ResolvedInference`] as JSON.
pub const ENV_INFERENCE: &str = "CRUCIBLE_INFERENCE";
pub const INFERENCE_WIRE_VERSION: u8 = 1;

const MAX_ENV_NAME_LEN: usize = 128;
const MAX_MODEL_LEN: usize = 128;
const MAX_URL_LEN: usize = 512;

/// What the inference is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceRole {
    /// The model an agent turn thinks with.
    Agent,
    /// The model a route task asks.
    Decision,
}

impl InferenceRole {
    pub fn as_str(self) -> &'static str {
        match self {
            InferenceRole::Agent => "agent",
            InferenceRole::Decision => "decision",
        }
    }
}

impl std::fmt::Display for InferenceRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The API spoken at a binding's URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceProtocol {
    Messages,
    ChatCompletions,
    Responses,
    SystemOne,
}

impl InferenceProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            InferenceProtocol::Messages => "messages",
            InferenceProtocol::ChatCompletions => "chat_completions",
            InferenceProtocol::Responses => "responses",
            InferenceProtocol::SystemOne => "system_one",
        }
    }

    pub fn serves(self, role: InferenceRole) -> bool {
        match role {
            InferenceRole::Agent => self != InferenceProtocol::SystemOne,
            InferenceRole::Decision => self == InferenceProtocol::SystemOne,
        }
    }
}

impl std::fmt::Display for InferenceProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The name of an environment variable, never its value.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EnvName(String);

impl EnvName {
    pub fn new(name: impl Into<String>) -> Result<Self, InferenceError> {
        let name = name.into();
        let mut chars = name.chars();
        let ok = name.len() <= MAX_ENV_NAME_LEN
            && chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
            && name != ENV_INFERENCE;
        if ok {
            Ok(EnvName(name))
        } else {
            Err(InferenceError::KeyEnvName { name })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for EnvName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

impl TryFrom<String> for EnvName {
    type Error = InferenceError;
    fn try_from(name: String) -> Result<Self, InferenceError> {
        Self::new(name)
    }
}

impl From<EnvName> for String {
    fn from(name: EnvName) -> String {
        name.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceBinding {
    pub role: InferenceRole,
    pub protocol: InferenceProtocol,
    /// Absent means the protocol's own service address. A decision binding always names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_env: Option<EnvName>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedInference {
    pub version: u8,
    #[serde(default)]
    pub bindings: Vec<InferenceBinding>,
}

impl Default for ResolvedInference {
    fn default() -> Self {
        ResolvedInference {
            version: INFERENCE_WIRE_VERSION,
            bindings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InferenceError {
    Decode {
        detail: String,
    },
    Version {
        got: u8,
    },
    DuplicateRole {
        role: InferenceRole,
    },
    ProtocolForRole {
        role: InferenceRole,
        protocol: InferenceProtocol,
    },
    MissingUrl {
        role: InferenceRole,
    },
    Url {
        role: InferenceRole,
        url: String,
        why: &'static str,
    },
    Model {
        role: InferenceRole,
        model: String,
    },
    KeyEnvName {
        name: String,
    },
}

impl std::fmt::Display for InferenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InferenceError::Decode { detail } => {
                write!(f, "{ENV_INFERENCE} does not decode: {detail}")
            }
            InferenceError::Version { got } => write!(
                f,
                "{ENV_INFERENCE} is version {got}; this engine reads version {INFERENCE_WIRE_VERSION}"
            ),
            InferenceError::DuplicateRole { role } => {
                write!(f, "{ENV_INFERENCE} holds more than one {role} binding")
            }
            InferenceError::ProtocolForRole { role, protocol } => {
                write!(
                    f,
                    "the {role} binding speaks {protocol}, which does not serve that role"
                )
            }
            InferenceError::MissingUrl { role } => {
                write!(
                    f,
                    "the {role} binding names no url, and that role has no default address"
                )
            }
            InferenceError::Url { role, url, why } => {
                write!(f, "the {role} binding's url {url:?} {why}")
            }
            InferenceError::Model { role, model } => write!(
                f,
                "the {role} binding's model {model:?} must be 1-{MAX_MODEL_LEN} characters with no whitespace"
            ),
            InferenceError::KeyEnvName { name } => write!(
                f,
                "key_env {name:?} is not an environment variable name a credential may use"
            ),
        }
    }
}

impl std::error::Error for InferenceError {}

fn check_url(role: InferenceRole, url: &str) -> Result<(), InferenceError> {
    let bad = |why| InferenceError::Url {
        role,
        url: url.to_owned(),
        why,
    };
    if url.len() > MAX_URL_LEN {
        return Err(bad("is too long"));
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(bad("contains whitespace or a control character"));
    }
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| bad("is not an absolute http or https URL"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Err(bad("carries credentials"));
    }
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host);
    if host.is_empty() {
        return Err(bad("names no host"));
    }
    Ok(())
}

impl ResolvedInference {
    /// Decode and validate the document. An empty or whitespace-only string is no bindings.
    pub fn parse(json: &str) -> Result<Self, InferenceError> {
        if json.trim().is_empty() {
            return Ok(Self::default());
        }
        let doc: ResolvedInference =
            serde_json::from_str(json).map_err(|e| InferenceError::Decode {
                detail: e.to_string(),
            })?;
        doc.validate()?;
        Ok(doc)
    }

    pub fn validate(&self) -> Result<(), InferenceError> {
        if self.version != INFERENCE_WIRE_VERSION {
            return Err(InferenceError::Version { got: self.version });
        }
        let mut roles = BTreeSet::new();
        for binding in &self.bindings {
            let role = binding.role;
            if !roles.insert(role) {
                return Err(InferenceError::DuplicateRole { role });
            }
            if !binding.protocol.serves(role) {
                return Err(InferenceError::ProtocolForRole {
                    role,
                    protocol: binding.protocol,
                });
            }
            match &binding.url {
                Some(url) => check_url(role, url)?,
                None if role == InferenceRole::Decision => {
                    return Err(InferenceError::MissingUrl { role });
                }
                None => {}
            }
            let model = &binding.model;
            if model.is_empty()
                || model.len() > MAX_MODEL_LEN
                || model.chars().any(|c| c.is_whitespace() || c.is_control())
            {
                return Err(InferenceError::Model {
                    role,
                    model: model.clone(),
                });
            }
        }
        Ok(())
    }

    pub fn binding(&self, role: InferenceRole) -> Option<&InferenceBinding> {
        self.bindings.iter().find(|b| b.role == role)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision() -> InferenceBinding {
        InferenceBinding {
            role: InferenceRole::Decision,
            protocol: InferenceProtocol::SystemOne,
            url: Some("http://dgemma.weaton-dev:8011/v1/systemone".into()),
            model: "dgemma".into(),
            key_env: None,
        }
    }

    fn agent() -> InferenceBinding {
        InferenceBinding {
            role: InferenceRole::Agent,
            protocol: InferenceProtocol::Messages,
            url: Some("https://claude.corp/v1".into()),
            model: "claude-opus-5".into(),
            key_env: Some(EnvName::new("INFERENCE_KEY_AGENT").unwrap()),
        }
    }

    fn doc(bindings: Vec<InferenceBinding>) -> ResolvedInference {
        ResolvedInference {
            version: INFERENCE_WIRE_VERSION,
            bindings,
        }
    }

    #[test]
    fn a_document_round_trips_and_never_serializes_an_absent_key() {
        let original = doc(vec![agent(), decision()]);
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(ResolvedInference::parse(&json).unwrap(), original);
        assert_eq!(json.matches("key_env").count(), 1);
        assert!(json.contains(r#""protocol":"system_one""#), "{json}");
        assert!(json.contains(r#""role":"decision""#), "{json}");
    }

    #[test]
    fn the_wire_shape_is_pinned() {
        let json = r#"{"version":1,"bindings":[{"role":"decision","protocol":"system_one","url":"http://dgemma.weaton-dev:8011/v1/systemone","model":"dgemma"}]}"#;
        assert_eq!(
            ResolvedInference::parse(json).unwrap(),
            doc(vec![decision()])
        );
        assert_eq!(serde_json::to_string(&doc(vec![decision()])).unwrap(), json);
    }

    #[test]
    fn an_absent_document_is_no_bindings() {
        for empty in ["", "  \n"] {
            let parsed = ResolvedInference::parse(empty).unwrap();
            assert!(parsed.bindings.is_empty());
            assert_eq!(parsed.binding(InferenceRole::Decision), None);
        }
    }

    #[test]
    fn binding_finds_by_role() {
        let d = doc(vec![agent(), decision()]);
        assert_eq!(d.binding(InferenceRole::Agent), Some(&agent()));
        assert_eq!(d.binding(InferenceRole::Decision), Some(&decision()));
        assert_eq!(doc(vec![agent()]).binding(InferenceRole::Decision), None);
    }

    #[test]
    fn an_unknown_version_names_both_versions() {
        let err = ResolvedInference::parse(r#"{"version":2,"bindings":[]}"#).unwrap_err();
        assert_eq!(err, InferenceError::Version { got: 2 });
        assert!(err.to_string().contains("version 2"), "{err}");
        assert!(err.to_string().contains("reads version 1"), "{err}");
    }

    #[test]
    fn an_unknown_field_is_rejected_by_name_at_both_levels() {
        for json in [
            r#"{"version":1,"bindings":[],"extra":true}"#,
            r#"{"version":1,"bindings":[{"role":"decision","protocol":"system_one","url":"http://h/x","model":"m","api_key":"sk-1"}]}"#,
        ] {
            let err = ResolvedInference::parse(json).unwrap_err().to_string();
            assert!(err.contains("unknown field"), "{err}");
        }
        let err = ResolvedInference::parse(
            r#"{"version":1,"bindings":[{"role":"decision","protocol":"system_one","url":"http://h/x","model":"m","api_key":"sk-1"}]}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("api_key"), "{err}");
    }

    #[test]
    fn an_unknown_role_or_protocol_does_not_decode() {
        for json in [
            r#"{"version":1,"bindings":[{"role":"judge","protocol":"system_one","url":"http://h/x","model":"m"}]}"#,
            r#"{"version":1,"bindings":[{"role":"decision","protocol":"grpc","url":"http://h/x","model":"m"}]}"#,
        ] {
            assert!(matches!(
                ResolvedInference::parse(json).unwrap_err(),
                InferenceError::Decode { .. }
            ));
        }
    }

    #[test]
    fn one_binding_per_role() {
        assert_eq!(
            doc(vec![decision(), decision()]).validate(),
            Err(InferenceError::DuplicateRole {
                role: InferenceRole::Decision
            })
        );
    }

    #[test]
    fn a_protocol_must_serve_its_role() {
        for protocol in [
            InferenceProtocol::Messages,
            InferenceProtocol::ChatCompletions,
            InferenceProtocol::Responses,
        ] {
            assert!(protocol.serves(InferenceRole::Agent));
            assert!(!protocol.serves(InferenceRole::Decision));
            let mut b = decision();
            b.protocol = protocol;
            assert_eq!(
                doc(vec![b]).validate(),
                Err(InferenceError::ProtocolForRole {
                    role: InferenceRole::Decision,
                    protocol
                })
            );
        }
        let mut b = agent();
        b.protocol = InferenceProtocol::SystemOne;
        assert!(matches!(
            doc(vec![b]).validate(),
            Err(InferenceError::ProtocolForRole {
                role: InferenceRole::Agent,
                ..
            })
        ));
    }

    #[test]
    fn a_url_is_absolute_http_with_a_host_and_no_credentials() {
        let cases = [
            ("ftp://host/x", "not an absolute http"),
            ("host:8011/v1", "not an absolute http"),
            ("http://:8011/v1", "names no host"),
            ("https:///v1", "names no host"),
            ("https://user:pw@host/v1", "carries credentials"),
            ("http://ho st/v1", "whitespace"),
        ];
        for (url, needle) in cases {
            let mut b = decision();
            b.url = Some(url.into());
            let err = doc(vec![b]).validate().unwrap_err().to_string();
            assert!(err.contains(needle), "{url}: {err}");
            assert!(err.contains("decision binding"), "{err}");
        }
        let mut long = decision();
        long.url = Some(format!("http://h/{}", "x".repeat(MAX_URL_LEN)));
        assert!(doc(vec![long]).validate().is_err());
        for ok in [
            "http://h",
            "https://h:443",
            "http://10.0.0.1:8011/v1/systemone?a=b",
        ] {
            let mut b = decision();
            b.url = Some(ok.into());
            doc(vec![b]).validate().unwrap();
        }
    }

    #[test]
    fn an_agent_binding_may_omit_its_url_and_a_decision_binding_may_not() {
        let mut stock = agent();
        stock.url = None;
        stock.key_env = None;
        doc(vec![stock.clone()]).validate().unwrap();
        let json = serde_json::to_string(&doc(vec![stock])).unwrap();
        assert!(!json.contains("url"), "{json}");
        let mut b = decision();
        b.url = None;
        assert_eq!(
            doc(vec![b]).validate(),
            Err(InferenceError::MissingUrl {
                role: InferenceRole::Decision
            })
        );
    }

    #[test]
    fn a_model_is_a_short_token() {
        for bad in ["", "two words", &"m".repeat(MAX_MODEL_LEN + 1)] {
            let mut b = decision();
            b.model = bad.to_string();
            assert!(matches!(
                doc(vec![b]).validate(),
                Err(InferenceError::Model { .. })
            ));
        }
    }

    #[test]
    fn a_key_env_is_a_variable_name_and_never_the_document_itself() {
        for ok in ["OPENAI_API_KEY", "_k", "K9"] {
            assert_eq!(EnvName::new(ok).unwrap().as_str(), ok);
        }
        for bad in ["", "9K", "MY-KEY", "A B", "sk=1", ENV_INFERENCE] {
            assert_eq!(
                EnvName::new(bad),
                Err(InferenceError::KeyEnvName { name: bad.into() })
            );
        }
        assert!(EnvName::new("K".repeat(MAX_ENV_NAME_LEN + 1)).is_err());
        let json = r#"{"version":1,"bindings":[{"role":"decision","protocol":"system_one","url":"http://h/x","model":"m","key_env":"not a name"}]}"#;
        assert!(matches!(
            ResolvedInference::parse(json).unwrap_err(),
            InferenceError::Decode { .. }
        ));
    }
}
