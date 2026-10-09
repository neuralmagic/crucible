//! The run's crucible-kueue access as the loop pod receives it from the controller: the server's
//! MCP URL and how the sandbox authenticates to it. Under a token grant the sandbox's supervisor
//! exchanges its JWT-SVID at the controller's token endpoint and injects the access token on egress
//! to the server alone; under a static grant the raw per-run bearer token goes only to the
//! gateway's provider store and the sandbox sees a placeholder resolved for the server alone.

use crate::openshell::grpc::EndpointCredentialBinding;
use crate::openshell::provider;
use crucible_contract::kueue::{ENV_KUEUE_TOKEN, ENV_KUEUE_URL};
use crucible_contract::sandbox::{ENV_AGENT_SVID_AUDIENCE, ENV_AGENT_TOKEN_ENDPOINT};
use std::fmt;

/// The `.mcp.json` server name the agent sees the crucible-kueue tools under.
pub const MCP_SERVER_NAME: &str = "kueue";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KueueAccessError {
    #[error("{set} is set but {missing} is not; the controller sets both or neither")]
    Partial {
        set: &'static str,
        missing: &'static str,
    },
    #[error(
        "both {ENV_KUEUE_TOKEN} and {ENV_AGENT_TOKEN_ENDPOINT} are set; the controller grants one \
         way or the other"
    )]
    Ambiguous,
    #[error("{ENV_KUEUE_URL} {url:?} is not an http(s) URL with a host: {reason}")]
    Url { url: String, reason: String },
}

/// How the sandbox authenticates to crucible-kueue.
pub enum KueueCredential {
    /// The run's static bearer token.
    Static(String),
    /// A token the supervisor obtains from `endpoint` with a JWT-SVID for `svid_audience`.
    TokenGrant {
        endpoint: String,
        svid_audience: String,
    },
}

pub struct KueueAccess {
    url: String,
    host: String,
    port: u16,
    credential: KueueCredential,
}

impl fmt::Debug for KueueAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let credential = match &self.credential {
            KueueCredential::Static(_) => "static <redacted>".to_string(),
            KueueCredential::TokenGrant { endpoint, .. } => format!("token grant at {endpoint}"),
        };
        f.debug_struct("KueueAccess")
            .field("url", &self.url)
            .field("credential", &credential)
            .finish()
    }
}

impl KueueAccess {
    /// The access from the loop pod's environment; `None` when this run has none.
    pub fn from_env() -> Result<Option<Self>, KueueAccessError> {
        let var = |k| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self::from_values(
            var(ENV_KUEUE_URL),
            var(ENV_KUEUE_TOKEN),
            var(ENV_AGENT_TOKEN_ENDPOINT),
            var(ENV_AGENT_SVID_AUDIENCE),
        )
    }

