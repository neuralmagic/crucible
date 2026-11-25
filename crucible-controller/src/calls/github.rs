//! The `github_workflow` call target (RFC-0008): the interface read from the workflow file at a
//! commit, dispatch with run details, progress from the run's jobs, the outcome read from the
//! declared artifact, and cancel.

pub mod artifact;
mod backend;
pub mod progress;
pub mod workflow;

use std::collections::BTreeMap;
use std::time::Duration;

use crucible_contract::calls::{
    CallStatus, CallTarget, FileRef, GitRefName, GithubOutputs, GithubRepo, LockedCall, NodeLink,
    Pin, ProgressGraph, TargetHandle, TargetInterface, WorkflowPath,
};
use crucible_contract::link::ExternalUrl;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::calls::GithubToken;
use crate::calls::github::artifact::{ResultFile, read_result};
use crate::calls::github::progress::{ApiJob, ApiRun, RunStatus, Settle, run_graph, settle};
use crate::calls::github::workflow::{Workflow, WorkflowError, dispatch_inputs};

const API: &str = "https://api.github.com";
const API_VERSION: &str = "2022-11-28";
const USER_AGENT: &str = "crucible-controller";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const GH_CLI_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_WORKFLOW_BYTES: u64 = 512 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_JOB_PAGES: u32 = 10;
const RUN_READ_ATTEMPTS: u32 = 5;
const RUN_READ_BACKOFF: Duration = Duration::from_secs(2);
const JOBS_PER_PAGE: u32 = 100;
const ERR_BODY_CHARS: usize = 300;

const JSON_ACCEPT: &str = "application/vnd.github+json";
const RAW_ACCEPT: &str = "application/vnd.github.raw";
const SHA_ACCEPT: &str = "application/vnd.github.sha";

/// Path characters left as they are: unreserved ones and `/`.
const PATH: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~')
    .remove(b'/');

/// Why a GitHub target operation failed.
#[derive(Debug, thiserror::Error)]
pub enum GithubError {
    #[error(
        "dispatching through the GitHub App installation is not supported by this controller; \
         run the local controller, which dispatches with your gh login"
    )]
    AppInstallation,
    #[error("`gh auth token` failed: {0}")]
    GhCli(String),
    #[error("{0} is not a github_workflow target")]
    WrongKind(String),
    #[error("{0}")]
    Refused(String),
    #[error("workflow {workflow} at {commit}: {error}")]
    Workflow {
        workflow: String,
        commit: String,
        error: WorkflowError,
    },
    #[error("GitHub answered {status} {doing}: {body}")]
    Api {
        doing: String,
        status: u16,
        body: String,
    },
    #[error("GitHub {doing}: {source}")]
    Http {
        doing: String,
        source: reqwest::Error,
    },
    #[error("GitHub {doing}: {detail}")]
    Response { doing: String, detail: String },
}

impl GithubError {
    /// Whether retrying the same operation later may succeed.
    pub fn is_transient(&self) -> bool {
        match self {
            GithubError::Http { .. } | GithubError::GhCli(_) => true,
            GithubError::Api { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }

    fn http(doing: &str) -> impl FnOnce(reqwest::Error) -> Self {
        let doing = doing.to_owned();
        move |source| GithubError::Http {
            doing,
            source: source.without_url(),
        }
    }

    fn response(doing: &str, detail: impl std::fmt::Display) -> Self {
        GithubError::Response {
            doing: doing.to_owned(),
            detail: detail.to_string(),
        }
    }
}

/// A GitHub token, held in memory for one operation. Never formatted.
struct Token(String);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

#[derive(Deserialize)]
struct ApiJobs {
    total_count: u64,
    jobs: Vec<ApiJob>,
}

#[derive(Deserialize)]
struct ApiArtifacts {
    artifacts: Vec<ApiArtifact>,
}

#[derive(Deserialize)]
struct ApiArtifact {
    id: u64,
    name: String,
    size_in_bytes: u64,
    #[serde(default)]
    expired: bool,
}

/// The body of a dispatch with `return_run_details`.
#[derive(Deserialize)]
struct Dispatched {
    workflow_run_id: u64,
    html_url: String,
}

/// What a dispatch created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubDispatched {
    pub handle: TargetHandle,
    pub link: Option<NodeLink>,
    /// The commit the run runs on.
    pub head_sha: String,
}

/// A completed run's settlement.
#[derive(Clone, Debug, PartialEq)]
pub struct GithubOutcome {
    pub status: CallStatus,
    pub note: String,
    /// The declared fields once passed; null otherwise.
    pub result: Value,
    pub files: Vec<ResultFile>,
}

/// One poll of a run: its progress, and its outcome once it completed.
#[derive(Clone, Debug, PartialEq)]
pub struct RunPoll {
    pub progress: ProgressGraph,
    pub head_sha: String,
    pub outcome: Option<GithubOutcome>,
}

/// The `github_workflow` target backend.
pub struct GithubWorkflowBackend {
    token: GithubToken,
    http: reqwest::Client,
}

impl GithubWorkflowBackend {
    pub fn new(token: GithubToken) -> Result<Self, GithubError> {
        crate::install_crypto_provider();
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(GithubError::http("building the client"))?;
        Ok(GithubWorkflowBackend { token, http })
    }

