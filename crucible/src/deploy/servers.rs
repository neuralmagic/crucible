//! The MCP servers that take agent tokens, as the deploy profile describes them: what each is
//! called, where it is, which clusters' runs get it, what a token for it must carry, and what bounds
//! it. `[kueue]`, `[buildit]` and each `[[agent_servers]]` entry describe one; the issuer and
//! dispatch read only these descriptions.

use crate::deploy::profile::DeployProfile;
use crucible_contract::agent_servers::{
    BuilditSite, LocalServer, NotServerName, ServerName, Transport,
};
use std::collections::BTreeMap;

/// One MCP server that takes agent tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    pub name: ServerName,
    /// The canonical MCP URL, a token's `aud`.
    pub url: String,
    /// The dispatch clusters whose runs get the server.
    pub clusters: Vec<String>,
    /// Whether the server authorizes as the token's `kube_user`, so a token without one is
    /// useless and is refused.
    pub requires_kube_user: bool,
    /// The namespaces a token for the server names, the first as the default. Empty: none.
    pub namespaces: Vec<String>,
    pub transport: Transport,
    /// What bounds it beyond its scopes, as the run page shows it.
    pub limits: BTreeMap<String, String>,
}

impl ServerSpec {
    /// Whether runs on `cluster` get this server.
    pub fn serves(&self, cluster: &str) -> bool {
        self.clusters.iter().any(|c| c == cluster)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServerSpecError {
    #[error(transparent)]
    Name(#[from] NotServerName),
    #[error("[buildit] run_issuer {issuer:?} gives no key set URI: {source}")]
    Issuer {
        issuer: String,
        source: url::ParseError,
    },
    #[error("agent server {name}'s URL {url:?} is not an http(s) URL with a host")]
    Url { name: ServerName, url: String },
    #[error("an agent server may not be named broker: its gateway provider would be the broker's")]
    Broker,
    #[error("two agent servers are named {0}")]
    DuplicateName(ServerName),
    #[error("agent servers {first} and {second} both serve {url} on cluster {cluster}")]
    DuplicateUrl {
        first: ServerName,
        second: ServerName,
        url: String,
        cluster: String,
    },
}

fn namespace_limit(namespaces: &[String]) -> BTreeMap<String, String> {
    match namespaces.is_empty() {
        true => BTreeMap::new(),
        false => BTreeMap::from([("namespace".to_string(), namespaces.join(","))]),
    }
}

impl DeployProfile {
    /// Every MCP server the profile describes. Names are unique, and no two servers share a URL
    /// on a cluster.
    pub fn agent_servers(&self) -> Result<Vec<ServerSpec>, ServerSpecError> {
        let mut servers = Vec::new();
        if let Some(k) = &self.kueue {
            let namespaces: Vec<String> = k
                .workload_namespaces
                .iter()
                .map(ToString::to_string)
                .collect();
            let mut limits = namespace_limit(&namespaces);
            if !k.pull_secrets.is_empty() {
                limits.insert("pull_secrets".to_string(), k.pull_secrets.join(","));
            }
            servers.push(ServerSpec {
                name: ServerName::parse("kueue")?,
                url: k.url(),
                clusters: k.clusters.clone(),
                requires_kube_user: true,
                namespaces,
                transport: Transport::Remote,
                limits,
            });
        }
        if let Some(b) = &self.buildit {
            let run_issuer = b
                .trusted_issuer()
                .map_err(|source| ServerSpecError::Issuer {
                    issuer: b.run_issuer.clone(),
                    source,
                })?;
            servers.push(ServerSpec {
                name: ServerName::parse("buildit")?,
                url: b.url(),
                clusters: b.clusters.clone(),
                requires_kube_user: false,
                namespaces: Vec::new(),
                transport: Transport::Local(LocalServer::Buildit(BuilditSite {
                    run_issuer,
                    namespace: b.namespace.to_string(),
                    push_repo: b.push_repo.clone(),
                })),
                limits: b
                    .push_repo
                    .iter()
                    .map(|repo| ("push_repo".to_string(), repo.clone()))
                    .collect(),
            });
        }
        for s in &self.agent_servers {
            let namespaces: Vec<String> = s.namespaces.iter().map(ToString::to_string).collect();
            servers.push(ServerSpec {
                name: s.name.clone(),
                url: s.url.to_string(),
                clusters: s.clusters.clone(),
                requires_kube_user: s.requires_kube_user,
                limits: namespace_limit(&namespaces),
                namespaces,
                transport: Transport::Remote,
            });
        }
        for s in &servers {
            let ok = url::Url::parse(&s.url).is_ok_and(|u| {
                matches!(u.scheme(), "http" | "https")
                    && u.host_str().is_some_and(|h| !h.is_empty())
            });
            if !ok {
                return Err(ServerSpecError::Url {
                    name: s.name.clone(),
                    url: s.url.clone(),
                });
            }
        }
        if servers.iter().any(|s| s.name.as_str() == "broker") {
            return Err(ServerSpecError::Broker);
        }
        for (i, a) in servers.iter().enumerate() {
            for b in &servers[i + 1..] {
                if a.name == b.name {
                    return Err(ServerSpecError::DuplicateName(a.name.clone()));
                }
                if let Some(cluster) = a.clusters.iter().find(|c| a.url == b.url && b.serves(c)) {
                    return Err(ServerSpecError::DuplicateUrl {
                        first: a.name.clone(),
                        second: b.name.clone(),
                        url: a.url.clone(),
                        cluster: cluster.clone(),
                    });
                }
            }
        }
        Ok(servers)
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

    const KUEUE_AND_BUILDIT: &str = r#"
        [kueue]
        image = "registry.example.com/crucible-kueue:latest"
        clusters = ["pirate", "waldorf"]
        namespace = "kueue-server"
        workload_namespaces = ["team-a", "team-b"]
        run_issuer = "http://crucible-controller.loops.svc:8080"
        kube_authz = "openshift"
        pull_secrets = ["quay-pull"]
        [buildit]
        cluster = "pirate"
        namespace = "builds"
        run_issuer = "http://crucible-controller.loops.svc:8080"
        push_repo = "quay.io/team/agent"
    "#;

    fn profile(tables: &str) -> DeployProfile {
        toml::from_str(&format!("{BASE}{tables}")).unwrap()
    }

    fn name(raw: &str) -> ServerName {
        ServerName::parse(raw).unwrap()
    }

    #[test]
    fn a_profile_without_server_tables_describes_no_server() {
        assert!(profile("").agent_servers().unwrap().is_empty());
    }

    #[test]
    fn kueue_needs_a_kube_user_and_names_its_namespaces_and_buildit_runs_in_the_loop_pod() {
        let servers = profile(KUEUE_AND_BUILDIT).agent_servers().unwrap();
        assert_eq!(
            servers,
            [
                ServerSpec {
                    name: name("kueue"),
                    url: "http://crucible-kueue.kueue-server.svc.cluster.local:8850/mcp".into(),
                    clusters: vec!["pirate".into(), "waldorf".into()],
                    requires_kube_user: true,
                    namespaces: vec!["team-a".into(), "team-b".into()],
                    transport: Transport::Remote,
                    limits: BTreeMap::from([
                        ("namespace".into(), "team-a,team-b".into()),
                        ("pull_secrets".into(), "quay-pull".into()),
                    ]),
                },
                ServerSpec {
                    name: name("buildit"),
                    url: crucible_contract::buildit::URL.into(),
                    clusters: vec!["pirate".into()],
                    requires_kube_user: false,
                    namespaces: vec![],
                    transport: Transport::Local(LocalServer::Buildit(BuilditSite {
                        run_issuer: "http://crucible-controller.loops.svc:8080=\
                            http://crucible-controller.loops.svc:8080/.well-known/jwks.json"
                            .into(),
                        namespace: "builds".into(),
                        push_repo: Some("quay.io/team/agent".into()),
                    })),
                    limits: BTreeMap::from([("push_repo".into(), "quay.io/team/agent".into())]),
                },
            ]
        );
        assert!(servers[0].serves("waldorf") && !servers[1].serves("waldorf"));
    }

    #[test]
    fn buildits_url_comes_from_its_table() {
        let servers = profile(&KUEUE_AND_BUILDIT.replace(
            "push_repo = ",
            "url = \"http://host.openshell.internal:9000/mcp\"\npush_repo = ",
        ))
        .agent_servers()
        .unwrap();
        assert_eq!(servers[1].url, "http://host.openshell.internal:9000/mcp");
    }

    #[test]
    fn a_third_server_needs_only_an_agent_servers_entry() {
        let servers = profile(
            r#"
            [[agent_servers]]
            name = "vault-mcp"
            url = "https://vault-mcp.tools.svc:8443/mcp"
            clusters = ["pirate"]
            requires_kube_user = true
            namespaces = ["team-a"]
            [[agent_servers]]
            name = "docs"
            url = "http://docs.example/mcp"
            "#,
        )
        .agent_servers()
        .unwrap();
        assert_eq!(
            servers[0],
            ServerSpec {
                name: name("vault-mcp"),
                url: "https://vault-mcp.tools.svc:8443/mcp".into(),
                clusters: vec!["pirate".into()],
                requires_kube_user: true,
                namespaces: vec!["team-a".into()],
                transport: Transport::Remote,
                limits: BTreeMap::from([("namespace".into(), "team-a".into())]),
            }
        );
        assert_eq!(servers[1].clusters, ["hub"]);
        assert!(!servers[1].requires_kube_user && servers[1].namespaces.is_empty());
    }

    #[test]
    fn names_are_unique_and_no_two_servers_share_a_url_on_a_cluster() {
        let twice = profile(&format!(
            "{KUEUE_AND_BUILDIT}[[agent_servers]]\nname = \"kueue\"\nurl = \"http://x/mcp\"\n"
        ));
        assert!(matches!(
            twice.agent_servers(),
            Err(ServerSpecError::DuplicateName(n)) if n.as_str() == "kueue"
        ));
        let shared = profile(&format!(
            "{KUEUE_AND_BUILDIT}[[agent_servers]]\nname = \"other\"\ncluster = \"waldorf\"\n\
             url = \"http://crucible-kueue.kueue-server.svc.cluster.local:8850/mcp\"\n"
        ));
        assert!(matches!(
            shared.agent_servers(),
            Err(ServerSpecError::DuplicateUrl { cluster, .. }) if cluster == "waldorf"
        ));
        let elsewhere = profile(&format!(
            "{KUEUE_AND_BUILDIT}[[agent_servers]]\nname = \"other\"\ncluster = \"east\"\n\
             url = \"http://crucible-kueue.kueue-server.svc.cluster.local:8850/mcp\"\n"
        ));
        assert!(
            elsewhere.agent_servers().is_ok(),
            "same URL, another cluster"
        );
        let broker = profile(&format!(
            "{KUEUE_AND_BUILDIT}[[agent_servers]]\nname = \"broker\"\nurl = \"http://b.example/mcp\"\n"
        ));
        assert!(matches!(
            broker.agent_servers(),
            Err(ServerSpecError::Broker)
        ));
        let ftp = profile(&format!(
            "{KUEUE_AND_BUILDIT}[[agent_servers]]\nname = \"files\"\nurl = \"ftp://files.example/mcp\"\n"
        ));
        assert!(matches!(
            ftp.agent_servers(),
            Err(ServerSpecError::Url { .. })
        ));
        let bad_name =
            format!("{BASE}[[agent_servers]]\nname = \"Docs\"\nurl = \"http://x/mcp\"\n");
        assert!(toml::from_str::<DeployProfile>(&bad_name).is_err());
    }
}
