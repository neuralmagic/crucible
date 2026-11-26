//! GitLab pipelines: create a pipeline with inputs, find the
//! pipelines a commit started, map a pipeline onto a progress graph, read a job's log and result
//! file, and cancel, over a host's REST API with a bearer token.
//!
//! ```text
//!   dispatch POST /projects/{id}/pipeline {ref, inputs}
//!   find     GET  /projects/{id}/pipelines?sha=…&source=…
//!   follow   GET  /projects/{id}/pipelines/{p}  + …/jobs  + …/bridges   (If-None-Match)
//!   access   GET  /projects/{id}/members/all/{u}  + …/protected_branches  + …/protected_tags
//!   shape    GET  /projects/{id}/ci/lint?dry_run=true&include_jobs=true&content_ref=…
//!   logs     GET  /projects/{id}/jobs/{j}/trace
//!   result   GET  /projects/{id}/jobs/{j}/artifacts/result.json
//!   cancel   POST /projects/{id}/pipelines/{p}/cancel
//! ```

pub mod access;
pub mod graph;
pub mod inputs;
pub mod merge;
pub mod model;

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde_json::Value;
use tracing::Instrument as _;
use url::Url;

use crate::commit::CommitId;
use crate::gitlab::model::{
    Bridge, Commit, Job, Lint, Member, Pipeline, ProtectedBranch, ProtectedTag, TokenInfo, User,
};
use crate::token::BearerToken;

const PER_PAGE: u32 = 100;
pub const MAX_PAGES: u32 = 10;
const ERR_BODY_CHARS: usize = 300;

/// A project's full path, such as `group/subgroup/project`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProjectPath(String);

