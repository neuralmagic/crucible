//! Playbook calls (RFC-0008): the target backends a call dispatches through, one per target
//! kind, the store of the target runs calls made, and the routes the engine and readers reach
//! them by.
//!
//! ```text
//!   engine (pod token) ──POST /calls──▶ api ──▶ Backends::get(kind) ──▶ TargetBackend::dispatch
//!                      ──GET  /calls/x─▶ api ──▶ progress + outcome ──▶ call_target_runs row
//! ```

pub(crate) mod api;
mod github;
pub(crate) mod lock;
mod playbook;
mod progress;
pub(crate) mod store;

use std::collections::BTreeMap;
use std::path::PathBuf;

use crucible_contract::calls::{
    CallAlias, CallBudget, CallId, CallKind, CallStatus, CallTarget, FileRef, LockedCall, NodeLink,
    Pin, ProgressGraph, TargetHandle, TargetInterface,
};
use serde_json::Value;

use crate::client::Db;

/// Where the GitHub backend takes the token it dispatches with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum GithubToken {
    /// `gh auth token`, run in-process at each dispatch and held in memory only. Local
    /// controllers only.
    #[value(name = "gh")]
    GhCli,
    /// The GitHub App installation on the target repository.
    #[value(name = "app")]
    AppInstallation,
}

/// Why a backend operation did not do what it was asked.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CallError {
    /// The call cannot be made as asked. No target run exists, and the call settles failing with
    /// this note.
    #[error("{0}")]
    Refused(String),
    /// This controller has no working backend for the kind.
    #[error("{kind} targets are not available on this controller: {reason}")]
    NotBuilt { kind: CallKind, reason: String },
    /// The call's row lacks what its backend recorded at dispatch.
    #[error("call {call} recorded no {what}")]
    Unrecorded { call: CallId, what: &'static str },
    /// A backend built to follow target runs was asked to start one.
    #[error("this {0} backend follows target runs and launches none")]
    FollowOnly(CallKind),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl CallError {
    /// The note a call that ends on this error settles with.
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
    /// The controller's scratch root, where a local run keeps its session and files.
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
    /// The calling run's launch key.
    pub launch: String,
    /// The launch owner a target run is launched with the authority of.
    pub owner: Option<String>,
    pub groups: Option<Value>,
    pub created_by: Option<String>,
    /// How many calling runs are above this one: 0 for a run no call made.
    pub depth: u32,
    /// Whether the calling run's pack is a draft, the only kind that may pin a draft head.
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
    /// What the backend keeps about the target run for its later operations.
    pub detail: Value,
}

/// How an ended target run came out.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Outcome {
    /// [`CallStatus::Passed`] or [`CallStatus::Failed`].
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
}

/// One look at a target run.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Polled {
    pub progress: ProgressGraph,
    /// `None` while the target run runs.
    pub outcome: Option<Outcome>,
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
    /// The manifest entry the call was dispatched for.
    pub target: CallTarget,
    /// The locked interface the call was dispatched against.
    pub interface: TargetInterface,
    /// `None` while the dispatch is in flight.
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

/// The operations RFC-0008 C-TARGETS gives every target kind (progress and outcome read in one
/// poll), plus reading one result file.
/// The engine performs none of them.
#[async_trait::async_trait]
pub(crate) trait TargetBackend: Send + Sync {
    /// The target's pin and interface as its system holds them now.
    async fn interface(&self, ctx: &CallCtx, target: &CallTarget) -> Result<Resolved, CallError>;

    /// Start one target run with the bound values.
    async fn dispatch(
        &self,
        ctx: &CallCtx,
        spec: &DispatchSpec<'_>,
    ) -> Result<Dispatched, CallError>;

    /// The target run's graph now, and how it ended once it has.
    async fn poll(&self, ctx: &CallCtx, call: &CallRecord) -> Result<Polled, CallError>;

    /// The bytes of one result file of an ended target run; `None` when it has no such file.
    async fn result_file(
        &self,
        ctx: &CallCtx,
        call: &CallRecord,
        path: &str,
    ) -> Result<Option<Vec<u8>>, CallError>;

    /// Stop the target run.
    async fn cancel(&self, ctx: &CallCtx, call: &CallRecord) -> Result<(), CallError>;
}

/// The backends of every target kind this controller implements.
pub(crate) struct Backends {
    playbook: playbook::PlaybookBackend,
    github: Option<Result<github::GithubWorkflowBackend, String>>,
}

impl Backends {
    /// The backends an API request dispatches with: the playbook backend launches under the
    /// request state's policy and caps.
    pub(crate) fn new(state: &crate::api::state::ApiState) -> Self {
        Backends {
            playbook: playbook::PlaybookBackend::new(Some(state.clone())),
            github: state.calls_github.map(github_backend),
        }
    }

    /// Backends that can follow and cancel target runs but launch nothing.
    pub(crate) fn following(token: Option<GithubToken>) -> Self {
        Backends {
            playbook: playbook::PlaybookBackend::new(None),
            github: token.map(github_backend),
        }
    }

    pub(crate) fn get(&self, kind: CallKind) -> Result<&dyn TargetBackend, CallError> {
        match kind {
            CallKind::Playbook => Ok(&self.playbook),
            CallKind::GithubWorkflow => match &self.github {
                Some(Ok(github)) => Ok(github),
                Some(Err(reason)) => Err(CallError::NotBuilt {
                    kind,
                    reason: reason.clone(),
                }),
                None => Err(CallError::NotBuilt {
                    kind,
                    reason: "no GitHub token source is configured \
                             (CONTROLLER_CALLS_GITHUB_TOKEN)"
                        .to_string(),
                }),
            },
        }
    }
}

fn github_backend(token: GithubToken) -> Result<github::GithubWorkflowBackend, String> {
    github::GithubWorkflowBackend::new(token).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use crate::calls::*;

    #[test]
    fn every_kind_routes_to_its_backend_or_says_why_not() {
        let without = Backends::following(None);
        assert!(without.get(CallKind::Playbook).is_ok());
        let refused = without
            .get(CallKind::GithubWorkflow)
            .err()
            .expect("no token source");
        assert!(matches!(
            refused,
            CallError::NotBuilt {
                kind: CallKind::GithubWorkflow,
                ..
            }
        ));
        assert!(refused.note().contains("CONTROLLER_CALLS_GITHUB_TOKEN"));
        let with = Backends::following(Some(GithubToken::GhCli));
        assert!(with.get(CallKind::GithubWorkflow).is_ok());
    }

    #[test]
    fn the_token_source_parses_from_its_config_names() {
        use clap::ValueEnum;
        assert_eq!(
            GithubToken::from_str("gh", false).ok(),
            Some(GithubToken::GhCli)
        );
        assert_eq!(
            GithubToken::from_str("app", false).ok(),
            Some(GithubToken::AppInstallation)
        );
        assert!(GithubToken::from_str("pat", false).is_err());
    }
}
