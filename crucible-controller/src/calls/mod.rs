//! Playbook calls: the target backends a call dispatches through, one per target
//! kind, the store of the target runs calls made, and the routes the engine and readers reach
//! them by.
//!
//! ```text
//!   engine (pod token) ──POST /calls──▶ api ──▶ TargetBackend::authorize + dispatch ──▶ row
//!   engine, readers    ──GET  ...─────▶ api ──▶ call_target_runs row
//!   refresh loop (every few seconds) ──▶ TargetBackend::poll / cancel ──▶ row
//! ```

pub(crate) mod access;
pub(crate) mod api;
pub(crate) mod compile;
mod github;
mod gitlab;
pub(crate) mod lock;
pub(crate) mod locking;
mod playbook;
mod progress;
pub(crate) mod reader;
pub(crate) mod refresh;
pub(crate) mod store;
pub(crate) mod tekton;

use std::collections::BTreeMap;
use std::path::PathBuf;

use crucible_contract::calls::{
    CallAlias, CallBudget, CallId, CallKind, CallStatus, CallTarget, FileRef, LockedCall, NodeLink,
    Pin, ProgressGraph, TargetHandle, TargetInterface,
};
use serde_json::Value;

use crate::client::Db;
use crate::config::{GithubToken, GitlabToken};

pub(crate) const REFRESH_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// Why a backend operation did not do what it was asked.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CallError {
    #[error("{0}")]
    Refused(String),
    #[error("{kind} targets are not available on this controller: {reason}")]
    NotBuilt { kind: CallKind, reason: String },
    #[error("call {call} recorded no {what}")]
    Unrecorded { call: CallId, what: &'static str },
    #[error("this {0} backend follows target runs and launches none")]
    FollowOnly(CallKind),
    #[error("{why}; asking again in {}s", after.as_secs())]
    Throttled {
        after: std::time::Duration,
        why: String,
    },
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl CallError {
    pub(crate) fn note(&self) -> String {
        match self {
            CallError::Internal(e) => format!("{e:#}"),
            other => other.to_string(),
        }
    }
}

/// What every backend operation may read.
#[derive(Clone)]
pub(crate) struct CallCtx {
    pub db: Db,
    pub scratch_dir: PathBuf,
}

/// A target's pin and interface as its system holds them now.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Resolved {
    pub pin: Pin,
    pub interface: TargetInterface,
}

/// The run that is calling, as its launch recorded it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Parent {
    pub run: String,
    pub launch: String,
    pub owner: Option<String>,
    pub groups: Option<Value>,
    pub created_by: Option<String>,
    pub depth: u32,
    pub draft: bool,
}

/// One dispatch, as the controller checked it.
pub(crate) struct DispatchSpec<'a> {
    pub call: &'a CallId,
    pub parent: &'a Parent,
    pub task: &'a str,
    pub instance: Option<&'a str>,
    pub alias: &'a CallAlias,
    pub target: &'a CallTarget,
    pub locked: &'a LockedCall,
    pub values: &'a BTreeMap<String, Value>,
    pub files: &'a BTreeMap<String, FileRef>,
    pub budget: Option<CallBudget>,
}

/// What a dispatch started.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Dispatched {
    pub handle: TargetHandle,
    pub link: Option<NodeLink>,
    pub detail: Value,
}

/// How an ended target run came out.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Outcome {
    pub status: CallStatus,
    pub note: String,
    pub result: Value,
    pub result_files: Vec<FileRef>,
}

impl Outcome {
    pub(crate) fn failed(note: impl Into<String>) -> Self {
        Outcome {
            status: CallStatus::Failed,
            note: note.into(),
            result: Value::Null,
            result_files: Vec::new(),
        }
    }

    pub(crate) fn cancelled(note: impl Into<String>) -> Self {
        Outcome {
            status: CallStatus::Cancelled,
            ..Outcome::failed(note)
        }
    }
}

/// One look at a target run.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Polled {
    pub progress: ProgressGraph,
    pub recorded: serde_json::Map<String, Value>,
    pub outcome: Option<Outcome>,
    pub link: Option<NodeLink>,
}

pub(crate) fn record_links(
    recorded: &mut serde_json::Map<String, Value>,
    links: &[crucible_contract::calls::NamedLink],
) {
    if let Ok(value) = serde_json::to_value(links) {
        recorded.insert(crucible_contract::calls::LINKS_DETAIL_KEY.to_owned(), value);
    }
}

