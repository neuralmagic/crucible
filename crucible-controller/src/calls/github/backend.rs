//! [`GithubWorkflowBackend`] as the [`TargetBackend`] a `github_workflow` call dispatches
//! through. Result files are kept in the database, so any replica serves them after a restart.

use crucible_contract::calls::{CallTarget, GithubRepo, LockedCall, TargetHandle};
use serde_json::{Value, json};

use forge::github::GithubError;

use crate::authz::model::Principal;
use crate::calls::github::{
    GithubOutcome, GithubWorkflowBackend, Seen, SeenRun, TargetError, github_target,
};
use crate::calls::{
    CallCtx, CallError, CallRecord, DispatchSpec, Dispatched, Outcome, Polled, Resolved,
    TargetBackend,
};

impl From<TargetError> for CallError {
    fn from(e: TargetError) -> Self {
        match e.is_transient() {
            true => CallError::Internal(anyhow::Error::new(e)),
            false => CallError::Refused(e.to_string()),
        }
    }
}

const FRESH_RUN: std::time::Duration = std::time::Duration::from_secs(10 * 60);

fn following(e: TargetError, call: &CallRecord, now: jiff::Timestamp) -> CallError {
    let fresh = call.age(now).is_some_and(|age| age < FRESH_RUN);
    if let TargetError::Github(api) = &e
        && let Some(after) = api.retry_after()
    {
        return CallError::Throttled {
            after,
            why: e.to_string(),
        };
    }
    match &e {
        TargetError::Github(GithubError::Api {
            status: 403 | 429, ..
        }) => CallError::Internal(anyhow::Error::new(e)),
        TargetError::Github(GithubError::Api { status: 404, .. }) if fresh => {
            CallError::Internal(anyhow::Error::new(e))
        }
        _ => e.into(),
    }
}

const RUN_ETAG: &str = "run_etag";
const JOBS_ETAG: &str = "jobs_etag";