impl ProjectPath {
    pub fn parse(raw: &str) -> Result<Self, GitlabError> {
        let segments: Vec<&str> = raw.split('/').collect();
        let ok = segments.len() >= 2
            && segments.iter().all(|s| {
                !s.is_empty()
                    && !s.starts_with('.')
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            });
        match ok {
            true => Ok(ProjectPath(raw.to_owned())),
            false => Err(GitlabError::Project(raw.to_owned())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GitlabError {
    #[error("{0:?} is not a GitLab project path")]
    Project(String),
    #[error("{0:?} is not an https GitLab address")]
    Host(String),
    #[error("{doing}: {source}")]
    Http {
        doing: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("{doing}: the body is over {cap} bytes")]
    TooLarge { doing: String, cap: u64 },
    #[error("{doing}: GitLab answered {status}: {body}")]
    Status {
        doing: String,
        status: u16,
        body: String,
    },
}

impl GitlabError {
    pub fn is_denied(&self) -> bool {
        matches!(
            self,
            GitlabError::Status {
                status: 401 | 403,
                ..
            }
        )
    }

    pub fn is_transient(&self) -> bool {
        match self {
            GitlabError::Http { .. } => true,
            GitlabError::Status { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }
}

/// A conditional read: unchanged since the ETag sent, or the new value and its ETag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Since<T> {
    Unchanged,
    Changed { value: T, etag: Option<String> },
}

/// One GitLab host's API, reached with the caller's client.
#[derive(Clone, Debug)]
pub struct Gitlab {
    http: reqwest::Client,
    api: Url,
}

impl Gitlab {
    pub fn new(http: reqwest::Client, host: &str) -> Result<Self, GitlabError> {
        let mut api = Url::parse(host).map_err(|_| GitlabError::Host(host.to_owned()))?;
        if api.scheme() != "https" || api.host_str().is_none() {
            return Err(GitlabError::Host(host.to_owned()));
        }
        api.set_path("/api/v4/");
        api.set_query(None);
        Ok(Gitlab { http, api })
    }

    fn url(&self, project: &ProjectPath, rest: &[&str], query: &[(&str, &str)]) -> Url {
        let mut url = self.api.clone();
        if let Ok(mut segments) = url.path_segments_mut() {
            segments.pop_if_empty();
            segments.push("projects");
            segments.push(project.as_str());
            segments.extend(rest);
        }
        if !query.is_empty() {
            let mut q = url.query_pairs_mut();
            for (k, v) in query {
                q.append_pair(k, v);
            }
        }
        url
    }

    pub async fn user(&self, token: &BearerToken) -> Result<User, GitlabError> {
        let mut url = self.api.clone();
        if let Ok(mut segments) = url.path_segments_mut() {
            segments.pop_if_empty();
            segments.push("user");
        }
        self.get_json(token, "reading the token's account", url)
            .await
    }

    pub async fn commit(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
        git_ref: &str,
    ) -> Result<CommitId, GitlabError> {
        let doing = format!("resolving {git_ref} of {}", project.as_str());
        let url = self.url(project, &["repository", "commits", git_ref], &[]);
        let commit: Commit = self.get_json(token, &doing, url).await?;
        CommitId::parse(&commit.id).map_err(|e| GitlabError::Status {
            doing,
            status: 200,
            body: e.to_string(),
        })
    }

    pub async fn member_access(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
        user_id: u64,
    ) -> Result<Option<Member>, GitlabError> {
        let doing = format!(
            "reading the access of user {user_id} to {}",
            project.as_str()
        );
        let url = self.url(project, &["members", "all", &user_id.to_string()], &[]);
        match self.get_json::<Member>(token, &doing, url).await {
            Ok(member) => Ok(Some(member)),
            Err(GitlabError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn protected_branches(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
    ) -> Result<Vec<ProtectedBranch>, GitlabError> {
        let doing = format!("listing the protected branches of {}", project.as_str());
        let url = self.url(project, &["protected_branches"], &[]);
        self.pages(token, &doing, url).await
    }

    pub async fn protected_tags(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
    ) -> Result<Vec<ProtectedTag>, GitlabError> {
        let doing = format!("listing the protected tags of {}", project.as_str());
        let url = self.url(project, &["protected_tags"], &[]);
        self.pages(token, &doing, url).await
    }

    pub async fn token_info(&self, token: &BearerToken) -> Result<TokenInfo, GitlabError> {
        let mut url = self.api.clone();
        url.set_path("/oauth/token/info");
        self.get_json(token, "reading which application the token belongs to", url)
            .await
    }

    pub async fn pipelines_for(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
        commit: &CommitId,
        sources: &[String],
    ) -> Result<Vec<Pipeline>, GitlabError> {
        let doing = format!(
            "listing the pipelines of {} in {}",
            commit.short(),
            project.as_str()
        );
        let url = self.url(project, &["pipelines"], &[("sha", commit.as_str())]);
        let all: Vec<Pipeline> = self.pages(token, &doing, url).await?;
        Ok(all
            .into_iter()
            .filter(|p| p.sha == commit.as_str())
            .filter(|p| {
                sources.is_empty()
                    || p.source
                        .as_deref()
                        .is_some_and(|s| sources.iter().any(|w| w == s))
            })
            .collect())
    }

    pub async fn latest_pipeline(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
        git_ref: &str,
    ) -> Result<Option<Pipeline>, GitlabError> {
        let doing = format!(
            "reading the latest pipeline of {} on {git_ref}",
            project.as_str()
        );
        let url = self.url(
            project,
            &["pipelines"],
            &[("ref", git_ref), ("per_page", "1")],
        );
        let page: Vec<Pipeline> = self.get_json(token, &doing, url).await?;
        Ok(page.into_iter().next())
    }

    pub async fn pipeline(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
        id: u64,
    ) -> Result<Option<Pipeline>, GitlabError> {
        let doing = format!("reading pipeline {id} of {}", project.as_str());
        let url = self.url(project, &["pipelines", &id.to_string()], &[]);
        match self.get_json(token, &doing, url).await {
            Ok(p) => Ok(Some(p)),
            Err(GitlabError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn pipeline_since(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
        id: u64,
        etag: Option<&str>,
    ) -> Result<Option<Since<Pipeline>>, GitlabError> {
        let doing = format!("reading pipeline {id} of {}", project.as_str());
        let url = self.url(project, &["pipelines", &id.to_string()], &[]);
        let r = match self
            .response(token, &doing, if_none_match(self.http.get(url), etag))
            .await
        {
            Ok(r) => r,
            Err(GitlabError::Status { status: 404, .. }) => return Ok(None),
            Err(e) => return Err(e),
        };
        if r.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(Some(Since::Unchanged));
        }
        let etag = etag_of(&r);
        let value = r
            .json()
            .await
            .map_err(|source| GitlabError::Http { doing, source })?;
        Ok(Some(Since::Changed { value, etag }))
    }

    pub async fn jobs_since(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
        pipeline: u64,
        etag: Option<&str>,
    ) -> Result<Since<Vec<Job>>, GitlabError> {
        let doing = format!("listing the jobs of pipeline {pipeline}");
        let url = self.url(project, &["pipelines", &pipeline.to_string(), "jobs"], &[]);
        self.pages_since(token, &doing, url, etag).await
    }

    pub async fn bridges_since(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
        pipeline: u64,
        etag: Option<&str>,
    ) -> Result<Since<Vec<Bridge>>, GitlabError> {
        let doing = format!("listing the trigger jobs of pipeline {pipeline}");
        let url = self.url(
            project,
            &["pipelines", &pipeline.to_string(), "bridges"],
            &[],
        );
        self.pages_since(token, &doing, url, etag).await
    }

    pub async fn jobs(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
        pipeline: u64,
    ) -> Result<Vec<Job>, GitlabError> {
        let doing = format!("listing the jobs of pipeline {pipeline}");
        let url = self.url(project, &["pipelines", &pipeline.to_string(), "jobs"], &[]);
        self.pages(token, &doing, url).await
    }

    pub async fn bridges(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
        pipeline: u64,
    ) -> Result<Vec<Bridge>, GitlabError> {
        let doing = format!("listing the trigger jobs of pipeline {pipeline}");
        let url = self.url(
            project,
            &["pipelines", &pipeline.to_string(), "bridges"],
            &[],
        );
        self.pages(token, &doing, url).await
    }

    pub async fn lint(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
        commit: &CommitId,
        git_ref: &str,
    ) -> Result<Lint, GitlabError> {
        let doing = format!(
            "validating the CI configuration of {} at {}",
            project.as_str(),
            commit.short()
        );
        let url = self.url(
            project,
            &["ci", "lint"],
            &[
                ("dry_run", "true"),
                ("include_jobs", "true"),
                ("content_ref", commit.as_str()),
                ("dry_run_ref", git_ref),
            ],
        );
        self.get_json(token, &doing, url).await
    }

    pub async fn file(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
        path: &str,
        git_ref: &str,
        cap: u64,
    ) -> Result<Option<String>, GitlabError> {
        let doing = format!("reading {path} of {} at {git_ref}", project.as_str());
        let url = self.url(
            project,
            &["repository", "files", path, "raw"],
            &[("ref", git_ref)],
        );
        match self.response(token, &doing, self.http.get(url)).await {
            Ok(r) => {
                let bytes = capped(r, cap, &doing).await?;
                String::from_utf8(bytes)
                    .map(Some)
                    .map_err(|_| GitlabError::Status {
                        doing,
                        status: 200,
                        body: "the file is not UTF-8".to_owned(),
                    })
            }
            Err(GitlabError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn create_pipeline(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
        git_ref: &str,
        inputs: &BTreeMap<String, Value>,
    ) -> Result<Pipeline, GitlabError> {
        let doing = format!("creating a pipeline of {} on {git_ref}", project.as_str());
        let url = self.url(project, &["pipeline"], &[]);
        let body = serde_json::json!({"ref": git_ref, "inputs": inputs});
        let r = self
            .response(token, &doing, self.http.post(url).json(&body))
            .await?;
        r.json()
            .await
            .map_err(|source| GitlabError::Http { doing, source })
    }

    pub async fn cancel(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
        pipeline: u64,
    ) -> Result<(), GitlabError> {
        let doing = format!("cancelling pipeline {pipeline} of {}", project.as_str());
        let url = self.url(
            project,
            &["pipelines", &pipeline.to_string(), "cancel"],
            &[],
        );
        self.response(token, &doing, self.http.post(url)).await?;
        Ok(())
    }

    pub async fn trace(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
        job: u64,
        keep: usize,
    ) -> Result<String, GitlabError> {
        let doing = format!("reading the log of job {job}");
        let url = self.url(project, &["jobs", &job.to_string(), "trace"], &[]);
        let r = self.response(token, &doing, self.http.get(url)).await?;
        let bytes = tail(r, keep, &doing).await?;
        Ok(String::from_utf8_lossy(char_start(&bytes)).into_owned())
    }

    pub async fn artifact(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
        job: u64,
        path: &str,
        cap: u64,
    ) -> Result<Option<Vec<u8>>, GitlabError> {
        let doing = format!("reading {path} from the artifacts of job {job}");
        let job_id = job.to_string();
        let mut rest = vec!["jobs", job_id.as_str(), "artifacts"];
        rest.extend(path.split('/'));
        let url = self.url(project, &rest, &[]);
        match self.response(token, &doing, self.http.get(url)).await {
            Ok(r) => capped(r, cap, &doing).await.map(Some),
            Err(GitlabError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        token: &BearerToken,
        doing: &str,
        url: Url,
    ) -> Result<T, GitlabError> {
        self.response(token, doing, self.http.get(url))
            .await?
            .json()
            .await
            .map_err(|source| GitlabError::Http {
                doing: doing.to_owned(),
                source,
            })
    }

    async fn pages_since<T: DeserializeOwned>(
        &self,
        token: &BearerToken,
        doing: &str,
        url: Url,
        etag: Option<&str>,
    ) -> Result<Since<Vec<T>>, GitlabError> {
        let mut first = url.clone();
        first
            .query_pairs_mut()
            .append_pair("per_page", &PER_PAGE.to_string())
            .append_pair("page", "1");
        let r = self
            .response(token, doing, if_none_match(self.http.get(first), etag))
            .await?;
        if r.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(Since::Unchanged);
        }
        let more = r
            .headers()
            .get("x-next-page")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| !v.is_empty());
        if more {
            let value = self.pages(token, doing, url).await?;
            return Ok(Since::Changed { value, etag: None });
        }
        let etag = etag_of(&r);
        let value = r.json().await.map_err(|source| GitlabError::Http {
            doing: doing.to_owned(),
            source,
        })?;
        Ok(Since::Changed { value, etag })
    }

    async fn pages<T: DeserializeOwned>(
        &self,
        token: &BearerToken,
        doing: &str,
        url: Url,
    ) -> Result<Vec<T>, GitlabError> {
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let mut paged = url.clone();
            paged
                .query_pairs_mut()
                .append_pair("per_page", &PER_PAGE.to_string())
                .append_pair("page", &page.to_string());
            let r = self.response(token, doing, self.http.get(paged)).await?;
            let next = r
                .headers()
                .get("x-next-page")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| !v.is_empty());
            let items: Vec<T> = r.json().await.map_err(|source| GitlabError::Http {
                doing: doing.to_owned(),
                source,
            })?;
            out.extend(items);
            if !next {
                break;
            }
        }
        Ok(out)
    }

    async fn response(
        &self,
        token: &BearerToken,
        doing: &str,
        req: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, GitlabError> {
        let http = |source| GitlabError::Http {
            doing: doing.to_owned(),
            source,
        };
        let request = req.bearer_auth(token.expose()).build().map_err(http)?;
        let span = crate::trace::request("gitlab", doing, &request);
        let r = self
            .http
            .execute(request)
            .instrument(span.clone())
            .await
            .map_err(http)?;
        crate::trace::response(&span, &r);
        let status = r.status();
        if status.is_success() || status == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(r);
        }
        let body = r.text().await.unwrap_or_default();
        Err(GitlabError::Status {
            doing: doing.to_owned(),
            status: status.as_u16(),
            body: body.chars().take(ERR_BODY_CHARS).collect(),
        })
    }
}

fn if_none_match(request: reqwest::RequestBuilder, etag: Option<&str>) -> reqwest::RequestBuilder {
    match etag {
        Some(etag) => request.header(reqwest::header::IF_NONE_MATCH, etag),
        None => request,
    }
}

fn etag_of(r: &reqwest::Response) -> Option<String> {
    r.headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

async fn capped(r: reqwest::Response, cap: u64, doing: &str) -> Result<Vec<u8>, GitlabError> {
    let over = || GitlabError::TooLarge {
        doing: doing.to_owned(),
        cap,
    };
    if r.content_length().is_some_and(|len| len > cap) {
        return Err(over());
    }
    let mut buf = Vec::new();
    chunks(r, doing, |chunk| {
        if buf.len() as u64 + chunk.len() as u64 > cap {
            return Err(over());
        }
        buf.extend_from_slice(chunk);
        Ok(())
    })
    .await?;
    Ok(buf)
}

async fn tail(r: reqwest::Response, keep: usize, doing: &str) -> Result<Vec<u8>, GitlabError> {
    let mut buf = Vec::new();
    chunks(r, doing, |chunk| {
        buf.extend_from_slice(chunk);
        keep_last(&mut buf, keep);
        Ok(())
    })
    .await?;
    Ok(buf)
}

#[cfg(not(target_arch = "wasm32"))]
async fn chunks(
    mut r: reqwest::Response,
    doing: &str,
    mut each: impl FnMut(&[u8]) -> Result<(), GitlabError>,
) -> Result<(), GitlabError> {
    while let Some(chunk) = r.chunk().await.map_err(|source| GitlabError::Http {
        doing: doing.to_owned(),
        source,
    })? {
        each(&chunk)?;
    }
    Ok(())
}

#[cfg(target_arch = "wasm32")]
async fn chunks(
    r: reqwest::Response,
    doing: &str,
    mut each: impl FnMut(&[u8]) -> Result<(), GitlabError>,
) -> Result<(), GitlabError> {
    let body = r.bytes().await.map_err(|source| GitlabError::Http {
        doing: doing.to_owned(),
        source,
    })?;
    each(&body)
}

fn char_start(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .take(4)
        .position(|b| b & 0xC0 != 0x80)
        .unwrap_or(0);
    &bytes[start..]
}

fn keep_last(buf: &mut Vec<u8>, keep: usize) {
    if buf.len() > keep {
        buf.drain(..buf.len() - keep);
    }
}

#[cfg(test)]
mod tests {
    use crate::gitlab::model::{Member, ProtectedBranch, TokenInfo};
    use crate::gitlab::{Gitlab, GitlabError, ProjectPath, char_start, keep_last};

    #[test]
    fn a_log_tail_keeps_its_last_bytes_from_a_char_boundary() {
        let mut buf = b"abcdef".to_vec();
        keep_last(&mut buf, 10);
        assert_eq!(buf, b"abcdef");
        keep_last(&mut buf, 3);
        assert_eq!(buf, b"def");
        let mut buf = "aé€".as_bytes().to_vec();
        keep_last(&mut buf, 4);
        assert_eq!(String::from_utf8_lossy(char_start(&buf)), "€");
        assert_eq!(char_start(b"plain"), b"plain");
    }

    #[test]
    fn only_an_active_member_is_active() {
        let member: Member = serde_json::from_value(serde_json::json!({
            "access_level": 30, "state": "active", "membership_state": "active"
        }))
        .unwrap();
        assert!(member.is_active());
        let bare: Member = serde_json::from_value(serde_json::json!({"access_level": 30})).unwrap();
        assert!(bare.is_active());
        for (state, membership) in [("blocked", "active"), ("active", "awaiting")] {
            let member: Member = serde_json::from_value(serde_json::json!({
                "access_level": 30, "state": state, "membership_state": membership
            }))
            .unwrap();
            assert!(!member.is_active(), "{state} {membership}");
        }
    }

    #[test]
    fn protected_refs_and_token_info_read_gitlabs_shapes() {
        let branch: ProtectedBranch = serde_json::from_value(serde_json::json!({
            "id": 1, "name": "release-*",
            "push_access_levels": [{"access_level": 40, "access_level_description": "Maintainers"}],
            "merge_access_levels": [{"access_level": 40, "user_id": 7, "group_id": null}]
        }))
        .unwrap();
        assert_eq!(branch.push_access_levels[0].access_level, Some(40));
        assert_eq!(branch.merge_access_levels[0].user_id, Some(7));
        let info: TokenInfo = serde_json::from_value(serde_json::json!({
            "resource_owner_id": 1, "scope": ["api"], "expires_in": 7200,
            "application": {"uid": "abc"}, "created_at": 1
        }))
        .unwrap();
        assert_eq!(info.application.map(|a| a.uid).as_deref(), Some("abc"));
    }

    #[test]
    fn project_paths_have_a_namespace_and_safe_segments() {
        assert!(ProjectPath::parse("group/sub/project.name").is_ok());
        for bad in ["project", "a//b", "a/../b", "a/.b", "a/b c", "/a/b", "a/b/"] {
            assert!(ProjectPath::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn urls_encode_the_project_and_file_paths() {
        let gl = Gitlab::new(
            reqwest::Client::new(),
            "https://gitlab.example.com/ignored?x=1",
        )
        .expect("host parses");
        let project = ProjectPath::parse("group/sub/app").expect("path");
        assert_eq!(
            gl.url(&project, &["pipelines"], &[("sha", "abc")]).as_str(),
            "https://gitlab.example.com/api/v4/projects/group%2Fsub%2Fapp/pipelines?sha=abc"
        );
        assert_eq!(
            gl.url(
                &project,
                &["repository", "files", ".gitlab-ci.yml", "raw"],
                &[]
            )
            .as_str(),
            "https://gitlab.example.com/api/v4/projects/group%2Fsub%2Fapp/repository/files/.gitlab-ci.yml/raw"
        );
        assert_eq!(
            gl.url(
                &project,
                &["repository", "files", "ci/main.yml", "raw"],
                &[]
            )
            .path(),
            "/api/v4/projects/group%2Fsub%2Fapp/repository/files/ci%2Fmain.yml/raw"
        );
    }

    #[test]
    fn only_https_hosts_are_accepted() {
        assert!(matches!(
            Gitlab::new(reqwest::Client::new(), "http://gitlab.example.com"),
            Err(GitlabError::Host(_))
        ));
    }
}
