//! The `gitlab_pipeline` call target over crucible-ci's GitLab client: the interface read from a
//! project's CI configuration at a commit, pipeline creation with typed inputs, progress from the
//! pipeline's jobs, the outcome read from a declared job's artifacts, job logs, and cancel.
//!
//! ```text
//! lock      GET  commits/{ref} ─▶ sha;  files/.gitlab-ci.yml@sha ─▶ inputs;  ci/lint@sha ─▶ graph
//! dispatch  POST pipeline {ref, inputs}            ─▶ handle {host, project, pipeline_id}
//! poll      GET  pipelines/{id} + jobs + bridges   ─▶ progress, outcome
//! result    GET  jobs/{job}/artifacts/result.json  ─▶ typed fields, files
//! ```

mod backend;
pub mod interface;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crucible_ci::commit::CommitId;
use crucible_ci::gitlab::access::{Runner, refused_by};
use crucible_ci::gitlab::graph::{PipelineState, pipeline_graph, pipeline_state};
use crucible_ci::gitlab::model::{Job, Pipeline};
use crucible_ci::gitlab::{Gitlab, GitlabError, ProjectPath};
use crucible_ci::token::BearerToken;
use crucible_contract::calls::{
    CallBudget, CallStatus, CallTarget, FileRef, GITLAB_RESULT_FILE, GitRefName, GitlabOutputs,
    LockedCall, MAX_FILE_PARAM_BYTES, NamedLink, NodeLink, Pin, ProgressGraph, TargetHandle,
    TargetInterface,
};
use crucible_contract::emits::value_type;
use crucible_contract::link::ExternalUrl;
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use crate::authz::model::Principal;
use crate::calls::github::artifact::{ResultFile, field_refusal};
use crate::calls::gitlab::interface::{interface, pipeline_inputs, shape_of};
use crate::identity::forge::ForgeHost;
use crate::identity::forge::held::SignedIn;

const CI_CONFIG: &str = ".gitlab-ci.yml";
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
const DEVELOPER: u32 = 30;
const MAX_LOG_BYTES: usize = 2 * 1024 * 1024;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const GLAB_TIMEOUT: Duration = Duration::from_secs(20);
const ERR_BODY_CHARS: usize = 300;

/// Why a GitLab target operation failed.
#[derive(Debug, thiserror::Error)]
pub enum TargetError {
    #[error(transparent)]
    Gitlab(#[from] GitlabError),
    #[error("{0} is not a gitlab_pipeline target")]
    WrongKind(String),
    #[error("{0}")]
    Refused(String),
    #[error("reading the GitLab token for {host}: {detail}")]
    Token { host: String, detail: String },
    #[error(
        "`glab` gave no token for {host}: {detail}; sign in to GitLab in the desktop app or run `glab auth login`"
    )]
    Glab { host: String, detail: String },
}

impl TargetError {
    pub fn is_transient(&self) -> bool {
        match self {
            TargetError::Gitlab(e) => e.is_transient(),
            TargetError::Token { .. } | TargetError::Glab { .. } => true,
            TargetError::WrongKind(_) | TargetError::Refused(_) => false,
        }
    }
}

/// Where the backend takes the token for a GitLab host.
#[derive(Clone, Debug)]
pub(crate) enum TokenSource {
    Person(SignedIn),
    Bot(BTreeMap<ForgeHost, PathBuf>),
}

/// What a dispatch created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitlabDispatched {
    pub handle: TargetHandle,
    pub link: Option<NodeLink>,
    pub commit: String,
}

/// A finished pipeline's settlement.
#[derive(Clone, Debug, PartialEq)]
pub struct GitlabOutcome {
    pub status: CallStatus,
    pub note: String,
    pub result: Value,
    pub files: Vec<ResultFile>,
}

/// One look at a pipeline: its progress, its number, and its outcome once it ended.
#[derive(Clone, Debug, PartialEq)]
pub struct PipelinePoll {
    pub progress: ProgressGraph,
    pub number: Option<u64>,
    pub outcome: Option<GitlabOutcome>,
    pub links: Vec<NamedLink>,
}

