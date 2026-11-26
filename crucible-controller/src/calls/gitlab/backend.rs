//! [`GitlabPipelineBackend`] as the [`TargetBackend`] a `gitlab_pipeline` call dispatches
//! through. Result files are kept in the database, so any replica serves them after a restart.

use crucible_ci::gitlab::GitlabError;
use crucible_contract::calls::{CallTarget, LockedCall, TargetHandle};
use serde_json::json;

use crate::authz::model::Principal;
use crate::calls::gitlab::{GitlabOutcome, GitlabPipelineBackend, TargetError};
use crate::calls::{
    CallCtx, CallError, CallRecord, DispatchSpec, Dispatched, Outcome, Polled, Resolved,
    TargetBackend,
};
use crate::identity::forge::{Forge, ForgeHost};

const FRESH_PIPELINE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

impl From<TargetError> for CallError {
    fn from(e: TargetError) -> Self {
        match e.is_transient() {
            true => CallError::Internal(anyhow::Error::new(e)),
            false => CallError::Refused(e.to_string()),
        }
    }
}

fn following(e: TargetError, call: &CallRecord, now: jiff::Timestamp) -> CallError {
    let fresh = age(call, now).is_some_and(|age| age < FRESH_PIPELINE);
    match &e {
        TargetError::Gitlab(GitlabError::Status {
            status: 403 | 429, ..
        }) => CallError::Internal(anyhow::Error::new(e)),
        TargetError::Gitlab(GitlabError::Status { status: 404, .. }) if fresh => {
            CallError::Internal(anyhow::Error::new(e))
        }
        _ => e.into(),
    }
}

fn age(call: &CallRecord, now: jiff::Timestamp) -> Option<std::time::Duration> {
    let started = call.created_at.parse::<jiff::Timestamp>().ok()?;
    let secs = now.duration_since(started).as_secs();
    u64::try_from(secs).ok().map(std::time::Duration::from_secs)
}

fn pipeline_of(call: &CallRecord) -> Result<u64, CallError> {
    match &call.handle {
        Some(TargetHandle::GitlabPipeline { pipeline_id, .. }) => Ok(*pipeline_id),
        _ => Err(CallError::Refused(format!(
            "call {} has no GitLab pipeline",
            call.id
        ))),
    }
}

impl GitlabPipelineBackend {
    async fn kept(
        &self,
        ctx: &CallCtx,
        call: &CallRecord,
        outcome: GitlabOutcome,
    ) -> Result<Outcome, CallError> {
        let mut result_files = Vec::with_capacity(outcome.files.len());
        for kept in outcome.files {
            crate::calls::store::put_result_file(ctx.db.pool(), &call.id, &kept.file, &kept.bytes)
                .await?;
            result_files.push(kept.file);
        }
        Ok(Outcome {
            status: outcome.status,
            note: outcome.note,
            result: outcome.result,
            result_files,
        })
    }
}

#[async_trait::async_trait]
impl TargetBackend for GitlabPipelineBackend {
    async fn interface(&self, _ctx: &CallCtx, target: &CallTarget) -> Result<Resolved, CallError> {
        let LockedCall { pin, interface, .. } =
            GitlabPipelineBackend::interface(self, target).await?;
        Ok(Resolved { pin, interface })
    }

    async fn authorize(
        &self,
        ctx: &CallCtx,
        who: &Principal,
        target: &CallTarget,
    ) -> Result<(), CallError> {
        let linked = match (who, target) {
            (Principal::User(login), CallTarget::GitlabPipeline { host, .. }) => {
                match ForgeHost::parse(host) {
                    Some(host) => crate::identity::forge::links::get(
                        ctx.db.pool(),
                        login,
                        Forge::Gitlab,
                        &host,
                    )
                    .await
                    .map_err(|e| CallError::Internal(e.into()))?,
                    None => None,
                }
            }
            _ => None,
        };
        Ok(
            GitlabPipelineBackend::authorize(self, who, target, linked.map(|l| l.account_id))
                .await?,
        )
    }

