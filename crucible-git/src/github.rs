//! GitHub as the run's App installation: the token, read fresh from its file, and the one REST
//! call the server makes.

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
    /// Where repositories are served, `https://github.com`.
    pub web: Url,
    /// The REST API, `https://api.github.com`.
    pub api: Url,
}

impl GitHub {
    pub fn new(token_file: Option<PathBuf>, web: Url, api: Url) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("crucible-git/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building the GitHub client")?;
        Ok(Self {
            token_file,
            http,
            web,
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

    /// The open pull request into `repo` from `head` (`owner:branch`), or a new draft one.
    pub async fn open_draft_pr(&self, pr: &DraftPr<'_>) -> Result<OpenedPr> {
        let token = self.token().await?;
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
            .call(self.http.get(open), &token)
            .await
            .context("listing open pull requests for the head")?;
        if let Some(found) = listed.into_iter().next() {
            return Ok(OpenedPr {
                url: found.html_url,
                number: found.number,
                existing: true,
            });
        }
        let created: Pull = self
            .call(
                self.http.post(pulls).json(&serde_json::json!({
                    "title": pr.title,
                    "body": pr.body,
                    "head": pr.head,
                    "base": pr.base,
                    "draft": true,
                })),
                &token,
            )
            .await
            .context("opening the draft pull request")?;
        Ok(OpenedPr {
            url: created.html_url,
            number: created.number,
            existing: false,
        })
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

pub struct DraftPr<'a> {
    /// `owner/name` the pull request is opened on.
    pub repo: &'a str,
    pub base: &'a str,
    /// `owner:branch`.
    pub head: &'a str,
    pub title: &'a str,
    pub body: &'a str,
}

#[derive(Debug, serde::Serialize)]
pub struct OpenedPr {
    pub url: String,
    pub number: u64,
    /// The pull request was already open, so nothing was created.
    pub existing: bool,
}

#[derive(Deserialize)]
struct Pull {
    html_url: String,
    number: u64,
}