/// A target's parts, checked.
pub(crate) struct Place<'a> {
    pub host: ForgeHost,
    pub project: ProjectPath,
    pub git_ref: &'a GitRefName,
    pub outputs: &'a GitlabOutputs,
}

/// The `gitlab_pipeline` target backend.
pub struct GitlabPipelineBackend {
    http: reqwest::Client,
    tokens: TokenSource,
    hosts: Vec<ForgeHost>,
    max_wall_clock: Duration,
}

impl GitlabPipelineBackend {
    pub(crate) fn new(
        tokens: TokenSource,
        hosts: Vec<ForgeHost>,
        max_wall_clock: Duration,
    ) -> Result<Self, TargetError> {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| TargetError::Refused(format!("building the GitLab client: {e}")))?;
        Ok(GitlabPipelineBackend {
            http,
            tokens,
            hosts,
            max_wall_clock,
        })
    }

    pub fn wall_clock(&self, budget: Option<CallBudget>) -> Duration {
        budget
            .and_then(|b| b.wall_clock_secs)
            .filter(|secs| *secs > 0)
            .map(Duration::from_secs)
            .map_or(self.max_wall_clock, |own| own.min(self.max_wall_clock))
    }

    pub(crate) fn place<'a>(&self, target: &'a CallTarget) -> Result<Place<'a>, TargetError> {
        let CallTarget::GitlabPipeline {
            host,
            project,
            git_ref,
            outputs,
        } = target
        else {
            return Err(TargetError::WrongKind(target.kind().to_string()));
        };
        let host = self.served(host)?;
        let project = ProjectPath::parse(project)?;
        Ok(Place {
            host,
            project,
            git_ref,
            outputs,
        })
    }

    fn served(&self, host: &str) -> Result<ForgeHost, TargetError> {
        let parsed = ForgeHost::parse(host)
            .ok_or_else(|| TargetError::Refused(format!("{host:?} is not a GitLab host name")))?;
        match self.hosts.contains(&parsed) {
            true => Ok(parsed),
            false => Err(TargetError::Refused(format!(
                "this controller calls no pipelines on {host} (CONTROLLER_GITLAB_HOSTS)"
            ))),
        }
    }

    fn api(&self, host: &ForgeHost) -> Result<Gitlab, TargetError> {
        Ok(Gitlab::new(
            self.http.clone(),
            &format!("https://{}", host.as_str()),
        )?)
    }

    async fn token(&self, host: &ForgeHost) -> Result<BearerToken, TargetError> {
        match &self.tokens {
            TokenSource::Person(held) => {
                match held.token(crate::identity::forge::Forge::Gitlab, host) {
                    Some(token) => Ok(BearerToken::new(token)),
                    None => glab_token(host).await,
                }
            }
            TokenSource::Bot(files) => {
                let path = files.get(host).ok_or_else(|| {
                    TargetError::Refused(format!(
                        "this controller has no GitLab token for {} (CONTROLLER_GITLAB_TOKEN_FILES)",
                        host.as_str()
                    ))
                })?;
                let text =
                    tokio::fs::read_to_string(path)
                        .await
                        .map_err(|e| TargetError::Token {
                            host: host.as_str().to_owned(),
                            detail: e.to_string(),
                        })?;
                let token = text.trim();
                match token.is_empty() || token.contains(char::is_whitespace) {
                    true => Err(TargetError::Token {
                        host: host.as_str().to_owned(),
                        detail: "the token file holds no single token".to_owned(),
                    }),
                    false => Ok(BearerToken::new(token)),
                }
            }
        }
    }

    pub async fn interface(&self, target: &CallTarget) -> Result<LockedCall, TargetError> {
        let place = self.place(target)?;
        let api = self.api(&place.host)?;
        let token = self.token(&place.host).await?;
        let commit = api
            .commit(&token, &place.project, place.git_ref.as_str())
            .await?;
        let interface = self.interface_at(&api, &token, &place, &commit).await?;
        let pin = Pin::GitlabRef {
            host: place.host.as_str().to_owned(),
            project: place.project.as_str().to_owned(),
            git_ref: place.git_ref.clone(),
            commit: commit.to_string(),
        };
        LockedCall::new(pin, interface)
            .map_err(|e| TargetError::Refused(format!("hashing the interface: {e}")))
    }

    async fn interface_at(
        &self,
        api: &Gitlab,
        token: &BearerToken,
        place: &Place<'_>,
        commit: &CommitId,
    ) -> Result<TargetInterface, TargetError> {
        let config = api
            .file(
                token,
                &place.project,
                CI_CONFIG,
                commit.as_str(),
                MAX_CONFIG_BYTES,
            )
            .await?
            .ok_or_else(|| {
                TargetError::Refused(format!(
                    "{} has no {CI_CONFIG} at {}",
                    place.project.as_str(),
                    place.git_ref.as_str()
                ))
            })?;
        let inputs = crucible_ci::gitlab::inputs::inputs(&config).map_err(|e| {
            TargetError::Refused(format!(
                "{} {CI_CONFIG} at {}: {e}",
                place.project.as_str(),
                place.git_ref.as_str()
            ))
        })?;
        let lint = api
            .lint(token, &place.project, commit, place.git_ref.as_str())
            .await?;
        if !lint.valid {
            return Err(TargetError::Refused(format!(
                "{} CI configuration at {} is invalid: {}",
                place.project.as_str(),
                place.git_ref.as_str(),
                lint.errors.join("; ")
            )));
        }
        Ok(interface(&inputs, &lint, place.outputs))
    }

    pub async fn authorize(
        &self,
        who: &Principal,
        target: &CallTarget,
        linked: Option<u64>,
    ) -> Result<(), TargetError> {
        let place = self.place(target)?;
        if matches!(self.tokens, TokenSource::Person(_)) {
            return Ok(());
        }
        let Principal::User(_) = who else {
            return Err(TargetError::Refused(format!(
                "{who} is not a person with a GitLab account, so it may not run the pipelines of \
                 {}",
                place.project.as_str()
            )));
        };
        let Some(user_id) = linked else {
            return Err(TargetError::Refused(format!(
                "{who} has no GitLab account on {} linked (sign in to it in the desktop app), so \
                 it may not run the pipelines of {}",
                place.host.as_str(),
                place.project.as_str()
            )));
        };
        let api = self.api(&place.host)?;
        let token = self.token(&place.host).await?;
        let member = match api.member_access(&token, &place.project, user_id).await? {
            Some(member) if member.is_active() && member.access_level >= DEVELOPER => member,
            held => {
                return Err(TargetError::Refused(format!(
                    "{who} may not run the pipelines of {}: their GitLab access level is {}, and \
                     running one needs an active Developer (30) or above",
                    place.project.as_str(),
                    held.filter(|m| m.is_active())
                        .map_or_else(|| "none".to_owned(), |m| m.access_level.to_string())
                )));
            }
        };
        let branches = api.protected_branches(&token, &place.project).await?;
        let tags = api.protected_tags(&token, &place.project).await?;
        let runner = Runner {
            user_id,
            access_level: member.access_level,
        };
        match refused_by(place.git_ref.as_str(), runner, &branches, &tags) {
            None => Ok(()),
            Some(rule) => Err(TargetError::Refused(format!(
                "{who} may not run pipelines on {} of {}: {rule} does not allow them",
                place.git_ref.as_str(),
                place.project.as_str()
            ))),
        }
    }

    pub async fn dispatch(
        &self,
        target: &CallTarget,
        locked: &LockedCall,
        values: &BTreeMap<String, Value>,
        files: &BTreeMap<String, FileRef>,
    ) -> Result<GitlabDispatched, TargetError> {
        let place = self.place(target)?;
        if !locked.pin.pins(target) {
            return Err(TargetError::Refused(format!(
                "calls.lock pins a different target than {} on {} at {}",
                place.project.as_str(),
                place.host.as_str(),
                place.git_ref.as_str()
            )));
        }
        let inputs = pipeline_inputs(&locked.interface.params, values, files)
            .map_err(TargetError::Refused)?;
        let api = self.api(&place.host)?;
        let token = self.token(&place.host).await?;
        let commit = api
            .commit(&token, &place.project, place.git_ref.as_str())
            .await?;
        let current = self.interface_at(&api, &token, &place, &commit).await?;
        if let Some(moved) = crate::calls::interface_moved(&locked.interface, &current) {
            return Err(TargetError::Refused(format!(
                "{} at {} (commit {}) {moved} than calls.lock records",
                place.project.as_str(),
                place.git_ref.as_str(),
                commit.short()
            )));
        }
        let created = api
            .create_pipeline(&token, &place.project, place.git_ref.as_str(), &inputs)
            .await?;
        Ok(GitlabDispatched {
            handle: TargetHandle::GitlabPipeline {
                host: place.host.as_str().to_owned(),
                project: place.project.as_str().to_owned(),
                pipeline_id: created.id,
            },
            link: created
                .web_url
                .and_then(|url| ExternalUrl::new(url).ok())
                .map(|url| NodeLink::External { url }),
            commit: created.sha,
        })
    }

    pub async fn poll(
        &self,
        target: &CallTarget,
        pipeline_id: u64,
        graph: &ProgressGraph,
        caller_cancelled: bool,
    ) -> Result<PipelinePoll, TargetError> {
        let place = self.place(target)?;
        let api = self.api(&place.host)?;
        let token = self.token(&place.host).await?;
        let Some(pipeline) = api.pipeline(&token, &place.project, pipeline_id).await? else {
            return Err(TargetError::Refused(format!(
                "pipeline {pipeline_id} of {} is gone",
                place.project.as_str()
            )));
        };
        let jobs = api.jobs(&token, &place.project, pipeline_id).await?;
        let bridges = api.bridges(&token, &place.project, pipeline_id).await?;
        let progress = pipeline_graph(&jobs, &bridges, &shape_of(graph)).graph;
        let outcome = match pipeline_state(&pipeline, &jobs) {
            PipelineState::Running | PipelineState::Blocked(_) => None,
            _ => Some(
                self.outcome(&api, &token, &place, &pipeline, &jobs, caller_cancelled)
                    .await?,
            ),
        };
        Ok(PipelinePoll {
            progress,
            number: pipeline.iid,
            outcome,
            links: pipeline_links(&place.host, &place.project, &pipeline),
        })
    }

    async fn outcome(
        &self,
        api: &Gitlab,
        token: &BearerToken,
        place: &Place<'_>,
        pipeline: &Pipeline,
        jobs: &[Job],
        caller_cancelled: bool,
    ) -> Result<GitlabOutcome, TargetError> {
        let failed = |note: String| GitlabOutcome {
            status: CallStatus::Failed,
            note,
            result: Value::Null,
            files: Vec::new(),
        };
        match pipeline_state(pipeline, jobs) {
            PipelineState::Failed(note) => Ok(failed(note)),
            PipelineState::Skipped => Ok(failed(format!("pipeline {} was skipped", pipeline.id))),
            PipelineState::Cancelled => Ok(GitlabOutcome {
                status: CallStatus::Cancelled,
                note: match caller_cancelled {
                    true => "the call was cancelled".to_owned(),
                    false => format!("pipeline {} was cancelled", pipeline.id),
                },
                result: Value::Null,
                files: Vec::new(),
            }),
            PipelineState::Passed => {
                let outputs = place.outputs;
                if outputs.fields.is_empty() && outputs.files.is_empty() {
                    return Ok(GitlabOutcome {
                        status: CallStatus::Passed,
                        note: String::new(),
                        result: json!({}),
                        files: Vec::new(),
                    });
                }
                let Some(job_name) = outputs.job.as_deref() else {
                    return Ok(failed(
                        "the target declares result fields or files but no job to read them from"
                            .to_owned(),
                    ));
                };
                let Some(job) = latest_job(jobs, job_name) else {
                    return Ok(failed(format!(
                        "pipeline {} ran no job {job_name:?}",
                        pipeline.id
                    )));
                };
                match self.read_result(api, token, place, job.id, outputs).await? {
                    Ok((result, files)) => Ok(GitlabOutcome {
                        status: CallStatus::Passed,
                        note: String::new(),
                        result,
                        files,
                    }),
                    Err(note) => Ok(failed(format!("job {job_name:?}: {note}"))),
                }
            }
            PipelineState::Running | PipelineState::Blocked(_) => {
                Ok(failed(format!("pipeline {} has not ended", pipeline.id)))
            }
        }
    }

    async fn read_result(
        &self,
        api: &Gitlab,
        token: &BearerToken,
        place: &Place<'_>,
        job: u64,
        outputs: &GitlabOutputs,
    ) -> Result<Result<(Value, Vec<ResultFile>), String>, TargetError> {
        let mut result = Map::new();
        if !outputs.fields.is_empty() {
            let read = api
                .artifact(
                    token,
                    &place.project,
                    job,
                    GITLAB_RESULT_FILE,
                    MAX_FILE_PARAM_BYTES,
                )
                .await;
            let Some(bytes) = settled(read, GITLAB_RESULT_FILE)? else {
                return Ok(Err(format!("its artifacts hold no {GITLAB_RESULT_FILE}")));
            };
            let bytes = match bytes {
                Ok(bytes) => bytes,
                Err(note) => return Ok(Err(note)),
            };
            match fields(&bytes, outputs) {
                Ok(read) => result = read,
                Err(note) => return Ok(Err(note)),
            }
        }
        let mut files = Vec::with_capacity(outputs.files.len());
        for path in &outputs.files {
            let read = api
                .artifact(token, &place.project, job, path, MAX_FILE_PARAM_BYTES)
                .await;
            let Some(bytes) = settled(read, path)? else {
                return Ok(Err(format!("its artifacts hold no {path:?}")));
            };
            let bytes = match bytes {
                Ok(bytes) => bytes,
                Err(note) => return Ok(Err(note)),
            };
            files.push(ResultFile {
                file: FileRef {
                    digest: format!("sha256:{:x}", Sha256::digest(&bytes)),
                    size: bytes.len() as u64,
                    path: Some(path.clone()),
                    media_type: None,
                },
                bytes,
            });
        }
        Ok(Ok((Value::Object(result), files)))
    }

    pub async fn node_log(
        &self,
        target: &CallTarget,
        pipeline_id: u64,
        node: &str,
    ) -> Result<Option<String>, TargetError> {
        let place = self.place(target)?;
        let api = self.api(&place.host)?;
        let token = self.token(&place.host).await?;
        let jobs = api.jobs(&token, &place.project, pipeline_id).await?;
        let Some(job) = latest_job(&jobs, node) else {
            return Ok(None);
        };
        let log = api
            .trace(&token, &place.project, job.id, MAX_LOG_BYTES)
            .await?;
        Ok(Some(log))
    }

    pub async fn cancel(&self, target: &CallTarget, pipeline_id: u64) -> Result<(), TargetError> {
        let place = self.place(target)?;
        let api = self.api(&place.host)?;
        let token = self.token(&place.host).await?;
        match api.cancel(&token, &place.project, pipeline_id).await {
            Ok(()) | Err(GitlabError::Status { status: 404, .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

async fn glab(host: &ForgeHost, args: &[&str]) -> Result<std::process::Output, TargetError> {
    let failed = |detail: String| TargetError::Glab {
        host: host.as_str().to_owned(),
        detail,
    };
    let run = tokio::process::Command::new("glab")
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    tokio::time::timeout(GLAB_TIMEOUT, run)
        .await
        .map_err(|_| failed(format!("no answer in {GLAB_TIMEOUT:?}")))?
        .map_err(|e| failed(e.to_string()))
}

async fn glab_token(host: &ForgeHost) -> Result<BearerToken, TargetError> {
    let failed = |detail: String| TargetError::Glab {
        host: host.as_str().to_owned(),
        detail,
    };
    let fresh = glab(host, &["api", "--hostname", host.as_str(), "user"]).await?;
    if !fresh.status.success() {
        return Err(failed(
            String::from_utf8_lossy(&fresh.stderr)
                .trim()
                .chars()
                .take(ERR_BODY_CHARS)
                .collect(),
        ));
    }
    let out = glab(host, &["config", "get", "token", "--host", host.as_str()]).await?;
    let token =
        String::from_utf8(out.stdout).map_err(|_| failed("the token is not UTF-8".into()))?;
    let token = token.trim();
    match out.status.success() && !token.is_empty() && !token.contains(char::is_whitespace) {
        true => Ok(BearerToken::new(token)),
        false => Err(failed("glab holds no token for the host".into())),
    }
}

fn pipeline_links(host: &ForgeHost, project: &ProjectPath, pipeline: &Pipeline) -> Vec<NamedLink> {
    let base = format!("https://{}/{}", host.as_str(), project.as_str());
    let merge_request = pipeline
        .git_ref
        .strip_prefix("refs/merge-requests/")
        .and_then(|rest| rest.split('/').next())
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    [
        Some(("Project", base.clone())),
        Some(("Commit", format!("{base}/-/commit/{}", pipeline.sha))),
        merge_request.map(|n| ("Merge request", format!("{base}/-/merge_requests/{n}"))),
    ]
    .into_iter()
    .flatten()
    .filter_map(|(label, url)| {
        ExternalUrl::new(url).ok().map(|url| NamedLink {
            label: label.to_owned(),
            url,
        })
    })
    .collect()
}

fn latest_job<'a>(jobs: &'a [Job], name: &str) -> Option<&'a Job> {
    jobs.iter().filter(|j| j.name == name).max_by_key(|j| j.id)
}

fn fields(bytes: &[u8], outputs: &GitlabOutputs) -> Result<Map<String, Value>, String> {
    let object = match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(object)) => object,
        Ok(other) => {
            return Err(format!(
                "{GITLAB_RESULT_FILE} is {}, not an object",
                value_type(&other)
            ));
        }
        Err(e) => return Err(format!("{GITLAB_RESULT_FILE} is not JSON: {e}")),
    };
    let mut result = Map::new();
    for (field, ty) in &outputs.fields {
        let value = object
            .get(field)
            .ok_or_else(|| format!("{GITLAB_RESULT_FILE} has no field {field:?}"))?;
        if let Some(why) = field_refusal(ty, value) {
            return Err(format!("{GITLAB_RESULT_FILE} field {field:?}: {why}"));
        }
        result.insert(field.clone(), value.clone());
    }
    Ok(result)
}

