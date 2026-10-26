//! The MCP servers a run's sandboxes reach with agent tokens, as the controller hands them to the
//! loop pod in [`ENV_AGENT_SERVERS`]. The engine gives each a token-grant provider, an egress entry
//! and a `.mcp.json` entry alike; one served by a process in the loop pod is started there first.
//! An empty or absent list grants nothing.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Loop-pod env carrying the run's servers, a JSON list of [`AgentServer`].
pub const ENV_AGENT_SERVERS: &str = "CRUCIBLE_AGENT_SERVERS";

/// A server's name: the `.mcp.json` key the agent sees its tools under, and the key its grant is
/// recorded by. A DNS-1123 label.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ServerName(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "{0:?} is not a server name: a DNS-1123 label (lowercase alphanumerics and '-', 1-63 chars)"
)]
pub struct NotServerName(pub String);

impl ServerName {
    pub fn parse(raw: &str) -> Result<Self, NotServerName> {
        let ok = (1..=63).contains(&raw.len())
            && raw
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !raw.starts_with('-')
            && !raw.ends_with('-');
        match ok {
            true => Ok(Self(raw.to_string())),
            false => Err(NotServerName(raw.to_string())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ServerName {
    type Error = NotServerName;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

impl From<ServerName> for String {
    fn from(name: ServerName) -> Self {
        name.0
    }
}

impl fmt::Display for ServerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One server a run's sandboxes are granted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentServer {
    pub name: ServerName,
    /// The MCP URL sandboxes reach it at, the audience of its tokens.
    pub url: String,
    /// The scopes its tokens are requested with.
    pub scopes: Vec<String>,
    #[serde(default)]
    pub transport: Transport,
}

/// Where a server runs.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    /// Somewhere the sandbox reaches over the network.
    #[default]
    Remote,
    /// A process the engine starts in the loop pod, listening on the URL's port.
    Local(LocalServer),
}

/// A server the engine runs in the loop pod.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalServer {
    /// `buildit mcp`.
    Buildit(BuilditSite),
}

/// What `buildit mcp` runs with for a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuilditSite {
    /// The issuer it trusts for run tokens, `<issuer>=<jwks uri>`.
    pub run_issuer: String,
    /// The namespace its builder pods run in, apart from the loop pods.
    pub namespace: String,
    /// The one repository it pushes to; none, it pushes nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_repo: Option<String>,
}

/// `servers` as [`ENV_AGENT_SERVERS`] carries them.
pub fn encode(servers: &[AgentServer]) -> Result<String, serde_json::Error> {
    serde_json::to_string(servers)
}

/// The servers [`ENV_AGENT_SERVERS`] carries; an empty value is none.
pub fn decode(raw: &str) -> Result<Vec<AgentServer>, serde_json::Error> {
    match raw.trim() {
        "" => Ok(Vec::new()),
        raw => serde_json::from_str(raw),
    }
}

#[cfg(test)]
mod tests {
    use crate::agent_servers::*;

    #[test]
    fn server_names_are_dns_labels() {
        for good in ["kueue", "buildit", "a-1"] {
            assert!(ServerName::parse(good).is_ok(), "{good}");
        }
        for bad in ["", "Kueue", "-a", "a-", "a_b", "a.b", &"a".repeat(64)] {
            assert!(ServerName::parse(bad).is_err(), "{bad}");
        }
        assert!(serde_json::from_str::<ServerName>("\"Kueue\"").is_err());
    }

    #[test]
    fn servers_round_trip_and_an_empty_value_is_none() {
        let servers = vec![
            AgentServer {
                name: ServerName::parse("kueue").unwrap(),
                url: "http://kueue.example:8850/mcp".into(),
                scopes: vec!["kueue.read".into()],
                transport: Transport::Remote,
            },
            AgentServer {
                name: ServerName::parse("buildit").unwrap(),
                url: "http://host.openshell.internal:8848/mcp".into(),
                scopes: vec!["buildit.read".into(), "buildit.build".into()],
                transport: Transport::Local(LocalServer::Buildit(BuilditSite {
                    run_issuer: "https://hub.example=https://hub.example/jwks".into(),
                    namespace: "builds".into(),
                    push_repo: None,
                })),
            },
        ];
        let raw = encode(&servers).unwrap();
        assert_eq!(decode(&raw).unwrap(), servers);
        assert!(decode("").unwrap().is_empty());
        assert!(decode("[]").unwrap().is_empty());
        assert_eq!(
            decode(r#"[{"name": "x", "url": "http://x/mcp", "scopes": []}]"#).unwrap()[0].transport,
            Transport::Remote,
            "remote is the default"
        );
        assert!(decode(r#"[{"name": "x", "url": "u", "scopes": [], "extra": 1}]"#).is_err());
    }
}
