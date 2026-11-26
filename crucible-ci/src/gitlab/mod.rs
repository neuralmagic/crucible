//! GitLab pipelines: create a pipeline with inputs, find the
//! pipelines a commit started, map a pipeline onto a progress graph, read a job's log and result
//! file, and cancel, over a host's REST API with a bearer token.
//!
//! ```text
//!   dispatch POST /projects/{id}/pipeline {ref, inputs}
//!   find     GET  /projects/{id}/pipelines?sha=…&source=…
//!   follow   GET  /projects/{id}/pipelines/{p}  + …/jobs  + …/bridges
//!   shape    GET  /projects/{id}/ci/lint?dry_run=true&include_jobs=true&ref=…
//!   logs     GET  /projects/{id}/jobs/{j}/trace
//!   result   GET  /projects/{id}/jobs/{j}/artifacts/result.json
//!   cancel   POST /projects/{id}/pipelines/{p}/cancel
//! ```

pub mod graph;
pub mod inputs;
pub mod model;

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde_json::Value;
use url::Url;

use crate::commit::CommitId;
use crate::gitlab::model::{Bridge, Job, Lint, Pipeline};
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
        git_ref: &str,
    ) -> Result<Lint, GitlabError> {
        let doing = format!(
            "validating the CI configuration of {} at {git_ref}",
            project.as_str()
        );
        let url = self.url(
            project,
            &["ci", "lint"],
            &[
                ("dry_run", "true"),
                ("include_jobs", "true"),
                ("ref", git_ref),
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
    ) -> Result<Option<String>, GitlabError> {
        let doing = format!("reading {path} of {} at {git_ref}", project.as_str());
        let url = self.url(
            project,
            &["repository", "files", path, "raw"],
            &[("ref", git_ref)],
        );
        match self.response(token, &doing, self.http.get(url)).await {
            Ok(r) => r
                .text()
                .await
                .map(Some)
                .map_err(|source| GitlabError::Http { doing, source }),
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
    ) -> Result<String, GitlabError> {
        let doing = format!("reading the log of job {job}");
        let url = self.url(project, &["jobs", &job.to_string(), "trace"], &[]);
        self.response(token, &doing, self.http.get(url))
            .await?
            .text()
            .await
            .map_err(|source| GitlabError::Http { doing, source })
    }

    pub async fn artifact(
        &self,
        token: &BearerToken,
        project: &ProjectPath,
        job: u64,
        path: &str,
    ) -> Result<Option<Vec<u8>>, GitlabError> {
        let doing = format!("reading {path} from the artifacts of job {job}");
        let job_id = job.to_string();
        let mut rest = vec!["jobs", job_id.as_str(), "artifacts"];
        rest.extend(path.split('/'));
        let url = self.url(project, &rest, &[]);
        match self.response(token, &doing, self.http.get(url)).await {
            Ok(r) => r
                .bytes()
                .await
                .map(|b| Some(b.to_vec()))
                .map_err(|source| GitlabError::Http { doing, source }),
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
        let r = req
            .bearer_auth(token.expose())
            .send()
            .await
            .map_err(|source| GitlabError::Http {
                doing: doing.to_owned(),
                source,
            })?;
        let status = r.status();
        if status.is_success() {
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

#[cfg(test)]
mod tests {
    use crate::gitlab::{Gitlab, GitlabError, ProjectPath};

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
