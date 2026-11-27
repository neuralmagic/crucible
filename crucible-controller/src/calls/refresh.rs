//! The call refresh loop: a source of the launch loop that brings each dispatched, unended call
//! up to date with its target run at its backend's cadence, and asks again to stop a target run
//! whose call ended while stopping it failed. Readers and the engine read the store; only this
//! loop polls target systems, settles, or cuts a call off.

use tracing::Instrument as _;

use crate::calls::{Backends, CallCtx, CallError, CallRecord, Outcome, Polled, REFRESH_EVERY};
use crate::client::Db;
use crate::daemon::queue::{BoxFuture, DueSource, Pass, WorkQueue};
use crucible_contract::calls::{CallStatus, NodeStatus};

const PASS_LIMIT: i64 = 64;

/// The launch-loop source that refreshes calls.
pub(crate) struct CallRefresh {
    db: Db,
    cfg: crate::config::ControllerCfg,
}

impl CallRefresh {
    pub(crate) fn new(db: Db, cfg: crate::config::ControllerCfg) -> Self {
        CallRefresh { db, cfg }
    }
}

impl DueSource for CallRefresh {
    fn next_due(&self, _held: Vec<String>) -> BoxFuture<anyhow::Result<Option<jiff::Timestamp>>> {
        let db = self.db.clone();
        Box::pin(async move {
            let Some(earliest) = crate::calls::store::earliest_due(db.pool()).await? else {
                return Ok(None);
            };
            Ok(Some(earliest.parse()?))
        })
    }

    fn pass(&self, _queue: WorkQueue, _held: Vec<String>) -> BoxFuture<anyhow::Result<Pass>> {
        let db = self.db.clone();
        let cfg = self.cfg.clone();
        Box::pin(async move {
            let ctx = CallCtx {
                db,
                scratch_dir: cfg.scratch_root().to_path_buf(),
            };
            let claimed =
                refresh_due(&Backends::following(&cfg), &ctx, jiff::Timestamp::now()).await?;
            Ok(Pass {
                claimed,
                held: Vec::new(),
            })
        })
    }
}

pub(crate) async fn refresh_due(
    backends: &Backends,
    ctx: &CallCtx,
    now: jiff::Timestamp,
) -> anyhow::Result<usize> {
    let due =
        crate::calls::store::due(ctx.db.pool(), &crate::clock::stamp(now), PASS_LIMIT).await?;
    let count = due.len();
    for (record, cancel_owed) in due {
        let id = record.id.clone();
        let every = backends
            .get(record.kind)
            .map_or(REFRESH_EVERY, |b| b.cadence(&record, now));
        let next = now.checked_add(jiff::SignedDuration::try_from(every)?)?;
        crate::calls::store::poll_at(ctx.db.pool(), &id, &crate::clock::stamp(next)).await?;
        let span = tracing::info_span!(
            "calls.poll",
            call = %id,
            kind = %record.kind,
            run = %record.parent_run,
            task = %record.task,
            "calls.next_poll_secs" = every.as_secs(),
        );
        let looked = async {
            match record.status.ended() {
                true if cancel_owed => stop_again(backends, ctx, &record).await,
                true => Ok(()),
                false => refresh(backends, ctx, record).await.map(drop),
            }
        }
        .instrument(span)
        .await;
        if let Err(e) = looked {
            tracing::warn!(call = %id, error = %format!("{e:#}"), "refreshing a call failed");
            crate::calls::store::touched(ctx.db.pool(), &id).await?;
        }
    }
    Ok(count)
}

async fn stop_again(backends: &Backends, ctx: &CallCtx, record: &CallRecord) -> anyhow::Result<()> {
    let pool = ctx.db.pool();
    let stopped = match backends.get(record.kind) {
        Ok(backend) => backend.cancel(ctx, record).await,
        Err(e) => Err(e),
    };
    match stopped {
        Ok(()) => crate::calls::store::owe_cancel(pool, &record.id, false).await,
        Err(CallError::Internal(e)) => {
            tracing::warn!(call = %record.id, error = %format!("{e:#}"), "stopping a call's target run failed again");
            crate::calls::store::touched(pool, &record.id).await
        }
        Err(e) => {
            tracing::warn!(call = %record.id, error = %e.note(), "a call's target run cannot be stopped; giving up");
            crate::calls::store::owe_cancel(pool, &record.id, false).await
        }
    }
}

