//! [`GithubWorkflowBackend`] as the [`TargetBackend`] a `github_workflow` call dispatches
//! through. Result files are kept under the controller's scratch root, named by digest, until
//! the calling run fetches them.

use std::path::PathBuf;

use crucible_contract::calls::{CallTarget, FileRef, GithubRepo, LockedCall, TargetHandle};
use serde_json::json;

use crate::calls::github::{GithubError, GithubWorkflowBackend, github_target};
use crate::calls::{
    CallCtx, CallError, CallRecord, DispatchSpec, Dispatched, Outcome, Polled, Resolved,
    TargetBackend,
};

impl From<GithubError> for CallError {
    fn from(e: GithubError) -> Self {
        match e.is_transient() {
            true => CallError::Internal(anyhow::Error::new(e)),
            false => CallError::Refused(e.to_string()),
        }
    }
}

/// How long a run this controller started may answer 404 before GitHub is believed.
const FRESH_RUN: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// What an error reading a run this controller already started means for its call. Being
/// rate limited, or a new run GitHub does not list yet, is asked again on the next poll; only
/// what stays wrong ends the call.
fn following(e: GithubError, call: &CallRecord, now: jiff::Timestamp) -> CallError {
    let fresh = call
        .created_at
        .parse::<jiff::Timestamp>()
        .is_ok_and(|started| now.duration_since(started).unsigned_abs() < FRESH_RUN);
    match &e {
        GithubError::Api {
            status: 403 | 429, ..
        } => CallError::Internal(anyhow::Error::new(e)),
        GithubError::Api { status: 404, .. } if fresh => CallError::Internal(anyhow::Error::new(e)),
        _ => e.into(),
    }
}

fn run_of(call: &CallRecord) -> Result<(&GithubRepo, u64), CallError> {
    match &call.handle {
        Some(TargetHandle::GithubWorkflow { repo, run_id }) => Ok((repo, *run_id)),
        _ => Err(CallError::Refused(format!(
            "call {} has no GitHub run",
            call.id
        ))),
    }
}

fn kept_dir(ctx: &CallCtx, call: &CallRecord) -> PathBuf {
    ctx.scratch_dir.join("call-results").join(&call.id.0)
}

fn kept_name(file: &FileRef) -> String {
    file.digest.replace(':', "-")
}

#[async_trait::async_trait]
impl TargetBackend for GithubWorkflowBackend {
    async fn interface(&self, _ctx: &CallCtx, target: &CallTarget) -> Result<Resolved, CallError> {
        let LockedCall { pin, interface, .. } =
            GithubWorkflowBackend::interface(self, target).await?;
        Ok(Resolved { pin, interface })
    }

    async fn dispatch(
        &self,
        _ctx: &CallCtx,
        spec: &DispatchSpec<'_>,
    ) -> Result<Dispatched, CallError> {
        let dispatched = GithubWorkflowBackend::dispatch(
            self,
            spec.target,
            spec.locked,
            spec.values,
            spec.files,
        )
        .await?;
        Ok(Dispatched {
            handle: dispatched.handle,
            link: dispatched.link,
            detail: json!({"head_sha": dispatched.head_sha}),
        })
    }

    async fn poll(&self, ctx: &CallCtx, call: &CallRecord) -> Result<Polled, CallError> {
        let (repo, run_id) = run_of(call)?;
        let (_, _, _, outputs) = github_target(&call.target)?;
        let polled =
            GithubWorkflowBackend::poll(self, repo, run_id, &call.interface.graph, outputs, false)
                .await
                .map_err(|e| following(e, call, jiff::Timestamp::now()))?;
        let Some(outcome) = polled.outcome else {
            return Ok(Polled {
                progress: polled.progress,
                outcome: None,
            });
        };
        let dir = kept_dir(ctx, call);
        let mut result_files = Vec::with_capacity(outcome.files.len());
        for kept in outcome.files {
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| CallError::Internal(e.into()))?;
            tokio::fs::write(dir.join(kept_name(&kept.file)), &kept.bytes)
                .await
                .map_err(|e| CallError::Internal(e.into()))?;
            result_files.push(kept.file);
        }
        Ok(Polled {
            progress: polled.progress,
            outcome: Some(Outcome {
                status: outcome.status,
                note: outcome.note,
                result: outcome.result,
                result_files,
            }),
        })
    }

    async fn result_file(
        &self,
        ctx: &CallCtx,
        call: &CallRecord,
        path: &str,
    ) -> Result<Option<Vec<u8>>, CallError> {
        let Some(file) = call
            .result_files
            .iter()
            .find(|f| f.path.as_deref() == Some(path))
        else {
            return Ok(None);
        };
        match tokio::fs::read(kept_dir(ctx, call).join(kept_name(file))).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CallError::Internal(e.into())),
        }
    }

    async fn cancel(&self, _ctx: &CallCtx, call: &CallRecord) -> Result<(), CallError> {
        let (repo, run_id) = run_of(call)?;
        Ok(GithubWorkflowBackend::cancel(self, repo, run_id).await?)
    }
}

#[cfg(test)]
mod tests {
    use crate::calls::github::GithubError;
    use crate::calls::github::backend::following;
    use crate::calls::{CallError, CallRecord};
    use crucible_contract::calls::{
        CallAlias, CallId, CallKind, CallStatus, CallTarget, PlaybookRef, TargetInterface,
    };
    use serde_json::{Value, json};

    fn record(created_at: &str) -> CallRecord {
        CallRecord {
            id: CallId("c".into()),
            parent_run: "r".into(),
            task: "t".into(),
            instance: None,
            attempt: 1,
            alias: CallAlias::parse("ci").unwrap(),
            kind: CallKind::GithubWorkflow,
            target: CallTarget::Playbook {
                playbook: PlaybookRef::Draft("d".into()),
            },
            interface: TargetInterface {
                params: json!({}),
                result: vec![],
                result_files: vec![],
                graph: Default::default(),
            },
            handle: None,
            detail: json!({}),
            budget: None,
            status: CallStatus::Running,
            note: String::new(),
            result: Value::Null,
            result_files: vec![],
            progress: Default::default(),
            link: None,
            created_at: created_at.to_string(),
        }
    }

    fn api(status: u16) -> GithubError {
        GithubError::Api {
            doing: "listing the run's jobs".into(),
            status,
            body: String::new(),
        }
    }

    #[test]
    fn a_run_being_followed_ends_only_on_errors_that_stay() {
        let now: jiff::Timestamp = "2026-11-25T12:05:00Z".parse().unwrap();
        let fresh = record("2026-11-25T12:00:00Z");
        let old = record("2026-11-25T11:00:00Z");
        let retried =
            |e, call: &CallRecord| matches!(following(e, call, now), CallError::Internal(_));
        assert!(
            retried(api(404), &fresh),
            "a new run GitHub does not list yet"
        );
        assert!(!retried(api(404), &old), "a run gone for good");
        for status in [403, 429, 500, 502] {
            assert!(retried(api(status), &old), "{status} is asked again");
        }
        assert!(!retried(api(410), &fresh));
        assert!(!retried(api(422), &old));
    }
}
