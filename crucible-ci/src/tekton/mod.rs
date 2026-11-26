//! Tekton and Konflux: find the Pipelines-as-Code runs a commit started, map
//! a run onto a progress graph, read its outcome and results, and read a step's log, all over a
//! cluster's API with a bearer token.
//!
//! ```text
//!   find   GET /apis/tekton.dev/v1/namespaces/{ns}/pipelineruns?labelSelector=sha,repository,…
//!   follow GET …/pipelineruns/{name}  +  GET …/taskruns?labelSelector=tekton.dev/pipelineRun={name}
//!   logs   GET /api/v1/namespaces/{ns}/pods/{pod}/log?container=step-…
//! ```
//!
//! Requests are built with `kube-core` and sent with the caller's `reqwest::Client`, so this
//! builds for wasm32 as well as natively.

pub mod dispatch;
pub mod find;
pub mod graph;
pub mod model;
pub mod outcome;
pub mod results;

use kube_core::params::{GetParams, ListParams, Patch, PatchParams, PostParams};
use kube_core::subresource::LogParams;
use serde::de::DeserializeOwned;
use url::Url;

use crate::tekton::find::Filter;
use crate::tekton::model::{List, PipelineRun, TaskRun};
use crate::token::BearerToken;

const PIPELINE_RUNS: &str = "/apis/tekton.dev/v1/namespaces/{ns}/pipelineruns";
const TASK_RUNS: &str = "/apis/tekton.dev/v1/namespaces/{ns}/taskruns";
const PODS: &str = "/api/v1/namespaces/{ns}/pods";
const SELF_REVIEWS: &str = "/apis/authorization.k8s.io/v1/selfsubjectaccessreviews";
const TASK_RUN_OWNER_LABEL: &str = "tekton.dev/pipelineRun";
const ERR_BODY_CHARS: usize = 300;
pub const CANCEL_STATUS: &str = "CancelledRunFinally";

/// A Kubernetes namespace name: a DNS-1123 label.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Namespace(String);