    fn from_values(
        url: Option<String>,
        token: Option<String>,
        endpoint: Option<String>,
        svid_audience: Option<String>,
    ) -> Result<Option<Self>, KueueAccessError> {
        let partial = |set, missing| Err(KueueAccessError::Partial { set, missing });
        let grant = match (endpoint, svid_audience) {
            (Some(endpoint), Some(svid_audience)) => Some(KueueCredential::TokenGrant {
                endpoint,
                svid_audience,
            }),
            (Some(_), None) => return partial(ENV_AGENT_TOKEN_ENDPOINT, ENV_AGENT_SVID_AUDIENCE),
            (None, Some(_)) => return partial(ENV_AGENT_SVID_AUDIENCE, ENV_AGENT_TOKEN_ENDPOINT),
            (None, None) => None,
        };
        let credential = match (token, grant) {
            (Some(_), Some(_)) => return Err(KueueAccessError::Ambiguous),
            (Some(token), None) => Some(KueueCredential::Static(token)),
            (None, grant) => grant,
        };
        let (url, credential) = match (url, credential) {
            (None, None) => return Ok(None),
            (Some(_), None) => return partial(ENV_KUEUE_URL, ENV_KUEUE_TOKEN),
            (None, Some(KueueCredential::Static(_))) => {
                return partial(ENV_KUEUE_TOKEN, ENV_KUEUE_URL);
            }
            (None, Some(KueueCredential::TokenGrant { .. })) => {
                return partial(ENV_AGENT_TOKEN_ENDPOINT, ENV_KUEUE_URL);
            }
            (Some(url), Some(credential)) => (url, credential),
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
            credential,
        }))
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn credential(&self) -> &KueueCredential {
        &self.credential
    }

    /// The sandbox egress entry: MCP-inspected, every known MCP method allowed.
    pub fn egress_endpoint(&self) -> String {
        format!("{}:{}:full:mcp", self.host, self.port)
    }

    /// Binds a static grant's placeholder to the server's endpoint and nowhere else. A token grant
    /// is bound by its profile's endpoint instead, so it needs none.
    pub fn credential_binding(&self) -> Option<EndpointCredentialBinding> {
        matches!(self.credential, KueueCredential::Static(_)).then(|| EndpointCredentialBinding {
            host: self.host.clone(),
            port: u32::from(self.port),
            provider: provider::KUEUE_PROVIDER_NAME.to_string(),
        })
    }

    /// The token-grant provider profile for this server, `None` for a static grant.
    pub fn token_grant_profile(&self) -> Option<openshell_core::proto::ProviderProfile> {
        match &self.credential {
            KueueCredential::Static(_) => None,
            KueueCredential::TokenGrant {
                endpoint,
                svid_audience,
            } => Some(provider::kueue_token_grant_profile(
                &self.host,
                self.port,
                &self.url,
                endpoint,
                svid_audience,
            )),
        }
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
        token: Option<&str>,
        endpoint: Option<&str>,
        audience: Option<&str>,
    ) -> Result<Option<KueueAccess>, KueueAccessError> {
        let own = |v: Option<&str>| v.map(str::to_string);
        KueueAccess::from_values(own(url), own(token), own(endpoint), own(audience))
    }

    fn access(url: &str) -> Result<Option<KueueAccess>, KueueAccessError> {
        values(Some(url), Some("tok"), None, None)
    }

    #[test]
    fn no_env_means_no_grant() {
        assert!(values(None, None, None, None).unwrap().is_none());
    }

    #[test]
    fn half_a_grant_is_refused_naming_the_missing_variable() {
        assert_eq!(
            values(Some(URL), None, None, None).unwrap_err(),
            KueueAccessError::Partial {
                set: ENV_KUEUE_URL,
                missing: ENV_KUEUE_TOKEN
            }
        );
        assert!(
            values(None, Some("t"), None, None)
                .unwrap_err()
                .to_string()
                .contains(ENV_KUEUE_URL)
        );
        assert_eq!(
            values(Some(URL), None, Some(ENDPOINT), None).unwrap_err(),
            KueueAccessError::Partial {
                set: ENV_AGENT_TOKEN_ENDPOINT,
                missing: ENV_AGENT_SVID_AUDIENCE
            }
        );
        assert_eq!(
            values(None, None, Some(ENDPOINT), Some(ISSUER)).unwrap_err(),
            KueueAccessError::Partial {
                set: ENV_AGENT_TOKEN_ENDPOINT,
                missing: ENV_KUEUE_URL
            }
        );
        assert_eq!(
            values(Some(URL), Some("t"), Some(ENDPOINT), Some(ISSUER)).unwrap_err(),
            KueueAccessError::Ambiguous
        );
    }

    #[test]
    fn the_service_url_becomes_an_mcp_egress_entry_bound_to_the_kueue_provider() {
        let a = access(URL).unwrap().unwrap();
        assert_eq!(
            a.egress_endpoint(),
            "crucible-kueue.crucible-system.svc.cluster.local:8850:full:mcp"
        );
        let b = a
            .credential_binding()
            .expect("a static grant binds its placeholder");
        assert_eq!(b.host, "crucible-kueue.crucible-system.svc.cluster.local");
        assert_eq!(b.port, 8850);
        assert_eq!(b.provider, "crucible-kueue");
        assert!(a.token_grant_profile().is_none());
    }

    #[test]
    fn a_token_grant_binds_through_its_profile_and_holds_no_secret() {
        let a = values(Some(URL), None, Some(ENDPOINT), Some(ISSUER))
            .unwrap()
            .unwrap();
        assert!(a.credential_binding().is_none());
        let profile = a.token_grant_profile().expect("a token-grant profile");
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
        assert!(profile.credentials[0].env_vars.is_empty(), "no placeholder");
        let shown = format!("{a:?}");
        assert!(shown.contains("token grant"), "{shown}");
    }

    #[test]
    fn a_url_without_a_port_takes_the_scheme_default() {
        let a = access("https://kueue.example.com/mcp").unwrap().unwrap();
        assert_eq!(a.egress_endpoint(), "kueue.example.com:443:full:mcp");
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

    #[test]
    fn debug_never_prints_the_token() {
        let a = access("http://h:1/mcp").unwrap().unwrap();
        let shown = format!("{a:?}");
        assert!(!shown.contains("tok\""), "{shown}");
        assert!(shown.contains("<redacted>"));
    }
}
