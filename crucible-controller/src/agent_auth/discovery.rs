//! Each agent cluster's SPIRE, found through the controller's own cluster client instead of
//! configured: its trust domain and JWT-SVID issuer, and the OIDC discovery Service its keys are
//! read from. On OpenShift they come from the Zero Trust Workload Identity Manager's resources
//! named `cluster`; elsewhere from the upstream chart's `spire-server` ConfigMap. A cluster that
//! cannot be read has agents off until a retry reads it; `CONTROLLER_AGENT_SPIRE` entries stand in
//! for discovery on clusters whose SPIRE is installed some other way.

use crate::agent_auth::cluster_spire::{
    ClusterSpire, DiscoveryService, GatewayPrefix, SpireDirectory, SpireState,
};
use crate::runs::clusters::ClusterClients;
use crucible::deploy::DeployProfile;
use crucible::deploy::profile::KubeName;
use crucible::deploy::spire;
use forge::fleet::Platform;
use std::sync::Arc;
use std::time::Duration;

/// How often clusters not yet read are tried again.
pub const RETRY: Duration = Duration::from_secs(60);
/// How often clusters already read are read again, for a SPIRE that was reinstalled.
pub const REFRESH: Duration = Duration::from_secs(600);

/// Where SPIRE runs by default on each platform: the namespace, the OIDC discovery Service, its
/// port and whether it serves TLS.
fn default_discovery(platform: Platform) -> Result<DiscoveryService, String> {
    let at = spire::discovery_location(platform);
    let name = |raw: &str| KubeName::try_from(raw.to_string()).map_err(|e| e.to_string());
    Ok(DiscoveryService {
        tls: at.tls,
        namespace: name(at.namespace)?,
        name: name(at.service)?,
        port: at.port,
    })
}

/// One agent cluster as the API shows it: whether runs there get agent tools, and why not.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct AgentCluster {
    pub cluster: String,
    /// `ready`, `pending` (not read yet) or `unavailable`.
    pub state: String,
    /// Where its SPIRE came from: `configured` (`CONTROLLER_AGENT_SPIRE`) or `discovered`.
    pub source: String,
    /// The JWT-SVID issuer its sandboxes' assertions carry, when ready.
    pub svid_issuer: Option<String>,
    /// Why agents are off there, when unavailable.
    pub reason: Option<String>,
}

/// Every agent cluster: those `stated` in `CONTROLLER_AGENT_SPIRE` and those `directory`
/// discovers, by cluster.
pub fn agent_clusters(stated: &[String], directory: &SpireDirectory) -> Vec<AgentCluster> {
    let mut out: Vec<AgentCluster> = stated
        .iter()
        .map(|cluster| AgentCluster {
            cluster: cluster.clone(),
            state: "ready".into(),
            source: "configured".into(),
            svid_issuer: None,
            reason: None,
        })
        .collect();
    for (cluster, state) in directory.states() {
        let (state, svid_issuer, reason) = match state {
            SpireState::Pending => ("pending", None, None),
            SpireState::Ready(spire) => ("ready", Some(spire.svid_issuer), None),
            SpireState::Unavailable(why) => ("unavailable", None, Some(why)),
        };
        out.push(AgentCluster {
            cluster,
            state: state.into(),
            source: "discovered".into(),
            svid_issuer,
            reason,
        });
    }
    out.sort_by(|a, b| a.cluster.cmp(&b.cluster));
    out
}

/// `cluster`'s SPIRE, read through its client.
pub async fn discover(
    clusters: &ClusterClients,
    profile: &DeployProfile,
    cluster: &str,
) -> Result<ClusterSpire, String> {
    let platform = clusters
        .platform(profile, cluster)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let client = clusters
        .client(cluster)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let discovery = default_discovery(platform)?;
    let (trust_domain, svid_issuer) = match platform {
        Platform::Openshift => (
            operator_field(&client, spire::TRUST_DOMAIN_KIND.0, "trustDomain").await?,
            operator_field(&client, spire::JWT_ISSUER_KIND.0, "jwtIssuer").await?,
        ),
        Platform::Kubernetes => server_conf(&client, discovery.namespace.as_str()).await?,
    };
    from_parts(cluster, &trust_domain, &svid_issuer, discovery)
}

