//! The MCP servers a run's sandboxes reach with agent tokens, as the controller hands them to the
//! loop pod in [`ENV_AGENT_SERVERS`]. The engine gives each a token-grant provider, an egress entry
//! and a `.mcp.json` entry alike; one served by a process in the loop pod is started there first.
//! An empty or absent list grants nothing.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU32;

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
    /// The Kueue LocalQueue its builder pods are submitted to; none, they carry no queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,
    /// Where and how large its builder pods are.
    #[serde(default, skip_serializing_if = "BuilderPods::is_unset")]
    pub builders: BuilderPods,
}

/// buildit's builder pods: the node architecture they run on, how many a sandbox may have live,
/// how long each lives, and their resources. Each field unset is buildit's own default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuilderPods {
    /// The `kubernetes.io/arch` their nodes carry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_arch: Option<NodeArch>,
    /// Live builder pods per sandbox; `build` is refused past this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_builds: Option<NonZeroU32>,
    /// A builder pod's lifetime, its `activeDeadlineSeconds`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_deadline_secs: Option<NonZeroU32>,
    /// Resource requests, `{ cpu = "8" }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<BTreeMap<String, String>>,
    /// Resource limits, `{ cpu = "32", memory = "64Gi" }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<BTreeMap<String, String>>,
}

impl BuilderPods {
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }

    /// These settings, taking each one unset here from `base`.
    pub fn over(&self, base: &Self) -> Self {
        Self {
            node_arch: self.node_arch.or(base.node_arch),
            max_builds: self.max_builds.or(base.max_builds),
            pod_deadline_secs: self.pod_deadline_secs.or(base.pod_deadline_secs),
            requests: self.requests.clone().or_else(|| base.requests.clone()),
            limits: self.limits.clone().or_else(|| base.limits.clone()),
        }
    }
}

/// A node architecture, as the `kubernetes.io/arch` label names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeArch {
    Amd64,
    Arm64,
    Ppc64le,
    S390x,
}

impl NodeArch {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Amd64 => "amd64",
            Self::Arm64 => "arm64",
            Self::Ppc64le => "ppc64le",
            Self::S390x => "s390x",
        }
    }

    /// The architecture of the node this process runs on, `None` for one Kubernetes has no
    /// label value for.
    pub fn host() -> Option<Self> {
        Self::of(std::env::consts::ARCH, cfg!(target_endian = "little"))
    }

    fn of(rust_arch: &str, little_endian: bool) -> Option<Self> {
        match rust_arch {
            "x86_64" => Some(Self::Amd64),
            "aarch64" => Some(Self::Arm64),
            "powerpc64" if little_endian => Some(Self::Ppc64le),
            "s390x" => Some(Self::S390x),
            _ => None,
        }
    }
}

impl fmt::Display for NodeArch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
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
                    queue: Some("builders".into()),
                    builders: BuilderPods::default(),
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

    #[test]
    fn builder_pods_ride_the_env_only_when_set() {
        let site = |builders| BuilditSite {
            run_issuer: "https://hub.example=https://hub.example/jwks".into(),
            namespace: "builds".into(),
            push_repo: None,
            queue: None,
            builders,
        };
        let plain = serde_json::to_value(site(BuilderPods::default())).unwrap();
        assert!(plain.get("builders").is_none(), "{plain}");
        let pinned = site(BuilderPods {
            node_arch: Some(NodeArch::Amd64),
            max_builds: NonZeroU32::new(2),
            pod_deadline_secs: NonZeroU32::new(3600),
            requests: Some(BTreeMap::from([("cpu".into(), "8".into())])),
            limits: Some(BTreeMap::from([("cpu".into(), "32".into())])),
        });
        let raw = serde_json::to_value(&pinned).unwrap();
        assert_eq!(
            raw["builders"],
            serde_json::json!({
                "node_arch": "amd64",
                "max_builds": 2,
                "pod_deadline_secs": 3600,
                "requests": {"cpu": "8"},
                "limits": {"cpu": "32"},
            })
        );
        assert_eq!(serde_json::from_value::<BuilditSite>(raw).unwrap(), pinned);
        assert!(serde_json::from_str::<BuilderPods>(r#"{"node_arch": "x86_64"}"#).is_err());
        assert!(serde_json::from_str::<BuilderPods>(r#"{"max_builds": 0}"#).is_err());
        assert!(serde_json::from_str::<BuilderPods>(r#"{"cpu": "8"}"#).is_err());
    }

    #[test]
    fn a_cluster_s_builder_pods_override_the_defaults_field_by_field() {
        let base = BuilderPods {
            node_arch: Some(NodeArch::Arm64),
            max_builds: NonZeroU32::new(4),
            pod_deadline_secs: None,
            requests: Some(BTreeMap::from([("cpu".into(), "8".into())])),
            limits: Some(BTreeMap::from([("cpu".into(), "32".into())])),
        };
        let cluster = BuilderPods {
            node_arch: Some(NodeArch::Amd64),
            pod_deadline_secs: NonZeroU32::new(600),
            limits: Some(BTreeMap::new()),
            ..BuilderPods::default()
        };
        assert_eq!(
            cluster.over(&base),
            BuilderPods {
                node_arch: Some(NodeArch::Amd64),
                max_builds: NonZeroU32::new(4),
                pod_deadline_secs: NonZeroU32::new(600),
                requests: Some(BTreeMap::from([("cpu".into(), "8".into())])),
                limits: Some(BTreeMap::new()),
            }
        );
        assert_eq!(BuilderPods::default().over(&base), base);
        assert_eq!(base.over(&BuilderPods::default()), base);
    }

    #[test]
    fn node_arches_are_kubernetes_label_values() {
        assert_eq!(NodeArch::of("x86_64", true), Some(NodeArch::Amd64));
        assert_eq!(NodeArch::of("aarch64", true), Some(NodeArch::Arm64));
        assert_eq!(NodeArch::of("powerpc64", true), Some(NodeArch::Ppc64le));
        assert_eq!(NodeArch::of("powerpc64", false), None);
        assert_eq!(NodeArch::of("s390x", false), Some(NodeArch::S390x));
        assert_eq!(NodeArch::of("riscv64", true), None);
        assert!(NodeArch::host().is_some(), "{}", std::env::consts::ARCH);
        for arch in [
            NodeArch::Amd64,
            NodeArch::Arm64,
            NodeArch::Ppc64le,
            NodeArch::S390x,
        ] {
            assert_eq!(
                serde_json::to_value(arch).unwrap(),
                serde_json::json!(arch.as_str())
            );
            assert_eq!(arch.to_string(), arch.as_str());
        }
    }
}
