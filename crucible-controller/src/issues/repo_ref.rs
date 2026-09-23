//! [`RepoRef`]: a format-validated `org/name` GitHub repo reference (Lane O3, `POST /api/repos`).
//!
//! Validation is split into three independent steps, run in this order (the API handler stops at
//! the first failure and 422s with which one):
//!
//!   1. **format** — [`RepoRef::from_str`], pure and sync: exactly one `/`, both segments
//!      non-empty, no whitespace, no `..`, ASCII alnum/`-`/`_`/`.` only. This alone rejects every
//!      unicode-lookalike and path-traversal-shaped input; it needs no DB or network.
//!   2. **org whitelist** — [`RepoWhitelist::check`], sync but needs the deploy-pinned allowed-org
//!      list (and the env-seeded exemption), so it's a separate step from parsing.
//!   3. **GitHub existence** — async (an API call), so it's the caller's job entirely; this module
//!      only carries the format + whitelist checks.

use std::fmt;
use std::str::FromStr;

/// A parsed, format-valid `org/name` repo reference. Parsing alone does NOT mean the repo is
/// allowed or exists — see the module doc for the other two checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    pub(crate) org: String,
    name: String,
}

impl RepoRef {
    /// The `owner/repo` wire form the rest of the controller (the `repos`/`issues` tables, the
    /// GitHub API paths) already speaks.
    #[cfg(feature = "autoresearch")]
    pub(crate) fn as_repo_string(&self) -> String {
        format!("{}/{}", self.org, self.name)
    }
}

impl fmt::Display for RepoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.org, self.name)
    }
}

/// Why [`RepoRef::from_str`] rejected an input — carries enough detail for the `POST /api/repos`
/// 422 body to say exactly which check failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoRefParseError {
    /// Not exactly one `/` (zero, or more than one — including a leading/trailing slash).
    NotOneSlash,
    /// The org or the repo-name segment was empty.
    EmptySegment,
    /// A segment contained `..` (path-traversal-shaped input).
    DotDot,
    /// A segment held a character outside ASCII alnum/`-`/`_`/`.` — this also rejects whitespace
    /// and any unicode lookalike (a non-ASCII byte is never in the allowed set).
    InvalidChar,
}

impl fmt::Display for RepoRefParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            RepoRefParseError::NotOneSlash => {
                "must be exactly one `org/name` pair (exactly one `/`, no leading/trailing slash)"
            }
            RepoRefParseError::EmptySegment => {
                "the org and repo-name segments must both be non-empty"
            }
            RepoRefParseError::DotDot => "segments may not contain `..`",
            RepoRefParseError::InvalidChar => {
                "segments may only contain ASCII letters, digits, `-`, `_`, or `.`"
            }
        };
        f.write_str(msg)
    }
}

impl std::error::Error for RepoRefParseError {}

impl FromStr for RepoRef {
    type Err = RepoRefParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.matches('/').count() != 1 {
            return Err(RepoRefParseError::NotOneSlash);
        }
        let (org, name) = s.split_once('/').ok_or(RepoRefParseError::NotOneSlash)?;
        if org.is_empty() || name.is_empty() {
            return Err(RepoRefParseError::EmptySegment);
        }
        if org.contains("..") || name.contains("..") {
            return Err(RepoRefParseError::DotDot);
        }
        let valid = |seg: &str| {
            seg.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        if !valid(org) || !valid(name) {
            return Err(RepoRefParseError::InvalidChar);
        }
        Ok(RepoRef {
            org: org.to_string(),
            name: name.to_string(),
        })
    }
}

/// Why [`RepoWhitelist::check`] rejected a format-valid [`RepoRef`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WhitelistError {
    /// The org isn't in the allowed-org list (and the repo wasn't env-seeded).
    OrgNotAllowed,
}

impl fmt::Display for WhitelistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("org is not in the allowed-org whitelist")
    }
}