/// The SPIRE of `cluster` from what discovery read.
fn from_parts(
    cluster: &str,
    trust_domain: &str,
    svid_issuer: &str,
    discovery: DiscoveryService,
) -> Result<ClusterSpire, String> {
    if trust_domain.is_empty() || trust_domain.contains(['/', ':']) {
        return Err(format!("{trust_domain:?} is not a SPIFFE trust domain"));
    }
    if url::Url::parse(svid_issuer).is_err() {
        return Err(format!("JWT-SVID issuer {svid_issuer:?} is not a URL"));
    }
    let gateway_prefix =
        GatewayPrefix::parse(&format!("spiffe://{trust_domain}/openshell/gateway/"))
            .map_err(|e| e.to_string())?;
    Ok(ClusterSpire {
        cluster: cluster.to_string(),
        svid_issuer: svid_issuer.to_string(),
        gateway_prefix,
        discovery,
    })
}

/// `spec.<field>` of the Zero Trust Workload Identity Manager resource `kind` named `cluster`.
async fn operator_field(client: &kube::Client, kind: &str, field: &str) -> Result<String, String> {
    let gvk =
        kube::api::GroupVersionKind::gvk(spire::OPERATOR_GROUP, spire::OPERATOR_VERSION, kind);
    let resource = kube::api::ApiResource::from_gvk(&gvk);
    let api: kube::Api<kube::api::DynamicObject> = kube::Api::all_with(client.clone(), &resource);
    let object = api
        .get(spire::OPERATOR_OBJECT)
        .await
        .map_err(|e| format!("reading {kind} {}: {e}", spire::OPERATOR_OBJECT))?;
    object
        .data
        .pointer(&format!("/spec/{field}"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("{kind} cluster sets no spec.{field}"))
}

/// The trust domain and JWT issuer in the upstream chart's `spire-server` ConfigMap.
async fn server_conf(client: &kube::Client, namespace: &str) -> Result<(String, String), String> {
    let api: kube::Api<k8s_openapi::api::core::v1::ConfigMap> =
        kube::Api::namespaced(client.clone(), namespace);
    let map = api.get(spire::SERVER_CONFIGMAP).await.map_err(|e| {
        format!(
            "reading ConfigMap {namespace}/{}: {e}",
            spire::SERVER_CONFIGMAP
        )
    })?;
    let raw = map
        .data
        .as_ref()
        .and_then(|d| d.get("server.conf"))
        .ok_or_else(|| format!("ConfigMap {namespace}/spire-server has no server.conf"))?;
    parse_server_conf(raw).map_err(|e| format!("ConfigMap {namespace}/spire-server: {e}"))
}

fn parse_server_conf(raw: &str) -> Result<(String, String), String> {
    let conf: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("server.conf is not JSON: {e}"))?;
    let field = |name: &str| {
        conf.pointer(&format!("/server/{name}"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("server.conf sets no server.{name}"))
    };
    Ok((field("trust_domain")?, field("jwt_issuer")?))
}

/// Read every cluster `directory` has not read, and again every [`REFRESH`] once all are read,
/// refusing a SPIRE whose namespace pack code reaches.
pub fn spawn(
    directory: SpireDirectory,
    clusters: Arc<ClusterClients>,
    profile: Arc<DeployProfile>,
) {
    tokio::spawn(async move {
        let mut since_refresh = Duration::ZERO;
        loop {
            let unread = directory.unread();
            let todo = match (unread.is_empty(), since_refresh >= REFRESH) {
                (false, _) => unread,
                (true, true) => directory.states().into_keys().collect(),
                (true, false) => Vec::new(),
            };
            if since_refresh >= REFRESH {
                since_refresh = Duration::ZERO;
            }
            for cluster in todo {
                let state = match discover(&clusters, &profile, &cluster)
                    .await
                    .and_then(|spire| {
                        crate::agent_auth::placement::check_spire_exposure(
                            std::slice::from_ref(&spire),
                            &profile,
                        )
                        .map(|()| spire)
                        .map_err(|e| e.to_string())
                    }) {
                    Ok(spire) => {
                        tracing::info!(
                            cluster,
                            svid_issuer = spire.svid_issuer.as_str(),
                            prefix = spire.gateway_prefix.as_str(),
                            "SPIRE discovered; agents may run there"
                        );
                        SpireState::Ready(spire)
                    }
                    Err(why) => {
                        tracing::warn!(cluster, why, "SPIRE not discovered; agents are off there");
                        SpireState::Unavailable(why)
                    }
                };
                directory.set(&cluster, state);
            }
            tokio::time::sleep(RETRY).await;
            since_refresh += RETRY;
        }
    });
}

#[cfg(test)]
mod tests {
    use crate::agent_auth::discovery::*;

    #[test]
    fn each_platform_reads_its_spire_where_its_installer_puts_it() {
        let ocp = default_discovery(Platform::Openshift).unwrap();
        assert_eq!(
            ocp.keys_path(),
            "/api/v1/namespaces/zero-trust-workload-identity-manager/services/\
             https:spire-spiffe-oidc-discovery-provider:443/proxy/keys"
        );
        let k8s = default_discovery(Platform::Kubernetes).unwrap();
        assert_eq!(
            k8s.keys_path(),
            "/api/v1/namespaces/spire-server/services/\
             http:spire-spiffe-oidc-discovery-provider:80/proxy/keys"
        );
    }

    #[test]
    fn a_trust_domain_and_issuer_make_the_gateway_prefix() {
        let spire = from_parts(
            "k8s",
            "k8s.crucible",
            "https://oidc-discovery.k8s.crucible",
            default_discovery(Platform::Kubernetes).unwrap(),
        )
        .unwrap();
        assert_eq!(spire.cluster, "k8s");
        assert_eq!(spire.svid_issuer, "https://oidc-discovery.k8s.crucible");
        assert_eq!(
            spire.gateway_prefix.as_str(),
            "spiffe://k8s.crucible/openshell/gateway/"
        );
        for (td, iss) in [
            ("", "https://oidc.example"),
            ("a/b", "https://oidc.example"),
            ("k8s.crucible", "not a url"),
        ] {
            assert!(
                from_parts(
                    "k8s",
                    td,
                    iss,
                    default_discovery(Platform::Kubernetes).unwrap()
                )
                .is_err(),
                "{td:?} {iss:?}"
            );
        }
    }

    #[test]
    fn the_upstream_chart_config_names_the_trust_domain_and_issuer() {
        let conf = r#"{"server": {"trust_domain": "k8s.crucible",
            "jwt_issuer": "https://oidc-discovery.k8s.crucible", "bind_port": "8081"}}"#;
        assert_eq!(
            parse_server_conf(conf).unwrap(),
            (
                "k8s.crucible".to_string(),
                "https://oidc-discovery.k8s.crucible".to_string()
            )
        );
        assert!(parse_server_conf(r#"{"server": {"trust_domain": "x"}}"#).is_err());
        assert!(parse_server_conf("trust_domain = \"x\"").is_err());
    }

    #[test]
    fn the_directory_holds_each_clusters_last_state() {
        let directory = SpireDirectory::default();
        assert!(!directory.enabled());
        directory.enable(["ocp".to_string(), "k8s".to_string()]);
        assert!(directory.enabled());
        assert!(directory.ready().is_empty());
        assert_eq!(directory.unread(), ["k8s", "ocp"]);
        let spire = from_parts(
            "k8s",
            "k8s.crucible",
            "https://oidc.k8s",
            default_discovery(Platform::Kubernetes).unwrap(),
        )
        .unwrap();
        directory.set("k8s", SpireState::Ready(spire.clone()));
        directory.set("ocp", SpireState::Unavailable("no ZTWIM".into()));
        assert_eq!(directory.ready(), [spire]);
        assert_eq!(directory.unread(), ["ocp"]);
        let shared = directory.clone();
        directory.enable(["k8s".to_string()]);
        assert_eq!(
            shared.ready().len(),
            1,
            "enabling again keeps what was read"
        );
    }

    #[test]
    fn the_api_shows_every_agent_cluster_and_why_one_is_off() {
        let directory = SpireDirectory::default();
        directory.enable(["k8s".to_string(), "ocp".to_string()]);
        directory.set(
            "k8s",
            SpireState::Ready(
                from_parts(
                    "k8s",
                    "k8s.crucible",
                    "https://oidc.k8s",
                    default_discovery(Platform::Kubernetes).unwrap(),
                )
                .unwrap(),
            ),
        );
        directory.set(
            "ocp",
            SpireState::Unavailable("no ZTWIM named cluster".into()),
        );
        let shown = agent_clusters(&["edge".to_string()], &directory);
        let summary: Vec<(&str, &str, &str)> = shown
            .iter()
            .map(|c| (c.cluster.as_str(), c.state.as_str(), c.source.as_str()))
            .collect();
        assert_eq!(
            summary,
            [
                ("edge", "ready", "configured"),
                ("k8s", "ready", "discovered"),
                ("ocp", "unavailable", "discovered"),
            ]
        );
        assert_eq!(shown[1].svid_issuer.as_deref(), Some("https://oidc.k8s"));
        assert_eq!(shown[2].reason.as_deref(), Some("no ZTWIM named cluster"));
    }
}
