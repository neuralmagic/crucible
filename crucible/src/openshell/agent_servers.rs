//! The run's MCP servers that take agent tokens, as the loop pod receives them from the controller
//! ([`ENV_AGENT_SERVERS`]) with its token endpoint. Every server becomes a token-grant provider, an
//! egress entry and a `.mcp.json` entry the same way; one served by a process in the loop pod is
//! started first. The sandbox's supervisor exchanges its JWT-SVID at the endpoint and injects the
//! token on egress to that server alone, so no token reaches the loop pod or the sandbox. No
//! servers, nothing: no provider, no egress entry, no `.mcp.json` entry.

use crate::openshell::{buildit, provider};
use anyhow::Result;
use crucible_contract::agent_servers::{AgentServer, ENV_AGENT_SERVERS, LocalServer, Transport};
use crucible_contract::sandbox::{ENV_AGENT_SVID_AUDIENCE, ENV_AGENT_TOKEN_ENDPOINT};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "the controller granted MCP server {name:?}, but this turn's {what} has that name too; \
     rename one so `.mcp.json` and the gateway providers keep them apart"
)]
pub struct NameClash {
    pub name: String,
    pub what: &'static str,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentServersError {
    #[error("{ENV_AGENT_SERVERS} names servers but {missing} is not set; the controller sets both")]
    Partial { missing: &'static str },
    #[error("{ENV_AGENT_SERVERS} is not a list of servers: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("server {name}'s URL {url:?} is not an http(s) URL with a host and port: {reason}")]
    Url {
        name: String,
        url: String,
        reason: String,
    },
}

/// One granted server, with the host and port its URL names.
#[derive(Debug)]
pub struct Granted {
    pub server: AgentServer,
    host: String,
    port: u16,
}

impl Granted {
    fn new(server: AgentServer) -> Result<Self, AgentServersError> {
        let bad = |reason: &str| AgentServersError::Url {
            name: server.name.to_string(),
            url: server.url.clone(),
            reason: reason.to_string(),
        };
        let parsed = url::Url::parse(&server.url).map_err(|e| bad(&e.to_string()))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(bad("the scheme must be http or https"));
        }
        let host = parsed
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| bad("it has no host"))?
            .to_string();
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| bad("it has no port"))?;
        Ok(Self { server, host, port })
    }

    /// The gateway provider carrying this server's grant.
    pub fn provider_name(&self) -> String {
        format!("crucible-{}", self.server.name)
    }

    /// The sandbox egress entry: HTTP-inspected, so the supervisor injects the token, with every
    /// request allowed. The server decides each call itself.
    pub fn egress_endpoint(&self) -> String {
        format!("{}:{}:full:rest", self.host, self.port)
    }

    /// Start the server in the loop pod when it runs there, and wait until it listens.
    pub fn start_local(&self) -> Result<()> {
        match &self.server.transport {
            Transport::Remote => Ok(()),
            Transport::Local(LocalServer::Buildit(site)) => buildit::Buildit {
                site,
                url: &self.server.url,
                host: &self.host,
                port: self.port,
            }
            .ensure_running(),
        }
    }
}

/// The run's servers and where their supervisors get tokens.
#[derive(Debug)]
pub struct AgentServers {
    servers: Vec<Granted>,
    endpoint: String,
    svid_audience: String,
}

impl AgentServers {
    /// The servers from the loop pod's environment; none when the controller granted none.
    pub fn from_env() -> Result<Self, AgentServersError> {
        let var = |k| std::env::var(k).ok().filter(|v: &String| !v.is_empty());
        Self::from_values(
            var(ENV_AGENT_SERVERS),
            var(ENV_AGENT_TOKEN_ENDPOINT),
            var(ENV_AGENT_SVID_AUDIENCE),
        )
    }

