//! A run's crucible-kueue access. A run dispatched onto the deploy profile's `[kueue]` cluster gets
//! the server's MCP URL, the controller's agent token endpoint, and the audience its sandboxes'
//! JWT-SVIDs are requested for, as plain env on the loop pod. The engine turns them into a
//! token-grant provider: each sandbox's supervisor obtains its own token. Nothing secret is minted
//! and nothing is recorded.

use crate::agent_auth::issuer::{IssuerUrl, TOKEN_PATH};
use crate::secrets::deliver::Delivery;
use crucible::deploy::profile::KueueCfg;
use crucible_contract::kueue::ENV_KUEUE_URL;
use crucible_contract::sandbox::{ENV_AGENT_SVID_AUDIENCE, ENV_AGENT_TOKEN_ENDPOINT};

/// Where sandboxes get agent tokens: the controller's issuer identifier (the SVID audience) and the
/// token endpoint supervisors reach, the issuer's own unless an operator routed it elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTokens {
    pub svid_audience: String,
    pub endpoint: String,
}

impl AgentTokens {
    /// From the controller's `CONTROLLER_AGENT_ISSUER` and optional
    /// `CONTROLLER_AGENT_TOKEN_ENDPOINT`; `None` when the controller issues no agent tokens.
    pub fn from_cfg(cfg: &crate::config::ControllerCfg) -> anyhow::Result<Option<Self>> {
        let Some(raw) = cfg.agent_issuer.as_deref().filter(|v| !v.trim().is_empty()) else {
            return Ok(None);
        };
        let issuer = IssuerUrl::parse(raw)?;
        Ok(Some(Self {
            svid_audience: issuer.as_str().to_string(),
            endpoint: cfg
                .agent_token_endpoint
                .clone()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| issuer.endpoint(TOKEN_PATH)),
        }))
    }
}

/// The loop pod's crucible-kueue env for a run dispatched onto `cluster`, `None` when the server
/// runs elsewhere.
pub fn access(cfg: &KueueCfg, cluster: &str, tokens: &AgentTokens) -> Option<Delivery> {
    cfg.serves(cluster).then(|| Delivery {
        plain_env: vec![
            (ENV_KUEUE_URL.to_string(), cfg.url()),
            (
                ENV_AGENT_TOKEN_ENDPOINT.to_string(),
                tokens.endpoint.clone(),
            ),
            (
                ENV_AGENT_SVID_AUDIENCE.to_string(),
                tokens.svid_audience.clone(),
            ),
        ],
        ..Delivery::default()
    })
}

#[cfg(test)]
mod tests {
    use crate::runs::workpod::kueue::*;
    use crucible::deploy::profile::DeployProfile;

    const PROFILE: &str = r#"
        [cluster]
        loop_namespace = "crucible-system"
        rig_namespace = "rig"
        service_account = "crucible-spoke"
        supervisor_image = "registry.example.com/openshell-supervisor:latest"
        [image]
        loop = "registry.example.com/crucible-loop:latest"
        pull_secret = "quay-pull"
        [kueue]
        image = "registry.example.com/crucible-kueue:latest"
        cluster = "pirate"
        namespace = "crucible-system"
        workload_namespace = "crucible-kueue-e2e"
        run_issuer = "http://crucible-controller.crucible-system.svc.cluster.local:8080"
        kube_authz = "openshift"
    "#;

    fn kueue() -> KueueCfg {
        toml::from_str::<DeployProfile>(PROFILE)
            .ok()
            .and_then(|p| p.kueue)
            .expect("profile parses")
    }

    fn tokens() -> AgentTokens {
        AgentTokens {
            svid_audience: "http://crucible-controller.crucible-system.svc:8080".into(),
            endpoint: "http://crucible-controller.crucible-system.svc:8080/oauth/token".into(),
        }
    }

    #[test]
    fn a_run_on_the_kueue_cluster_gets_the_server_and_the_token_endpoint_and_no_secret() {
        let delivery = access(&kueue(), "pirate", &tokens()).expect("access");
        assert!(delivery.is_empty(), "no Secret data: {delivery:?}");
        assert!(delivery.env.is_empty());
        assert_eq!(
            delivery.plain_env,
            [
                (
                    ENV_KUEUE_URL.to_string(),
                    "http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp".to_string()
                ),
                (
                    ENV_AGENT_TOKEN_ENDPOINT.to_string(),
                    "http://crucible-controller.crucible-system.svc:8080/oauth/token".to_string()
                ),
                (
                    ENV_AGENT_SVID_AUDIENCE.to_string(),
                    "http://crucible-controller.crucible-system.svc:8080".to_string()
                ),
            ]
        );
    }

    #[test]
    fn a_run_elsewhere_gets_nothing() {
        assert!(access(&kueue(), "hub", &tokens()).is_none());
    }

    #[test]
    fn a_run_on_any_of_the_servers_clusters_gets_access() {
        let both = toml::from_str::<DeployProfile>(&PROFILE.replace(
            "cluster = \"pirate\"",
            "clusters = [\"pirate\", \"waldorf\"]",
        ))
        .ok()
        .and_then(|p| p.kueue)
        .expect("profile parses");
        for cluster in ["pirate", "waldorf"] {
            assert!(access(&both, cluster, &tokens()).is_some(), "{cluster}");
        }
        assert!(access(&both, "hub", &tokens()).is_none());
    }

    #[test]
    fn the_token_endpoint_is_the_issuers_unless_routed_elsewhere() {
        let cfg = |extra: &[&str]| {
            let mut args = vec!["ctl"];
            args.extend_from_slice(extra);
            crate::testing::cfg_from_args(args)
        };
        assert_eq!(AgentTokens::from_cfg(&cfg(&[])).unwrap(), None);
        let own = AgentTokens::from_cfg(&cfg(&[
            "--agent-issuer",
            "http://crucible-controller.crucible-system.svc:8080/",
        ]))
        .unwrap()
        .expect("tokens");
        assert_eq!(own, tokens(), "the trailing slash is the issuer's to drop");
        let routed = AgentTokens::from_cfg(&cfg(&[
            "--agent-issuer",
            "http://crucible-controller.crucible-system.svc:8080",
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
        assert!(AgentTokens::from_cfg(&cfg(&["--agent-issuer", "not a url"])).is_err());
    }
}