impl std::error::Error for WhitelistError {}

/// The deploy-pinned org whitelist (`CONTROLLER_ALLOWED_ORGS` / `--allowed-org`) plus the
/// env-seeded repo list (`CONTROLLER_WATCHED_REPOS` / `--repo`) that's exempt from it — those
/// repos were operator-provisioned at deploy time, not added live through the API. An empty
/// whitelist locks new-repo addition closed by default (matching [`crate::identity::auth::Roles`]'s
/// empty-list-locks-closed convention), while env-seeded repos keep working untouched.
#[cfg(feature = "autoresearch")]
#[derive(Debug, Clone, Default)]
pub struct RepoWhitelist {
    allowed_orgs: Vec<String>,
    env_repos: Vec<String>,
}

#[cfg(feature = "autoresearch")]
fn normalize(items: Vec<String>) -> Vec<String> {
    items
        .into_iter()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(feature = "autoresearch")]
impl RepoWhitelist {
    /// Build from the raw config lists (arbitrary case/whitespace; normalized once here).
    pub(crate) fn new(allowed_orgs: Vec<String>, env_repos: Vec<String>) -> Self {
        RepoWhitelist {
            allowed_orgs: normalize(allowed_orgs),
            env_repos: normalize(env_repos),
        }
    }

    /// Whether `repo` (`org/name`) was seeded from the boot-time env config — exempt from the
    /// whitelist check, compared case-insensitively (repo casing in config/DB isn't normalized).
    fn is_env_seeded(&self, repo: &str) -> bool {
        let repo = repo.to_lowercase();
        self.env_repos.contains(&repo)
    }

    /// The deploy-pinned allowed-org list (normalized). Surfaced read-only (`GET /api/access`) so
    /// the `/admin` repo-add form can show the boundary instead of leaving admins to guess which
    /// orgs `POST /api/repos` will accept.
    pub(crate) fn allowed_orgs(&self) -> &[String] {
        &self.allowed_orgs
    }

