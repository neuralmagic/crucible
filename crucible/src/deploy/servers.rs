//! The MCP servers that take agent tokens, as the deploy profile describes them: where each is,
//! which clusters' runs get it, and what a token for it must carry. `[kueue]` and `[buildit]` each
//! describe one; the issuer and dispatch read only these descriptions.

use crate::deploy::profile::DeployProfile;

/// One MCP server that takes agent tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    /// The canonical MCP URL, a token's `aud`.
    pub url: String,
    /// The dispatch clusters whose runs get the server.
    pub clusters: Vec<String>,
    /// Whether the server authorizes as the token's `kube_user`, so a token without one is
    /// useless and is refused.
    pub requires_kube_user: bool,
    /// The namespaces a token for the server names, the first as the default. Empty: none.
    pub namespaces: Vec<String>,
}

impl ServerSpec {
    /// Whether runs on `cluster` get this server.
    pub fn serves(&self, cluster: &str) -> bool {
        self.clusters.iter().any(|c| c == cluster)
    }
}

impl DeployProfile {
    /// Every MCP server the profile describes.
    pub fn agent_servers(&self) -> Vec<ServerSpec> {
        let kueue = self.kueue.as_ref().map(|k| ServerSpec {
            url: k.url(),
            clusters: k.clusters.clone(),
            requires_kube_user: true,
            namespaces: k
                .workload_namespaces
                .iter()
                .map(ToString::to_string)
                .collect(),
        });
        let buildit = self.buildit.as_ref().map(|b| ServerSpec {
            url: crucible_contract::buildit::URL.to_string(),
            clusters: b.clusters.clone(),
            requires_kube_user: false,
            namespaces: Vec::new(),
        });
        kueue.into_iter().chain(buildit).collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::deploy::profile::DeployProfile;
    use crate::deploy::servers::*;

    const BASE: &str = r#"
        [cluster]
        loop_namespace = "loops"
        rig_namespace = "rig"
        service_account = "crucible-spoke"
        supervisor_image = "registry.example.com/openshell-supervisor:latest"
        [image]
        loop = "registry.example.com/crucible-loop:latest"
        pull_secret = "pull"
    "#;

    fn profile(tables: &str) -> DeployProfile {
        toml::from_str(&format!("{BASE}{tables}")).unwrap()
    }

    #[test]
    fn a_profile_without_server_tables_describes_no_server() {
        assert!(profile("").agent_servers().is_empty());
    }

    #[test]
    fn kueue_needs_a_kube_user_and_names_its_namespaces_and_buildit_needs_neither() {
        let servers = profile(
            r#"
            [kueue]
            image = "registry.example.com/crucible-kueue:latest"
            clusters = ["pirate", "waldorf"]
            namespace = "kueue-server"
            workload_namespaces = ["team-a", "team-b"]
            run_issuer = "http://crucible-controller.loops.svc:8080"
            kube_authz = "openshift"
            [buildit]
            cluster = "pirate"
            namespace = "builds"
            run_issuer = "http://crucible-controller.loops.svc:8080"
            "#,
        )
        .agent_servers();
        assert_eq!(
            servers,
            [
                ServerSpec {
                    url: "http://crucible-kueue.kueue-server.svc.cluster.local:8850/mcp".into(),
                    clusters: vec!["pirate".into(), "waldorf".into()],
                    requires_kube_user: true,
                    namespaces: vec!["team-a".into(), "team-b".into()],
                },
                ServerSpec {
                    url: crucible_contract::buildit::URL.into(),
                    clusters: vec!["pirate".into()],
                    requires_kube_user: false,
                    namespaces: vec![],
                },
            ]
        );
        assert!(servers[0].serves("waldorf") && !servers[1].serves("waldorf"));
    }
}
