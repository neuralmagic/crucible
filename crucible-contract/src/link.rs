//! External results a task reports: an http(s) url, and what its host and path say it points at.

use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use url::Url;

/// The longest url accepted, measured on the normalized form.
pub const MAX_URL_LEN: usize = 2048;

/// Percent-encoding expands a byte to three, so nothing longer can normalize under the cap.
const MAX_RAW_LEN: usize = MAX_URL_LEN * 3;

/// Why a string is not an external link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    TooLong { len: usize },
    Malformed { value: String },
    Scheme { scheme: String },
    Credentials,
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::TooLong { len } => {
                write!(f, "a link is at most {MAX_URL_LEN} characters, not {len}")
            }
            LinkError::Malformed { value } => write!(f, "{value:?} is not a url"),
            LinkError::Scheme { scheme } => {
                write!(f, "scheme {scheme:?} is not linked; use http or https")
            }
            LinkError::Credentials => f.write_str("a link must not carry credentials"),
        }
    }
}

impl std::error::Error for LinkError {}

/// An http(s) url, normalized. The only constructor is the check, so a value of this type has
/// already been refused a non-http scheme, a missing host, and embedded credentials.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ExternalUrl(String);

impl std::fmt::Debug for ExternalUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

impl std::fmt::Display for ExternalUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl ExternalUrl {
    pub fn new(value: impl Into<String>) -> Result<Self, LinkError> {
        Ok(Self::checked(&value.into())?.0)
    }

    fn checked(value: &str) -> Result<(Self, Url), LinkError> {
        if value.len() > MAX_RAW_LEN {
            return Err(LinkError::TooLong { len: value.len() });
        }
        let parsed = Url::parse(value).map_err(|_| LinkError::Malformed {
            value: redact(value),
        })?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(LinkError::Scheme {
                scheme: parsed.scheme().to_owned(),
            });
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(LinkError::Credentials);
        }
        if parsed.host_str().is_none_or(str::is_empty) {
            return Err(LinkError::Malformed {
                value: redact(value),
            });
        }
        let normalized = String::from(parsed.clone());
        if normalized.len() > MAX_URL_LEN {
            return Err(LinkError::TooLong {
                len: normalized.len(),
            });
        }
        Ok((Self(normalized), parsed))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Replace the userinfo of a url with `***`, so a token a task put in one never reaches a note.
fn redact(value: &str) -> String {
    let Some(at) = value.find("//") else {
        return value.to_owned();
    };
    let start = at + 2;
    let rest = &value[start..];
    let authority = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..authority].find('@') {
        Some(userinfo) => format!("{}***@{}", &value[..start], &rest[userinfo + 1..]),
        None => value.to_owned(),
    }
}

impl TryFrom<String> for ExternalUrl {
    type Error = LinkError;
    fn try_from(value: String) -> Result<Self, LinkError> {
        Self::new(value)
    }
}

impl From<ExternalUrl> for String {
    fn from(value: ExternalUrl) -> String {
        value.0
    }
}

/// The service a link's host names. Anything unrecognized is [`LinkProvider::Other`] and renders
/// with the generic mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkProvider {
    #[serde(rename = "github")]
    GitHub,
    #[serde(rename = "gitlab")]
    GitLab,
    Jira,
    /// Any other host, and any provider token this build does not know.
    #[serde(other)]
    Other,
}

impl LinkProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkProvider::GitHub => "github",
            LinkProvider::GitLab => "gitlab",
            LinkProvider::Jira => "jira",
            LinkProvider::Other => "other",
        }
    }
}

/// What the link's path says it points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    PullRequest,
    MergeRequest,
    Branch,
    Commit,
    Compare,
    Issue,
    /// Anything the path does not place, and any kind token this build does not know.
    #[serde(other)]
    Page,
}

impl LinkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::PullRequest => "pull_request",
            LinkKind::MergeRequest => "merge_request",
            LinkKind::Branch => "branch",
            LinkKind::Commit => "commit",
            LinkKind::Compare => "compare",
            LinkKind::Issue => "issue",
            LinkKind::Page => "page",
        }
    }
}

/// One external result: the url, and the provider, kind and short label read off it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalLink {
    pub url: ExternalUrl,
    pub provider: LinkProvider,
    pub kind: LinkKind,
    /// What a reader writes beside the provider mark: `#123`, a branch, a short sha, an issue key.
    /// Never empty; it falls back to the host.
    pub label: String,
}