fn settled(
    read: Result<Option<Vec<u8>>, GitlabError>,
    path: &str,
) -> Result<Option<Result<Vec<u8>, String>>, TargetError> {
    match read {
        Ok(found) => Ok(found.map(Ok)),
        Err(GitlabError::TooLarge { cap, .. }) => Ok(Some(Err(format!(
            "{path:?} is over the {cap} bytes a result file may be"
        )))),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use crate::calls::gitlab::*;
    use crucible_contract::emits::FieldType;

    fn backend(hosts: &[&str]) -> GitlabPipelineBackend {
        GitlabPipelineBackend::new(
            TokenSource::Person(SignedIn::default()),
            hosts.iter().filter_map(|h| ForgeHost::parse(h)).collect(),
            Duration::from_secs(3600),
        )
        .unwrap()
    }

    fn target(host: &str, project: &str) -> CallTarget {
        serde_json::from_value(json!({
            "kind": "gitlab_pipeline",
            "host": host,
            "project": project,
            "ref": "main",
        }))
        .unwrap()
    }

    fn job(id: u64, name: &str) -> Job {
        Job {
            id,
            name: name.into(),
            stage: "s".into(),
            status: "success".into(),
            allow_failure: false,
            started_at: None,
            finished_at: None,
            web_url: None,
        }
    }

    #[test]
    fn only_configured_hosts_and_project_paths_are_reached() {
        let backend = backend(&["gitlab.example.com"]);
        assert!(
            backend
                .place(&target("gitlab.example.com", "g/app"))
                .is_ok()
        );
        let other = backend
            .place(&target("evil.example.com", "g/app"))
            .err()
            .unwrap();
        assert!(
            other.to_string().contains("CONTROLLER_GITLAB_HOSTS"),
            "{other}"
        );
        assert!(backend.place(&target("gitlab.example.com", "app")).is_err());
        assert!(backend.place(&target("http://x", "g/app")).is_err());
        let playbook: CallTarget =
            serde_json::from_value(json!({"kind": "playbook", "playbook": "p"})).unwrap();
        assert!(matches!(
            backend.place(&playbook),
            Err(TargetError::WrongKind(_))
        ));
    }

    #[tokio::test]
    async fn a_person_controller_needs_a_linked_gitlab_sign_in() {
        let held = SignedIn::default();
        let backend = GitlabPipelineBackend::new(
            TokenSource::Person(held.clone()),
            vec![ForgeHost::parse("gitlab.example.com").unwrap()],
            Duration::from_secs(60),
        )
        .unwrap();
        let host = ForgeHost::parse("gitlab.example.com").unwrap();
        held.set(
            crate::identity::forge::Forge::Gitlab,
            &host,
            Some("glpat-x".into()),
        );
        assert!(backend.token(&host).await.is_ok());
        let who = Principal::User("wren".into());
        backend
            .authorize(&who, &target("gitlab.example.com", "g/app"), None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_bot_controller_reads_its_token_file_per_host() {
        let dir = tempfile::tempdir().unwrap();
        let host = ForgeHost::parse("gitlab.example.com").unwrap();
        let file = dir.path().join("token");
        std::fs::write(&file, "glpat-bot\n").unwrap();
        let backend = GitlabPipelineBackend::new(
            TokenSource::Bot(BTreeMap::from([(host.clone(), file.clone())])),
            vec![host.clone()],
            Duration::from_secs(60),
        )
        .unwrap();
        assert!(backend.token(&host).await.is_ok());
        let unknown = ForgeHost::parse("gitlab.com").unwrap();
        assert!(
            backend
                .token(&unknown)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("CONTROLLER_GITLAB_TOKEN_FILES")
        );
        std::fs::write(&file, "two tokens").unwrap();
        assert!(matches!(
            backend.token(&host).await,
            Err(TargetError::Token { .. })
        ));
        let team = Principal::Team(crate::authz::model::TeamSlug::parse("ops").unwrap());
        let refused = backend
            .authorize(&team, &target("gitlab.example.com", "g/app"), Some(1))
            .await
            .err()
            .unwrap();
        assert!(refused.to_string().contains("not a person"), "{refused}");
        let unlinked = backend
            .authorize(
                &Principal::User("wren".into()),
                &target("gitlab.example.com", "g/app"),
                None,
            )
            .await
            .err()
            .unwrap();
        assert!(
            unlinked.to_string().contains("no GitLab account"),
            "{unlinked}"
        );
    }

    #[test]
    fn the_result_is_the_declared_fields_of_result_json() {
        let outputs = GitlabOutputs {
            job: Some("report".into()),
            fields: BTreeMap::from([
                ("version".to_owned(), FieldType::String),
                ("count".to_owned(), FieldType::Number),
            ]),
            files: vec![],
        };
        let read = fields(
            br#"{"version": "1.2", "count": 3, "extra": true}"#,
            &outputs,
        )
        .unwrap();
        assert_eq!(Value::Object(read), json!({"version": "1.2", "count": 3}));
        let missing = fields(br#"{"version": "1.2"}"#, &outputs).unwrap_err();
        assert!(missing.contains("no field \"count\""), "{missing}");
        let wrong = fields(br#"{"version": 1, "count": 3}"#, &outputs).unwrap_err();
        assert!(wrong.contains("field \"version\""), "{wrong}");
        assert!(
            fields(b"[1]", &outputs)
                .unwrap_err()
                .contains("not an object")
        );
        assert!(fields(b"nope", &outputs).unwrap_err().contains("not JSON"));
    }

    #[test]
    fn a_pipeline_names_its_project_commit_and_merge_request() {
        let host = ForgeHost::parse("gitlab.example.com").unwrap();
        let project = ProjectPath::parse("g/app").unwrap();
        let mut pipeline: Pipeline = serde_json::from_value(json!({
            "id": 9, "sha": "abc123", "ref": "main", "status": "running",
        }))
        .unwrap();
        let shown = |links: Vec<NamedLink>| -> Vec<(String, String)> {
            links
                .into_iter()
                .map(|l| (l.label, l.url.as_str().to_owned()))
                .collect()
        };
        assert_eq!(
            shown(pipeline_links(&host, &project, &pipeline)),
            [
                (
                    "Project".to_owned(),
                    "https://gitlab.example.com/g/app".to_owned()
                ),
                (
                    "Commit".to_owned(),
                    "https://gitlab.example.com/g/app/-/commit/abc123".to_owned()
                ),
            ]
        );
        pipeline.git_ref = "refs/merge-requests/42/head".into();
        let links = shown(pipeline_links(&host, &project, &pipeline));
        assert_eq!(
            links.last(),
            Some(&(
                "Merge request".to_owned(),
                "https://gitlab.example.com/g/app/-/merge_requests/42".to_owned()
            ))
        );
        pipeline.git_ref = "refs/merge-requests/x/head".into();
        assert_eq!(pipeline_links(&host, &project, &pipeline).len(), 2);
    }

    #[test]
    fn the_latest_retry_of_a_job_is_the_one_read() {
        let jobs = [job(1, "report"), job(9, "report"), job(5, "build")];
        assert_eq!(latest_job(&jobs, "report").map(|j| j.id), Some(9));
        assert!(latest_job(&jobs, "deploy").is_none());
    }

    #[test]
    fn an_oversized_artifact_is_a_note_and_other_errors_stay_errors() {
        assert_eq!(settled(Ok(None), "a").unwrap(), None);
        assert_eq!(settled(Ok(Some(vec![1])), "a").unwrap(), Some(Ok(vec![1])));
        let big = settled(
            Err(GitlabError::TooLarge {
                doing: "reading".into(),
                cap: 4,
            }),
            "out/x",
        )
        .unwrap();
        assert_eq!(
            big,
            Some(Err(
                "\"out/x\" is over the 4 bytes a result file may be".into()
            ))
        );
        assert!(
            settled(
                Err(GitlabError::Status {
                    doing: "reading".into(),
                    status: 500,
                    body: String::new(),
                }),
                "a"
            )
            .is_err()
        );
    }

    #[test]
    fn the_wall_clock_is_the_calls_own_bounded_by_the_controllers() {
        let backend = backend(&[]);
        assert_eq!(backend.wall_clock(None), Duration::from_secs(3600));
        let own = CallBudget {
            cost_usd: None,
            wall_clock_secs: Some(60),
        };
        assert_eq!(backend.wall_clock(Some(own)), Duration::from_secs(60));
        let over = CallBudget {
            cost_usd: None,
            wall_clock_secs: Some(7200),
        };
        assert_eq!(backend.wall_clock(Some(over)), Duration::from_secs(3600));
    }
}
