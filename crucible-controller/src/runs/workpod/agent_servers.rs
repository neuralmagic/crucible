//! A run's access to the MCP servers that take agent tokens: each server the deploy profile
//! describes on the run's cluster ([`DeployProfile::agent_servers`]) whose tokens the issuer can
//! mint there (its SPIRE is trusted), with the server's scopes. The loop pod gets them as [`ENV_AGENT_SERVERS`], plus the controller's agent token
//! endpoint and the audience its sandboxes' JWT-SVIDs are requested for, as plain env. The engine
//! turns each server into a token-grant provider: each sandbox's supervisor obtains its own token.
//! Nothing secret is minted and nothing is recorded.
//!
//! [`DeployProfile::agent_servers`]: crucible::deploy::DeployProfile::agent_servers

use crate::agent_auth::cluster_spire::ClusterSpire;
use crate::agent_auth::issuer::{IssuerUrl, TOKEN_PATH, parse_list};
use crate::secrets::deliver::Delivery;
use anyhow::Context;
use crucible::deploy::servers::ServerSpec;
use crucible_contract::agent_servers::{AgentServer, ENV_AGENT_SERVERS};
use crucible_contract::sandbox::{ENV_AGENT_SVID_AUDIENCE, ENV_AGENT_TOKEN_ENDPOINT};

/// Where sandboxes get agent tokens and for what: the controller's issuer identifier (the SVID
/// audience), the token endpoint supervisors reach (the issuer's own unless an operator routed it
/// elsewhere), and the clusters whose SPIRE the issuer trusts: those `CONTROLLER_AGENT_SPIRE`
/// names, and those discovery has read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTokens {
    pub svid_audience: String,
    pub endpoint: String,
    /// The clusters `CONTROLLER_AGENT_SPIRE` names.
    pub clusters: Vec<String>,
    pub spire: crate::agent_auth::cluster_spire::SpireDirectory,
}

impl AgentTokens {
    /// From the controller's `CONTROLLER_AGENT_ISSUER`, `CONTROLLER_AGENT_SPIRE` and optional
    /// `CONTROLLER_AGENT_TOKEN_ENDPOINT`; `None` when the controller mints no agent tokens.
    pub fn from_cfg(cfg: &crate::config::ControllerCfg) -> anyhow::Result<Option<Self>> {
        if !cfg.issues_agent_tokens() {
            return Ok(None);
        }
        let issuer = IssuerUrl::parse(cfg.agent_issuer.as_deref().unwrap_or_default())?;
        let spire: Vec<ClusterSpire> = parse_list(cfg.agent_spire.as_deref().unwrap_or_default())?;
        Ok(Some(Self {
            svid_audience: issuer.as_str().to_string(),
            endpoint: cfg
                .agent_token_endpoint
                .clone()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| issuer.endpoint(TOKEN_PATH)),
            clusters: spire.into_iter().map(|s| s.cluster).collect(),
            spire: cfg.spire.clone(),
        }))
    }

    /// Whether the issuer mints tokens for runs on `cluster`: it trusts that cluster's SPIRE.
    pub fn mints_on(&self, cluster: &str) -> bool {
        self.clusters.iter().any(|c| c == cluster)
            || self.spire.ready().iter().any(|s| s.cluster == cluster)
    }
}

/// The servers in `servers` a run on `cluster` gets: those serving it, when the issuer mints
/// tokens there.
pub fn granted<'a>(
    servers: &'a [ServerSpec],
    cluster: &str,
    tokens: &AgentTokens,
) -> Vec<&'a ServerSpec> {
    match tokens.mints_on(cluster) {
        true => servers.iter().filter(|s| s.serves(cluster)).collect(),
        false => Vec::new(),
    }
}

/// The loop pod's env for a run dispatched onto `cluster`, `None` when no server serves it.
pub fn access(
    servers: &[ServerSpec],
    cluster: &str,
    tokens: &AgentTokens,
) -> anyhow::Result<Option<Delivery>> {
    let granted: Vec<AgentServer> = granted(servers, cluster, tokens)
        .into_iter()
        .map(|s| {
            Ok(AgentServer {
                name: s.name.clone(),
                url: s.url.clone(),
                scopes: s.scopes.clone(),
                transport: s.transport(&tokens.svid_audience).with_context(|| {
                    format!("deriving {}'s key set URI from the agent issuer", s.name)
                })?,
            })
        })
        .collect::<anyhow::Result<_>>()?;
    if granted.is_empty() {
        return Ok(None);
    }
    let encoded = crucible_contract::agent_servers::encode(&granted)
        .context("encoding the run's agent servers")?;
    Ok(Some(Delivery {
        plain_env: vec![
            (
                ENV_AGENT_TOKEN_ENDPOINT.to_string(),
                tokens.endpoint.clone(),
            ),
            (
                ENV_AGENT_SVID_AUDIENCE.to_string(),
                tokens.svid_audience.clone(),
            ),
            (ENV_AGENT_SERVERS.to_string(), encoded),
        ],
        ..Delivery::default()
    }))
}

