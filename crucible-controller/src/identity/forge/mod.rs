//! The forges (GitHub, GitLab hosts) a person links accounts on: the accounts each person
//! linked by presenting one of their tokens, and, on a controller that acts with the person's own
//! access, the tokens themselves, held in memory only.

pub mod held;
pub(crate) mod links;

pub(crate) const GITHUB_HOST: &str = "github.com";

#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Forge {
    Github,
    Gitlab,
}

impl Forge {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Forge::Github => "github",
            Forge::Gitlab => "gitlab",
        }
    }

    fn parse(s: &str) -> Option<Forge> {
        match s {
            "github" => Some(Forge::Github),
            "gitlab" => Some(Forge::Gitlab),
            _ => None,
        }
    }
}

/// A forge host: a bare DNS name, lowercased, with an optional port.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ForgeHost(String);

impl ForgeHost {
    pub(crate) fn parse(raw: &str) -> Option<ForgeHost> {
        let host = raw.trim().to_ascii_lowercase();
        let (name, port) = match host.split_once(':') {
            Some((name, port)) => (name, Some(port)),
            None => (host.as_str(), None),
        };
        let name_ok = !name.is_empty()
            && name.len() <= 253
            && name.split('.').all(|label| {
                !label.is_empty()
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            });
        let port_ok = port.is_none_or(|p| p.parse::<u16>().is_ok_and(|p| p > 0));
        (name_ok && port_ok).then_some(ForgeHost(host))
    }

    pub(crate) fn github() -> ForgeHost {
        ForgeHost(GITHUB_HOST.to_owned())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use crate::identity::forge::*;

    #[test]
    fn a_host_is_a_lowercase_name_with_an_optional_port() {
        assert_eq!(
            ForgeHost::parse(" GitLab.Example.com ").unwrap().as_str(),
            "gitlab.example.com"
        );
        assert!(ForgeHost::parse("gitlab.example.com:8443").is_some());
        for bad in [
            "",
            "https://gitlab.com",
            "gitlab.com/x",
            "-a.com",
            "a..com",
            "a.com:0",
            "a.com:x",
            "a b.com",
            "a.com@evil",
        ] {
            assert!(ForgeHost::parse(bad).is_none(), "{bad:?}");
        }
    }
}
