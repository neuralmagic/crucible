//! The controller's OpenShift OAuth client for linking people's usernames on a cluster: the
//! redirect URI the controller sends and the `OAuthClient` the deploy renderer registers must be
//! the same string, so both build it here.

use url::Url;

/// The `OAuthClient` name the controller uses unless configured otherwise.
pub const DEFAULT_CLIENT_ID: &str = "crucible-controller";

/// The only scope the controller asks a cluster for: enough to say who the token belongs to.
pub const SCOPE: &str = "user:info";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExternalUrlError {
    #[error("controller URL {0:?} does not parse: {1}")]
    Parse(String, String),
    #[error("controller URL {0:?} must be https (http only to a loopback host)")]
    NotHttps(String),
    #[error("controller URL {0:?} must carry no query or fragment")]
    QueryOrFragment(String),
    #[error("cluster name {0:?} is not a DNS-1123 label")]
    Cluster(String),
}

/// The controller's external URL, as the browser reaches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalUrl(Url);

impl ExternalUrl {
    pub fn parse(raw: &str) -> Result<Self, ExternalUrlError> {
        let mut url =
            Url::parse(raw).map_err(|e| ExternalUrlError::Parse(raw.into(), e.to_string()))?;
        if !https_or_loopback(&url) {
            return Err(ExternalUrlError::NotHttps(raw.into()));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(ExternalUrlError::QueryOrFragment(raw.into()));
        }
        if !url.path().ends_with('/') {
            let path = format!("{}/", url.path());
            url.set_path(&path);
        }
        Ok(Self(url))
    }

    /// The exact redirect URI for `cluster`, under any path prefix the URL carries.
    pub fn callback(&self, cluster: &str) -> Result<Url, ExternalUrlError> {
        let label = !cluster.is_empty()
            && cluster.len() <= 63
            && cluster
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !cluster.starts_with('-')
            && !cluster.ends_with('-');
        if !label {
            return Err(ExternalUrlError::Cluster(cluster.into()));
        }
        self.0
            .join(&format!("oauth/cluster-link/{cluster}/callback"))
            .map_err(|e| ExternalUrlError::Parse(self.0.to_string(), e.to_string()))
    }
}

/// https, or http to a loopback host.
pub fn https_or_loopback(url: &Url) -> bool {
    let loopback = match url.host() {
        Some(url::Host::Domain(d)) => d == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    url.scheme() == "https" || (url.scheme() == "http" && loopback)
}

#[cfg(test)]
mod tests {
    use crate::cluster_link::*;

    #[test]
    fn callbacks_join_under_the_controller_urls_path() {
        let root = ExternalUrl::parse("https://crucible.example.com").unwrap();
        assert_eq!(
            root.callback("ocp").unwrap().as_str(),
            "https://crucible.example.com/oauth/cluster-link/ocp/callback"
        );
        let prefixed = ExternalUrl::parse("https://h.example/app").unwrap();
        assert_eq!(
            prefixed.callback("hub").unwrap().as_str(),
            "https://h.example/app/oauth/cluster-link/hub/callback"
        );
        assert!(ExternalUrl::parse("http://127.0.0.1:8080").is_ok());
        assert!(ExternalUrl::parse("http://[::1]:8080").is_ok());
    }

    #[test]
    fn a_controller_url_off_https_or_with_a_query_is_refused() {
        for (raw, want) in [
            (
                "http://crucible.example.com",
                ExternalUrlError::NotHttps("http://crucible.example.com".into()),
            ),
            (
                "https://crucible.example.com/?x=1",
                ExternalUrlError::QueryOrFragment("https://crucible.example.com/?x=1".into()),
            ),
            (
                "https://crucible.example.com/#f",
                ExternalUrlError::QueryOrFragment("https://crucible.example.com/#f".into()),
            ),
        ] {
            assert_eq!(ExternalUrl::parse(raw), Err(want));
        }
        assert!(matches!(
            ExternalUrl::parse("not a url"),
            Err(ExternalUrlError::Parse(..))
        ));
    }

    #[test]
    fn a_cluster_name_that_could_leave_the_callback_path_is_refused() {
        let url = ExternalUrl::parse("https://crucible.example.com").unwrap();
        for bad in ["", "../x", "a/b", "Ocp", "-a", "a-", "a?b", &"a".repeat(64)] {
            assert_eq!(
                url.callback(bad),
                Err(ExternalUrlError::Cluster(bad.into())),
                "{bad:?}"
            );
        }
    }
}