#[cfg(test)]
mod tests {
    use crate::runs::workpod::agent_servers::*;
    use crucible::deploy::profile::DeployProfile;
    use crucible_contract::agent_servers::{BuilditSite, LocalServer, Transport};

    const PROFILE: &str = r#"
        [cluster]
        loop_namespace = "crucible-system"
        rig_namespace = "rig"
        service_account = "crucible-spoke"
        supervisor_image = "registry.example.com/openshell-supervisor:latest"
        [image]
        loop = "registry.example.com/crucible-loop:latest"
        pull_secret = "quay-pull"
    "#;

    const KUEUE: &str = r#"
        [kueue]
        image = "registry.example.com/crucible-kueue:latest"
        cluster = "ocp"
        namespace = "crucible-system"
        workload_namespace = "crucible-kueue-e2e"
        kube_authz = "openshift"
    "#;

    const BUILDIT: &str = r#"
        [buildit]
        clusters = ["ocp", "k8s"]
        namespace = "crucible-builds"
        run_jwks_uri = "https://crucible-ext.hub.example/.well-known/jwks.json"
        push_repo = "quay.io/example/crucible-smoke"
    "#;

    const KUEUE_URL: &str = "http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp";
    const SPIRE: &str = "ocp=https://oidc.ocp.example spiffe://ocp.example/openshell/gateway/ spire/oidc:443,\
         k8s=https://oidc.k8s.example spiffe://k8s.example/openshell/gateway/ spire/oidc:443";

    fn tokens() -> AgentTokens {
        AgentTokens {
            svid_audience: "http://crucible-controller.crucible-system.svc:8080".into(),
            endpoint: "http://crucible-controller.crucible-system.svc:8080/oauth/token".into(),
            clusters: vec!["ocp".into(), "k8s".into()],
            spire: Default::default(),
        }
    }

    fn servers(tables: &str) -> Vec<ServerSpec> {
        toml::from_str::<DeployProfile>(&format!("{PROFILE}{tables}"))
            .expect("profile parses")
            .agent_servers()
            .expect("servers")
    }

