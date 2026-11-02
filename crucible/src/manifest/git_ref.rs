use serde::{Deserialize, Serialize};

/// A branch, tag or commit name in git's ref grammar (libgit2's `check-ref-format`), normalized
/// the way git normalizes it, and never with a leading `-`, which git would read as an option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GitRef(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{got:?} is not a branch, tag or commit name: {reason}")]
pub struct GitRefError {
    got: String,
    reason: String,
}

impl GitRef {
    pub fn parse(raw: &str) -> Result<Self, GitRefError> {
        let refused = |reason: &str| GitRefError {
            got: raw.to_owned(),
            reason: reason.to_owned(),
        };
        let name = git2::Reference::normalize_name(
            raw,
            git2::ReferenceFormat::ALLOW_ONELEVEL | git2::ReferenceFormat::REFSPEC_SHORTHAND,
        )
        .map_err(|e| refused(e.message()))?;
        if name.starts_with('-') {
            return Err(refused("it starts with -, which git reads as an option"));
        }
        if name.len() > 255 {
            return Err(refused("it is longer than 255 bytes"));
        }
        Ok(GitRef(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A full commit id (SHA-1 or SHA-256) rather than a name.
    pub fn is_commit(&self) -> bool {
        matches!(self.0.len(), 40 | 64) && self.0.chars().all(|c| c.is_ascii_hexdigit())
    }
}

impl std::fmt::Display for GitRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for GitRef {
    type Error = GitRefError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        GitRef::parse(&raw)
    }
}

impl From<GitRef> for String {
    fn from(r: GitRef) -> String {
        r.0
    }
}

const PULL: &str = "refs/pull/";

/// What a ref is matched as: a branch or tag by its short name, or a pull request ref in full.
/// `None` for any other `refs/` namespace, which no pattern admits.
pub fn match_name(name: &str) -> Option<&str> {
    if let Some(short) = name
        .strip_prefix("refs/heads/")
        .or_else(|| name.strip_prefix("refs/tags/"))
    {
        return Some(short);
    }
    if name.starts_with("refs/") && !name.starts_with(PULL) {
        return None;
    }
    Some(name)
}

/// One `[repo.overrides].refs` entry: a glob over branch and tag short names, where `*` stops
/// at `/` and `**` crosses it. A branch and a tag of the same short name match alike. Pull
/// request refs match only a pattern that spells `refs/pull/`.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "String")]
pub struct RefPattern {
    raw: String,
    glob: glob::Pattern,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RefPatternError {
    #[error("[repo.overrides].refs entry {got:?} is not a glob: {reason}")]
    NotAGlob { got: String, reason: String },
    #[error(
        "[repo.overrides].refs entry {got:?} must be a branch or tag short name, or start with \
         refs/pull/"
    )]
    Namespace { got: String },
}

const MATCH: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

impl RefPattern {
    fn matches(&self, name: &str) -> bool {
        self.raw.starts_with(PULL) == name.starts_with(PULL) && self.glob.matches_with(name, MATCH)
    }
}

impl TryFrom<String> for RefPattern {
    type Error = RefPatternError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        let qualified = raw.starts_with("refs/") && !raw.starts_with(PULL);
        if raw.is_empty() || raw.starts_with('-') || qualified {
            return Err(RefPatternError::Namespace { got: raw });
        }
        let glob = glob::Pattern::new(&raw).map_err(|e| RefPatternError::NotAGlob {
            got: raw.clone(),
            reason: e.msg.to_owned(),
        })?;
        Ok(RefPattern { raw, glob })
    }
}

/// `[repo.overrides]`: the refs a launch may check out with `repo.ref`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoOverridesCfg {
    /// Empty: a launch checks out nothing but `[repo].ref`.
    #[serde(default)]
    pub refs: Vec<RefPattern>,
}

impl RepoOverridesCfg {
    pub fn is_empty(&self) -> bool {
        self.refs.is_empty()
    }

    /// Whether a branch, tag or pull request ref, short or fully qualified, matches a pattern.
    pub fn allows(&self, name: &str) -> bool {
        match_name(name).is_some_and(|name| self.refs.iter().any(|p| p.matches(name)))
    }
}

