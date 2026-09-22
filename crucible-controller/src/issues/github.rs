//! The controller's shared GitHub REST read path: the retrying GET the discovery sweep funnels
//! through, the single-attempt GET the approvals use, `Link`-header pagination, PR-url parsing, the
//! approver authz gate, and the wire DTOs both [`crate::issues::triage`] and [`crate::issues::approvals`] decode.
//!
//! Auth honors `GITHUB_API_URL` + `GITHUB_TOKEN`/`GH_TOKEN` (the `scope.rs` pattern) so tests point
//! it at a local wiremock. Every client is built through [`crate::issues::http::client`] (UA + timeout) and
//! every non-2xx is classified by [`crate::issues::http::expect_2xx`] into a typed [`crate::issues::http::HttpError`].

#![allow(clippy::disallowed_macros)]

use crate::issues::http::HttpError;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::time::Duration;

/// Per-attempt timeout on a GitHub read: fetch is GET-only, so no request may hang a poll forever.
const READ_TIMEOUT: Duration = Duration::from_secs(20);
/// Bounded retries for the idempotent GET path (GET is safe to repeat within these bounds).
const MAX_RETRIES: u32 = 5;
/// A sane ceiling on pages per repo per poll — catches a runaway `Link` header instead of looping
/// forever against a misbehaving (or malicious) API endpoint.
const MAX_PAGES: usize = 200;
/// Cap on how long a single 429/rate-limit backoff sleeps, regardless of what `Retry-After` or
/// `X-RateLimit-Reset` ask for — a wedged clock or a huge reset window must not hang a poll forever.
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(120);
/// How much of a non-2xx error body to keep in [`HttpError::Status`].
const MAX_ERR_BODY: usize = 1024;

pub(crate) fn github_api_base() -> String {
    std::env::var("GITHUB_API_URL").unwrap_or_else(|_| "https://api.github.com".into())
}

pub(crate) fn github_token() -> Option<String> {
    std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .ok()
        .filter(|t| !t.is_empty())
}

/// A client for the retrying GET path (used by triage's paginated fetchers, reused across pages so
/// the connection pool stays warm). Built through [`crate::issues::http::client`] so it carries the UA +
/// timeout.
pub(crate) fn read_client() -> Result<reqwest::Client> {
    crate::issues::http::client(READ_TIMEOUT).context("building the GitHub API client")
}

