//! The GitHub Actions REST client: workflow-file reads, `workflow_dispatch` with the created run's
//! details, run and job reads, artifact and job-log downloads, and cancel. Image builds
//! ([`build`]) and playbook calls dispatch through it.
//!
//! ```text
//!   GET  /repos/{r}/commits/{ref}                       ref -> commit
//!   GET  /repos/{r}/contents/{workflow}?ref={commit}    workflow file -> [`workflow::Workflow`]
//!   POST /repos/{r}/actions/workflows/{file}/dispatches 200 {workflow_run_id, run_url, html_url}
//!   GET  /repos/{r}/actions/runs/{id}  (+ /jobs)        poll; a fresh run may 404 for a few seconds
//!   GET  /repos/{r}/actions/runs/{id}/artifacts         -> 302 signed URL, fetched without the token
//!   GET  /repos/{r}/actions/jobs/{job}/logs             -> 302 signed URL, fetched without the token
//!   POST /repos/{r}/actions/runs/{id}/cancel
//! ```

pub mod build;
pub mod workflow;

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;

use crate::github::build::BuildFailure;
use crate::github::workflow::WorkflowError;

const API: &str = "https://api.github.com";
/// The REST API version every request asks for. From this version a dispatch answers 200 with
/// the created run's details.
const API_VERSION: &str = "2026-03-10";
const USER_AGENT: &str = "crucible";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_WORKFLOW_BYTES: u64 = 1024 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
/// How much of a job's log [`Client::job_log`] keeps: its last bytes.
pub const MAX_JOB_LOG_BYTES: usize = 2 * 1024 * 1024;
const MAX_JOB_PAGES: u32 = 10;
const JOBS_PER_PAGE: u32 = 100;
const RUN_READ_ATTEMPTS: u32 = 5;
const RUN_READ_BACKOFF: Duration = Duration::from_secs(2);
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