#[cfg(test)]
mod tests {
    use crate::manifest::git_ref::{GitRef, RepoOverridesCfg};

    fn overrides(refs: &[&str]) -> RepoOverridesCfg {
        toml::from_str(&format!(
            "refs = [{}]",
            refs.iter()
                .map(|r| format!("{r:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .expect("parses")
    }

    #[test]
    fn a_git_ref_follows_gits_grammar_and_is_never_an_option() {
        for ok in [
            "main",
            "feature/faster-decode",
            "refs/heads/feature/x",
            "refs/pull/12/head",
            "v1.2.0",
            "release-2026.10",
            "user@host+x",
            "308097caa4b55975e27d58465b1d441bc0bb6c63",
        ] {
            assert_eq!(GitRef::parse(ok).expect(ok).as_str(), ok);
        }
        assert_eq!(
            GitRef::parse("feature//x").expect("normalized").as_str(),
            "feature/x",
            "git collapses repeated slashes, and so does the parse"
        );
        for bad in [
            "",
            "-",
            "--upload-pack=evil",
            "-x/y",
            "main..feature",
            "main/",
            "main.",
            "main.lock",
            "feature/.hidden",
            ".hidden",
            "main@{1}",
            "HEAD~1",
            "main^",
            "a b",
            "x:y",
            "feature/*",
            "feature/[x]",
            "back\\slash",
            "bell\u{7}",
        ] {
            assert!(GitRef::parse(bad).is_err(), "{bad:?} parsed");
        }
        assert!(GitRef::parse(&"a".repeat(256)).is_err());
        let err = GitRef::parse("--upload-pack=evil").expect_err("an option");
        assert!(err.to_string().contains("--upload-pack=evil"), "{err}");
    }

    #[test]
    fn only_a_full_hex_id_is_a_commit() {
        assert!(
            GitRef::parse("308097caa4b55975e27d58465b1d441bc0bb6c63")
                .unwrap()
                .is_commit()
        );
        assert!(GitRef::parse(&"a".repeat(64)).unwrap().is_commit());
        assert!(!GitRef::parse("308097c").unwrap().is_commit());
        assert!(!GitRef::parse("main").unwrap().is_commit());
    }

    #[test]
    fn patterns_match_short_names_and_pull_refs_only_when_spelled() {
        let o = overrides(&["main", "feature/*", "release/**", "v*"]);
        for allowed in [
            "main",
            "refs/heads/main",
            "feature/x",
            "refs/heads/feature/x",
            "release/2026/10",
            "v1.2",
            "refs/tags/v1.2",
        ] {
            assert!(o.allows(allowed), "{allowed} refused");
        }
        for refused in [
            "mainline",
            "feature/x/y",
            "feature",
            "hotfix/x",
            "refs/pull/12/head",
            "refs/notes/commits",
            "refs/remotes/origin/main",
        ] {
            assert!(!o.allows(refused), "{refused} allowed");
        }

        let everything = overrides(&["**"]);
        assert!(everything.allows("a/b/c"));
        assert!(
            !everything.allows("refs/pull/12/head"),
            "a pull request ref is code from a fork; only a pattern that names refs/pull/ admits it"
        );
        let pulls = overrides(&["refs/pull/*/head"]);
        assert!(pulls.allows("refs/pull/12/head"));
        assert!(!pulls.allows("refs/pull/12/merge"));
        assert!(!pulls.allows("main"));
        assert!(!RepoOverridesCfg::default().allows("main"));
    }

    #[test]
    fn a_pattern_that_cannot_match_a_launch_ref_is_refused() {
        let refused = |raw: &str| {
            toml::from_str::<RepoOverridesCfg>(&format!("refs = [{raw:?}]"))
                .expect_err(raw)
                .to_string()
        };
        for qualified in [
            "refs/heads/feature/*",
            "refs/tags/v*",
            "refs/notes/*",
            "",
            "-x",
        ] {
            assert!(refused(qualified).contains("short name"), "{qualified:?}");
        }
        assert!(refused("feature/[").contains("not a glob"));
        assert!(toml::from_str::<RepoOverridesCfg>("refs = [\"main\"]\nextra = 1").is_err());
    }
}