    /// The target's pin (the ref's commit now) and its interface at that commit.
    pub async fn interface(&self, target: &CallTarget) -> Result<LockedCall, GithubError> {
        let (repo, workflow, git_ref, outputs) = github_target(target)?;
        let token = self.token().await?;
        let commit = self.resolve(&token, repo, git_ref.as_str()).await?;
        let interface = self
            .interface_at(&token, repo, workflow, &commit, outputs)
            .await?;
        let pin = Pin::GithubRef {
            repo: repo.clone(),
            workflow: workflow.clone(),
            git_ref: git_ref.clone(),
            commit,
        };
        LockedCall::new(pin, interface)
            .map_err(|e| GithubError::response("hashing the interface", e))
    }

    /// Dispatch one run of `target` on its ref with `values`, after checking the interface at
    /// the ref still is the locked one.
    pub async fn dispatch(
        &self,
        target: &CallTarget,
        locked: &LockedCall,
        values: &BTreeMap<String, Value>,
        files: &BTreeMap<String, FileRef>,
    ) -> Result<GithubDispatched, GithubError> {
        let (repo, workflow, git_ref, outputs) = github_target(target)?;
        if !locked.pin.pins(target) {
            return Err(GithubError::Refused(format!(
                "calls.lock pins a different target than {repo} {} at {}",
                workflow.as_str(),
                git_ref.as_str()
            )));
        }
        let inputs = dispatch_inputs(&locked.interface.params, values, files)?;
        let token = self.token().await?;
        let commit = self.resolve(&token, repo, git_ref.as_str()).await?;
        let current = self
            .interface_at(&token, repo, workflow, &commit, outputs)
            .await?;
        if let Some(moved) = difference(&locked.interface, &current) {
            return Err(GithubError::Refused(format!(
                "{repo} {} at {} (commit {}) {moved} than calls.lock records",
                workflow.as_str(),
                git_ref.as_str(),
                short(&commit)
            )));
        }
        let url = format!(
            "{API}/repos/{repo}/actions/workflows/{}/dispatches",
            encode(workflow.file_name())
        );
        let body = json!({"ref": git_ref.as_str(), "inputs": inputs, "return_run_details": true});
        let doing = "dispatching the workflow";
        let resp = self
            .send(self.http.post(url).json(&body), &token, JSON_ACCEPT, doing)
            .await?;
        let created: Dispatched = resp.json().await.map_err(GithubError::http(doing))?;
        let run = self.run(&token, repo, created.workflow_run_id).await?;
        if run.head_sha != commit {
            let at_head = self
                .interface_at(&token, repo, workflow, &run.head_sha, outputs)
                .await?;
            if let Some(moved) = difference(&locked.interface, &at_head) {
                self.cancel_with(&token, repo, run.id).await?;
                return Err(GithubError::Refused(format!(
                    "{} moved to commit {} during dispatch, whose workflow {moved} than \
                     calls.lock records; run {} was cancelled",
                    git_ref.as_str(),
                    short(&run.head_sha),
                    run.id
                )));
            }
        }
        Ok(GithubDispatched {
            handle: TargetHandle::GithubWorkflow {
                repo: repo.clone(),
                run_id: created.workflow_run_id,
            },
            link: ExternalUrl::new(created.html_url)
                .ok()
                .map(|url| NodeLink::External { url }),
            head_sha: run.head_sha,
        })
    }

