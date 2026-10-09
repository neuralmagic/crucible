//! The run's crucible-kueue access as the loop pod receives it from the controller: the server's
//! MCP URL and the controller's token endpoint. The sandbox's supervisor exchanges its JWT-SVID at
//! that endpoint and injects the access token on egress to the server alone, so no token reaches
//! the loop pod or the sandbox.

use crate::openshell::provider;
use crucible_contract::kueue::ENV_KUEUE_URL;
use crucible_contract::sandbox::{ENV_AGENT_SVID_AUDIENCE, ENV_AGENT_TOKEN_ENDPOINT};

/// The `.mcp.json` server name the agent sees the crucible-kueue tools under.
pub const MCP_SERVER_NAME: &str = "kueue";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KueueAccessError {
    #[error("{set} is set but {missing} is not; the controller sets all three or none")]
    Partial {
        set: &'static str,
        missing: &'static str,
    },
    #[error("{ENV_KUEUE_URL} {url:?} is not an http(s) URL with a host: {reason}")]
    Url { url: String, reason: String },
}

#[derive(Debug)]
pub struct KueueAccess {
    url: String,
    host: String,
    port: u16,
    /// The controller's token endpoint.
    endpoint: String,
    /// The audience the supervisor requests its JWT-SVID for: the controller's issuer.
    svid_audience: String,
}

impl KueueAccess {
    /// The access from the loop pod's environment; `None` when this run has none.
    pub fn from_env() -> Result<Option<Self>, KueueAccessError> {
        let var = |k| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self::from_values(
            var(ENV_KUEUE_URL),
            var(ENV_AGENT_TOKEN_ENDPOINT),
            var(ENV_AGENT_SVID_AUDIENCE),
        )
    }

    fn from_values(
        url: Option<String>,
        endpoint: Option<String>,
        svid_audience: Option<String>,
    ) -> Result<Option<Self>, KueueAccessError> {
        let (url, endpoint, svid_audience) = match (url, endpoint, svid_audience) {
            (None, None, None) => return Ok(None),
            (Some(u), Some(e), Some(a)) => (u, e, a),
            (u, e, a) => {
                let named = [
                    (ENV_KUEUE_URL, u.is_some()),
                    (ENV_AGENT_TOKEN_ENDPOINT, e.is_some()),
                    (ENV_AGENT_SVID_AUDIENCE, a.is_some()),
                ];
                let set = named.iter().find(|(_, s)| *s).map(|(n, _)| *n);
                let missing = named.iter().find(|(_, s)| !*s).map(|(n, _)| *n);
                return match (set, missing) {
                    (Some(set), Some(missing)) => Err(KueueAccessError::Partial { set, missing }),
                    _ => Ok(None),
                };
            }
        };
        let bad = |reason: &str| KueueAccessError::Url {
            url: url.clone(),
            reason: reason.to_string(),
        };
        let parsed = url::Url::parse(&url).map_err(|e| bad(&e.to_string()))?;
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
        Ok(Some(Self {
            url,
            host,
            port,
            endpoint,
            svid_audience,
        }))
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// The sandbox egress entry: HTTP-inspected, so the supervisor injects the token, with every
    /// request allowed. crucible-kueue decides each call itself.
    pub fn egress_endpoint(&self) -> String {
        format!("{}:{}:full:rest", self.host, self.port)
    }

    /// The token-grant provider profile for this server. Its endpoint binds the injected token to
    /// the server alone.
    pub fn token_grant_profile(&self) -> openshell_core::proto::ProviderProfile {
        provider::kueue_token_grant_profile(
            &self.host,
            self.port,
            &self.url,
            &self.endpoint,
            &self.svid_audience,
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::openshell::kueue::*;

    const URL: &str = "http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp";
    const ENDPOINT: &str = "http://crucible-controller.crucible-system.svc:8080/oauth/token";
    const ISSUER: &str = "http://crucible-controller.crucible-system.svc:8080";

    fn values(
        url: Option<&str>,
        endpoint: Option<&str>,
        audience: Option<&str>,
    ) -> Result<Option<KueueAccess>, KueueAccessError> {
        let own = |v: Option<&str>| v.map(str::to_string);
        KueueAccess::from_values(own(url), own(endpoint), own(audience))
    }

    fn access(url: &str) -> Result<Option<KueueAccess>, KueueAccessError> {
        values(Some(url), Some(ENDPOINT), Some(ISSUER))
    }

    #[test]
    fn no_env_means_no_access() {
        assert!(values(None, None, None).unwrap().is_none());
    }

    #[test]
    fn partial_access_is_refused_naming_a_missing_variable() {
        assert_eq!(
            values(Some(URL), None, None).unwrap_err(),
            KueueAccessError::Partial {
                set: ENV_KUEUE_URL,
                missing: ENV_AGENT_TOKEN_ENDPOINT
            }
        );
        assert_eq!(
            values(Some(URL), Some(ENDPOINT), None).unwrap_err(),
            KueueAccessError::Partial {
                set: ENV_KUEUE_URL,
                missing: ENV_AGENT_SVID_AUDIENCE
            }
        );
        assert_eq!(
            values(None, Some(ENDPOINT), Some(ISSUER)).unwrap_err(),
            KueueAccessError::Partial {
                set: ENV_AGENT_TOKEN_ENDPOINT,
                missing: ENV_KUEUE_URL
            }
        );
    }

    #[test]
    fn the_token_grant_is_bound_to_the_server_and_holds_no_secret() {
        let a = access(URL).unwrap().unwrap();
        assert_eq!(
            a.egress_endpoint(),
            "crucible-kueue.crucible-system.svc.cluster.local:8850:full:rest"
        );
        let profile = a.token_grant_profile();
        assert_eq!(profile.id, provider::KUEUE_GRANT_PROFILE_ID);
        let endpoint = &profile.endpoints[0];
        assert_eq!(
            (endpoint.host.as_str(), endpoint.port),
            ("crucible-kueue.crucible-system.svc.cluster.local", 8850)
        );
        let grant = profile.credentials[0]
            .token_grant
            .as_ref()
            .expect("token grant");
        assert_eq!(grant.token_endpoint, ENDPOINT);
        assert_eq!(grant.jwt_svid_audience, ISSUER);
        assert_eq!(grant.audience, URL, "the token is for this server");
        assert_eq!(profile.credentials[0].auth_style, "bearer");
        assert_eq!(profile.credentials[0].header_name, "authorization");
        assert_eq!(endpoint.protocol, "rest", "as the sandbox policy's entry");
        assert_eq!(
            endpoint.access,
            openshell_core::proto::NetworkAccessPreset::Full as i32
        );
        assert!(profile.credentials[0].env_vars.is_empty(), "no placeholder");
    }

    #[test]
    fn a_url_without_a_port_takes_the_scheme_default() {
        let a = access("https://kueue.example.com/mcp").unwrap().unwrap();
        assert_eq!(a.egress_endpoint(), "kueue.example.com:443:full:rest");
    }

    #[test]
    fn a_non_http_url_is_refused() {
        assert!(matches!(
            access("ftp://h/mcp"),
            Err(KueueAccessError::Url { .. })
        ));
        assert!(matches!(
            access("not a url"),
            Err(KueueAccessError::Url { .. })
        ));
    }
}