/// Everything a GitHub Actions operation can fail with.
#[derive(Debug, thiserror::Error)]
pub enum GithubError {
    #[error("github repo {repo:?} must be \"owner/repo\"")]
    MalformedRepo { repo: String },
    #[error(transparent)]
    Build(#[from] BuildFailure),
    #[error("workflow {workflow} in {repo} at {git_ref}: {error}")]
    Workflow {
        repo: String,
        workflow: String,
        git_ref: String,
        error: Box<WorkflowError>,
    },
    #[error("{workflow} not found in {repo} at {git_ref}")]
    WorkflowNotFound {
        repo: String,
        workflow: String,
        git_ref: String,
    },
    /// GitHub answered with a status that is not success.
    #[error("GitHub answered {status} {doing}: {body}")]
    Api {
        doing: String,
        status: u16,
        body: String,
    },
    #[error("GitHub {doing}: {cause}")]
    Http {
        doing: String,
        cause: reqwest::Error,
    },
    /// GitHub answered success with a body this client cannot use.
    #[error("GitHub {doing}: {detail}")]
    Response { doing: String, detail: String },
}

impl GithubError {
    /// Whether the same operation may succeed when asked again later.
    pub fn is_transient(&self) -> bool {
        match self {
            GithubError::Http { .. } => true,
            GithubError::Api { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }

    fn http(doing: &str) -> impl FnOnce(reqwest::Error) -> Self {
        let doing = doing.to_owned();
        move |cause| GithubError::Http {
            doing,
            cause: cause.without_url(),
        }
    }

    fn response(doing: &str, detail: impl fmt::Display) -> Self {
        GithubError::Response {
            doing: doing.to_owned(),
            detail: detail.to_string(),
        }
    }
}

/// A repository as `owner/name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    owner: String,
    name: String,
}

impl Repo {
    /// Exactly one `/`, with both halves non-empty.
    pub fn parse(repo: &str) -> Result<Self, GithubError> {
        match repo.split_once('/') {
            Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/') => {
                Ok(Repo {
                    owner: owner.to_owned(),
                    name: name.to_owned(),
                })
            }
            _ => Err(GithubError::MalformedRepo {
                repo: repo.to_owned(),
            }),
        }
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for Repo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// A GitHub token, held in memory for the operations it is passed to. Never formatted.
pub struct Token(String);

impl Token {
    pub fn new(secret: impl Into<String>) -> Self {
        Token(secret.into())
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

/// A full commit sha.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitSha(String);

impl CommitSha {
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        (raw.len() == 40 && raw.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| CommitSha(raw.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first 12 characters, for messages.
    pub fn short(&self) -> &str {
        self.0.get(..12).unwrap_or(&self.0)
    }
}

impl fmt::Display for CommitSha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A workflow run's or job's `status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Requested,
    Queued,
    Pending,
    Waiting,
    InProgress,
    Completed,
    #[serde(other)]
    Unknown,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Requested => "requested",
            RunStatus::Queued => "queued",
            RunStatus::Pending => "pending",
            RunStatus::Waiting => "waiting",
            RunStatus::InProgress => "in_progress",
            RunStatus::Completed => "completed",
            RunStatus::Unknown => "an unknown status",
        }
    }
}

/// A completed workflow run's or job's `conclusion`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunConclusion {
    Success,
    Failure,
    Neutral,
    Cancelled,
    Skipped,
    TimedOut,
    ActionRequired,
    StartupFailure,
    Stale,
    #[serde(other)]
    Unknown,
}

impl RunConclusion {
    pub fn as_str(self) -> &'static str {
        match self {
            RunConclusion::Success => "success",
            RunConclusion::Failure => "failure",
            RunConclusion::Neutral => "neutral",
            RunConclusion::Cancelled => "cancelled",
            RunConclusion::Skipped => "skipped",
            RunConclusion::TimedOut => "timed_out",
            RunConclusion::ActionRequired => "action_required",
            RunConclusion::StartupFailure => "startup_failure",
            RunConclusion::Stale => "stale",
            RunConclusion::Unknown => "an unknown conclusion",
        }
    }
}

/// A workflow run as `GET .../actions/runs/{id}` answers.
#[derive(Clone, Debug, Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
    pub status: Option<RunStatus>,
    pub conclusion: Option<RunConclusion>,
    pub head_sha: String,
    #[serde(default)]
    pub html_url: String,
    /// The number GitHub shows for the run within its workflow, `#12`.
    #[serde(default)]
    pub run_number: Option<u64>,
}

/// One job as `GET .../actions/runs/{id}/jobs` answers.
#[derive(Clone, Debug, Deserialize)]
pub struct Job {
    pub id: u64,
    pub name: String,
    pub status: RunStatus,
    pub conclusion: Option<RunConclusion>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub html_url: Option<String>,
    #[serde(default)]
    pub steps: Vec<Step>,
}

/// One step of a [`Job`].
#[derive(Clone, Debug, Deserialize)]
pub struct Step {
    pub name: String,
    pub status: RunStatus,
    pub conclusion: Option<RunConclusion>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

/// The run a dispatch created.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct DispatchedRun {
    #[serde(rename = "workflow_run_id")]
    pub run_id: u64,
    pub run_url: String,
    pub html_url: String,
}

#[derive(Deserialize)]
struct Jobs {
    total_count: u64,
    jobs: Vec<Job>,
}

#[derive(Deserialize)]
struct Artifacts {
    artifacts: Vec<Artifact>,
}

#[derive(Deserialize)]
struct Artifact {
    id: u64,
    name: String,
    size_in_bytes: u64,
    #[serde(default)]
    expired: bool,
}

/// The GitHub Actions client. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Client {
    http: reqwest::Client,
}

impl Client {
    pub fn new() -> Result<Self, GithubError> {
        install_crypto_provider();
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(GithubError::http("building the client"))?;
        Ok(Client { http })
    }

    /// The commit `git_ref` names now.
    pub async fn resolve(
        &self,
        token: &Token,
        repo: &Repo,
        git_ref: &str,
    ) -> Result<CommitSha, GithubError> {
        let url = format!("{API}/repos/{repo}/commits/{}", encode(git_ref));
        let doing = "resolving the ref";
        let resp = self
            .send(self.http.get(url), token, SHA_ACCEPT, doing)
            .await?;
        let text = resp.text().await.map_err(GithubError::http(doing))?;
        CommitSha::parse(&text)
            .ok_or_else(|| GithubError::response(doing, "the answer is not a commit sha"))
    }

    /// The text of the file at `path` (`.github/workflows/x.yml`) at `git_ref`.
    pub async fn workflow_file(
        &self,
        token: &Token,
        repo: &Repo,
        path: &str,
        git_ref: &str,
    ) -> Result<String, GithubError> {
        let url = format!(
            "{API}/repos/{repo}/contents/{}?ref={}",
            encode(path),
            encode(git_ref)
        );
        let doing = "reading the workflow file";
        let resp = match self
            .send(self.http.get(url), token, RAW_ACCEPT, doing)
            .await
        {
            Err(GithubError::Api { status: 404, .. }) => {
                return Err(GithubError::WorkflowNotFound {
                    repo: repo.to_string(),
                    workflow: path.to_owned(),
                    git_ref: git_ref.to_owned(),
                });
            }
            other => other?,
        };
        let bytes = capped(resp, MAX_WORKFLOW_BYTES, doing).await?;
        String::from_utf8(bytes).map_err(|e| GithubError::response(doing, e))
    }

    /// Dispatch the workflow `file_name` on `git_ref` and return the run GitHub created.
    pub async fn dispatch(
        &self,
        token: &Token,
        repo: &Repo,
        file_name: &str,
        git_ref: &str,
        inputs: &BTreeMap<String, String>,
    ) -> Result<DispatchedRun, GithubError> {
        let url = format!(
            "{API}/repos/{repo}/actions/workflows/{}/dispatches",
            encode(file_name)
        );
        let body = serde_json::json!({"ref": git_ref, "inputs": inputs});
        let doing = "dispatching the workflow";
        let resp = self
            .send(self.http.post(url).json(&body), token, JSON_ACCEPT, doing)
            .await?;
        resp.json().await.map_err(GithubError::http(doing))
    }

    /// The run `run_id`. A run id GitHub just handed out can answer 404 for a few seconds, so a
    /// 404 is asked again before it is believed.
    pub async fn run(
        &self,
        token: &Token,
        repo: &Repo,
        run_id: u64,
    ) -> Result<WorkflowRun, GithubError> {
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

    /// The jobs of the run's latest attempt, at most [`MAX_JOB_PAGES`] pages of them.
    pub async fn jobs(
        &self,
        token: &Token,
        repo: &Repo,
        run_id: u64,
    ) -> Result<Vec<Job>, GithubError> {
        let doing = "listing the run's jobs";
        let mut jobs = Vec::new();
        for page in 1..=MAX_JOB_PAGES {
            let url = format!(
                "{API}/repos/{repo}/actions/runs/{run_id}/jobs?per_page={JOBS_PER_PAGE}&page={page}"
            );
            let resp = self
                .send(self.http.get(url), token, JSON_ACCEPT, doing)
                .await?;
            let batch: Jobs = resp.json().await.map_err(GithubError::http(doing))?;
            let done = batch.jobs.len() < JOBS_PER_PAGE as usize
                || jobs.len() as u64 + batch.jobs.len() as u64 >= batch.total_count;
            jobs.extend(batch.jobs);
            if done {
                break;
            }
        }
        Ok(jobs)
    }

    /// The zip of the run's unexpired artifact `name`; `None` when it has none.
    pub async fn artifact(
        &self,
        token: &Token,
        repo: &Repo,
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
        let listed: Artifacts = resp.json().await.map_err(GithubError::http(doing))?;
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
        let resp = self.redirected(url, token, doing).await?;
        capped(resp, MAX_ARTIFACT_BYTES, doing).await.map(Some)
    }

    /// The last [`MAX_JOB_LOG_BYTES`] of job `job_id`'s log.
    pub async fn job_log(
        &self,
        token: &Token,
        repo: &Repo,
        job_id: u64,
    ) -> Result<String, GithubError> {
        let doing = "downloading the job's log";
        let url = format!("{API}/repos/{repo}/actions/jobs/{job_id}/logs");
        let resp = self.redirected(url, token, doing).await?;
        let bytes = tail(resp, MAX_JOB_LOG_BYTES, doing).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Ask GitHub to cancel the run. A run that already completed is left as it is.
    pub async fn cancel(&self, token: &Token, repo: &Repo, run_id: u64) -> Result<(), GithubError> {
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

    /// One request with the token. Success, or a redirect for the callers that expect one.
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

    /// GET `url` with the token, then the signed URL it redirects to without it.
    async fn redirected(
        &self,
        url: String,
        token: &Token,
        doing: &str,
    ) -> Result<reqwest::Response, GithubError> {
        let resp = self
            .send(self.http.get(url), token, JSON_ACCEPT, doing)
            .await?;
        if !resp.status().is_redirection() {
            return Ok(resp);
        }
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| GithubError::response(doing, "a redirect with no location"))?;
        let request = self.signed(location, doing)?;
        self.http
            .execute(request)
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(GithubError::http(doing))
    }

    /// The request for a signed download URL: https only, and carrying no credential.
    fn signed(&self, location: &str, doing: &str) -> Result<reqwest::Request, GithubError> {
        let url = reqwest::Url::parse(location)
            .map_err(|e| GithubError::response(doing, format!("the redirect is not a URL: {e}")))?;
        if url.scheme() != "https" {
            return Err(GithubError::response(
                doing,
                "the redirect is not to an https URL",
            ));
        }
        self.http.get(url).build().map_err(GithubError::http(doing))
    }
}

fn encode(path: &str) -> String {
    utf8_percent_encode(path, PATH).to_string()
}

/// rustls 0.23 picks no CryptoProvider when several are linked; choose one before any TLS
/// handshake. A second install returns Err, which is fine.
fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
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

/// The last `keep` bytes of the body.
async fn tail(
    mut resp: reqwest::Response,
    keep: usize,
    doing: &str,
) -> Result<Vec<u8>, GithubError> {
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(GithubError::http(doing))? {
        buf.extend_from_slice(&chunk);
        keep_last(&mut buf, keep);
    }
    Ok(buf)
}

fn keep_last(buf: &mut Vec<u8>, keep: usize) {
    if buf.len() > keep {
        buf.drain(..buf.len() - keep);
    }
}

#[cfg(test)]
mod tests {
    use crate::github::*;

    #[test]
    fn a_token_never_formats() {
        let token = Token::new("ghp_secretvalue");
        assert_eq!(format!("{token:?}"), "Token(..)");
    }

    #[test]
    fn a_repo_is_owner_and_name() {
        let repo = Repo::parse("neuralmagic/crucible").unwrap();
        assert_eq!((repo.owner(), repo.name()), ("neuralmagic", "crucible"));
        assert_eq!(repo.to_string(), "neuralmagic/crucible");
        for bad in ["crucible", "/crucible", "neuralmagic/", "a/b/c", ""] {
            assert!(
                matches!(Repo::parse(bad), Err(GithubError::MalformedRepo { .. })),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_commit_sha_is_forty_hex_characters() {
        let sha = CommitSha::parse(" 0123456789ABCDEF0123456789abcdef01234567\n").unwrap();
        assert_eq!(sha.as_str(), "0123456789abcdef0123456789abcdef01234567");
        assert_eq!(sha.short(), "0123456789ab");
        assert!(CommitSha::parse("0123456789abcdef").is_none());
        assert!(CommitSha::parse(&"g".repeat(40)).is_none());
    }

    #[test]
    fn transient_errors_are_transport_rate_limits_and_server_faults() {
        let api = |status| GithubError::Api {
            doing: "x".into(),
            status,
            body: String::new(),
        };
        for status in [429, 500, 502] {
            assert!(api(status).is_transient(), "{status}");
        }
        for status in [401, 403, 404, 409, 422] {
            assert!(!api(status).is_transient(), "{status}");
        }
        assert!(!GithubError::response("x", "y").is_transient());
    }

    #[test]
    fn the_dispatch_answer_carries_the_run() {
        let created: DispatchedRun = serde_json::from_value(serde_json::json!({
            "workflow_run_id": 42,
            "run_url": "https://api.github.com/repos/o/r/actions/runs/42",
            "html_url": "https://github.com/o/r/actions/runs/42",
        }))
        .unwrap();
        assert_eq!(created.run_id, 42);
        assert!(created.html_url.ends_with("/runs/42"));
    }

    #[test]
    fn unknown_statuses_and_conclusions_still_parse() {
        let wire: Vec<RunConclusion> =
            serde_json::from_value(serde_json::json!(["success", "timed_out", "brand_new"]))
                .unwrap();
        assert_eq!(
            wire,
            [
                RunConclusion::Success,
                RunConclusion::TimedOut,
                RunConclusion::Unknown
            ]
        );
        let status: RunStatus = serde_json::from_value(serde_json::json!("in_progress")).unwrap();
        assert_eq!(status, RunStatus::InProgress);
        let odd: RunStatus = serde_json::from_value(serde_json::json!("parked")).unwrap();
        assert_eq!(odd, RunStatus::Unknown);
    }

    #[test]
    fn a_signed_download_carries_no_credential_and_must_be_https() {
        let client = Client::new().unwrap();
        let request = client
            .signed(
                "https://pipelines.actions.githubusercontent.com/x/logs?sig=abc",
                "downloading",
            )
            .unwrap();
        assert!(
            !request
                .headers()
                .contains_key(reqwest::header::AUTHORIZATION)
        );
        assert_eq!(
            request.url().host_str(),
            Some("pipelines.actions.githubusercontent.com")
        );
        assert!(client.signed("http://example.com/x", "d").is_err());
        assert!(client.signed("not a url", "d").is_err());
    }

    #[test]
    fn a_log_keeps_its_last_bytes() {
        let mut buf = b"0123456789".to_vec();
        keep_last(&mut buf, 4);
        assert_eq!(buf, b"6789");
        let mut short = b"ab".to_vec();
        keep_last(&mut short, 4);
        assert_eq!(short, b"ab");
    }

    #[test]
    fn paths_are_encoded_but_keep_their_slashes() {
        assert_eq!(
            encode(".github/workflows/a b.yml"),
            ".github/workflows/a%20b.yml"
        );
        assert_eq!(encode("feature/x#1"), "feature/x%231");
    }

    /// Reads a public workflow file and its ref with your gh login.
    /// `CRUCIBLE_GH_LIVE=1 GH_TOKEN=$(gh auth token) cargo test -p forge github -- --ignored`
    #[tokio::test]
    #[ignore = "reads GitHub; set CRUCIBLE_GH_LIVE=1 and GH_TOKEN"]
    async fn live_reads_of_a_workflow() {
        if std::env::var("CRUCIBLE_GH_LIVE").as_deref() != Ok("1") {
            return;
        }
        let token = Token::new(std::env::var("GH_TOKEN").unwrap());
        let client = Client::new().unwrap();
        let repo = Repo::parse("wseaton/snapshotd").unwrap();
        let commit = client
            .resolve(&token, &repo, "crucible-call-demo")
            .await
            .unwrap();
        let text = client
            .workflow_file(
                &token,
                &repo,
                ".github/workflows/crucible-call-demo.yml",
                commit.as_str(),
            )
            .await
            .unwrap();
        let workflow = crate::github::workflow::Workflow::parse(&text).unwrap();
        assert!(!workflow.jobs.is_empty());
    }
}