fn seen(call: &CallRecord) -> Seen<'_> {
    let text = |key: &str| call.detail.get(key).and_then(Value::as_str);
    let run = match (text(RUN_ETAG), text("head_sha")) {
        (Some(etag), Some(head_sha)) => Some(SeenRun {
            etag,
            head_sha,
            run_number: call.detail.get("run_number").and_then(Value::as_u64),
        }),
        _ => None,
    };
    Seen {
        run,
        jobs: text(JOBS_ETAG).map(|etag| (etag, &call.progress)),
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

fn run_links(repo: &GithubRepo, head_sha: &str) -> Vec<crucible_contract::calls::NamedLink> {
    let base = format!("https://github.com/{}", repo.as_str());
    [
        ("Repo", base.clone()),
        ("Commit", format!("{base}/commit/{head_sha}")),
    ]
    .into_iter()
    .filter(|(label, _)| *label != "Commit" || !head_sha.is_empty())
    .filter_map(|(label, url)| {
        crucible_contract::link::ExternalUrl::new(url)
            .ok()
            .map(|url| crucible_contract::calls::NamedLink {
                label: label.to_owned(),
                url,
            })
    })
    .collect()
}

fn unchecked_commit(detail: &Value) -> Option<&str> {
    match detail.get("head_sha") {
        Some(_) => None,
        None => detail.get("commit").and_then(Value::as_str),
    }
}

impl GithubWorkflowBackend {
    async fn kept(
        &self,
        ctx: &CallCtx,
        call: &CallRecord,
        outcome: GithubOutcome,
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
impl TargetBackend for GithubWorkflowBackend {
    async fn interface(&self, _ctx: &CallCtx, target: &CallTarget) -> Result<Resolved, CallError> {
        let LockedCall { pin, interface, .. } =
            GithubWorkflowBackend::interface(self, target).await?;
        Ok(Resolved { pin, interface })
    }

    async fn authorize(
        &self,
        ctx: &CallCtx,
        who: &Principal,
        target: &CallTarget,
    ) -> Result<(), CallError> {
        let (repo, ..) = github_target(target)?;
        let linked = match who {
            Principal::User(login) => crate::identity::forge::links::get(
                ctx.db.pool(),
                login,
                crate::identity::forge::Forge::Github,
                &crate::identity::forge::ForgeHost::github(),
            )
            .await
            .map_err(|e| CallError::Internal(e.into()))?,
            _ => None,
        };
        let linked = linked.map(|l| l.account_id);
        Ok(GithubWorkflowBackend::authorize(self, who, repo, linked).await?)
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
            detail: json!({"commit": dispatched.commit}),
        })
    }

    fn cadence(&self, call: &CallRecord, now: jiff::Timestamp) -> std::time::Duration {
        call.aging_cadence(now)
    }

    async fn poll(&self, ctx: &CallCtx, call: &CallRecord) -> Result<Polled, CallError> {
        let (repo, run_id) = run_of(call)?;
        let (_, _, _, outputs) = github_target(&call.target)?;
        let now = jiff::Timestamp::now();
        let polled = GithubWorkflowBackend::poll(
            self,
            repo,
            run_id,
            &call.interface.graph,
            outputs,
            false,
            seen(call),
        )
        .await
        .map_err(|e| following(e, call, now))?;
        let mut recorded = serde_json::Map::new();
        if let Some(number) = polled.run_number {
            recorded.insert("run_number".to_owned(), number.into());
        }
        for (key, etag) in [(RUN_ETAG, &polled.run_etag), (JOBS_ETAG, &polled.jobs_etag)] {
            if let Some(etag) = etag {
                recorded.insert(key.to_owned(), etag.clone().into());
            }
        }
        if let Some(commit) = unchecked_commit(&call.detail) {
            let moved = self
                .moved_head(&call.target, &call.interface, commit, &polled.head_sha)
                .await
                .map_err(|e| following(e, call, now))?;
            if let Some(moved) = moved {
                GithubWorkflowBackend::cancel(self, repo, run_id)
                    .await
                    .map_err(|e| following(e, call, now))?;
                return Ok(Polled {
                    progress: polled.progress,
                    recorded,
                    outcome: Some(Outcome::failed(format!(
                        "{moved}; run {run_id} was cancelled"
                    ))),
                    link: None,
                });
            }
        }
        crate::calls::record_links(&mut recorded, &run_links(repo, &polled.head_sha));
        recorded.insert("head_sha".to_owned(), polled.head_sha.into());
        let outcome = match polled.outcome {
            Some(outcome) => Some(self.kept(ctx, call, outcome).await?),
            None => {
                let bound = self.wall_clock(call.budget);
                match call.age(now).is_some_and(|age| age > bound) {
                    true => {
                        GithubWorkflowBackend::cancel(self, repo, run_id)
                            .await
                            .map_err(|e| following(e, call, now))?;
                        Some(Outcome::failed(format!(
                            "the workflow run ran past the call's wall clock of {}s",
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
            link: None,
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
        let (repo, run_id) = run_of(call)?;
        Ok(GithubWorkflowBackend::cancel(self, repo, run_id).await?)
    }

    async fn node_log(
        &self,
        _ctx: &CallCtx,
        call: &CallRecord,
        node: &str,
    ) -> Result<String, CallError> {
        let (repo, run_id) = run_of(call)?;
        GithubWorkflowBackend::node_log(self, repo, run_id, &call.interface.graph, node)
            .await
            .map_err(|e| following(e, call, jiff::Timestamp::now()))?
            .ok_or_else(|| {
                CallError::Refused(format!("the workflow run has no job drawn as {node:?}"))
            })
    }
}

#[cfg(test)]
mod tests {
    use forge::github::GithubError;

    use crate::calls::github::TargetError;
    use crate::calls::github::backend::{following, run_links, seen, unchecked_commit};
    use crate::calls::{CallError, CallRecord};
    use crucible_contract::calls::{
        CallAlias, CallId, CallKind, CallStatus, CallTarget, FileRef, PlaybookRef, TargetInterface,
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

    fn api(status: u16) -> TargetError {
        TargetError::Github(GithubError::Api {
            doing: "listing the run's jobs".into(),
            status,
            body: String::new(),
            retry_after: None,
        })
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
        let limited = TargetError::Github(GithubError::Api {
            doing: "reading the run".into(),
            status: 403,
            body: String::new(),
            retry_after: Some(90),
        });
        assert!(matches!(
            following(limited, &old, now),
            CallError::Throttled { after, .. } if after.as_secs() == 90
        ));
    }

    #[test]
    fn a_look_sends_the_etags_the_last_look_kept() {
        let mut call = record("2026-11-25T12:00:00Z");
        let first = seen(&call);
        assert!(first.run.is_none() && first.jobs.is_none());
        call.detail = json!({"run_etag": "W/\"r\"", "jobs_etag": "W/\"j\""});
        let partial = seen(&call);
        assert!(
            partial.run.is_none(),
            "a run ETag without the head it saw is not used"
        );
        assert_eq!(partial.jobs.map(|(etag, _)| etag), Some("W/\"j\""));
        call.detail = json!({
            "run_etag": "W/\"r\"",
            "head_sha": "abc",
            "run_number": 14,
        });
        let full = seen(&call).run.expect("run seen");
        assert_eq!(
            (full.etag, full.head_sha, full.run_number),
            ("W/\"r\"", "abc", Some(14))
        );
    }

    #[test]
    fn a_run_is_looked_at_less_often_as_it_ages() {
        let now: jiff::Timestamp = "2026-11-25T12:00:00Z".parse().unwrap();
        let every = |created: &str| record(created).aging_cadence(now).as_secs();
        assert_eq!(every("2026-11-25T11:59:30Z"), 10);
        assert_eq!(every("2026-11-25T11:50:00Z"), 30);
        assert_eq!(every("2026-11-25T11:00:00Z"), 60);
        assert_eq!(every("not a time"), 30);
    }

    #[test]
    fn a_runs_head_is_checked_once_against_the_dispatched_commit() {
        assert_eq!(unchecked_commit(&json!({"commit": "abc"})), Some("abc"));
        assert_eq!(
            unchecked_commit(&json!({"commit": "abc", "head_sha": "abc"})),
            None,
            "a head already recorded was already checked"
        );
        assert_eq!(unchecked_commit(&json!({"head_sha": "abc"})), None);
        assert_eq!(unchecked_commit(&json!({})), None);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn result_files_are_served_from_the_database(pool: sqlx::PgPool) {
        use crate::calls::TargetBackend as _;
        crate::runs::store::insert_run(
            &pool,
            &crate::NewRun {
                run_id: "r".into(),
                scope: None,
                issue: None,
                identity_digest: None,
                status: "running".into(),
                pod: None,
                session_uri: None,
                best_score: None,
                cost_usd: None,
            },
        )
        .await
        .unwrap();
        let alias = CallAlias::parse("ci").unwrap();
        let mut call = record("2026-11-25T12:00:00Z");
        let crate::calls::store::Claimed::Fresh(id) = crate::calls::store::claim(
            &pool,
            &crate::calls::store::NewCall {
                parent_run: "r",
                task: "t",
                instance: None,
                attempt: 1,
                alias: &alias,
                target: &call.target,
                interface: &call.interface,
                bound: json!({}),
                budget: None,
            },
        )
        .await
        .unwrap() else {
            panic!("identity already held");
        };
        call.id = id;
        let bytes = b"# notes\n".to_vec();
        let file = FileRef {
            digest: crucible_contract::artifact::content_digest(&bytes),
            size: bytes.len() as u64,
            path: Some("notes.md".into()),
            media_type: None,
        };
        crate::calls::store::put_result_file(&pool, &call.id, &file, &bytes)
            .await
            .unwrap();
        call.result_files = vec![file.clone()];

        let ctx = |dir: &std::path::Path| crate::calls::CallCtx {
            db: crate::client::Db::new(pool.clone()),
            scratch_dir: dir.to_path_buf(),
        };
        let backend = crate::calls::github::GithubWorkflowBackend::new(
            crate::calls::github::TokenSource::Person(Default::default()),
            std::time::Duration::from_secs(60),
        )
        .unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        assert_eq!(
            backend
                .result_file(&ctx(elsewhere.path()), &call, "notes.md")
                .await
                .unwrap(),
            Some(bytes),
            "a scratch dir that never saw the run serves the file"
        );
        assert_eq!(
            backend
                .result_file(&ctx(elsewhere.path()), &call, "other.md")
                .await
                .unwrap(),
            None
        );
        call.result_files[0].digest = "sha256:00".into();
        assert_eq!(
            backend
                .result_file(&ctx(elsewhere.path()), &call, "notes.md")
                .await
                .unwrap(),
            None,
            "only the bytes the call settled with"
        );
    }

    #[test]
    fn a_calls_age_runs_from_its_dispatch() {
        let now: jiff::Timestamp = "2026-11-25T12:05:00Z".parse().unwrap();
        assert_eq!(
            record("2026-11-25T12:00:00Z").age(now),
            Some(std::time::Duration::from_secs(300))
        );
        assert_eq!(record("not a time").age(now), None);
    }

    #[test]
    fn a_run_names_its_repo_and_commit() {
        let repo = crucible_contract::calls::GithubRepo::parse("acme/app").unwrap();
        let links: Vec<(String, String)> = run_links(&repo, "abc")
            .into_iter()
            .map(|l| (l.label, l.url.as_str().to_owned()))
            .collect();
        assert_eq!(
            links,
            [
                ("Repo".to_owned(), "https://github.com/acme/app".to_owned()),
                (
                    "Commit".to_owned(),
                    "https://github.com/acme/app/commit/abc".to_owned()
                ),
            ]
        );
        assert_eq!(run_links(&repo, "").len(), 1);
    }
}