    async fn dispatch(
        &self,
        _ctx: &CallCtx,
        spec: &DispatchSpec<'_>,
    ) -> Result<Dispatched, CallError> {
        let dispatched = GitlabPipelineBackend::dispatch(
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
            detail: json!({"commit": dispatched.commit}),
        })
    }

    async fn poll(&self, ctx: &CallCtx, call: &CallRecord) -> Result<Polled, CallError> {
        let pipeline_id = pipeline_of(call)?;
        let now = jiff::Timestamp::now();
        let polled = GitlabPipelineBackend::poll(
            self,
            &call.target,
            pipeline_id,
            &call.interface.graph,
            false,
        )
        .await
        .map_err(|e| following(e, call, now))?;
        let mut recorded = serde_json::Map::new();
        if let Some(number) = polled.number {
            recorded.insert("run_number".to_owned(), number.into());
        }
        let outcome = match polled.outcome {
            Some(outcome) => Some(self.kept(ctx, call, outcome).await?),
            None => {
                let bound = self.wall_clock(call.budget);
                match age(call, now).is_some_and(|age| age > bound) {
                    true => {
                        GitlabPipelineBackend::cancel(self, &call.target, pipeline_id)
                            .await
                            .map_err(|e| following(e, call, now))?;
                        Some(Outcome::failed(format!(
                            "the pipeline ran past the call's wall clock of {}s",
                            bound.as_secs()
                        )))
                    }
                    false => None,
                }
            }
        };
        Ok(Polled {
            progress: polled.progress,
            recorded,
            outcome,
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
        Ok(crate::calls::store::result_file(ctx.db.pool(), &call.id, path, &file.digest).await?)
    }

    async fn cancel(&self, _ctx: &CallCtx, call: &CallRecord) -> Result<(), CallError> {
        let pipeline_id = pipeline_of(call)?;
        Ok(GitlabPipelineBackend::cancel(self, &call.target, pipeline_id).await?)
    }

    async fn node_log(
        &self,
        _ctx: &CallCtx,
        call: &CallRecord,
        node: &str,
    ) -> Result<String, CallError> {
        let pipeline_id = pipeline_of(call)?;
        GitlabPipelineBackend::node_log(self, &call.target, pipeline_id, node)
            .await
            .map_err(|e| following(e, call, jiff::Timestamp::now()))?
            .ok_or_else(|| CallError::Refused(format!("the pipeline has no job {node:?}")))
    }
}

#[cfg(test)]
mod tests {
    use crucible_ci::gitlab::GitlabError;
    use crucible_contract::calls::{
        CallAlias, CallId, CallKind, CallStatus, CallTarget, PlaybookRef, TargetHandle,
        TargetInterface,
    };
    use serde_json::json;

    use crate::calls::gitlab::TargetError;
    use crate::calls::gitlab::backend::{following, pipeline_of};
    use crate::calls::{CallError, CallRecord};

    fn record(created_at: &str, handle: Option<TargetHandle>) -> CallRecord {
        CallRecord {
            id: CallId("c".into()),
            parent_run: "r".into(),
            task: "t".into(),
            instance: None,
            attempt: 1,
            alias: CallAlias::parse("ship").unwrap(),
            kind: CallKind::GitlabPipeline,
            target: CallTarget::Playbook {
                playbook: PlaybookRef::Draft("d".into()),
            },
            interface: TargetInterface {
                params: json!({}),
                result: vec![],
                result_files: vec![],
                graph: Default::default(),
            },
            handle,
            detail: json!({}),
            budget: None,
            status: CallStatus::Running,
            note: String::new(),
            result: json!(null),
            result_files: vec![],
            progress: Default::default(),
            link: None,
            created_at: created_at.into(),
        }
    }

    fn status(code: u16) -> TargetError {
        TargetError::Gitlab(GitlabError::Status {
            doing: "reading".into(),
            status: code,
            body: String::new(),
        })
    }

    #[test]
    fn a_pipeline_being_followed_ends_only_on_errors_that_stay() {
        let now: jiff::Timestamp = "2026-11-26T12:00:00Z".parse().unwrap();
        let fresh = record("2026-11-26T11:58:00Z", None);
        let old = record("2026-11-26T10:00:00Z", None);
        assert!(matches!(
            following(status(404), &fresh, now),
            CallError::Internal(_)
        ));
        assert!(matches!(
            following(status(404), &old, now),
            CallError::Refused(_)
        ));
        assert!(matches!(
            following(status(429), &old, now),
            CallError::Internal(_)
        ));
        assert!(matches!(
            following(status(403), &old, now),
            CallError::Internal(_)
        ));
        assert!(matches!(
            following(status(502), &old, now),
            CallError::Internal(_)
        ));
        assert!(matches!(
            following(status(400), &fresh, now),
            CallError::Refused(_)
        ));
    }

    #[test]
    fn only_a_gitlab_handle_names_a_pipeline() {
        let handled = record(
            "2026-11-26T11:58:00Z",
            Some(TargetHandle::GitlabPipeline {
                host: "gitlab.com".into(),
                project: "g/p".into(),
                pipeline_id: 42,
            }),
        );
        assert_eq!(pipeline_of(&handled).unwrap(), 42);
        assert!(pipeline_of(&record("2026-11-26T11:58:00Z", None)).is_err());
    }
}