    /// Check `repo_ref` against the whitelist: allowed if its org (case-insensitively) is on the
    /// list, or if the full `org/name` was env-seeded at boot.
    pub(crate) fn check(&self, repo_ref: &RepoRef) -> Result<(), WhitelistError> {
        if self.is_env_seeded(&repo_ref.as_repo_string()) {
            return Ok(());
        }
        let org = repo_ref.org.to_lowercase();
        if self.allowed_orgs.contains(&org) {
            Ok(())
        } else {
            Err(WhitelistError::OrgNotAllowed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "autoresearch")]
    #[test]
    fn parses_a_well_formed_repo() {
        let r: RepoRef = "owner/repo".parse().expect("parses");
        assert_eq!(r.org, "owner");
        assert_eq!(r.name, "repo");
        assert_eq!(r.as_repo_string(), "owner/repo");
    }

    #[test]
    fn rejects_wrong_slash_count() {
        for bad in [
            "ownerrepo",
            "owner/repo/extra",
            "/owner/repo",
            "owner/repo/",
            "a/b/c",
        ] {
            assert_eq!(
                bad.parse::<RepoRef>().unwrap_err(),
                RepoRefParseError::NotOneSlash,
                "input: {bad}"
            );
        }
    }

    #[test]
    fn rejects_empty_segments() {
        for bad in ["/repo", "owner/", "/"] {
            let err = bad.parse::<RepoRef>().unwrap_err();
            assert!(
                matches!(
                    err,
                    RepoRefParseError::EmptySegment | RepoRefParseError::NotOneSlash
                ),
                "input {bad}: {err:?}"
            );
        }
    }

    #[test]
    fn rejects_dot_dot_path_traversal_shapes() {
        for bad in ["../etc", "owner/..", "..%2f/repo", "owner/../repo"] {
            let err = bad.parse::<RepoRef>();
            // Some of these also fail the slash-count or char-class check first; either way they
            // must be rejected, never parsed as a valid RepoRef.
            assert!(err.is_err(), "input {bad} must be rejected");
        }
        assert_eq!(
            "own..er/repo".parse::<RepoRef>().unwrap_err(),
            RepoRefParseError::DotDot
        );
    }

    #[test]
    fn rejects_whitespace() {
        for bad in ["owner /repo", "owner/ repo", "owner/repo\n", "own er/repo"] {
            assert_eq!(
                bad.parse::<RepoRef>().unwrap_err(),
                RepoRefParseError::InvalidChar,
                "input: {bad:?}"
            );
        }
    }

    #[test]
    fn rejects_non_ascii_unicode_lookalikes() {
        // A Cyrillic 'а' (U+0430) that looks identical to ASCII 'a'.
        for bad in ["\u{0430}wner/repo", "owner/rep\u{0430}", "owner/rеpo"] {
            assert_eq!(
                bad.parse::<RepoRef>().unwrap_err(),
                RepoRefParseError::InvalidChar,
                "input: {bad:?}"
            );
        }
    }

    #[test]
    fn rejects_other_invalid_characters() {
        for bad in ["owner/repo!", "own@er/repo", "owner/re po", "owner/re$po"] {
            assert_eq!(
                bad.parse::<RepoRef>().unwrap_err(),
                RepoRefParseError::InvalidChar,
                "input: {bad}"
            );
        }
    }

    #[test]
    fn accepts_dash_underscore_dot_in_segments() {
        let r: RepoRef = "my-org_1/repo.name_v2".parse().expect("parses");
        assert_eq!(r.org, "my-org_1");
        assert_eq!(r.name, "repo.name_v2");
    }

    #[cfg(feature = "autoresearch")]
    #[test]
    fn whitelist_matches_org_case_insensitively() {
        let wl = RepoWhitelist::new(vec!["NeuralMagic".to_string()], vec![]);
        let r: RepoRef = "neuralmagic/llm-d".parse().expect("parses");
        assert!(wl.check(&r).is_ok());
        let r2: RepoRef = "NEURALMAGIC/llm-d".parse().expect("parses");
        assert!(wl.check(&r2).is_ok());
    }

    #[cfg(feature = "autoresearch")]
    #[test]
    fn whitelist_rejects_org_not_on_the_list() {
        let wl = RepoWhitelist::new(vec!["neuralmagic".to_string()], vec![]);
        let r: RepoRef = "someoneelse/repo".parse().expect("parses");
        assert_eq!(wl.check(&r).unwrap_err(), WhitelistError::OrgNotAllowed);
    }

    #[cfg(feature = "autoresearch")]
    #[test]
    fn empty_whitelist_locks_closed() {
        let wl = RepoWhitelist::new(vec![], vec![]);
        let r: RepoRef = "anyone/repo".parse().expect("parses");
        assert_eq!(wl.check(&r).unwrap_err(), WhitelistError::OrgNotAllowed);
    }

    #[cfg(feature = "autoresearch")]
    #[test]
    fn env_seeded_repo_is_exempt_from_the_whitelist() {
        let wl = RepoWhitelist::new(vec![], vec!["owner/repo".to_string()]);
        let r: RepoRef = "owner/repo".parse().expect("parses");
        assert!(
            wl.check(&r).is_ok(),
            "env-seeded repo bypasses the org check"
        );
        // A different repo under the same org is NOT exempt just because one sibling was seeded.
        let r2: RepoRef = "owner/other".parse().expect("parses");
        assert_eq!(wl.check(&r2).unwrap_err(), WhitelistError::OrgNotAllowed);
    }

    #[cfg(feature = "autoresearch")]
    #[test]
    fn env_seeded_match_is_case_insensitive() {
        let wl = RepoWhitelist::new(vec![], vec!["Owner/Repo".to_string()]);
        let r: RepoRef = "owner/repo".parse().expect("parses");
        assert!(wl.check(&r).is_ok());
    }
}
