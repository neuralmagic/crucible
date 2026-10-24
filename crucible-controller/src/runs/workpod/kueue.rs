//! A run's access to the MCP servers that take agent tokens. A run dispatched onto one of the
//! deploy profile's `[kueue]` clusters gets the server's MCP URL; one on a `[buildit]` cluster gets
//! the issuer buildit trusts and where it may push, and the engine starts buildit in the loop pod.
//! Either way the loop pod gets the controller's agent token endpoint and the audience its
//! sandboxes' JWT-SVIDs are requested for, as plain env, and the engine turns each server into a
//! token-grant provider: each sandbox's supervisor obtains its own token. Nothing secret is minted
//! and nothing is recorded. A server is granted only where the issuer can mint its tokens: on a
//! cluster whose SPIRE it trusts, for an audience it is configured with.

use crate::agent_auth::issuer::{Audience, ClusterSpire, IssuerUrl, TOKEN_PATH, parse_list};
use crate::secrets::deliver::Delivery;
use anyhow::Context;
use crucible::deploy::profile::{BuilditCfg, KueueCfg};
use crucible_contract::buildit::{ENV_BUILDIT_PUSH_REPO, ENV_BUILDIT_RUN_ISSUER};
use crucible_contract::kueue::ENV_KUEUE_URL;
use crucible_contract::sandbox::{ENV_AGENT_SVID_AUDIENCE, ENV_AGENT_TOKEN_ENDPOINT};

/// Where sandboxes get agent tokens and for what: the controller's issuer identifier (the SVID
/// audience), the token endpoint supervisors reach (the issuer's own unless an operator routed it
/// elsewhere), the clusters whose SPIRE the issuer trusts, and the audiences it mints for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTokens {
    pub svid_audience: String,
    pub endpoint: String,
    pub clusters: Vec<String>,
    pub audiences: Vec<Audience>,
}

impl AgentTokens {
    /// From the controller's `CONTROLLER_AGENT_ISSUER`, `CONTROLLER_AGENT_SPIRE`,
    /// `CONTROLLER_AGENT_AUDIENCES` and optional `CONTROLLER_AGENT_TOKEN_ENDPOINT`; `None` when
    /// the controller mints no agent tokens.
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
            audiences: parse_list(cfg.agent_audiences.as_deref().unwrap_or_default())?,
        }))
    }

    /// The audience a run on `cluster` gets tokens for at `url`, when the issuer can mint them.
    pub fn audience(&self, cluster: &str, url: &str) -> Option<&Audience> {
        self.clusters
            .iter()
            .any(|c| c == cluster)
            .then(|| self.audiences.iter().find(|a| a.resource == url))
            .flatten()
    }
}