impl ExternalLink {
    pub fn parse(value: &str) -> Result<Self, LinkError> {
        let (url, parsed) = ExternalUrl::checked(value)?;
        let host = parsed.host_str().unwrap_or_default().to_owned();
        let segments: Vec<String> = parsed
            .path_segments()
            .map(|s| {
                s.filter(|seg| !seg.is_empty())
                    .map(|seg| percent_decode_str(seg).decode_utf8_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
        let (provider, kind, label) = classify(&host, &segments);
        Ok(Self {
            url,
            provider,
            kind,
            label: if label.is_empty() { host } else { label },
        })
    }
}

/// Read a stored or received list of links, dropping the entries this build cannot decode.
///
/// A link rides inside a task result. Refusing one must not cost the reader the result it came
/// with, so anything that does not decode is left out of the list rather than failed.
pub fn decode_links(raw: &serde_json::Value) -> Vec<ExternalLink> {
    raw.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| serde_json::from_value(item.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn classify(host: &str, segments: &[&str]) -> (LinkProvider, LinkKind, String) {
    let (provider, kind, label) = if is_github(host) {
        let (kind, label) = github_path(segments);
        (LinkProvider::GitHub, kind, label)
    } else if is_gitlab(host) {
        let (kind, label) = gitlab_path(segments);
        (LinkProvider::GitLab, kind, label)
    } else if is_jira(host) {
        let (kind, label) = jira_path(segments);
        (LinkProvider::Jira, kind, label)
    } else {
        return (LinkProvider::Other, LinkKind::Page, String::new());
    };
    // A mark beside a bare `#123` must never stand for a host the provider does not run, and a
    // task's output is not trusted input: name any other instance in the label.
    if is_canonical(host) || label.is_empty() {
        (provider, kind, label)
    } else {
        (provider, kind, format!("{host} {label}"))
    }
}

fn is_canonical(host: &str) -> bool {
    matches!(
        host,
        "github.com" | "www.github.com" | "gitlab.com" | "www.gitlab.com"
    ) || host.ends_with(".atlassian.net")
}

fn is_github(host: &str) -> bool {
    host == "github.com" || host == "www.github.com"
}

fn is_gitlab(host: &str) -> bool {
    host == "www.gitlab.com" || host.starts_with("gitlab.")
}

fn is_jira(host: &str) -> bool {
    host.ends_with(".atlassian.net") || host.starts_with("jira.")
}

/// `/<owner>/<repo>/<what>/<rest…>`.
fn github_path(segments: &[&str]) -> (LinkKind, String) {
    let [owner, repo, rest @ ..] = segments else {
        return (LinkKind::Page, segments.join("/"));
    };
    let repo_label = format!("{owner}/{repo}");
    match rest {
        ["pull", number, ..] => numbered(LinkKind::PullRequest, '#', number, &repo_label),
        ["issues", number, ..] => numbered(LinkKind::Issue, '#', number, &repo_label),
        ["commit", sha, ..] => (LinkKind::Commit, short_sha(sha)),
        ["compare", spec, ..] => (LinkKind::Compare, (*spec).to_owned()),
        ["tree", branch @ ..] if !branch.is_empty() => (LinkKind::Branch, branch.join("/")),
        _ => (LinkKind::Page, repo_label),
    }
}

/// GitLab nests a project's own pages under a `-` segment, and the project path above it may be
/// any number of groups deep.
fn gitlab_path(segments: &[&str]) -> (LinkKind, String) {
    let Some(dash) = segments.iter().position(|seg| *seg == "-") else {
        return (LinkKind::Page, segments.join("/"));
    };
    let project = segments[..dash].join("/");
    match &segments[dash + 1..] {
        ["merge_requests", number, ..] => numbered(LinkKind::MergeRequest, '!', number, &project),
        ["issues", number, ..] => numbered(LinkKind::Issue, '#', number, &project),
        ["commit", sha, ..] => (LinkKind::Commit, short_sha(sha)),
        ["compare", spec, ..] => (LinkKind::Compare, (*spec).to_owned()),
        ["tree", branch @ ..] if !branch.is_empty() => (LinkKind::Branch, branch.join("/")),
        _ => (LinkKind::Page, project),
    }
}

/// `/browse/<KEY-123>`, with or without a leading site path.
fn jira_path(segments: &[&str]) -> (LinkKind, String) {
    match segments.iter().position(|seg| *seg == "browse") {
        Some(at) => match segments.get(at + 1) {
            Some(key) => (LinkKind::Issue, (*key).to_owned()),
            None => (LinkKind::Page, String::new()),
        },
        None => (LinkKind::Page, String::new()),
    }
}

fn numbered(kind: LinkKind, sigil: char, number: &str, fallback: &str) -> (LinkKind, String) {
    if number.chars().all(|c| c.is_ascii_digit()) && !number.is_empty() {
        (kind, format!("{sigil}{number}"))
    } else {
        (LinkKind::Page, fallback.to_owned())
    }
}

fn short_sha(sha: &str) -> String {
    if sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
        sha[..7].to_owned()
    } else {
        sha.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use crate::link::{ExternalLink, ExternalUrl, LinkError, LinkKind, LinkProvider, MAX_URL_LEN};

    fn parsed(url: &str) -> ExternalLink {
        ExternalLink::parse(url).unwrap_or_else(|e| panic!("{url}: {e}"))
    }

    #[test]
    fn only_http_and_https_urls_with_a_host_are_links() {
        assert!(ExternalUrl::new("https://example.com/x").is_ok());
        assert!(ExternalUrl::new("http://example.com").is_ok());
        assert_eq!(
            ExternalUrl::new("javascript:alert(1)"),
            Err(LinkError::Scheme {
                scheme: "javascript".into()
            })
        );
        assert_eq!(
            ExternalUrl::new("data:text/html,<script>"),
            Err(LinkError::Scheme {
                scheme: "data".into()
            })
        );
        assert_eq!(
            ExternalUrl::new("file:///etc/passwd"),
            Err(LinkError::Scheme {
                scheme: "file".into()
            })
        );
        assert_eq!(
            ExternalUrl::new("ftp://example.com/x"),
            Err(LinkError::Scheme {
                scheme: "ftp".into()
            })
        );
        assert_eq!(
            ExternalUrl::new("http://"),
            Err(LinkError::Malformed {
                value: "http://".into()
            })
        );
        assert_eq!(
            ExternalUrl::new("github.com/a/b"),
            Err(LinkError::Malformed {
                value: "github.com/a/b".into()
            })
        );
        assert_eq!(
            ExternalUrl::new(""),
            Err(LinkError::Malformed { value: "".into() })
        );
        assert_eq!(
            ExternalUrl::new("https://"),
            Err(LinkError::Malformed {
                value: "https://".into()
            })
        );
    }

    #[test]
    fn a_url_carrying_credentials_is_refused() {
        assert_eq!(
            ExternalUrl::new("https://token@github.com/a/b"),
            Err(LinkError::Credentials)
        );
        assert_eq!(
            ExternalUrl::new("https://user:pw@github.com/a/b"),
            Err(LinkError::Credentials)
        );
    }

    #[test]
    fn a_refusal_never_repeats_the_credentials_it_refused() {
        for url in [
            "https://user:hunter2@github.com/a/b",
            "http://user:hunter2@/a/b",
            "https://hunter2@nope",
        ] {
            let why = ExternalUrl::new(url).expect_err(url).to_string();
            assert!(!why.contains("hunter2"), "{url}: {why}");
        }
    }

    #[test]
    fn a_url_over_the_cap_is_refused() {
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN));
        let len = long.len();
        assert_eq!(ExternalUrl::new(long), Err(LinkError::TooLong { len }));
    }

    /// The cap is what a reader will see, not what the task wrote: percent-encoding grows a url
    /// as it is normalized, and a link that parses but cannot be read back is lost data.
    #[test]
    fn the_cap_is_measured_on_the_normalized_url() {
        let url = format!("https://example.com/{}", "é".repeat(MAX_URL_LEN / 4));
        assert!(url.len() < MAX_URL_LEN, "the raw url is under the cap");
        let Err(LinkError::TooLong { len }) = ExternalUrl::new(url) else {
            panic!("a url that normalizes over the cap is refused");
        };
        assert!(
            len > MAX_URL_LEN,
            "the length reported is the normalized one"
        );
    }

    #[test]
    fn every_link_that_parses_decodes_again() {
        for url in [
            "https://github.com/neuralmagic/crucible/pull/123",
            "https://gitlab.com/g/p/-/merge_requests/1",
            "https://acme.atlassian.net/browse/A-1",
            &format!("https://example.com/{}", "é".repeat(300)),
        ] {
            let link = parsed(url);
            let text = serde_json::to_string(&link).unwrap_or_else(|e| panic!("{url}: {e}"));
            let back: ExternalLink =
                serde_json::from_str(&text).unwrap_or_else(|e| panic!("{url}: {e}"));
            assert_eq!(back, link, "{url}");
        }
    }

    #[test]
    fn a_link_this_build_cannot_read_costs_only_itself() {
        let raw = serde_json::json!([
            {"url": "https://github.com/a/b/pull/1", "provider": "github", "kind": "pull_request", "label": "#1"},
            {"url": "javascript:alert(1)", "provider": "other", "kind": "page", "label": "x"},
            {"nope": true},
            {"url": "https://gitlab.com/g/p/-/issues/2", "provider": "gitlab", "kind": "issue", "label": "#2"},
        ]);
        let labels: Vec<String> = crate::link::decode_links(&raw)
            .into_iter()
            .map(|link| link.label)
            .collect();
        assert_eq!(labels, vec!["#1", "#2"]);
        assert!(crate::link::decode_links(&serde_json::Value::Null).is_empty());
    }

    #[test]
    fn the_stored_url_is_the_normalized_one() {
        assert_eq!(
            ExternalUrl::new("HTTPS://GitHub.com/a/b").unwrap().as_str(),
            "https://github.com/a/b"
        );
    }

    #[test]
    fn github_paths_name_what_they_point_at() {
        let cases = [
            (
                "https://github.com/neuralmagic/crucible/pull/123",
                LinkKind::PullRequest,
                "#123",
            ),
            (
                "https://github.com/neuralmagic/crucible/pull/123/files#diff-1",
                LinkKind::PullRequest,
                "#123",
            ),
            (
                "https://github.com/neuralmagic/crucible/issues/7",
                LinkKind::Issue,
                "#7",
            ),
            (
                "https://github.com/neuralmagic/crucible/tree/feature/links",
                LinkKind::Branch,
                "feature/links",
            ),
            (
                "https://github.com/neuralmagic/crucible/commit/0123456789abcdef",
                LinkKind::Commit,
                "0123456",
            ),
            (
                "https://github.com/neuralmagic/crucible/compare/main...topic",
                LinkKind::Compare,
                "main...topic",
            ),
            (
                "https://github.com/neuralmagic/crucible",
                LinkKind::Page,
                "neuralmagic/crucible",
            ),
            ("https://github.com/", LinkKind::Page, "github.com"),
        ];
        for (url, kind, label) in cases {
            let link = parsed(url);
            assert_eq!(link.provider, LinkProvider::GitHub, "{url}");
            assert_eq!(link.kind, kind, "{url}");
            assert_eq!(link.label, label, "{url}");
        }
    }

    #[test]
    fn a_github_number_that_is_not_a_number_falls_back_to_the_repo() {
        let link = parsed("https://github.com/a/b/pull/not-a-number");
        assert_eq!(link.kind, LinkKind::Page);
        assert_eq!(link.label, "a/b");
    }

    #[test]
    fn gitlab_paths_are_read_below_the_dash_segment() {
        let cases = [
            (
                "https://gitlab.com/group/sub/proj/-/merge_requests/42",
                LinkKind::MergeRequest,
                "!42",
            ),
            (
                "https://gitlab.com/group/proj/-/issues/9",
                LinkKind::Issue,
                "#9",
            ),
            (
                "https://gitlab.com/group/proj/-/tree/release/1.2",
                LinkKind::Branch,
                "release/1.2",
            ),
            (
                "https://gitlab.com/group/proj/-/commit/abcdef0123456",
                LinkKind::Commit,
                "abcdef0",
            ),
            (
                "https://gitlab.com/group/proj/-/compare/main...topic",
                LinkKind::Compare,
                "main...topic",
            ),
            (
                "https://gitlab.com/group/proj",
                LinkKind::Page,
                "group/proj",
            ),
        ];
        for (url, kind, label) in cases {
            let link = parsed(url);
            assert_eq!(link.provider, LinkProvider::GitLab, "{url}");
            assert_eq!(link.kind, kind, "{url}");
            assert_eq!(link.label, label, "{url}");
        }
    }

    #[test]
    fn jira_browse_paths_carry_the_issue_key() {
        let link = parsed("https://acme.atlassian.net/browse/INFERENG-1234");
        assert_eq!(link.provider, LinkProvider::Jira);
        assert_eq!(link.kind, LinkKind::Issue);
        assert_eq!(link.label, "INFERENG-1234");

        let dashboard = parsed("https://acme.atlassian.net/jira/software/projects");
        assert_eq!(dashboard.provider, LinkProvider::Jira);
        assert_eq!(dashboard.kind, LinkKind::Page);
        assert_eq!(dashboard.label, "acme.atlassian.net");
    }

    /// A self-hosted instance keeps its mark, but the label names the host, so a link on a domain
    /// the provider does not run cannot be read as one it does.
    #[test]
    fn a_non_canonical_host_is_named_in_the_label() {
        let hosted = parsed("https://jira.example.com/browse/ABC-1");
        assert_eq!(hosted.provider, LinkProvider::Jira);
        assert_eq!(hosted.label, "jira.example.com ABC-1");

        let spoof = parsed("https://gitlab.evil.example/group/proj/-/merge_requests/42");
        assert_eq!(spoof.provider, LinkProvider::GitLab);
        assert_eq!(spoof.label, "gitlab.evil.example !42");

        let canonical = parsed("https://gitlab.com/group/proj/-/merge_requests/42");
        assert_eq!(canonical.label, "!42");
    }

    #[test]
    fn a_label_reads_as_the_branch_was_written() {
        let link = parsed("https://github.com/a/b/tree/feat/a%20b");
        assert_eq!(link.kind, LinkKind::Branch);
        assert_eq!(link.label, "feat/a b");
        assert_eq!(link.url.as_str(), "https://github.com/a/b/tree/feat/a%20b");
    }

    #[test]
    fn an_unrecognized_host_is_a_generic_page() {
        let link = parsed("https://build.example.com/job/42");
        assert_eq!(link.provider, LinkProvider::Other);
        assert_eq!(link.kind, LinkKind::Page);
        assert_eq!(link.label, "build.example.com");
    }

    #[test]
    fn a_link_round_trips_and_a_bad_url_does_not_decode() {
        let link = parsed("https://github.com/a/b/pull/1");
        let text = serde_json::to_string(&link).unwrap();
        assert_eq!(
            text,
            r##"{"url":"https://github.com/a/b/pull/1","provider":"github","kind":"pull_request","label":"#1"}"##
        );
        assert_eq!(serde_json::from_str::<ExternalLink>(&text).unwrap(), link);

        let err = serde_json::from_str::<ExternalLink>(
            r#"{"url":"javascript:alert(1)","provider":"other","kind":"page","label":"x"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not linked"), "{err}");
    }

    #[test]
    fn provider_and_kind_tokens_match_their_wire_form() {
        for provider in [
            LinkProvider::GitHub,
            LinkProvider::GitLab,
            LinkProvider::Jira,
            LinkProvider::Other,
        ] {
            let text = serde_json::to_string(&provider).unwrap();
            assert_eq!(text, format!("{:?}", provider.as_str()));
        }
        for kind in [
            LinkKind::PullRequest,
            LinkKind::MergeRequest,
            LinkKind::Branch,
            LinkKind::Commit,
            LinkKind::Compare,
            LinkKind::Issue,
            LinkKind::Page,
        ] {
            let text = serde_json::to_string(&kind).unwrap();
            assert_eq!(text, format!("{:?}", kind.as_str()));
        }
    }

    /// A reader older than the writer keeps the link: a provider or kind it does not know falls
    /// back to the generic pair rather than failing the whole record it rode in on.
    #[test]
    fn a_provider_or_kind_from_a_newer_writer_decodes_generically() {
        let link: ExternalLink = serde_json::from_str(
            r#"{"url":"https://forge.example.com/x","provider":"forge","kind":"release","label":"v2"}"#,
        )
        .expect("decodes");
        assert_eq!(link.provider, LinkProvider::Other);
        assert_eq!(link.kind, LinkKind::Page);
        assert_eq!(link.label, "v2");
    }
}