    /// The run's progress over the locked `graph`, and its outcome once it completed.
    pub async fn poll(
        &self,
        repo: &GithubRepo,
        run_id: u64,
        graph: &ProgressGraph,
        outputs: &GithubOutputs,
        caller_cancelled: bool,
    ) -> Result<RunPoll, GithubError> {
        let token = self.token().await?;
        let run = self.run(&token, repo, run_id).await?;
        let jobs = self.jobs(&token, repo, run_id).await?;
        let progress = run_graph(graph, &jobs);
        let outcome = match run.status {
            Some(RunStatus::Completed) => Some(
                self.outcome(&token, repo, &run, outputs, caller_cancelled)
                    .await?,
            ),
            _ => None,
        };
        Ok(RunPoll {
            progress,
            head_sha: run.head_sha,
            outcome,
        })
    }

    /// Ask GitHub to cancel the run. A run that already completed is left as it is.
    pub async fn cancel(&self, repo: &GithubRepo, run_id: u64) -> Result<(), GithubError> {
        let token = self.token().await?;
        self.cancel_with(&token, repo, run_id).await
    }

    async fn cancel_with(
        &self,
        token: &Token,
        repo: &GithubRepo,
        run_id: u64,
    ) -> Result<(), GithubError> {
        let url = format!("{API}/repos/{repo}/actions/runs/{run_id}/cancel");
        match self
            .send(
                self.http.post(url),
                token,
                JSON_ACCEPT,
                "cancelling the run",
            )
            .await
        {
            Err(GithubError::Api { status: 409, .. }) | Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn outcome(
        &self,
        token: &Token,
        repo: &GithubRepo,
        run: &ApiRun,
        outputs: &GithubOutputs,
        caller_cancelled: bool,
    ) -> Result<GithubOutcome, GithubError> {
        let failed = |note: String| GithubOutcome {
            status: CallStatus::Failed,
            note,
            result: Value::Null,
            files: Vec::new(),
        };
        match settle(run.conclusion, caller_cancelled) {
            Settle::Fail(note) => Ok(failed(note)),
            Settle::Cancelled => Ok(GithubOutcome {
                status: CallStatus::Cancelled,
                note: "the call was cancelled".to_owned(),
                result: Value::Null,
                files: Vec::new(),
            }),
            Settle::Pass { read_result: false } => Ok(GithubOutcome {
                status: CallStatus::Passed,
                note: String::new(),
                result: json!({}),
                files: Vec::new(),
            }),
            Settle::Pass { read_result: true } => {
                let Some(zip) = self
                    .artifact(token, repo, run.id, &outputs.artifact)
                    .await?
                else {
                    return Ok(failed(format!(
                        "the run uploaded no artifact {:?}",
                        outputs.artifact
                    )));
                };
                Ok(match read_result(&zip, outputs) {
                    Ok((result, files)) => GithubOutcome {
                        status: CallStatus::Passed,
                        note: String::new(),
                        result,
                        files,
                    },
                    Err(note) => failed(note),
                })
            }
        }
    }

    async fn token(&self) -> Result<Token, GithubError> {
        match &self.token {
            GithubToken::GhCli => gh_cli_token().await,
            GithubToken::AppInstallation => Err(GithubError::AppInstallation),
        }
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        token: &Token,
        accept: &str,
        doing: &str,
    ) -> Result<reqwest::Response, GithubError> {
        let resp = request
            .bearer_auth(&token.0)
            .header(reqwest::header::ACCEPT, accept)
            .header("X-GitHub-Api-Version", API_VERSION)
            .send()
            .await
            .map_err(GithubError::http(doing))?;
        let status = resp.status();
        if status.is_success() || status.is_redirection() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        Err(GithubError::Api {
            doing: doing.to_owned(),
            status: status.as_u16(),
            body: body.chars().take(ERR_BODY_CHARS).collect(),
        })
    }

    async fn resolve(
        &self,
        token: &Token,
        repo: &GithubRepo,
        git_ref: &str,
    ) -> Result<String, GithubError> {
        let url = format!("{API}/repos/{repo}/commits/{}", encode(git_ref));
        let doing = "resolving the ref";
        let resp = self
            .send(self.http.get(url), token, SHA_ACCEPT, doing)
            .await?;
        let sha = resp.text().await.map_err(GithubError::http(doing))?;
        let sha = sha.trim();
        match sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
            true => Ok(sha.to_owned()),
            false => Err(GithubError::response(
                doing,
                "the answer is not a commit sha",
            )),
        }
    }

    async fn interface_at(
        &self,
        token: &Token,
        repo: &GithubRepo,
        workflow: &WorkflowPath,
        commit: &str,
        outputs: &GithubOutputs,
    ) -> Result<TargetInterface, GithubError> {
        let url = format!(
            "{API}/repos/{repo}/contents/{}?ref={commit}",
            encode(workflow.as_str())
        );
        let doing = "reading the workflow file";
        let resp = self
            .send(self.http.get(url), token, RAW_ACCEPT, doing)
            .await?;
        let bytes = capped(resp, MAX_WORKFLOW_BYTES, doing).await?;
        let text = String::from_utf8(bytes).map_err(|e| GithubError::response(doing, e))?;
        let parsed = Workflow::parse(&text).map_err(|error| GithubError::Workflow {
            workflow: workflow.as_str().to_owned(),
            commit: short(commit).to_owned(),
            error,
        })?;
        Ok(parsed.interface(outputs))
    }

    /// The run GitHub named. A run id GitHub just handed out can answer 404 for a few seconds,
    /// so a 404 is asked again before it is believed.
    async fn run(
        &self,
        token: &Token,
        repo: &GithubRepo,
        run_id: u64,
    ) -> Result<ApiRun, GithubError> {
        let url = format!("{API}/repos/{repo}/actions/runs/{run_id}");
        let doing = "reading the run";
        let mut attempt = 1;
        loop {
            match self
                .send(self.http.get(&url), token, JSON_ACCEPT, doing)
                .await
            {
                Ok(resp) => return resp.json().await.map_err(GithubError::http(doing)),
                Err(GithubError::Api { status: 404, .. }) if attempt < RUN_READ_ATTEMPTS => {
                    tokio::time::sleep(RUN_READ_BACKOFF * attempt).await;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn jobs(
        &self,
        token: &Token,
        repo: &GithubRepo,
        run_id: u64,
    ) -> Result<Vec<ApiJob>, GithubError> {
        let doing = "listing the run's jobs";
        let mut jobs = Vec::new();
        for page in 1..=MAX_JOB_PAGES {
            let url = format!(
                "{API}/repos/{repo}/actions/runs/{run_id}/jobs?per_page={JOBS_PER_PAGE}&page={page}"
            );
            let resp = self
                .send(self.http.get(url), token, JSON_ACCEPT, doing)
                .await?;
            let batch: ApiJobs = resp.json().await.map_err(GithubError::http(doing))?;
            let done = batch.jobs.len() < JOBS_PER_PAGE as usize
                || jobs.len() as u64 + batch.jobs.len() as u64 >= batch.total_count;
            jobs.extend(batch.jobs);
            if done {
                break;
            }
        }
        Ok(jobs)
    }

    /// The zip of the run's artifact named `name`; `None` when the run has no such artifact.
    async fn artifact(
        &self,
        token: &Token,
        repo: &GithubRepo,
        run_id: u64,
        name: &str,
    ) -> Result<Option<Vec<u8>>, GithubError> {
        let doing = "listing the run's artifacts";
        let url = format!(
            "{API}/repos/{repo}/actions/runs/{run_id}/artifacts?per_page=100&name={}",
            encode(name)
        );
        let resp = self
            .send(self.http.get(url), token, JSON_ACCEPT, doing)
            .await?;
        let listed: ApiArtifacts = resp.json().await.map_err(GithubError::http(doing))?;
        let Some(artifact) = listed
            .artifacts
            .into_iter()
            .find(|a| a.name == name && !a.expired)
        else {
            return Ok(None);
        };
        let doing = "downloading the artifact";
        if artifact.size_in_bytes > MAX_ARTIFACT_BYTES {
            return Err(GithubError::response(
                doing,
                format!(
                    "{} bytes is over the {MAX_ARTIFACT_BYTES} byte limit",
                    artifact.size_in_bytes
                ),
            ));
        }
        let url = format!("{API}/repos/{repo}/actions/artifacts/{}/zip", artifact.id);
        let resp = self
            .send(self.http.get(url), token, JSON_ACCEPT, doing)
            .await?;
        let resp = match resp.status().is_redirection() {
            false => resp,
            true => {
                let location = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| GithubError::response(doing, "a redirect with no location"))?
                    .to_owned();
                self.http
                    .get(location)
                    .send()
                    .await
                    .and_then(reqwest::Response::error_for_status)
                    .map_err(GithubError::http(doing))?
            }
        };
        capped(resp, MAX_ARTIFACT_BYTES, doing).await.map(Some)
    }
}

fn github_target(
    target: &CallTarget,
) -> Result<(&GithubRepo, &WorkflowPath, &GitRefName, &GithubOutputs), GithubError> {
    match target {
        CallTarget::GithubWorkflow {
            repo,
            workflow,
            git_ref,
            outputs,
        } => Ok((repo, workflow, git_ref, outputs)),
        other => Err(GithubError::WrongKind(other.kind().to_string())),
    }
}

/// Which parts of `now` differ from `locked`, as a phrase ("has different params and graph").
fn difference(locked: &TargetInterface, now: &TargetInterface) -> Option<String> {
    let mut parts = Vec::new();
    if locked.params != now.params {
        parts.push("params");
    }
    if locked.result != now.result || locked.result_files != now.result_files {
        parts.push("result");
    }
    if locked.graph != now.graph {
        parts.push("graph");
    }
    match parts.as_slice() {
        [] => None,
        parts => Some(format!("has a different {}", parts.join(" and "))),
    }
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

fn encode(path: &str) -> String {
    utf8_percent_encode(path, PATH).to_string()
}

async fn capped(
    mut resp: reqwest::Response,
    cap: u64,
    doing: &str,
) -> Result<Vec<u8>, GithubError> {
    let over = || GithubError::response(doing, format!("the body is over {cap} bytes"));
    if resp.content_length().is_some_and(|len| len > cap) {
        return Err(over());
    }
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(GithubError::http(doing))? {
        if buf.len() as u64 + chunk.len() as u64 > cap {
            return Err(over());
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// The gh CLI's token for github.com, read now and held only by the caller.
async fn gh_cli_token() -> Result<Token, GithubError> {
    let run = tokio::process::Command::new("gh")
        .args(["auth", "token", "--hostname", "github.com"])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(GH_CLI_TIMEOUT, run)
        .await
        .map_err(|_| GithubError::GhCli(format!("no answer in {GH_CLI_TIMEOUT:?}")))?
        .map_err(|e| GithubError::GhCli(e.to_string()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(GithubError::GhCli(
            stderr.trim().chars().take(ERR_BODY_CHARS).collect(),
        ));
    }
    let token = String::from_utf8(out.stdout)
        .map_err(|_| GithubError::GhCli("the token is not UTF-8".to_owned()))?;
    let token = token.trim();
    match token.is_empty() || token.contains(char::is_whitespace) {
        true => Err(GithubError::GhCli(
            "gh printed no token; run `gh auth login`".to_owned(),
        )),
        false => Ok(Token(token.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crucible_contract::calls::{
        CallStatus, CallTarget, GithubRepo, ProgressGraph, TargetHandle,
    };
    use serde_json::json;

    use crate::calls::GithubToken;
    use crate::calls::github::{GithubError, GithubWorkflowBackend, Token};

    #[test]
    fn a_token_never_formats() {
        let token = Token("ghp_secretvalue".to_owned());
        assert_eq!(format!("{token:?}"), "Token(..)");
    }

    #[tokio::test]
    async fn the_app_installation_path_is_refused_clearly() {
        let backend = GithubWorkflowBackend::new(GithubToken::AppInstallation).unwrap();
        let target: CallTarget = serde_json::from_value(json!({
            "kind": "github_workflow",
            "repo": "o/r",
            "workflow": ".github/workflows/x.yml",
            "ref": "main",
            "outputs": {"artifact": "a"},
        }))
        .unwrap();
        let err = backend.interface(&target).await.unwrap_err();
        assert!(matches!(err, GithubError::AppInstallation));
        assert!(err.to_string().contains("not supported"));
        let playbook: CallTarget =
            serde_json::from_value(json!({"kind": "playbook", "playbook": "triage"})).unwrap();
        assert!(matches!(
            backend.interface(&playbook).await,
            Err(GithubError::WrongKind(kind)) if kind == "playbook"
        ));
    }

    /// Dispatches the demo workflow with your gh login and waits for it.
    /// `CRUCIBLE_GH_LIVE=1 cargo test -p crucible-controller calls::github -- --ignored`
    #[tokio::test]
    #[ignore = "dispatches a real workflow; set CRUCIBLE_GH_LIVE=1"]
    async fn live_dispatch_of_the_demo_workflow() {
        if std::env::var("CRUCIBLE_GH_LIVE").as_deref() != Ok("1") {
            return;
        }
        let target: CallTarget = serde_json::from_value(json!({
            "kind": "github_workflow",
            "repo": "wseaton/snapshotd",
            "workflow": ".github/workflows/crucible-call-demo.yml",
            "ref": "crucible-call-demo",
            "outputs": {
                "artifact": "crucible-result",
                "fields": {"summary": "string", "flavor": "string", "loud": "boolean", "legs": "integer", "payload_keys": "list"},
                "files": ["notes.md"],
            },
        }))
        .unwrap();
        let CallTarget::GithubWorkflow { outputs, .. } = &target else {
            unreachable!()
        };
        let backend = GithubWorkflowBackend::new(GithubToken::GhCli).unwrap();
        let locked = backend.interface(&target).await.unwrap();
        assert_eq!(
            locked.interface.params["required"],
            json!(["message"]),
            "{}",
            locked.interface.params
        );
        let ids: Vec<&str> = locked
            .interface
            .graph
            .nodes
            .iter()
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(ids, ["prepare", "build", "report"]);

        let values = BTreeMap::from([
            ("message".to_owned(), json!("hello from crucible")),
            ("flavor".to_owned(), json!("thorough")),
            ("loud".to_owned(), json!(true)),
            ("payload".to_owned(), json!({"alpha": 1, "beta": [2]})),
        ]);
        let dispatched = backend
            .dispatch(&target, &locked, &values, &BTreeMap::new())
            .await
            .unwrap();
        let TargetHandle::GithubWorkflow { repo, run_id } = &dispatched.handle else {
            panic!("not a github handle");
        };
        assert_eq!(repo, &GithubRepo::parse("wseaton/snapshotd").unwrap());
        eprintln!("dispatched run {run_id} on {}", dispatched.head_sha);

        let mut seen_legs = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(900);
        let outcome = loop {
            let poll = backend
                .poll(repo, *run_id, &locked.interface.graph, outputs, false)
                .await
                .unwrap();
            seen_legs |= poll
                .progress
                .nodes
                .iter()
                .filter(|n| n.group.as_deref() == Some("build"))
                .count()
                == 2;
            let statuses: Vec<String> = poll
                .progress
                .nodes
                .iter()
                .map(|n| format!("{}={}", n.id, n.status.as_str()))
                .collect();
            eprintln!("{}", statuses.join(" "));
            if let Some(outcome) = poll.outcome {
                break outcome;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the run did not finish"
            );
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        };
        assert!(seen_legs, "the matrix legs were never drawn as a group");
        assert_eq!(outcome.status, CallStatus::Passed, "{}", outcome.note);
        assert_eq!(
            outcome.result,
            json!({
                "summary": "THOROUGH BUILD OF 'HELLO FROM CRUCIBLE' OVER 2 LEGS",
                "flavor": "thorough",
                "loud": true,
                "legs": 2,
                "payload_keys": ["alpha", "beta"],
            })
        );
        assert_eq!(outcome.files.len(), 1);
        assert!(String::from_utf8_lossy(&outcome.files[0].bytes).contains("hello from crucible"));

        let unused = ProgressGraph::default();
        let again = backend
            .poll(repo, *run_id, &unused, outputs, false)
            .await
            .unwrap();
        assert!(again.outcome.is_some());
        backend.cancel(repo, *run_id).await.unwrap();
    }
}