impl Namespace {
    pub fn parse(raw: &str) -> Result<Self, TektonError> {
        let ok = !raw.is_empty()
            && raw.len() <= 63
            && raw
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !raw.starts_with('-')
            && !raw.ends_with('-');
        match ok {
            true => Ok(Namespace(raw.to_owned())),
            false => Err(TektonError::Namespace(raw.to_owned())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TektonError {
    #[error("{0:?} is not a namespace name")]
    Namespace(String),
    #[error("{0:?} is not an https API server address")]
    Api(String),
    #[error("building the request to {doing}: {source}")]
    Request {
        doing: String,
        #[source]
        source: kube_core::request::Error,
    },
    #[error("{doing}: {message}")]
    Body { doing: String, message: String },
    #[error("{doing}: {source}")]
    Http {
        doing: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("{doing}: the API server answered {status}: {body}")]
    Status {
        doing: String,
        status: u16,
        body: String,
    },
}

impl TektonError {
    pub fn is_denied(&self) -> bool {
        matches!(
            self,
            TektonError::Status {
                status: 401 | 403,
                ..
            }
        )
    }

    pub fn is_transient(&self) -> bool {
        match self {
            TektonError::Http { .. } => true,
            TektonError::Status { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }
}

/// One cluster's API, reached with the caller's client.
#[derive(Clone, Debug)]
pub struct Cluster {
    http: reqwest::Client,
    api: String,
}

impl Cluster {
    pub fn new(http: reqwest::Client, api: &str) -> Result<Self, TektonError> {
        let url = Url::parse(api).map_err(|_| TektonError::Api(api.to_owned()))?;
        if url.scheme() != "https" || url.host_str().is_none() {
            return Err(TektonError::Api(api.to_owned()));
        }
        Ok(Cluster {
            http,
            api: api.trim_end_matches('/').to_owned(),
        })
    }

    pub async fn find(
        &self,
        token: &BearerToken,
        ns: &Namespace,
        filter: &Filter,
    ) -> Result<Vec<PipelineRun>, TektonError> {
        let runs = self.pipeline_runs(token, ns, &filter.selector()).await?;
        Ok(runs.into_iter().filter(|r| filter.admits(r)).collect())
    }

    pub async fn pipeline_runs(
        &self,
        token: &BearerToken,
        ns: &Namespace,
        selector: &str,
    ) -> Result<Vec<PipelineRun>, TektonError> {
        let doing = format!("listing PipelineRuns ({selector}) in {}", ns.as_str());
        let params = ListParams::default().labels(selector);
        let req = kube_core::Request::new(path(PIPELINE_RUNS, ns))
            .list(&params)
            .map_err(|source| TektonError::Request {
                doing: doing.clone(),
                source,
            })?;
        let list: List<PipelineRun> = self.send(token, &doing, req).await?;
        Ok(list.items)
    }

    pub async fn pipeline_run(
        &self,
        token: &BearerToken,
        ns: &Namespace,
        name: &str,
    ) -> Result<Option<PipelineRun>, TektonError> {
        let doing = format!("reading PipelineRun {name} in {}", ns.as_str());
        let req = kube_core::Request::new(path(PIPELINE_RUNS, ns))
            .get(name, &GetParams::default())
            .map_err(|source| TektonError::Request {
                doing: doing.clone(),
                source,
            })?;
        match self.send(token, &doing, req).await {
            Ok(run) => Ok(Some(run)),
            Err(TektonError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn task_runs(
        &self,
        token: &BearerToken,
        ns: &Namespace,
        run: &str,
    ) -> Result<Vec<TaskRun>, TektonError> {
        let doing = format!("listing the TaskRuns of {run} in {}", ns.as_str());
        let params = ListParams::default().labels(&format!("{TASK_RUN_OWNER_LABEL}={run}"));
        let req = kube_core::Request::new(path(TASK_RUNS, ns))
            .list(&params)
            .map_err(|source| TektonError::Request {
                doing: doing.clone(),
                source,
            })?;
        let list: List<TaskRun> = self.send(token, &doing, req).await?;
        Ok(list.items)
    }

    pub async fn create_pipeline_run(
        &self,
        token: &BearerToken,
        ns: &Namespace,
        run: &serde_json::Value,
    ) -> Result<PipelineRun, TektonError> {
        let doing = format!("creating a PipelineRun in {}", ns.as_str());
        let body = serde_json::to_vec(run).map_err(|e| TektonError::Body {
            doing: doing.clone(),
            message: e.to_string(),
        })?;
        let req = kube_core::Request::new(path(PIPELINE_RUNS, ns))
            .create(&PostParams::default(), body)
            .map_err(|source| TektonError::Request {
                doing: doing.clone(),
                source,
            })?;
        self.send(token, &doing, req).await
    }

    pub async fn cancel(
        &self,
        token: &BearerToken,
        ns: &Namespace,
        name: &str,
    ) -> Result<(), TektonError> {
        let doing = format!("cancelling PipelineRun {name} in {}", ns.as_str());
        let patch = Patch::Merge(serde_json::json!({"spec": {"status": CANCEL_STATUS}}));
        let req = kube_core::Request::new(path(PIPELINE_RUNS, ns))
            .patch(name, &PatchParams::default(), &patch)
            .map_err(|source| TektonError::Request {
                doing: doing.clone(),
                source,
            })?;
        let _: PipelineRun = self.send(token, &doing, req).await?;
        Ok(())
    }

    pub async fn step_log(
        &self,
        token: &BearerToken,
        ns: &Namespace,
        pod: &str,
        container: &str,
        tail_lines: Option<i64>,
    ) -> Result<String, TektonError> {
        let doing = format!("reading the log of {pod}/{container} in {}", ns.as_str());
        let params = LogParams {
            container: Some(container.to_owned()),
            tail_lines,
            ..LogParams::default()
        };
        let req = kube_core::Request::new(path(PODS, ns))
            .logs(pod, &params)
            .map_err(|source| TektonError::Request {
                doing: doing.clone(),
                source,
            })?;
        let response = self.response(token, &doing, req).await?;
        response
            .text()
            .await
            .map_err(|source| TektonError::Http { doing, source })
    }

    pub async fn may(
        &self,
        token: &BearerToken,
        ns: &Namespace,
        verb: &str,
    ) -> Result<bool, TektonError> {
        let doing = format!(
            "asking whether this token may {verb} PipelineRuns in {}",
            ns.as_str()
        );
        let review = serde_json::json!({
            "apiVersion": "authorization.k8s.io/v1",
            "kind": "SelfSubjectAccessReview",
            "spec": {"resourceAttributes": {
                "group": "tekton.dev",
                "resource": "pipelineruns",
                "verb": verb,
                "namespace": ns.as_str(),
            }},
        });
        let body = serde_json::to_vec(&review).map_err(|e| TektonError::Body {
            doing: doing.clone(),
            message: e.to_string(),
        })?;
        let req = kube_core::Request::new(SELF_REVIEWS)
            .create(&PostParams::default(), body)
            .map_err(|source| TektonError::Request {
                doing: doing.clone(),
                source,
            })?;
        let answer: serde_json::Value = self.send(token, &doing, req).await?;
        Ok(answer
            .pointer("/status/allowed")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false))
    }

    async fn send<T: DeserializeOwned>(
        &self,
        token: &BearerToken,
        doing: &str,
        req: http::Request<Vec<u8>>,
    ) -> Result<T, TektonError> {
        let response = self.response(token, doing, req).await?;
        response.json().await.map_err(|source| TektonError::Http {
            doing: doing.to_owned(),
            source,
        })
    }

    async fn response(
        &self,
        token: &BearerToken,
        doing: &str,
        req: http::Request<Vec<u8>>,
    ) -> Result<reqwest::Response, TektonError> {
        let url = format!("{}{}", self.api, req.uri());
        let (parts, body) = req.into_parts();
        let mut builder = self
            .http
            .request(parts.method, url)
            .bearer_auth(token.expose())
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(kind) = parts.headers.get(http::header::CONTENT_TYPE) {
            builder = builder.header(reqwest::header::CONTENT_TYPE, kind.clone());
        }
        if !body.is_empty() {
            builder = builder.body(body);
        }
        let response = builder.send().await.map_err(|source| TektonError::Http {
            doing: doing.to_owned(),
            source,
        })?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().await.unwrap_or_default();
        Err(TektonError::Status {
            doing: doing.to_owned(),
            status: status.as_u16(),
            body: body.chars().take(ERR_BODY_CHARS).collect(),
        })
    }
}

fn path(template: &str, ns: &Namespace) -> String {
    template.replace("{ns}", ns.as_str())
}

#[cfg(test)]
mod tests {
    use crate::tekton::{Cluster, Namespace, TektonError};

    #[test]
    fn namespaces_are_dns_labels() {
        assert!(Namespace::parse("open-data-hub-tenant").is_ok());
        for bad in ["", "-a", "a-", "A", "a_b", "a/b", &"a".repeat(64)] {
            assert!(Namespace::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_api_server_must_be_https() {
        let http = reqwest::Client::new();
        assert!(Cluster::new(http.clone(), "https://api.example.com:6443/").is_ok());
        assert!(matches!(
            Cluster::new(http.clone(), "http://api.example.com"),
            Err(TektonError::Api(_))
        ));
        assert!(matches!(
            Cluster::new(http, "not a url"),
            Err(TektonError::Api(_))
        ));
    }

    #[test]
    fn denial_and_transience_read_from_the_status() {
        let status = |status| TektonError::Status {
            doing: "x".into(),
            status,
            body: String::new(),
        };
        assert!(status(403).is_denied() && !status(403).is_transient());
        assert!(status(401).is_denied());
        assert!(status(503).is_transient() && status(429).is_transient());
        assert!(!status(404).is_transient() && !status(404).is_denied());
    }
}
