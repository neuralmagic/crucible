//! A cluster SPIRE as the issuer trusts it: its JWT-SVID issuer, the SPIFFE ID path its sandbox
//! supervisors sit under, and the Service its keys are read through; and the directory of every
//! agent cluster's SPIRE that discovery keeps.

use crucible::deploy::profile::{KubeName, check_cluster_name};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

#[derive(Debug, thiserror::Error)]
pub enum SpireError {
    #[error(
        "gateway prefix {0:?} must be a spiffe:// URI ending in /, with at most one {{tenant}} path segment"
    )]
    Prefix(String),
    #[error(
        "CONTROLLER_AGENT_SPIRE entry {0:?} is not \
         <cluster>=<svid issuer> <gateway prefix> [http://]<namespace>/<service>:<port>"
    )]
    Entry(String),
}

/// The SPIFFE ID path under which every sandbox supervisor's ID sits, ending in `/`. The rest of
/// the ID is `<gateway id>/sandbox/<sandbox id>`, both read from labels the gateway that created
/// the supervisor pod stamped. One path segment may be `{tenant}`, matching a DNS-1123 label:
///
/// ```text
///   spiffe://td/openshell/gateway/            spiffe://td/openshell/gateway/<gw>/sandbox/<id>
///   spiffe://td/openshell/{tenant}/gateway/   spiffe://td/openshell/<tenant>/gateway/<gw>/sandbox/<id>
///   └──────── head ──────┘└ tail ─┘
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayPrefix {
    raw: String,
    /// Everything before `{tenant}`, or the whole prefix without one.
    head: String,
    /// Everything after `{tenant}/`, when the prefix has one.
    tail: Option<String>,
}

const TENANT_SEGMENT: &str = "/{tenant}/";

impl GatewayPrefix {
    pub fn parse(raw: &str) -> Result<Self, SpireError> {
        let bad = || SpireError::Prefix(raw.to_string());
        let path = raw.strip_prefix("spiffe://").ok_or_else(bad)?;
        if path.is_empty() || !raw.ends_with('/') {
            return Err(bad());
        }
        let (head, tail) = match raw.split_once(TENANT_SEGMENT) {
            Some((head, tail)) => (format!("{head}/"), Some(tail.to_string())),
            None => (raw.to_string(), None),
        };
        let trust_domain = head
            .strip_prefix("spiffe://")
            .and_then(|p| p.split('/').next());
        let braces = |v: &str| v.contains(['{', '}']);
        if trust_domain.is_none_or(str::is_empty)
            || braces(&head)
            || tail.as_deref().is_some_and(braces)
        {
            return Err(bad());
        }
        Ok(Self {
            raw: raw.to_string(),
            head,
            tail,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The tenant (when the prefix has a `{tenant}` segment) and the rest of `spiffe_id` after the
    /// prefix; `None` when the ID is not under it.
    pub fn split<'a>(&self, spiffe_id: &'a str) -> Option<(Option<&'a str>, &'a str)> {
        let rest = spiffe_id.strip_prefix(self.head.as_str())?;
        let Some(tail) = &self.tail else {
            return Some((None, rest));
        };
        let (tenant, rest) = rest.split_once('/')?;
        KubeName::try_from(tenant.to_string()).ok()?;
        Some((Some(tenant), rest.strip_prefix(tail.as_str())?))
    }
}

/// A cluster's SPIRE OIDC discovery provider Service. Its key set is read through that cluster's
/// API server Service proxy with the controller's own client for the cluster, so it needs no
/// route of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryService {
    /// Whether the Service serves TLS: `http://` before the namespace says it does not.
    pub tls: bool,
    pub namespace: KubeName,
    pub name: KubeName,
    pub port: u16,
}

impl DiscoveryService {
    /// The API server path of the provider's JWKS.
    pub fn keys_path(&self) -> String {
        let scheme = match self.tls {
            true => "https",
            false => "http",
        };
        format!(
            "/api/v1/namespaces/{}/services/{scheme}:{}:{}/proxy/keys",
            self.namespace, self.name, self.port
        )
    }
}

/// One cluster's SPIRE, trusted for the SVIDs of sandboxes whose runs it hosts:
/// `<cluster>=<svid issuer> <gateway prefix> [http://]<namespace>/<service>:<port>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterSpire {
    pub cluster: String,
    /// The `iss` of its JWT-SVIDs, compared as an exact string.
    pub svid_issuer: String,
    /// `spiffe://<its trust domain>/openshell/gateway/`.
    pub gateway_prefix: GatewayPrefix,
    pub discovery: DiscoveryService,
}

