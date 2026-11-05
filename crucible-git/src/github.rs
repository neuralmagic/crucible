//! GitHub as the run's App installation: the token, read fresh from its file, and the one REST
//! call the server makes.

use crate::allow::Allow;
use anyhow::{Context, Result};
use axum::http::StatusCode;
use base64::Engine;
use serde::Deserialize;
use std::path::PathBuf;
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("this run binds no GitHub App token")]
    Unbound,
    #[error("reading the GitHub App token from {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("the GitHub App token file {0} is empty")]
    Empty(String),
}

#[derive(Debug, thiserror::Error)]
#[error("GitHub answered {status}: {body}")]
pub struct ApiError {
    status: reqwest::StatusCode,
    body: String,
}

impl TokenError {
    pub fn status(&self) -> StatusCode {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

pub struct GitHub {
    token_file: Option<PathBuf>,
    pub http: reqwest::Client,
    /// The REST API, `https://api.github.com`.
    pub api: Url,
}

impl GitHub {
    pub fn new(token_file: Option<PathBuf>, api: Url) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("crucible-git/", env!("CARGO_PKG_VERSION")))
            .dns_resolver(std::sync::Arc::new(PastHostsFile::new()?))
            .connect_timeout(std::time::Duration::from_secs(10))
            .read_timeout(std::time::Duration::from_secs(300))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building the GitHub client")?;
        Ok(Self {
            token_file,
            http,
            api,
        })
    }

    /// The token as its file holds it now.
    pub async fn token(&self) -> Result<String, TokenError> {
        let path = self.token_file.as_ref().ok_or(TokenError::Unbound)?;
        let raw = tokio::fs::read_to_string(path)
            .await
            .map_err(|source| TokenError::Read {
                path: path.display().to_string(),
                source,
            })?;
        let token = raw.trim();
        if token.is_empty() {
            return Err(TokenError::Empty(path.display().to_string()));
        }
        Ok(token.to_string())
    }