pub(crate) async fn refresh(
    backends: &Backends,
    ctx: &CallCtx,
    record: CallRecord,
) -> anyhow::Result<CallRecord> {
    if record.status.ended() || record.handle.is_none() {
        return Ok(record);
    }
    let pool = ctx.db.pool();
    let backend = match backends.get(record.kind) {
        Ok(backend) => backend,
        Err(e) => {
            crate::calls::store::settle(
                pool,
                &record.id,
                &Outcome::failed(e.note()),
                &record.progress,
            )
            .await?;
            return crate::calls::api::reread(ctx, record).await;
        }
    };
    match backend.poll(ctx, &record).await {
        Ok(Polled {
            progress,
            recorded,
            outcome,
            link,
        }) => {
            let new = recorded
                .iter()
                .any(|(key, value)| record.detail.get(key) != Some(value));
            if new {
                crate::calls::store::noted(pool, &record.id, &recorded).await?;
            }
            if let Some(link) = link.filter(|link| record.link.as_ref() != Some(link)) {
                crate::calls::store::linked(pool, &record.id, &link).await?;
            }
            match outcome {
                Some(outcome) => {
                    crate::calls::store::settle(pool, &record.id, &outcome, &progress).await?;
                }
                None => {
                    let started = progress
                        .nodes
                        .iter()
                        .any(|n| n.status != NodeStatus::Pending);
                    let status = match started {
                        true => CallStatus::Running,
                        false => record.status,
                    };
                    match status == record.status && progress == record.progress {
                        true => crate::calls::store::touched(pool, &record.id).await?,
                        false => {
                            crate::calls::store::progressed(pool, &record.id, status, &progress)
                                .await?
                        }
                    }
                }
            }
        }
        Err(CallError::Internal(e)) => {
            tracing::warn!(call = %record.id, error = %format!("{e:#}"), "following a call's target run failed; its last state stands");
            crate::calls::store::touched(pool, &record.id).await?;
        }
        Err(CallError::Throttled { after, why }) => {
            tracing::warn!(call = %record.id, wait_secs = after.as_secs(), error = %why, "the target system is rate limiting; waiting");
            let next =
                jiff::Timestamp::now().checked_add(jiff::SignedDuration::try_from(after)?)?;
            crate::calls::store::poll_at(pool, &record.id, &crate::clock::stamp(next)).await?;
        }
        Err(refused) => {
            crate::calls::api::stop_target(backends, ctx, &record).await?;
            crate::calls::store::settle(
                pool,
                &record.id,
                &Outcome::failed(refused.note()),
                &record.progress,
            )
            .await?;
        }
    }
    crate::calls::api::reread(ctx, record).await
}

#[cfg(test)]
mod tests {
    use crate::calls::refresh::*;
    use crate::calls::store::{Claimed, NewCall};
    use crate::calls::{CallCtx, Dispatched};
    use crucible_contract::calls::{
        CallAlias, CallId, CallTarget, PlaybookRef, ProgressGraph, ProgressNode, TargetHandle,
        TargetInterface,
    };
    use serde_json::json;
    use sqlx::PgPool;

    fn interface() -> TargetInterface {
        TargetInterface {
            params: json!({"type": "object", "properties": {}}),
            result: vec![],
            result_files: vec![],
            graph: ProgressGraph {
                nodes: vec![ProgressNode {
                    id: "hello".into(),
                    label: "hello".into(),
                    status: NodeStatus::Running,
                    group: None,
                    started: None,
                    ended: None,
                    detail: None,
                    link: None,
                }],
                edges: vec![],
                omitted: 0,
            },
        }
    }

    async fn run(pool: &PgPool, run: &str, launch: &str, status: &str) {
        crate::issues::store::upsert_issue(
            pool,
            &crate::issues::model::NewIssue {
                key: launch.to_string(),
                repo: "(draft)".to_string(),
                priority: 0,
                evidence_url: None,
                title: None,
                author: None,
                body: None,
                labels: Vec::new(),
                upstream_updated_at: None,
            },
        )
        .await
        .expect("launch");
        crate::runs::store::insert_run(
            pool,
            &crate::NewRun {
                run_id: run.to_string(),
                scope: None,
                issue: Some(launch.to_string()),
                identity_digest: None,
                status: status.to_string(),
                pod: None,
                session_uri: None,
                best_score: None,
                cost_usd: None,
            },
        )
        .await
        .expect("run");
    }