/// GET `url` with the `scope.rs` auth/UA pattern, retrying up to [`MAX_RETRIES`] times: rate-limit
/// signals (429, or 403 with a spent budget) honor `Retry-After` / `X-RateLimit-Reset`; transient
/// network/5xx failures take a plain capped backoff. Returns the typed [`HttpError`] so callers
/// branch on the variant — notably [`HttpError::NotFound`] for the existence check.
#[tracing::instrument(
    name = "github.get",
    skip_all,
    fields(
        otel.kind = "client",
        peer.service = "github",
        http.url = %url,
        http.status_code = tracing::field::Empty,
        attempts = tracing::field::Empty,
    ),
    err
)]
pub(crate) async fn get_retryable(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
) -> Result<reqwest::Response, HttpError> {
    let mut attempt = 0u32;
    // A fine-grained PAT is deny-by-default outside its resource owner, so an authenticated GET of
    // an out-of-scope PUBLIC repo 403s where an anonymous one succeeds. On the first plain 403 drop
    // the token and retry unauthenticated — public repos read fine, and a genuinely-private repo
    // just 404s anonymously (the callers' not-found handling). `auth` is per-call state, never global.
    let mut auth = token;
    loop {
        attempt += 1;
        let mut req = client
            .get(url)
            .header("Accept", "application/vnd.github+json");
        if let Some(token) = auth {
            req = req.bearer_auth(token);
        }
        let resp = match req.send().await {
            Ok(resp) => resp,
            Err(e) => {
                if attempt > MAX_RETRIES {
                    return Err(HttpError::Transport(e));
                }
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
        };
        // Compute the rate-limit backoff from the headers before `expect_2xx` consumes the response.
        let rl_wait = rate_limit_wait(&resp);
        match crate::issues::http::expect_2xx(resp, url, MAX_ERR_BODY).await {
            Ok(resp) => {
                let span = tracing::Span::current();
                span.record("http.status_code", resp.status().as_u16());
                span.record("attempts", attempt);
                return Ok(resp);
            }
            Err(HttpError::RateLimited) => {
                if attempt > MAX_RETRIES {
                    return Err(HttpError::RateLimited);
                }
                tokio::time::sleep(rl_wait).await;
            }
            Err(HttpError::Status { code: 403, .. }) if auth.is_some() => {
                tracing::debug!(%url, "GitHub 403 with the org-scoped token; retrying anonymously");
                auth = None;
            }
            Err(HttpError::Status { code, body }) if (500..600).contains(&code) => {
                if attempt > MAX_RETRIES {
                    return Err(HttpError::Status { code, body });
                }
                tokio::time::sleep(backoff(attempt)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// GET `<api_base>/<path>` with the GitHub JSON accept header + optional bearer, decoding the body.
/// One attempt (the approval polls run on a coarse cadence; a transient miss is retried next tick).
#[tracing::instrument(
    name = "github.get_json",
    skip_all,
    fields(otel.kind = "client", peer.service = "github", path = %path),
    err
)]
pub(crate) async fn get_json<T: serde::de::DeserializeOwned>(
    api_base: &str,
    token: Option<&str>,
    path: &str,
) -> Result<T> {
    let client = read_client()?;
    let url = format!(
        "{}/{}",
        api_base.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let mut req = client
        .get(&url)
        .header("Accept", "application/vnd.github+json");
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.with_context(|| format!("GET {url}"))?;
    let resp = crate::issues::http::expect_2xx(resp, &url, MAX_ERR_BODY)
        .await
        .with_context(|| format!("GET {url}"))?;
    resp.json().await.with_context(|| format!("decoding {url}"))
}

/// How long to sleep after a rate-limit signal: prefer `Retry-After` (seconds), else
/// `X-RateLimit-Reset` (unix epoch seconds) minus now, else a fixed fallback — always capped at
/// [`MAX_RATE_LIMIT_WAIT`].
fn rate_limit_wait(resp: &reqwest::Response) -> Duration {
    let header_secs = |name: &str| -> Option<u64> {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
    };
    let wait = if let Some(secs) = header_secs("retry-after") {
        Duration::from_secs(secs)
    } else if let Some(reset) = header_secs("x-ratelimit-reset") {
        let now = u64::try_from(jiff::Timestamp::now().as_second()).unwrap_or(0);
        Duration::from_secs(reset.saturating_sub(now))
    } else {
        Duration::from_secs(30)
    };
    wait.min(MAX_RATE_LIMIT_WAIT)
}

/// Plain exponential backoff for transient failures (1s, 2s, 4s, ...), capped like the rate-limit
/// wait so a flaky network can't stall a poll indefinitely.
fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.min(6)).min(MAX_RATE_LIMIT_WAIT)
}

/// Pull the `rel="next"` URL out of a GitHub `Link` header, or `None` on the last page.
pub(crate) fn next_link(header: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        let mut segments = part.split(';');
        let url_part = segments.next()?.trim();
        let is_next = segments.any(|p| p.trim() == r#"rel="next""#);
        if !is_next {
            return None;
        }
        url_part
            .trim_start_matches('<')
            .trim_end_matches('>')
            .to_string()
            .into()
    })
}

/// GET `first_url` and every `rel="next"` page after it through [`get_retryable`], decoding each
/// page as a `Vec<T>`. `what` names the listing in errors.
pub(crate) async fn paginate<T: serde::de::DeserializeOwned>(
    token: Option<&str>,
    first_url: String,
    what: &str,
) -> Result<Vec<T>> {
    let client = read_client()?;
    let mut url = Some(first_url);
    let mut out = Vec::new();
    let mut pages = 0usize;
    while let Some(u) = url.take() {
        pages += 1;
        if pages > MAX_PAGES {
            bail!("{what} exceeded {MAX_PAGES} pages (runaway pagination?)");
        }
        let resp = get_retryable(&client, &u, token)
            .await
            .with_context(|| format!("GET {u}"))?;
        let next = resp
            .headers()
            .get(reqwest::header::LINK)
            .and_then(|v| v.to_str().ok())
            .and_then(next_link);
        let page: Vec<T> = resp
            .json()
            .await
            .with_context(|| format!("decoding {what} page from {u}"))?;
        out.extend(page);
        url = next;
    }
    Ok(out)
}

// --- wire DTOs -------------------------------------------------------------------------------

/// A GitHub user as any read here needs it — just the login. `#[serde(default)]` tolerates the
/// (rare/deleted-account) case GitHub omits the field.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct GhUser {
    #[serde(default)]
    pub(crate) login: String,
}

/// One PR review (`/pulls/{n}/reviews`). `state` is `APPROVED`/`CHANGES_REQUESTED`/`COMMENTED`/…
#[derive(Debug, Deserialize, Clone)]
pub struct Review {
    #[serde(default)]
    pub(crate) user: GhUser,
    #[serde(default)]
    pub(crate) state: String,
    #[serde(default)]
    pub(crate) author_association: String,
}

/// One PR conversation comment (`/issues/{n}/comments`).
#[derive(Debug, Deserialize, Clone)]
pub struct Comment {
    pub(crate) id: u64,
    #[serde(default)]
    pub(crate) body: String,
    #[serde(default)]
    pub(crate) user: GhUser,
    #[serde(default)]
    pub(crate) author_association: String,
}

/// One upstream issue as the approvals read it (state for close-detection, title+body for the hash).
#[derive(Debug, Deserialize, Clone)]
pub struct UpstreamIssue {
    #[serde(default)]
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) body: Option<String>,
    #[serde(default)]
    pub(crate) state: String,
}