    /// The `Authorization` value git over HTTPS takes an installation token as.
    pub fn basic(token: &str) -> String {
        let pair = format!("x-access-token:{token}");
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(pair)
        )
    }

    /// The open pull request into `repo` from `head` (`owner:branch`), or a new one. A pull
    /// request created here spends one of `allow`'s count, one already open spends none, and
    /// `pr.labels` and `pr.reviewers` are applied only to one this run opened.
    pub async fn open_pr(&self, pr: &NewPr<'_>, allow: &Allow) -> Result<OpenedPr> {
        let token = self.token().await?;
        let mut opened = self.find_or_create(pr, &token, allow).await?;
        let ours = allow.opened_number(pr.repo, opened.number);
        if ours && !pr.labels.is_empty() {
            let url = self
                .api
                .join(&format!(
                    "repos/{}/issues/{}/labels",
                    pr.repo, opened.number
                ))
                .context("building the labels URL")?;
            let _: serde_json::Value = self
                .call(
                    self.http
                        .post(url)
                        .json(&serde_json::json!({ "labels": pr.labels })),
                    &token,
                )
                .await
                .context("labelling the pull request")?;
            opened.labels = pr.labels.to_vec();
        }
        if ours && !pr.reviewers.is_empty() {
            let (teams, users): (Vec<&str>, Vec<&str>) = pr
                .reviewers
                .iter()
                .map(String::as_str)
                .partition(|r| r.contains('/'));
            let teams: Vec<&str> = teams
                .into_iter()
                .filter_map(|t| t.split_once('/').map(|(_, slug)| slug))
                .collect();
            let url = self
                .api
                .join(&format!(
                    "repos/{}/pulls/{}/requested_reviewers",
                    pr.repo, opened.number
                ))
                .context("building the reviewers URL")?;
            let _: serde_json::Value = self
                .call(
                    self.http.post(url).json(&serde_json::json!({
                        "reviewers": users,
                        "team_reviewers": teams,
                    })),
                    &token,
                )
                .await
                .context("requesting reviewers")?;
            opened.reviewers = pr.reviewers.to_vec();
        }
        Ok(opened)
    }

    async fn find_or_create(&self, pr: &NewPr<'_>, token: &str, allow: &Allow) -> Result<OpenedPr> {
        let pulls = self
            .api
            .join(&format!("repos/{}/pulls", pr.repo))
            .context("building the pulls URL")?;
        let mut open = pulls.clone();
        open.query_pairs_mut()
            .append_pair("state", "open")
            .append_pair("head", pr.head)
            .append_pair("base", pr.base);
        let listed: Vec<Pull> = self
            .call(self.http.get(open), token)
            .await
            .context("listing open pull requests for the head")?;
        if let Some(found) = listed.into_iter().next() {
            return Ok(OpenedPr {
                url: found.html_url,
                number: found.number,
                draft: found.draft,
                existing: true,
                labels: Vec::new(),
                reviewers: Vec::new(),
            });
        }
        allow.open(pr.repo).await?;
        let created: Pull = self
            .call(
                self.http.post(pulls).json(&serde_json::json!({
                    "title": pr.title,
                    "body": pr.body,
                    "head": pr.head,
                    "base": pr.base,
                    "draft": pr.draft,
                })),
                token,
            )
            .await
            .context("opening the pull request")?;
        allow.record(pr.repo, Some(created.number), created.node_id.as_deref());
        Ok(OpenedPr {
            url: created.html_url,
            number: created.number,
            draft: created.draft,
            existing: false,
            labels: Vec::new(),
            reviewers: Vec::new(),
        })
    }

    /// Whether `repo` is public, as the App installation sees it.
    pub async fn is_public(&self, repo: &str) -> Result<bool> {
        Ok(!self.repo(repo).await?.private)
    }

    /// `repo`'s GraphQL node id.
    pub async fn node_id(&self, repo: &str) -> Result<String> {
        Ok(self.repo(repo).await?.node_id)
    }

    async fn repo(&self, repo: &str) -> Result<RepoInfo> {
        let token = self.token().await?;
        let url = self
            .api
            .join(&format!("repos/{repo}"))
            .context("building the repository URL")?;
        self.call(self.http.get(url), &token).await
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
        token: &str,
    ) -> Result<T> {
        let resp = req
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError {
                status,
                body: body.chars().take(500).collect(),
            }
            .into());
        }
        Ok(resp.json().await?)
    }
}

pub struct NewPr<'a> {
    /// `owner/name` the pull request is opened on.
    pub repo: &'a str,
    pub base: &'a str,
    /// `owner:branch`.
    pub head: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub draft: bool,
    pub labels: &'a [String],
    /// Users, and teams as `org/team`.
    pub reviewers: &'a [String],
}

#[derive(Debug, serde::Serialize)]
pub struct OpenedPr {
    pub url: String,
    pub number: u64,
    pub draft: bool,
    /// The pull request was already open, so nothing was created.
    pub existing: bool,
    /// The labels this call added.
    pub labels: Vec<String>,
    /// The reviewers this call asked.
    pub reviewers: Vec<String>,
}

#[derive(Deserialize)]
struct Pull {
    html_url: String,
    number: u64,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    node_id: Option<String>,
}

#[derive(Deserialize)]
struct RepoInfo {
    private: bool,
    node_id: String,
}

/// Resolves through DNS alone: the pod maps GitHub's hostnames to `crucible-git` itself in
/// `/etc/hosts`, and its own requests must reach GitHub.
struct PastHostsFile(hickory_resolver::TokioResolver);

impl PastHostsFile {
    fn new() -> Result<Self> {
        let mut builder = hickory_resolver::TokioResolver::builder_tokio()
            .context("reading the system resolver configuration")?;
        builder.options_mut().use_hosts_file = hickory_resolver::config::ResolveHosts::Never;
        Ok(Self(builder.build().context("building the DNS resolver")?))
    }
}

impl reqwest::dns::Resolve for PastHostsFile {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let resolver = self.0.clone();
        Box::pin(async move {
            let lookup = resolver.lookup_ip(name.as_str()).await?;
            let addrs: Vec<std::net::SocketAddr> = lookup
                .iter()
                .map(|ip| std::net::SocketAddr::new(ip, 0))
                .collect();
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}