    async fn claim(pool: &PgPool, parent: &str, attempt: u32, target: &CallTarget) -> CallId {
        let alias = CallAlias::parse("ask").expect("alias");
        match crate::calls::store::claim(
            pool,
            &NewCall {
                parent_run: parent,
                task: "ask",
                instance: None,
                attempt,
                alias: &alias,
                target,
                interface: &interface(),
                bound: json!({}),
                budget: None,
            },
        )
        .await
        .expect("claim")
        {
            Claimed::Fresh(id) => id,
            Claimed::Existing(_) => panic!("identity already held"),
        }
    }

    fn playbook() -> CallTarget {
        CallTarget::Playbook {
            playbook: PlaybookRef::Registered("triage".into()),
        }
    }

    fn started(launch: &str, run: &str) -> Dispatched {
        Dispatched {
            handle: TargetHandle::Playbook { run: run.into() },
            link: None,
            detail: json!({"launch": launch}),
        }
    }

    fn ctx(pool: &PgPool, dir: &std::path::Path) -> (Backends, CallCtx) {
        let mut cfg = crate::testing::cfg_with(dir);
        cfg.calls_github_token = None;
        let ctx = CallCtx {
            db: Db::new(pool.clone()),
            scratch_dir: dir.to_path_buf(),
        };
        (Backends::following(&cfg), ctx)
    }

    fn later(secs: i64) -> jiff::Timestamp {
        jiff::Timestamp::now()
            .checked_add(jiff::SignedDuration::from_secs(secs))
            .expect("later")
    }