/// The loop pod's env for a run dispatched onto `cluster`, `None` when neither crucible-kueue nor
/// buildit serves it.
pub fn access(
    kueue: Option<&KueueCfg>,
    buildit: Option<&BuilditCfg>,
    cluster: &str,
    tokens: &AgentTokens,
) -> anyhow::Result<Option<Delivery>> {
    let kueue = kueue.filter(|k| k.serves(cluster) && tokens.audience(cluster, &k.url()).is_some());
    let buildit = buildit.filter(|b| {
        b.serves(cluster)
            && tokens
                .audience(cluster, crucible_contract::buildit::URL)
                .is_some()
    });
    if kueue.is_none() && buildit.is_none() {
        return Ok(None);
    }
    let mut plain_env = vec![
        (
            ENV_AGENT_TOKEN_ENDPOINT.to_string(),
            tokens.endpoint.clone(),
        ),
        (
            ENV_AGENT_SVID_AUDIENCE.to_string(),
            tokens.svid_audience.clone(),
        ),
    ];
    if let Some(k) = kueue {
        plain_env.push((ENV_KUEUE_URL.to_string(), k.url()));
    }
    if let Some(b) = buildit {
        let issuer = b
            .trusted_issuer()
            .with_context(|| format!("deriving the key set URI of {}", b.run_issuer))?;
        plain_env.push((ENV_BUILDIT_RUN_ISSUER.to_string(), issuer));
        if let Some(repo) = &b.push_repo {
            plain_env.push((ENV_BUILDIT_PUSH_REPO.to_string(), repo.clone()));
        }
    }
    Ok(Some(Delivery {
        plain_env,
        ..Delivery::default()
    }))
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

    const KUEUE_URL: &str = "http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp";
    const SPIRE: &str = "pirate=https://oidc.pirate.example spiffe://pirate.example/openshell/gateway/ spire/oidc:443,\
         waldorf=https://oidc.waldorf.example spiffe://waldorf.example/openshell/gateway/ spire/oidc:443";

    fn audience(resource: &str, scopes: &str) -> Audience {
        format!("{resource}={scopes}")
            .parse()
            .expect("audience parses")
    }

    fn tokens() -> AgentTokens {
        AgentTokens {
            svid_audience: "http://crucible-controller.crucible-system.svc:8080".into(),
            endpoint: "http://crucible-controller.crucible-system.svc:8080/oauth/token".into(),
            clusters: vec!["pirate".into(), "waldorf".into()],
            audiences: vec![
                audience(KUEUE_URL, "kueue.read kueue.submit"),
                audience(
                    crucible_contract::buildit::URL,
                    "buildit.read buildit.build",
                ),
            ],
        }
    }

    fn profile(extra: &str) -> DeployProfile {
        toml::from_str::<DeployProfile>(&format!("{PROFILE}{extra}")).expect("profile parses")
    }

    const BUILDIT: &str = r#"
        [buildit]
        clusters = ["pirate", "waldorf"]
        run_issuer = "https://crucible.hub.example"
        run_jwks_uri = "https://crucible-ext.hub.example/.well-known/jwks.json"
        push_repo = "quay.io/example/crucible-smoke"
    "#;

    fn env_of(d: &Delivery) -> std::collections::BTreeMap<&str, &str> {
        d.plain_env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    #[test]
    fn a_run_on_the_kueue_cluster_gets_the_server_and_the_token_endpoint_and_no_secret() {
        let delivery = access(Some(&kueue()), None, "pirate", &tokens())
            .unwrap()
            .expect("access");
        assert!(delivery.is_empty(), "no Secret data: {delivery:?}");
        assert!(delivery.env.is_empty());
        assert_eq!(
            env_of(&delivery),
            std::collections::BTreeMap::from([
                (
                    ENV_KUEUE_URL,
                    "http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp"
                ),
                (
                    ENV_AGENT_TOKEN_ENDPOINT,
                    "http://crucible-controller.crucible-system.svc:8080/oauth/token"
                ),
                (
                    ENV_AGENT_SVID_AUDIENCE,
                    "http://crucible-controller.crucible-system.svc:8080"
                ),
            ])
        );
    }

    #[test]
    fn a_run_elsewhere_gets_nothing() {
        assert!(
            access(Some(&kueue()), None, "hub", &tokens())
                .unwrap()
                .is_none()
        );
        assert!(access(None, None, "pirate", &tokens()).unwrap().is_none());
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
            assert!(
                access(Some(&both), None, cluster, &tokens())
                    .unwrap()
                    .is_some(),
                "{cluster}"
            );
        }
        assert!(
            access(Some(&both), None, "hub", &tokens())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_server_the_issuer_cannot_mint_for_is_not_granted() {
        let p = profile(BUILDIT);
        let no_spire = AgentTokens {
            clusters: vec!["waldorf".into()],
            ..tokens()
        };
        assert!(
            access(p.kueue.as_ref(), None, "pirate", &no_spire)
                .unwrap()
                .is_none(),
            "no SPIRE for pirate, so no tokens for its sandboxes"
        );
        let no_audience = AgentTokens {
            audiences: vec![audience(KUEUE_URL, "kueue.read")],
            ..tokens()
        };
        let env = access(p.kueue.as_ref(), p.buildit.as_ref(), "pirate", &no_audience)
            .unwrap()
            .expect("kueue");
        let env = env_of(&env);
        assert!(env.contains_key(ENV_KUEUE_URL));
        assert!(
            !env.contains_key(ENV_BUILDIT_RUN_ISSUER),
            "buildit is no audience, so no buildit"
        );
        assert!(
            access(None, p.buildit.as_ref(), "waldorf", &no_audience)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_run_on_a_buildit_cluster_gets_the_issuer_buildit_trusts_and_its_push_repo() {
        let p = profile(BUILDIT);
        let on_waldorf = access(p.kueue.as_ref(), p.buildit.as_ref(), "waldorf", &tokens())
            .unwrap()
            .expect("buildit serves waldorf");
        let env = env_of(&on_waldorf);
        assert_eq!(
            env[ENV_BUILDIT_RUN_ISSUER],
            "https://crucible.hub.example=https://crucible-ext.hub.example/.well-known/jwks.json"
        );
        assert_eq!(env[ENV_BUILDIT_PUSH_REPO], "quay.io/example/crucible-smoke");
        assert!(env.contains_key(ENV_AGENT_TOKEN_ENDPOINT));
        assert!(!env.contains_key(ENV_KUEUE_URL), "kueue serves pirate only");
        assert!(on_waldorf.env.is_empty() && on_waldorf.is_empty());

        let on_pirate = access(p.kueue.as_ref(), p.buildit.as_ref(), "pirate", &tokens())
            .unwrap()
            .unwrap();
        let env = env_of(&on_pirate);
        assert!(env.contains_key(ENV_KUEUE_URL) && env.contains_key(ENV_BUILDIT_RUN_ISSUER));
        assert_eq!(
            on_pirate
                .plain_env
                .iter()
                .filter(|(k, _)| k == ENV_AGENT_TOKEN_ENDPOINT)
                .count(),
            1,
            "the token endpoint once for both servers"
        );

        let own = profile(
            &BUILDIT
                .replace(
                    "run_jwks_uri = \"https://crucible-ext.hub.example/.well-known/jwks.json\"\n",
                    "",
                )
                .replace("push_repo = \"quay.io/example/crucible-smoke\"\n", ""),
        );
        let env = access(None, own.buildit.as_ref(), "waldorf", &tokens())
            .unwrap()
            .unwrap();
        let env = env_of(&env);
        assert_eq!(
            env[ENV_BUILDIT_RUN_ISSUER],
            "https://crucible.hub.example=https://crucible.hub.example/.well-known/jwks.json"
        );
        assert!(!env.contains_key(ENV_BUILDIT_PUSH_REPO), "no repo, no push");
    }

    #[test]
    fn the_token_endpoint_is_the_issuers_unless_routed_elsewhere() {
        let cfg = |extra: &[&str]| {
            let mut args = vec!["ctl"];
            args.extend_from_slice(extra);
            crate::testing::cfg_from_args(args)
        };
        let audiences = format!(
            "{KUEUE_URL}=kueue.read kueue.submit,{}=buildit.read buildit.build",
            crucible_contract::buildit::URL
        );
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
            "--agent-audiences",
            &audiences,
        ]))
        .unwrap()
        .expect("tokens");
        assert_eq!(own, tokens(), "the trailing slash is the issuer's to drop");
        let routed = AgentTokens::from_cfg(&cfg(&[
            "--agent-issuer",
            "http://crucible-controller.crucible-system.svc:8080",
            "--agent-spire",
            SPIRE,
            "--agent-audiences",
            &audiences,
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
}
