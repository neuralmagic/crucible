//! The GitHub logins an operator paired with crucible logins (`CONTROLLER_GITHUB_LOGINS`).

/// The GitHub logins an operator paired with crucible logins (`CONTROLLER_GITHUB_LOGINS`, each
/// entry `<github login>=<crucible login>`). Both sides match case-insensitively.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GithubLogins {
    /// GitHub login (lowercased) to crucible login.
    by_github: std::collections::BTreeMap<String, String>,
}

impl GithubLogins {
    /// The pairs among `entries`; an entry that is not `<github>=<crucible>` is logged and left
    /// out.
    pub(crate) fn parse<'a>(entries: impl IntoIterator<Item = &'a str>) -> Self {
        let mut by_github = std::collections::BTreeMap::new();
        for entry in entries.into_iter().map(str::trim).filter(|e| !e.is_empty()) {
            match entry.split_once('=') {
                Some((github, login)) if !github.trim().is_empty() && !login.trim().is_empty() => {
                    by_github.insert(github.trim().to_lowercase(), login.trim().to_string());
                }
                _ => tracing::warn!(
                    entry,
                    "CONTROLLER_GITHUB_LOGINS entry is not <github login>=<crucible login>; ignored"
                ),
            }
        }
        GithubLogins { by_github }
    }

    /// The crucible login paired with GitHub login `github`.
    #[cfg(feature = "autoresearch")]
    pub(crate) fn crucible_login(&self, github: &str) -> Option<&str> {
        self.by_github
            .get(&github.to_lowercase())
            .map(String::as_str)
    }

    /// Every GitHub login paired with crucible login `crucible`.
    pub(crate) fn github_logins(&self, crucible: &str) -> Vec<&str> {
        self.by_github
            .iter()
            .filter(|(_, login)| login.eq_ignore_ascii_case(crucible))
            .map(|(github, _)| github.as_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::identity::github_logins::GithubLogins;

    #[test]
    fn a_crucible_login_finds_every_github_login_paired_with_it() {
        let logins = GithubLogins::parse(["Maint=alice", "alt=ALICE", "other=bob", "junk", "=x"]);
        assert_eq!(logins.github_logins("alice"), ["alt", "maint"]);
        assert_eq!(logins.github_logins("Bob"), ["other"]);
        assert!(logins.github_logins("maint").is_empty());
        assert!(logins.github_logins("junk").is_empty());
    }
}