    async fn status(pool: &PgPool, id: &CallId) -> (String, bool, String) {
        sqlx::query_as("SELECT status, cancel_owed, note FROM call_target_runs WHERE id = $1")
            .bind(&id.0)
            .fetch_one(pool)
            .await
            .expect("call row")
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_call_is_looked_at_once_it_is_due(pool: PgPool) {
        let dir = tempfile::tempdir().expect("dir");
        let (backends, ctx) = ctx(&pool, dir.path());
        run(&pool, "caller", "playbook:caller:1", "running").await;
        let id = claim(&pool, "caller", 1, &playbook()).await;
        let claimed = crate::calls::store::get(&pool, "caller", &id.0)
            .await
            .expect("read")
            .expect("row");
        assert_eq!(claimed.progress.nodes.len(), 1);
        assert_eq!(claimed.progress.nodes[0].status, NodeStatus::Pending);
        assert!(
            !crate::calls::store::dispatched(&pool, &id, &started("playbook:gone:1", "gone-1"))
                .await
                .expect("dispatched")
        );

        assert_eq!(
            refresh_due(&backends, &ctx, jiff::Timestamp::now())
                .await
                .expect("pass"),
            0,
            "a call just dispatched is not due"
        );
        assert_eq!(
            status(&pool, &id).await.0,
            "pending",
            "nothing settles before the loop looks"
        );
        assert_eq!(
            refresh_due(&backends, &ctx, later(60)).await.expect("pass"),
            1
        );
        let (settled, owed, note) = status(&pool, &id).await;
        assert_eq!(settled, "failed");
        assert!(!owed);
        assert!(note.contains("playbook:gone:1 is gone"), "{note}");
        assert_eq!(
            refresh_due(&backends, &ctx, later(120))
                .await
                .expect("pass"),
            0,
            "an ended call owing nothing is not looked at again"
        );
        assert!(
            crate::calls::store::earliest_due(&pool)
                .await
                .expect("due")
                .is_none()
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn each_call_waits_its_backends_cadence_between_looks(pool: PgPool) {
        let dir = tempfile::tempdir().expect("dir");
        let (backends, ctx) = ctx(&pool, dir.path());
        run(&pool, "caller", "playbook:caller:1", "running").await;
        run(&pool, "t-1", "playbook:triage:1", "running").await;
        let id = claim(&pool, "caller", 1, &playbook()).await;
        crate::calls::store::dispatched(&pool, &id, &started("playbook:triage:1", "t-1"))
            .await
            .expect("dispatched");

        let first = later(60);
        assert_eq!(refresh_due(&backends, &ctx, first).await.expect("pass"), 1);
        assert_eq!(
            status(&pool, &id).await.0,
            "pending",
            "the target still runs"
        );
        let next = crate::calls::store::earliest_due(&pool)
            .await
            .expect("due")
            .expect("still followed");
        assert_eq!(
            next,
            crate::clock::stamp(
                first
                    .checked_add(jiff::SignedDuration::try_from(REFRESH_EVERY).unwrap())
                    .unwrap()
            )
        );
        let at = |secs: i64| {
            first
                .checked_add(jiff::SignedDuration::from_secs(secs))
                .unwrap()
        };
        assert_eq!(
            refresh_due(&backends, &ctx, at(3)).await.expect("pass"),
            0,
            "not before its next look"
        );
        assert_eq!(refresh_due(&backends, &ctx, at(6)).await.expect("pass"), 1);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_owed_cancel_is_asked_again(pool: PgPool) {
        let dir = tempfile::tempdir().expect("dir");
        let (backends, ctx) = ctx(&pool, dir.path());
        run(&pool, "caller", "playbook:caller:1", "running").await;
        let owed = claim(&pool, "caller", 1, &playbook()).await;
        crate::calls::store::dispatched(&pool, &owed, &started("playbook:triage:1", "t-1"))
            .await
            .expect("dispatched");
        let github: CallTarget = serde_json::from_value(json!({
            "kind": "github_workflow",
            "repo": "acme/app",
            "workflow": ".github/workflows/x.yml",
            "ref": "main",
            "outputs": {"artifact": "result"},
        }))
        .expect("target");
        let unreachable = claim(&pool, "caller", 2, &github).await;
        crate::calls::store::dispatched(
            &pool,
            &unreachable,
            &Dispatched {
                handle: TargetHandle::GithubWorkflow {
                    repo: crucible_contract::calls::GithubRepo::parse("acme/app").expect("repo"),
                    run_id: 7,
                },
                link: None,
                detail: json!({}),
            },
        )
        .await
        .expect("dispatched");
        for id in [&owed, &unreachable] {
            crate::calls::store::settle(
                &pool,
                id,
                &Outcome::cancelled("cancelled"),
                &ProgressGraph::default(),
            )
            .await
            .expect("settle");
            crate::calls::store::owe_cancel(&pool, id, true)
                .await
                .expect("owe");
        }
        assert!(
            crate::calls::store::earliest_due(&pool)
                .await
                .expect("due")
                .is_some()
        );

        assert_eq!(
            refresh_due(&backends, &ctx, later(60)).await.expect("pass"),
            2
        );
        assert_eq!(
            status(&pool, &owed).await,
            ("cancelled".into(), false, "cancelled".into())
        );
        assert!(
            !status(&pool, &unreachable).await.1,
            "a kind this controller cannot reach is given up"
        );
        let stashed: Vec<String> =
            sqlx::query_scalar("SELECT key FROM pending_overrides WHERE kind = 'cancel'")
                .fetch_all(&pool)
                .await
                .expect("overrides");
        assert_eq!(stashed, ["playbook:triage:1"]);
        assert_eq!(
            refresh_due(&backends, &ctx, later(120))
                .await
                .expect("pass"),
            0
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_cancel_during_dispatch_stops_the_target_it_started(pool: PgPool) {
        let dir = tempfile::tempdir().expect("dir");
        let cfg = crate::testing::cfg_with(dir.path());
        let db = Db::new(pool.clone());
        run(&pool, "caller", "playbook:caller:1", "running").await;
        let in_flight = claim(&pool, "caller", 1, &playbook()).await;
        assert_eq!(
            crate::calls::api::cancel_for_launch(&db, &cfg, "playbook:caller:1")
                .await
                .expect("cancel"),
            1,
            "a call still being dispatched is cancelled with its launch"
        );
        assert_eq!(status(&pool, &in_flight).await.0, "cancelled");
        assert!(
            crate::calls::store::dispatched(&pool, &in_flight, &started("playbook:t:1", "t-1"))
                .await
                .expect("dispatched"),
            "the dispatch lands on an ended call"
        );

        let live = claim(&pool, "caller", 2, &playbook()).await;
        assert!(
            !crate::calls::store::dispatched(&pool, &live, &started("playbook:t:2", "t-2"))
                .await
                .expect("dispatched")
        );
        crate::runs::store::set_run_status(&pool, "caller", "cancelled")
            .await
            .expect("cancel the caller");
        let orphan = claim(&pool, "caller", 3, &playbook()).await;
        assert!(
            crate::calls::store::dispatched(&pool, &orphan, &started("playbook:t:3", "t-3"))
                .await
                .expect("dispatched"),
            "the calling run was cancelled"
        );
        assert!(
            crate::calls::store::dispatched(&pool, &CallId("nobody".into()), &started("k", "r"))
                .await
                .is_err(),
            "a dispatch lands only on a claim"
        );
    }
}