/// A dispatched call as the store holds it, which every operation after dispatch reads.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CallRecord {
    pub id: CallId,
    pub parent_run: String,
    pub task: String,
    pub instance: Option<String>,
    pub attempt: u32,
    pub alias: CallAlias,
    pub kind: CallKind,
    pub target: CallTarget,
    pub interface: TargetInterface,
    pub handle: Option<TargetHandle>,
    pub detail: Value,
    pub budget: Option<CallBudget>,
    pub status: CallStatus,
    pub note: String,
    pub result: Value,
    pub result_files: Vec<FileRef>,
    pub progress: ProgressGraph,
    pub link: Option<NodeLink>,
    pub created_at: String,
}

impl CallRecord {
    pub(crate) fn age(&self, now: jiff::Timestamp) -> Option<std::time::Duration> {
        let started = self.created_at.parse::<jiff::Timestamp>().ok()?;
        let secs = now.duration_since(started).as_secs();
        u64::try_from(secs).ok().map(std::time::Duration::from_secs)
    }

    pub(crate) fn aging_cadence(&self, now: jiff::Timestamp) -> std::time::Duration {
        let secs = match self.age(now).map(|a| a.as_secs()) {
            Some(0..120) => 10,
            Some(120..1800) | None => 30,
            Some(_) => 60,
        };
        std::time::Duration::from_secs(secs)
    }
}

/// The operations every target kind implements (progress and outcome read in one
/// poll), plus reading one result file.
/// The engine performs none of them.
#[async_trait::async_trait]
pub(crate) trait TargetBackend: Send + Sync {
    async fn interface(&self, ctx: &CallCtx, target: &CallTarget) -> Result<Resolved, CallError>;

    async fn authorize(
        &self,
        ctx: &CallCtx,
        who: &crate::authz::model::Principal,
        target: &CallTarget,
    ) -> Result<(), CallError>;

    async fn dispatch(
        &self,
        ctx: &CallCtx,
        spec: &DispatchSpec<'_>,
    ) -> Result<Dispatched, CallError>;

    async fn poll(&self, ctx: &CallCtx, call: &CallRecord) -> Result<Polled, CallError>;

    fn cadence(&self, _call: &CallRecord, _now: jiff::Timestamp) -> std::time::Duration {
        REFRESH_EVERY
    }

    async fn result_file(
        &self,
        ctx: &CallCtx,
        call: &CallRecord,
        path: &str,
    ) -> Result<Option<Vec<u8>>, CallError>;

    async fn cancel(&self, ctx: &CallCtx, call: &CallRecord) -> Result<(), CallError>;

    async fn node_log(
        &self,
        ctx: &CallCtx,
        call: &CallRecord,
        node: &str,
    ) -> Result<String, CallError>;
}