impl std::str::FromStr for ClusterSpire {
    type Err = SpireError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let bad = || SpireError::Entry(raw.to_string());
        let (cluster, rest) = raw.split_once('=').ok_or_else(bad)?;
        let [issuer, prefix, service] = rest.split_whitespace().collect::<Vec<_>>()[..] else {
            return Err(bad());
        };
        let (tls, service) = match service.strip_prefix("http://") {
            Some(rest) => (false, rest),
            None => (true, service),
        };
        let (namespace, name_port) = service.split_once('/').ok_or_else(bad)?;
        let (name, port) = name_port.split_once(':').ok_or_else(bad)?;
        let kube_name = |v: &str| KubeName::try_from(v.to_string()).map_err(|_| bad());
        let cluster = cluster.trim();
        check_cluster_name(cluster).map_err(|_| bad())?;
        if issuer.is_empty() {
            return Err(bad());
        }
        Ok(Self {
            cluster: cluster.to_string(),
            svid_issuer: issuer.to_string(),
            gateway_prefix: GatewayPrefix::parse(prefix)?,
            discovery: DiscoveryService {
                tls,
                namespace: kube_name(namespace)?,
                name: kube_name(name)?,
                port: port.parse().map_err(|_| bad())?,
            },
        })
    }
}

/// What the controller knows of one cluster's SPIRE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpireState {
    /// Not read yet.
    Pending,
    Ready(ClusterSpire),
    /// Read and found wanting, or not readable; agents are off on the cluster until a retry.
    Unavailable(String),
}

/// The SPIRE of every agent cluster, as discovery last found it, shared by the issuer, dispatch
/// and the API. Clones share one map.
#[derive(Debug, Clone, Default)]
pub struct SpireDirectory(Arc<RwLock<Directory>>);

impl PartialEq for SpireDirectory {
    fn eq(&self, other: &Self) -> bool {
        if Arc::ptr_eq(&self.0, &other.0) {
            return true;
        }
        let (a, b) = (self.read(), other.read());
        a.enabled == b.enabled && a.clusters == b.clusters
    }
}

impl Eq for SpireDirectory {}

#[derive(Debug, Default)]
struct Directory {
    /// Whether agent tokens are configured at all: an issuer, an isolated profile and a server.
    enabled: bool,
    clusters: BTreeMap<String, SpireState>,
}

impl SpireDirectory {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Directory> {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Directory> {
        self.0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Agent tokens are configured for `clusters`, each pending until discovery reads it.
    pub fn enable(&self, clusters: impl IntoIterator<Item = String>) {
        let mut d = self.write();
        d.enabled = true;
        for cluster in clusters {
            d.clusters.entry(cluster).or_insert(SpireState::Pending);
        }
    }

    /// Whether agent tokens are configured, read or not.
    pub fn enabled(&self) -> bool {
        self.read().enabled
    }

    pub fn set(&self, cluster: &str, state: SpireState) {
        self.write().clusters.insert(cluster.to_string(), state);
    }

    /// Every cluster's state, by cluster.
    pub fn states(&self) -> BTreeMap<String, SpireState> {
        self.read().clusters.clone()
    }

    /// The SPIRE of each cluster discovery has read.
    pub fn ready(&self) -> Vec<ClusterSpire> {
        self.read()
            .clusters
            .values()
            .filter_map(|s| match s {
                SpireState::Ready(spire) => Some(spire.clone()),
                _ => None,
            })
            .collect()
    }

    /// The clusters not read yet or found wanting.
    pub fn unread(&self) -> Vec<String> {
        self.read()
            .clusters
            .iter()
            .filter(|(_, s)| !matches!(s, SpireState::Ready(_)))
            .map(|(c, _)| c.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::agent_auth::cluster_spire::*;

    #[test]
    fn a_gateway_prefix_may_hold_one_tenant_segment() {
        let plain = GatewayPrefix::parse("spiffe://td/openshell/gateway/").unwrap();
        assert_eq!(
            plain.split("spiffe://td/openshell/gateway/gw/sandbox/s"),
            Some((None, "gw/sandbox/s"))
        );
        assert_eq!(
            plain.split("spiffe://td/openshell/team-a/gateway/gw/sandbox/s"),
            None
        );
        let tenanted = GatewayPrefix::parse("spiffe://td/openshell/{tenant}/gateway/").unwrap();
        assert_eq!(
            tenanted.split("spiffe://td/openshell/team-a/gateway/gw/sandbox/s"),
            Some((Some("team-a"), "gw/sandbox/s"))
        );
        for outside in [
            "spiffe://td/openshell/gateway/gw/sandbox/s",
            "spiffe://td/openshell/Team_A/gateway/gw/sandbox/s",
            "spiffe://td/openshell/team-a/other/gateway/gw/sandbox/s",
            "spiffe://other/openshell/team-a/gateway/gw/sandbox/s",
        ] {
            assert_eq!(tenanted.split(outside), None, "{outside}");
        }
        for bad in [
            "spiffe://td/openshell/gateway",
            "https://td/openshell/gateway/",
            "spiffe:///openshell/gateway/",
            "spiffe://td/openshell/{tenant}/{tenant}/gateway/",
            "spiffe://td/openshell/x{tenant}/gateway/",
            "spiffe://td/openshell/{team}/gateway/",
            "spiffe://{tenant}/gateway/",
        ] {
            assert!(GatewayPrefix::parse(bad).is_err(), "{bad}");
        }
    }
}
