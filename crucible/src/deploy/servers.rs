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
    /// The scopes its tokens are minted with.
    pub scopes: Vec<String>,
    /// The dispatch clusters whose runs get the server.
    pub clusters: Vec<String>,
    /// Whether the server authorizes as the token's `kube_user`, so a token without one is
    /// useless and is refused.
    pub requires_kube_user: bool,
    /// The namespaces a token for the server names, the first as the default. Empty: none.
    pub namespaces: Vec<String>,
    pub hosting: Hosting,
    /// What bounds it beyond its scopes, as the run page shows it.
    pub limits: BTreeMap<String, String>,
}

/// Where a server runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hosting {
    /// Somewhere sandboxes reach over the network.
    Remote,
    /// `buildit mcp` in the loop pod, building in `namespace` and pushing to `push_repo` only.
    Buildit {
        namespace: String,
        push_repo: Option<String>,
        /// Where it fetches the agent issuer's keys, when not from the issuer itself.
        run_jwks_uri: Option<url::Url>,
    },
}

impl ServerSpec {
    /// Whether runs on `cluster` get this server.
    pub fn serves(&self, cluster: &str) -> bool {
        self.clusters.iter().any(|c| c == cluster)
    }

    /// Where the server runs, as the loop pod is told, trusting `issuer`, the controller's agent
    /// issuer.
    pub fn transport(&self, issuer: &str) -> Result<Transport, url::ParseError> {
        Ok(match &self.hosting {
            Hosting::Remote => Transport::Remote,
            Hosting::Buildit {
                namespace,
                push_repo,
                run_jwks_uri,
            } => {
                let jwks = crate::deploy::profile::jwks_uri(issuer, run_jwks_uri.as_ref())?;
                Transport::Local(LocalServer::Buildit(BuilditSite {
                    run_issuer: format!("{issuer}={jwks}"),
                    namespace: namespace.clone(),
                    push_repo: push_repo.clone(),
                }))
            }
        })
    }
}

