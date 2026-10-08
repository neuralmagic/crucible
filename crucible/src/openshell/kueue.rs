//! The run's crucible-kueue grant as the loop pod receives it from the controller: the server's
//! MCP URL and the raw per-run bearer token. The token goes only to the gateway's provider store;
//! the sandbox sees a placeholder that the proxy resolves for the server's endpoint alone.

use crate::openshell::grpc::EndpointCredentialBinding;
use crate::openshell::provider;
use crucible_contract::kueue::{ENV_KUEUE_TOKEN, ENV_KUEUE_URL};
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
    #[error("{ENV_KUEUE_URL} {url:?} is not an http(s) URL with a host: {reason}")]
    Url { url: String, reason: String },
}

pub struct KueueAccess {
    url: String,
    host: String,
    port: u16,
    token: String,
}

impl fmt::Debug for KueueAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KueueAccess")
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl KueueAccess {
    /// The grant from the loop pod's environment; `None` when this run has none.
    pub fn from_env() -> Result<Option<Self>, KueueAccessError> {
        let var = |k| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self::from_values(var(ENV_KUEUE_URL), var(ENV_KUEUE_TOKEN))
    }

    fn from_values(
        url: Option<String>,
        token: Option<String>,
    ) -> Result<Option<Self>, KueueAccessError> {
        let (url, token) = match (url, token) {
            (None, None) => return Ok(None),
            (Some(_), None) => {
                return Err(KueueAccessError::Partial {
                    set: ENV_KUEUE_URL,
                    missing: ENV_KUEUE_TOKEN,
                });
            }
            (None, Some(_)) => {
                return Err(KueueAccessError::Partial {
                    set: ENV_KUEUE_TOKEN,
                    missing: ENV_KUEUE_URL,
                });
            }
            (Some(url), Some(token)) => (url, token),
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
            token,
        }))
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// The sandbox egress entry: MCP-inspected, every known MCP method allowed.
    pub fn egress_endpoint(&self) -> String {
        format!("{}:{}:full:mcp", self.host, self.port)
    }

    /// Binds the `crucible-kueue` provider's credential to the server's endpoint and nowhere else.
    pub fn credential_binding(&self) -> EndpointCredentialBinding {
        EndpointCredentialBinding {
            host: self.host.clone(),
            port: u32::from(self.port),
            provider: provider::KUEUE_PROVIDER_NAME.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::openshell::kueue::*;

    fn access(url: &str) -> Result<Option<KueueAccess>, KueueAccessError> {
        KueueAccess::from_values(Some(url.to_string()), Some("tok".to_string()))
    }

    #[test]
    fn no_env_means_no_grant() {
        assert!(KueueAccess::from_values(None, None).unwrap().is_none());
    }

    #[test]
    fn half_a_grant_is_refused_naming_the_missing_variable() {
        let err = KueueAccess::from_values(Some("http://h:1/mcp".into()), None).unwrap_err();
        assert_eq!(
            err,
            KueueAccessError::Partial {
                set: ENV_KUEUE_URL,
                missing: ENV_KUEUE_TOKEN
            }
        );
        let err = KueueAccess::from_values(None, Some("t".into())).unwrap_err();
        assert!(err.to_string().contains(ENV_KUEUE_URL));
    }

    #[test]
    fn the_service_url_becomes_an_mcp_egress_entry_bound_to_the_kueue_provider() {
        let a = access("http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp")
            .unwrap()
            .unwrap();
        assert_eq!(
            a.egress_endpoint(),
            "crucible-kueue.crucible-system.svc.cluster.local:8850:full:mcp"
        );
        let b = a.credential_binding();
        assert_eq!(b.host, "crucible-kueue.crucible-system.svc.cluster.local");
        assert_eq!(b.port, 8850);
        assert_eq!(b.provider, "crucible-kueue");
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