    fn from_values(
        servers: Option<String>,
        endpoint: Option<String>,
        svid_audience: Option<String>,
    ) -> Result<Self, AgentServersError> {
        let servers = crucible_contract::agent_servers::decode(servers.as_deref().unwrap_or(""))?;
        if servers.is_empty() {
            return Ok(Self {
                servers: Vec::new(),
                endpoint: endpoint.unwrap_or_default(),
                svid_audience: svid_audience.unwrap_or_default(),
            });
        }
        let endpoint = endpoint.ok_or(AgentServersError::Partial {
            missing: ENV_AGENT_TOKEN_ENDPOINT,
        })?;
        let svid_audience = svid_audience.ok_or(AgentServersError::Partial {
            missing: ENV_AGENT_SVID_AUDIENCE,
        })?;
        Ok(Self {
            servers: servers
                .into_iter()
                .map(Granted::new)
                .collect::<Result<_, _>>()?,
            endpoint,
            svid_audience,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Granted> {
        self.servers.iter()
    }

    /// Refuse a granted server named like the turn's broker or one of its `[mcp]` servers: their
    /// `.mcp.json` keys would collide, and a server named `broker` would share the broker's
    /// gateway provider.
    pub fn check_names<'a>(
        &self,
        broker: Option<&str>,
        mcp: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), NameClash> {
        let mcp: Vec<&str> = mcp.into_iter().collect();
        for g in &self.servers {
            let name = g.server.name.as_str();
            let clash = |what| NameClash {
                name: name.to_string(),
                what,
            };
            if name == "broker" || broker == Some(name) {
                return Err(clash("provisioning broker"));
            }
            if mcp.contains(&name) {
                return Err(clash("[mcp] server"));
            }
        }
        Ok(())
    }

    /// The token-grant provider profile for `granted`. Its endpoint binds the injected token to
    /// that server alone.
    pub fn token_grant_profile(&self, granted: &Granted) -> openshell_core::proto::ProviderProfile {
        let name = granted.server.name.as_str();
        provider::token_grant_profile(&provider::GrantedServer {
            profile_id: &format!("{}-grant", granted.provider_name()),
            name,
            host: &granted.host,
            port: granted.port,
            url: &granted.server.url,
            token_endpoint: &self.endpoint,
            svid_audience: &self.svid_audience,
            scopes: &granted.server.scopes,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::openshell::agent_servers::*;
    use crucible_contract::agent_servers::{BuilditSite, ServerName};

    const ENDPOINT: &str = "http://crucible-controller.crucible-system.svc:8080/oauth/token";
    const ISSUER: &str = "http://crucible-controller.crucible-system.svc:8080";
    const KUEUE: &str = "http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp";

    fn server(name: &str, url: &str, transport: Transport) -> AgentServer {
        AgentServer {
            name: ServerName::parse(name).unwrap(),
            url: url.into(),
            scopes: vec![format!("{name}.read")],
            transport,
        }
    }

    fn granted(servers: &[AgentServer]) -> AgentServers {
        AgentServers::from_values(
            Some(crucible_contract::agent_servers::encode(servers).unwrap()),
            Some(ENDPOINT.into()),
            Some(ISSUER.into()),
        )
        .unwrap()
    }

    #[test]
    fn no_servers_grant_nothing() {
        for servers in [None, Some(String::new()), Some("[]".to_string())] {
            for (endpoint, audience) in [(None, None), (Some(ENDPOINT.into()), Some(ISSUER.into()))]
            {
                let none = AgentServers::from_values(servers.clone(), endpoint, audience).unwrap();
                assert!(none.is_empty());
                assert_eq!(none.iter().count(), 0);
            }
        }
    }

    #[test]
    fn servers_need_the_token_endpoint_and_the_svid_audience() {
        let raw =
            crucible_contract::agent_servers::encode(&[server("kueue", KUEUE, Transport::Remote)])
                .unwrap();
        assert!(matches!(
            AgentServers::from_values(Some(raw.clone()), None, Some(ISSUER.into())),
            Err(AgentServersError::Partial {
                missing: ENV_AGENT_TOKEN_ENDPOINT
            })
        ));
        assert!(matches!(
            AgentServers::from_values(Some(raw), Some(ENDPOINT.into()), None),
            Err(AgentServersError::Partial {
                missing: ENV_AGENT_SVID_AUDIENCE
            })
        ));
        assert!(matches!(
            AgentServers::from_values(Some("{".into()), None, None),
            Err(AgentServersError::Decode(_))
        ));
        for bad in ["ftp://x/mcp", "http://:8080/mcp", "not a url"] {
            let raw =
                crucible_contract::agent_servers::encode(&[server("x", bad, Transport::Remote)])
                    .unwrap();
            assert!(
                matches!(
                    AgentServers::from_values(
                        Some(raw),
                        Some(ENDPOINT.into()),
                        Some(ISSUER.into())
                    ),
                    Err(AgentServersError::Url { .. })
                ),
                "{bad}"
            );
        }
    }

    /// A remote server and one in the loop pod get the same provider, egress and grant shape.
    #[test]
    fn every_server_gets_a_grant_bound_to_its_own_host_with_its_own_scopes() {
        let buildit = Transport::Local(LocalServer::Buildit(BuilditSite {
            run_issuer: format!("{ISSUER}={ISSUER}/.well-known/jwks.json"),
            namespace: "builds".into(),
            push_repo: None,
        }));
        let servers = granted(&[
            server("kueue", KUEUE, Transport::Remote),
            server(
                "buildit",
                "http://host.openshell.internal:8848/mcp",
                buildit,
            ),
            server("docs", "https://docs.example/mcp", Transport::Remote),
        ]);
        let rows: Vec<(String, String)> = servers
            .iter()
            .map(|g| (g.provider_name(), g.egress_endpoint()))
            .collect();
        assert_eq!(
            rows,
            [
                (
                    "crucible-kueue".to_string(),
                    "crucible-kueue.crucible-system.svc.cluster.local:8850:full:rest".to_string()
                ),
                (
                    "crucible-buildit".to_string(),
                    "host.openshell.internal:8848:full:rest".to_string()
                ),
                (
                    "crucible-docs".to_string(),
                    "docs.example:443:full:rest".to_string()
                ),
            ]
        );
        for g in servers.iter() {
            let profile = servers.token_grant_profile(g);
            assert_eq!(profile.id, format!("crucible-{}-grant", g.server.name));
            let endpoint = &profile.endpoints[0];
            assert_eq!(
                format!(
                    "{}:{}:full:{}",
                    endpoint.host, endpoint.port, endpoint.protocol
                ),
                g.egress_endpoint()
            );
            let grant = profile.credentials[0].token_grant.as_ref().unwrap();
            assert_eq!(grant.audience, g.server.url);
            assert_eq!(grant.token_endpoint, ENDPOINT);
            assert_eq!(grant.jwt_svid_audience, ISSUER);
            assert_eq!(grant.scopes, g.server.scopes);
            assert!(profile.credentials[0].env_vars.is_empty(), "no placeholder");
        }
    }

    #[test]
    fn a_granted_server_may_not_share_a_name_with_the_broker_or_a_pack_server() {
        let servers = granted(&[
            server("kueue", KUEUE, Transport::Remote),
            server("docs", "https://docs.example/mcp", Transport::Remote),
        ]);
        assert_eq!(servers.check_names(Some("crucible"), ["notes"]), Ok(()));
        assert_eq!(
            servers.check_names(Some("docs"), []),
            Err(NameClash {
                name: "docs".into(),
                what: "provisioning broker"
            })
        );
        assert_eq!(
            servers.check_names(None, ["notes", "kueue"]),
            Err(NameClash {
                name: "kueue".into(),
                what: "[mcp] server"
            })
        );
        let broker = granted(&[server("broker", "https://b.example/mcp", Transport::Remote)]);
        assert!(
            broker.check_names(None, []).is_err(),
            "crucible-broker is the broker's provider"
        );
    }

    #[test]
    fn a_remote_server_starts_nothing() {
        let servers = granted(&[server("kueue", KUEUE, Transport::Remote)]);
        for g in servers.iter() {
            g.start_local().unwrap();
        }
    }
}