    fn env_of(d: &Delivery) -> std::collections::BTreeMap<&str, &str> {
        d.plain_env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    fn granted_servers(d: &Delivery) -> Vec<AgentServer> {
        crucible_contract::agent_servers::decode(env_of(d)[ENV_AGENT_SERVERS]).unwrap()
    }

    fn names(d: &Delivery) -> Vec<String> {
        granted_servers(d)
            .into_iter()
            .map(|s| s.name.to_string())
            .collect()
    }

    #[test]
    fn a_run_on_the_kueue_cluster_gets_the_server_and_the_token_endpoint_and_no_secret() {
        let delivery = access(&servers(KUEUE), "ocp", &tokens())
            .unwrap()
            .expect("access");
        assert!(delivery.is_empty(), "no Secret data: {delivery:?}");
        assert!(delivery.env.is_empty());
        let env = env_of(&delivery);
        assert_eq!(
            env.keys().copied().collect::<Vec<_>>(),
            [
                ENV_AGENT_SERVERS,
                ENV_AGENT_SVID_AUDIENCE,
                ENV_AGENT_TOKEN_ENDPOINT
            ]
        );
        assert_eq!(
            env[ENV_AGENT_TOKEN_ENDPOINT],
            "http://crucible-controller.crucible-system.svc:8080/oauth/token"
        );
        assert_eq!(
            env[ENV_AGENT_SVID_AUDIENCE],
            "http://crucible-controller.crucible-system.svc:8080"
        );
        assert_eq!(
            granted_servers(&delivery),
            [AgentServer {
                name: crucible_contract::agent_servers::ServerName::parse("kueue").unwrap(),
                url: KUEUE_URL.into(),
                scopes: vec!["kueue.read".into(), "kueue.submit".into()],
                transport: Transport::Remote,
            }]
        );
    }

    #[test]
    fn a_run_elsewhere_or_without_servers_gets_nothing() {
        assert!(access(&servers(KUEUE), "hub", &tokens()).unwrap().is_none());
        assert!(access(&[], "ocp", &tokens()).unwrap().is_none());
    }

    #[test]
    fn a_run_on_any_of_the_servers_clusters_gets_access() {
        let both = servers(&KUEUE.replace("cluster = \"ocp\"", "clusters = [\"ocp\", \"k8s\"]"));
        for cluster in ["ocp", "k8s"] {
            assert!(
                access(&both, cluster, &tokens()).unwrap().is_some(),
                "{cluster}"
            );
        }
        assert!(access(&both, "hub", &tokens()).unwrap().is_none());
    }

    #[test]
    fn a_server_the_issuer_cannot_mint_for_is_not_granted() {
        let all = servers(&format!("{KUEUE}{BUILDIT}"));
        let no_spire = AgentTokens {
            clusters: vec!["k8s".into()],
            ..tokens()
        };
        assert_eq!(
            names(&access(&all, "k8s", &no_spire).unwrap().unwrap()),
            ["buildit"]
        );
        assert!(
            access(&servers(KUEUE), "ocp", &no_spire).unwrap().is_none(),
            "no SPIRE for ocp, so no tokens for its sandboxes"
        );
    }

    #[test]
    fn buildit_is_an_entry_served_in_the_loop_pod_with_its_issuer_and_push_repo() {
        let all = servers(&format!("{KUEUE}{BUILDIT}"));
        let on_k8s = access(&all, "k8s", &tokens()).unwrap().unwrap();
        assert_eq!(
            granted_servers(&on_k8s),
            [AgentServer {
                name: crucible_contract::agent_servers::ServerName::parse("buildit").unwrap(),
                url: crucible_contract::buildit::URL.into(),
                scopes: vec!["buildit.read".into(), "buildit.build".into()],
                transport: Transport::Local(LocalServer::Buildit(BuilditSite {
                    run_issuer: format!(
                        "{}=https://crucible-ext.hub.example/.well-known/jwks.json",
                        tokens().svid_audience
                    ),
                    namespace: "crucible-builds".into(),
                    push_repo: Some("quay.io/example/crucible-smoke".into()),
                })),
            }]
        );
        let on_ocp = access(&all, "ocp", &tokens()).unwrap().unwrap();
        assert_eq!(names(&on_ocp), ["kueue", "buildit"]);
        assert_eq!(
            on_ocp
                .plain_env
                .iter()
                .filter(|(k, _)| k == ENV_AGENT_TOKEN_ENDPOINT)
                .count(),
            1,
            "the token endpoint once for both servers"
        );
    }

    #[test]
    fn a_third_server_from_the_profile_is_granted_like_the_others() {
        let docs = r#"
            [[agent_servers]]
            name = "docs"
            url = "https://docs.example/mcp"
            scopes = ["docs.read"]
            cluster = "ocp"
        "#;
        let delivery = access(&servers(docs), "ocp", &tokens()).unwrap().unwrap();
        let granted = granted_servers(&delivery);
        assert_eq!(names(&delivery), ["docs"]);
        assert_eq!(granted[0].scopes, ["docs.read"]);
        assert_eq!(granted[0].transport, Transport::Remote);
    }

    #[test]
    fn the_token_endpoint_is_the_issuers_unless_routed_elsewhere() {
        let cfg = |extra: &[&str]| {
            let mut args = vec!["ctl"];
            args.extend_from_slice(extra);
            crate::testing::cfg_from_args(args)
        };
        assert_eq!(AgentTokens::from_cfg(&cfg(&[])).unwrap(), None);
        assert_eq!(
            AgentTokens::from_cfg(&cfg(&[
                "--agent-issuer",
                "http://crucible-controller.crucible-system.svc:8080/",
            ]))
            .unwrap(),
            None,
            "an issuer without SPIRE serves people only"
        );
        let own = AgentTokens::from_cfg(&cfg(&[
            "--agent-issuer",
            "http://crucible-controller.crucible-system.svc:8080/",
            "--agent-spire",
            SPIRE,
        ]))
        .unwrap()
        .expect("tokens");
        assert_eq!(own, tokens(), "the trailing slash is the issuer's to drop");
        let routed = AgentTokens::from_cfg(&cfg(&[
            "--agent-issuer",
            "http://crucible-controller.crucible-system.svc:8080",
            "--agent-spire",
            SPIRE,
            "--agent-token-endpoint",
            "https://crucible.hub.example/oauth/token",
        ]))
        .unwrap()
        .expect("tokens");
        assert_eq!(routed.endpoint, "https://crucible.hub.example/oauth/token");
        assert_eq!(
            routed.svid_audience,
            "http://crucible-controller.crucible-system.svc:8080"
        );
        assert!(
            AgentTokens::from_cfg(&cfg(&[
                "--agent-issuer",
                "not a url",
                "--agent-spire",
                SPIRE
            ]))
            .is_err()
        );
    }

    /// A cluster discovery has read gets the servers that serve it; one it has not read does not.
    #[test]
    fn a_discovered_cluster_gets_its_servers_once_read() {
        use crate::agent_auth::cluster_spire::SpireState;
        let all = servers(&format!("{KUEUE}{BUILDIT}"));
        let tokens = AgentTokens {
            clusters: Vec::new(),
            ..tokens()
        };
        tokens.spire.enable(["k8s".to_string()]);
        assert!(access(&all, "k8s", &tokens).unwrap().is_none(), "pending");
        let spire: crate::agent_auth::cluster_spire::ClusterSpire =
            "k8s=https://oidc.k8s.example spiffe://k8s.example/openshell/gateway/ spire/oidc:443"
                .parse()
                .unwrap();
        tokens.spire.set("k8s", SpireState::Ready(spire));
        assert_eq!(
            names(&access(&all, "k8s", &tokens).unwrap().unwrap()),
            ["buildit"]
        );
        assert!(access(&all, "ocp", &tokens).unwrap().is_none());
    }
}