pub(crate) fn interface_moved(locked: &TargetInterface, now: &TargetInterface) -> Option<String> {
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

/// The backends of every target kind this controller implements.
pub(crate) struct Backends {
    playbook: playbook::PlaybookBackend,
    github: Result<github::GithubWorkflowBackend, String>,
    gitlab: Result<gitlab::GitlabPipelineBackend, String>,
    tekton: Result<tekton::TektonBackend, String>,
}

impl Backends {
    pub(crate) fn following(cfg: &crate::config::ControllerCfg) -> Self {
        Backends {
            playbook: playbook::PlaybookBackend::new(None),
            github: github_backend(
                cfg.calls_github_token,
                cfg.github_app.as_ref(),
                &cfg.github_logins(),
                &cfg.forge_signed_in,
                &cfg.playbook_max_time_cap,
            ),
            gitlab: gitlab_backend(
                cfg.calls_gitlab_token,
                &cfg.gitlab_token_files(),
                cfg.gitlab_hosts(),
                &cfg.forge_signed_in,
                &cfg.playbook_max_time_cap,
            ),
            tekton: cfg
                .calls_tekton_clusters()
                .and_then(tekton::TektonBackend::new),
        }
    }

    pub(crate) fn get(&self, kind: CallKind) -> Result<&dyn TargetBackend, CallError> {
        match kind {
            CallKind::Playbook => Ok(&self.playbook),
            CallKind::GithubWorkflow => match &self.github {
                Ok(github) => Ok(github),
                Err(reason) => Err(CallError::NotBuilt {
                    kind,
                    reason: reason.clone(),
                }),
            },
            CallKind::GitlabPipeline => match &self.gitlab {
                Ok(gitlab) => Ok(gitlab),
                Err(reason) => Err(CallError::NotBuilt {
                    kind,
                    reason: reason.clone(),
                }),
            },
            CallKind::Tekton => match &self.tekton {
                Ok(tekton) => Ok(tekton),
                Err(reason) => Err(CallError::NotBuilt {
                    kind,
                    reason: reason.clone(),
                }),
            },
        }
    }
}

fn gitlab_backend(
    token: Option<GitlabToken>,
    files: &Result<BTreeMap<crate::identity::forge::ForgeHost, std::path::PathBuf>, String>,
    hosts: Vec<crate::identity::forge::ForgeHost>,
    signed_in: &crate::identity::forge::held::SignedIn,
    max_time: &crate::model::MaxTime,
) -> Result<gitlab::GitlabPipelineBackend, String> {
    let tokens = match token {
        None => {
            return Err(
                "no GitLab token source is configured (CONTROLLER_CALLS_GITLAB_TOKEN)".to_owned(),
            );
        }
        Some(GitlabToken::Person) => gitlab::TokenSource::Person(signed_in.clone()),
        Some(GitlabToken::Bot) => gitlab::TokenSource::Bot(files.clone()?),
    };
    if hosts.is_empty() {
        return Err("no GitLab host is configured (CONTROLLER_GITLAB_HOSTS)".to_owned());
    }
    let max_wall_clock = std::time::Duration::from_secs(max_time.secs());
    gitlab::GitlabPipelineBackend::new(tokens, hosts, max_wall_clock).map_err(|e| e.to_string())
}

fn github_backend(
    token: Option<GithubToken>,
    app: Option<&crate::secrets::github_app::GithubAppTokenSource>,
    logins: &crate::identity::github_logins::GithubLogins,
    signed_in: &crate::identity::forge::held::SignedIn,
    max_time: &crate::model::MaxTime,
) -> Result<github::GithubWorkflowBackend, String> {
    let tokens = match (token, app) {
        (None, _) => {
            return Err(
                "no GitHub token source is configured (CONTROLLER_CALLS_GITHUB_TOKEN)".to_owned(),
            );
        }
        (Some(GithubToken::Person), _) => github::TokenSource::Person(signed_in.clone()),
        (Some(GithubToken::AppInstallation), Some(app)) => github::TokenSource::App {
            app: Box::new(app.clone()),
            logins: logins.clone(),
        },
        (Some(GithubToken::AppInstallation), None) => {
            return Err(
                "CONTROLLER_CALLS_GITHUB_TOKEN=app, but no GitHub App is configured \
                 (CONTROLLER_GITHUB_APP_*)"
                    .to_owned(),
            );
        }
    };
    let max_wall_clock = std::time::Duration::from_secs(max_time.secs());
    github::GithubWorkflowBackend::new(tokens, max_wall_clock).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use crate::calls::*;

    #[test]
    fn every_kind_routes_to_its_backend_or_says_why_not() {
        let hours = crate::model::MaxTime::hours(4);
        let logins = crate::identity::github_logins::GithubLogins::default();
        let signed_in = crate::identity::forge::held::SignedIn::default();
        let refused = |token, app| match github_backend(token, app, &logins, &signed_in, &hours) {
            Err(reason) => reason,
            Ok(_) => panic!("built a GitHub backend"),
        };
        assert!(refused(None, None).contains("CONTROLLER_CALLS_GITHUB_TOKEN"));
        assert!(
            refused(Some(GithubToken::AppInstallation), None).contains("CONTROLLER_GITHUB_APP_")
        );
        assert!(
            github_backend(Some(GithubToken::Person), None, &logins, &signed_in, &hours).is_ok()
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let app = crate::secrets::github_app::GithubAppTokenSource::new(
            "1",
            "2",
            crate::secrets::github_app::testkey::write_test_key(dir.path()),
            "https://api.github.com",
        );
        assert!(
            github_backend(
                Some(GithubToken::AppInstallation),
                Some(&app),
                &logins,
                &signed_in,
                &hours
            )
            .is_ok()
        );

        let cfg = crate::testing::cfg_with(dir.path());
        let without = Backends::following(&cfg);
        assert!(without.get(CallKind::Playbook).is_ok());
        let not_built = without
            .get(CallKind::GithubWorkflow)
            .err()
            .expect("no token source");
        assert!(matches!(
            not_built,
            CallError::NotBuilt {
                kind: CallKind::GithubWorkflow,
                ..
            }
        ));
        assert!(not_built.note().contains("CONTROLLER_CALLS_GITHUB_TOKEN"));
    }

    #[test]
    fn the_token_source_parses_from_its_config_names() {
        use clap::ValueEnum;
        assert_eq!(
            GithubToken::from_str("gh", false).ok(),
            Some(GithubToken::Person)
        );
        assert_eq!(
            GithubToken::from_str("app", false).ok(),
            Some(GithubToken::AppInstallation)
        );
        assert!(GithubToken::from_str("pat", false).is_err());
    }
}
