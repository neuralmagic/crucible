//! Signing in to a forge (GitHub, or a GitLab host) through its OAuth device flow, keeping the
//! user token where the app keeps keys, renewing it before it expires, and linking it to a
//! controller.
//!
//! ```text
//! POST <device endpoint> {client_id, scope} ──▶ user_code, verification_uri
//!   person enters the code at verification_uri
//! POST <token endpoint> {device_code} every `interval` ──▶ access + refresh tokens
//! PUT <controller>/api/me/forge-accounts/<forge>/<host> {token} ──▶ the controller records it
//! ```

pub mod device;
pub mod link;
pub mod session;

const GITHUB_HOST: &str = "github.com";
const GITLAB_SCOPE: &str = "api";

/// An account on one forge host.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Account {
    Github,
    Gitlab(String),
}

impl Account {
    pub fn forge(&self) -> &'static str {
        match self {
            Account::Github => "github",
            Account::Gitlab(_) => "gitlab",
        }
    }

    pub fn host(&self) -> &str {
        match self {
            Account::Github => GITHUB_HOST,
            Account::Gitlab(host) => host,
        }
    }

    pub fn name(&self) -> String {
        match self {
            Account::Github => "GitHub".into(),
            Account::Gitlab(host) => format!("GitLab {host}"),
        }
    }

    pub fn store_key(&self) -> String {
        format!("{}:{}", self.forge(), self.host())
    }

    fn device_url(&self) -> String {
        match self {
            Account::Github => "https://github.com/login/device/code".into(),
            Account::Gitlab(host) => format!("https://{host}/oauth/authorize_device"),
        }
    }

    fn token_url(&self) -> String {
        match self {
            Account::Github => "https://github.com/login/oauth/access_token".into(),
            Account::Gitlab(host) => format!("https://{host}/oauth/token"),
        }
    }

    fn revoke_url(&self) -> Option<String> {
        match self {
            Account::Github => None,
            Account::Gitlab(host) => Some(format!("https://{host}/oauth/revoke")),
        }
    }

    fn user_url(&self) -> String {
        match self {
            Account::Github => "https://api.github.com/user".into(),
            Account::Gitlab(host) => format!("https://{host}/api/v4/user"),
        }
    }

    fn scope(&self) -> Option<&'static str> {
        match self {
            Account::Github => None,
            Account::Gitlab(_) => Some(GITLAB_SCOPE),
        }
    }

    fn link_path(&self) -> String {
        format!("/api/me/forge-accounts/{}/{}", self.forge(), self.host())
    }
}

pub fn gitlab_host(raw: &str) -> Option<String> {
    let host = raw.trim().trim_end_matches('/').to_ascii_lowercase();
    let host = host.strip_prefix("https://").unwrap_or(&host).to_string();
    let (name, port) = match host.split_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host.as_str(), None),
    };
    let name_ok = !name.is_empty()
        && name.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        });
    let port_ok = port.is_none_or(|p| p.parse::<u16>().is_ok_and(|p| p > 0));
    (name_ok && port_ok && host != GITHUB_HOST).then_some(host)
}

#[cfg(test)]
mod tests {
    use crate::forge::*;

    #[test]
    fn each_forge_has_its_own_endpoints_and_link() {
        let gh = Account::Github;
        assert_eq!(gh.device_url(), "https://github.com/login/device/code");
        assert_eq!(
            gh.token_url(),
            "https://github.com/login/oauth/access_token"
        );
        assert_eq!(gh.user_url(), "https://api.github.com/user");
        assert_eq!(gh.scope(), None);
        assert_eq!(gh.revoke_url(), None);
        assert_eq!(gh.link_path(), "/api/me/forge-accounts/github/github.com");
        assert_eq!(gh.store_key(), "github:github.com");
        let gl = Account::Gitlab("gitlab.example.com".into());
        assert_eq!(
            gl.device_url(),
            "https://gitlab.example.com/oauth/authorize_device"
        );
        assert_eq!(gl.token_url(), "https://gitlab.example.com/oauth/token");
        assert_eq!(gl.user_url(), "https://gitlab.example.com/api/v4/user");
        assert_eq!(gl.scope(), Some("api"));
        assert_eq!(
            gl.revoke_url().as_deref(),
            Some("https://gitlab.example.com/oauth/revoke")
        );
        assert_eq!(
            gl.link_path(),
            "/api/me/forge-accounts/gitlab/gitlab.example.com"
        );
        assert_eq!(gl.store_key(), "gitlab:gitlab.example.com");
        assert_eq!(gl.name(), "GitLab gitlab.example.com");
    }

    #[test]
    fn a_typed_gitlab_host_is_a_bare_name() {
        assert_eq!(
            gitlab_host(" https://GitLab.Example.com/ ").as_deref(),
            Some("gitlab.example.com")
        );
        assert_eq!(
            gitlab_host("gl.example.com:8443").as_deref(),
            Some("gl.example.com:8443")
        );
        for bad in [
            "",
            "github.com",
            "a/b",
            "http://x.com",
            "a..b",
            "x.com:0",
            "x y",
        ] {
            assert_eq!(gitlab_host(bad), None, "{bad:?}");
        }
    }
}