/// An MCP resource people's clients may get tokens for, on one cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeopleResource {
    /// The URL people's clients reach it at, a person's token's `aud`.
    pub url: String,
    pub cluster: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ServerSpecError {
    #[error(transparent)]
    Name(#[from] NotServerName),
    #[error("agent server {name}'s URL {url:?} is not an http(s) URL with a host")]
    Url { name: ServerName, url: String },
    #[error("an agent server may not be named broker: its gateway provider would be the broker's")]
    Broker,
    #[error("two agent servers are named {0}")]
    DuplicateName(ServerName),
    #[error(
        "people would reach {url} on clusters {first} and {second}; a person's token names its \
         cluster by its URL, so give each cluster its own ([kueue] people_urls for crucible-kueue)"
    )]
    PeopleUrlTwice {
        url: String,
        first: String,
        second: String,
    },
    #[error("[kueue] people_urls names cluster {0}, which is not one of [kueue] clusters")]
    PeopleUrlCluster(String),
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
                scopes: [
                    crucible_contract::kueue::SCOPE_READ,
                    crucible_contract::kueue::SCOPE_SUBMIT,
                ]
                .map(String::from)
                .to_vec(),
                clusters: k.clusters.clone(),
                requires_kube_user: true,
                namespaces,
                hosting: Hosting::Remote,
                limits,
            });
        }
        if let Some(b) = &self.buildit {
            servers.push(ServerSpec {
                name: ServerName::parse("buildit")?,
                url: b.url(),
                scopes: [
                    crucible_contract::buildit::SCOPE_READ,
                    crucible_contract::buildit::SCOPE_BUILD,
                ]
                .map(String::from)
                .to_vec(),
                clusters: b.clusters.clone(),
                requires_kube_user: false,
                namespaces: Vec::new(),
                hosting: Hosting::Buildit {
                    namespace: b.namespace.to_string(),
                    push_repo: b.push_repo.clone(),
                    run_jwks_uri: b.run_jwks_uri.clone(),
                },
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
                scopes: s.scopes.clone(),
                clusters: s.clusters.clone(),
                requires_kube_user: s.requires_kube_user,
                limits: namespace_limit(&namespaces),
                namespaces,
                hosting: Hosting::Remote,
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

impl DeployProfile {
    /// Every resource people's MCP clients may get tokens for: each `[kueue]` cluster's server
    /// under `people = true`, and each `[[agent_servers]]` entry with `people`, on each of its
    /// clusters. No two share a URL.
    pub fn people_resources(&self) -> Result<Vec<PeopleResource>, ServerSpecError> {
        let mut resources = Vec::new();
        if let Some(k) = &self.kueue {
            if let Some(stray) = k.people_urls.keys().find(|c| !k.serves(c)) {
                return Err(ServerSpecError::PeopleUrlCluster(stray.clone()));
            }
            if k.people {
                for cluster in &k.clusters {
                    resources.push(PeopleResource {
                        url: k.people_url(cluster),
                        cluster: cluster.clone(),
                        scopes: [
                            crucible_contract::kueue::SCOPE_READ,
                            crucible_contract::kueue::SCOPE_SUBMIT,
                        ]
                        .map(String::from)
                        .to_vec(),
                    });
                }
            }
        }
        for s in self.agent_servers.iter().filter(|s| s.people) {
            for cluster in &s.clusters {
                resources.push(PeopleResource {
                    url: s.url.to_string(),
                    cluster: cluster.clone(),
                    scopes: s.scopes.clone(),
                });
            }
        }
        for (i, a) in resources.iter().enumerate() {
            if let Some(b) = resources[i + 1..].iter().find(|b| b.url == a.url) {
                return Err(ServerSpecError::PeopleUrlTwice {
                    url: a.url.clone(),
                    first: a.cluster.clone(),
                    second: b.cluster.clone(),
                });
            }
        }
        Ok(resources)
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
        clusters = ["ocp", "k8s"]
        namespace = "kueue-server"
        workload_namespaces = ["team-a", "team-b"]
        kube_authz = "openshift"
        pull_secrets = ["quay-pull"]
        [buildit]
        cluster = "ocp"
        namespace = "builds"
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
                    scopes: vec!["kueue.read".into(), "kueue.submit".into()],
                    clusters: vec!["ocp".into(), "k8s".into()],
                    requires_kube_user: true,
                    namespaces: vec!["team-a".into(), "team-b".into()],
                    hosting: Hosting::Remote,
                    limits: BTreeMap::from([
                        ("namespace".into(), "team-a,team-b".into()),
                        ("pull_secrets".into(), "quay-pull".into()),
                    ]),
                },
                ServerSpec {
                    name: name("buildit"),
                    url: crucible_contract::buildit::URL.into(),
                    scopes: vec!["buildit.read".into(), "buildit.build".into()],
                    clusters: vec!["ocp".into()],
                    requires_kube_user: false,
                    namespaces: vec![],
                    hosting: Hosting::Buildit {
                        namespace: "builds".into(),
                        push_repo: Some("quay.io/team/agent".into()),
                        run_jwks_uri: None,
                    },
                    limits: BTreeMap::from([("push_repo".into(), "quay.io/team/agent".into())]),
                },
            ]
        );
        assert!(servers[0].serves("k8s") && !servers[1].serves("k8s"));
    }

    /// buildit trusts the controller's agent issuer, which the controller supplies: the profile
    /// names it nowhere, and a key set route overrides only where its keys are fetched.
    #[test]
    fn buildit_trusts_the_issuer_it_is_handed() {
        let issuer = "https://crucible.hub.example";
        let servers = profile(KUEUE_AND_BUILDIT).agent_servers().unwrap();
        assert_eq!(servers[0].transport(issuer).unwrap(), Transport::Remote);
        assert_eq!(
            servers[1].transport(issuer).unwrap(),
            Transport::Local(LocalServer::Buildit(BuilditSite {
                run_issuer: format!("{issuer}={issuer}/.well-known/jwks.json"),
                namespace: "builds".into(),
                push_repo: Some("quay.io/team/agent".into()),
            }))
        );
        let routed = profile(&KUEUE_AND_BUILDIT.replace(
            "push_repo = ",
            "run_jwks_uri = \"https://route.example/jwks.json\"\npush_repo = ",
        ))
        .agent_servers()
        .unwrap();
        let Transport::Local(LocalServer::Buildit(site)) = routed[1].transport(issuer).unwrap()
        else {
            panic!("buildit runs in the loop pod")
        };
        assert_eq!(
            site.run_issuer,
            format!("{issuer}=https://route.example/jwks.json")
        );
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
            scopes = ["vault.read"]
            clusters = ["ocp"]
            requires_kube_user = true
            namespaces = ["team-a"]
            [[agent_servers]]
            name = "docs"
            url = "http://docs.example/mcp"
            scopes = []
            "#,
        )
        .agent_servers()
        .unwrap();
        assert_eq!(
            servers[0],
            ServerSpec {
                name: name("vault-mcp"),
                url: "https://vault-mcp.tools.svc:8443/mcp".into(),
                scopes: vec!["vault.read".into()],
                clusters: vec!["ocp".into()],
                requires_kube_user: true,
                namespaces: vec!["team-a".into()],
                hosting: Hosting::Remote,
                limits: BTreeMap::from([("namespace".into(), "team-a".into())]),
            }
        );
        assert_eq!(servers[1].clusters, ["hub"]);
        assert!(!servers[1].requires_kube_user && servers[1].namespaces.is_empty());
    }

    #[test]
    fn names_are_unique_and_no_two_servers_share_a_url_on_a_cluster() {
        let twice = profile(&format!(
            "{KUEUE_AND_BUILDIT}[[agent_servers]]\nname = \"kueue\"\nurl = \"http://x/mcp\"\nscopes = []\n"
        ));
        assert!(matches!(
            twice.agent_servers(),
            Err(ServerSpecError::DuplicateName(n)) if n.as_str() == "kueue"
        ));
        let shared = profile(&format!(
            "{KUEUE_AND_BUILDIT}[[agent_servers]]\nname = \"other\"\ncluster = \"k8s\"\n\
             url = \"http://crucible-kueue.kueue-server.svc.cluster.local:8850/mcp\"\n"
        ));
        assert!(matches!(
            shared.agent_servers(),
            Err(ServerSpecError::DuplicateUrl { cluster, .. }) if cluster == "k8s"
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
        let bad_name = format!(
            "{BASE}[[agent_servers]]\nname = \"Docs\"\nurl = \"http://x/mcp\"\nscopes = []\n"
        );
        assert!(toml::from_str::<DeployProfile>(&bad_name).is_err());
    }

    /// People reach crucible-kueue on each of its clusters at its Service URL, or where
    /// `people_urls` says; two clusters at one URL would be one resource, so that is refused.
    #[test]
    fn people_resources_come_from_the_tables_that_serve_people() {
        assert!(
            profile(KUEUE_AND_BUILDIT)
                .people_resources()
                .unwrap()
                .is_empty()
        );
        let one = profile(&KUEUE_AND_BUILDIT.replace(
            "clusters = [\"ocp\", \"k8s\"]",
            "clusters = [\"ocp\"]\npeople = true",
        ));
        assert_eq!(
            one.people_resources().unwrap(),
            [PeopleResource {
                url: "http://crucible-kueue.kueue-server.svc.cluster.local:8850/mcp".into(),
                cluster: "ocp".into(),
                scopes: vec!["kueue.read".into(), "kueue.submit".into()],
            }]
        );
        let both = KUEUE_AND_BUILDIT.replace(
            "clusters = [\"ocp\", \"k8s\"]",
            "clusters = [\"ocp\", \"k8s\"]\npeople = true",
        );
        assert!(matches!(
            profile(&both).people_resources(),
            Err(ServerSpecError::PeopleUrlTwice { .. })
        ));
        let routed = profile(&both.replace(
            "people = true",
            "people = true\npeople_urls = { ocp = \"https://kueue.ocp.example/mcp\" }",
        ));
        let urls: Vec<(String, String)> = routed
            .people_resources()
            .unwrap()
            .into_iter()
            .map(|r| (r.cluster, r.url))
            .collect();
        assert_eq!(
            urls,
            [
                ("ocp".into(), "https://kueue.ocp.example/mcp".into()),
                (
                    "k8s".into(),
                    "http://crucible-kueue.kueue-server.svc.cluster.local:8850/mcp".into()
                ),
            ]
        );
        assert!(matches!(
            profile(&both.replace(
                "people = true",
                "people = true\npeople_urls = { east = \"https://kueue.east.example/mcp\" }",
            ))
            .people_resources(),
            Err(ServerSpecError::PeopleUrlCluster(c)) if c == "east"
        ));
        let docs = profile(
            "[[agent_servers]]\nname = \"docs\"\nurl = \"https://docs.example/mcp\"\n\
             scopes = [\"docs.read\"]\npeople = true\n",
        );
        assert_eq!(
            docs.people_resources().unwrap(),
            [PeopleResource {
                url: "https://docs.example/mcp".into(),
                cluster: "hub".into(),
                scopes: vec!["docs.read".into()],
            }]
        );
    }
}