// --- PR reference ----------------------------------------------------------------------------

/// A parsed `owner/repo/pull/N` reference (also tolerates `/issues/N`). Mirrors
/// `crucible/src/pr_watch.rs::parse_pr_url` — this crate can't depend on the binary crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRef {
    pub(crate) owner: String,
    pub(crate) repo: String,
    pub(crate) number: u64,
}

impl PrRef {
    /// `owner/repo` — the slug the `gh` write path and the GitHub REST base both want.
    pub(crate) fn repo_slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

/// Parse `https://github.com/OWNER/REPO/pull/N` (also `/issues/N`); trailing path/anchor tolerated.
pub(crate) fn parse_pr_url(url: &str) -> Option<PrRef> {
    let rest = url
        .trim()
        .strip_prefix("https://github.com/")
        .or_else(|| url.trim().strip_prefix("http://github.com/"))?;
    let mut parts = rest.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    let kw = parts.next()?;
    if (kw != "pull" && kw != "issues") || owner.is_empty() || repo.is_empty() {
        return None;
    }
    let num_tok = parts.next()?;
    let digits: String = num_tok.chars().take_while(|c| c.is_ascii_digit()).collect();
    let number: u64 = digits.parse().ok()?;
    Some(PrRef {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
    })
}

// --- authorization (mirrors pr_watch::Authz) -------------------------------------------------

/// Author associations GitHub reports for someone with write access / org standing (the default
/// trust boundary for a privileged action — they could push code directly anyway).
const TRUSTED_ASSOCIATIONS: &[&str] = &["OWNER", "MEMBER", "COLLABORATOR"];

/// Who may approve a pack / steer a run. Mirrors `crucible/src/pr_watch.rs::Authz`: an explicit
/// login allowlist wins when set (most restrictive); otherwise GitHub's server-computed
/// `author_association` must be one we trust. Both matches case-insensitive.
#[derive(Debug, Clone, Default)]
pub struct Authz {
    allow_users: Vec<String>,
}

impl Authz {
    /// Read the allowlist from `CONTROLLER_APPROVERS` (comma-separated logins); empty falls back to
    /// the trusted-association gate.
    pub(crate) fn from_env() -> Self {
        let allow_users = std::env::var("CONTROLLER_APPROVERS")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Authz { allow_users }
    }

    /// Is `login` (carrying repo `assoc`) allowed? Allowlist wins when present; else the association
    /// must be trusted.
    pub(crate) fn authorized(&self, login: &str, assoc: &str) -> bool {
        if !self.allow_users.is_empty() {
            return self
                .allow_users
                .iter()
                .any(|u| u.eq_ignore_ascii_case(login));
        }
        TRUSTED_ASSOCIATIONS
            .iter()
            .any(|a| a.eq_ignore_ascii_case(assoc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pr_url_handles_pull_and_issue_with_trailing_path() {
        assert_eq!(
            parse_pr_url("https://github.com/o/r/pull/42"),
            Some(PrRef {
                owner: "o".into(),
                repo: "r".into(),
                number: 42
            })
        );
        assert_eq!(
            parse_pr_url("https://github.com/o/r/pull/7/files").map(|p| p.number),
            Some(7)
        );
        assert!(parse_pr_url("https://gitlab.com/o/r/pull/1").is_none());
        assert!(parse_pr_url("https://github.com/o/r/tree/main").is_none());
    }

    #[test]
    fn next_link_extracts_rel_next() {
        let header = r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=9>; rel="last""#;
        assert_eq!(
            next_link(header).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
        assert!(next_link(r#"<https://api.github.com/x?page=9>; rel="last""#).is_none());
    }

    #[test]
    fn authz_gates_on_association_then_allowlist() {
        let def = Authz::default();
        assert!(def.authorized("owner", "OWNER"));
        assert!(def.authorized("c", "COLLABORATOR"));
        assert!(!def.authorized("rando", "NONE"));
        assert!(!def.authorized("ext", "CONTRIBUTOR"));

        let allow = Authz {
            allow_users: vec!["ext".into()],
        };
        assert!(
            allow.authorized("ext", "CONTRIBUTOR"),
            "allowlisted despite CONTRIBUTOR"
        );
        assert!(
            !allow.authorized("owner", "OWNER"),
            "OWNER not on allowlist can't approve"
        );
    }
}
