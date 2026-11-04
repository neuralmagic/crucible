//! The claim a dispatch commits before it creates anything outside the database. The issue moves
//! to `running` and its run row, run name, and work-pod row land in one transaction, so a
//! controller that restarts mid-dispatch finds the issue claimed and never dispatches it twice. A
//! dispatch that fails after claiming releases the claim in one transaction too.

use crate::client::Db;
use crate::event_log::{self, Event};
use crate::model::Status;
use crate::runs::model::{NewRun, RunDispatch, RunImage, RunLocation};
use crate::runs::names::RunName;
use crate::runs::workpod::{NewWorkPod, WorkPodState};
use anyhow::Result;

/// The reason stamped on an attempt that could not start the engine. The launch view reads its
/// dispatch state back off these events, so the text is a shared constant rather than a literal.
pub(crate) const PLAYBOOK_DISPATCH_FAILED: &str = "playbook dispatch failed";

/// The reason stamped on a loop run whose dispatch failed.
#[cfg(feature = "autoresearch")]
pub(crate) const LOOP_DISPATCH_FAILED: &str = "loop-run dispatch failed";

/// What a run is dispatched for, which decides what its claim records.
#[derive(Debug, Clone)]
pub(crate) enum RunOrigin {
    /// A playbook launch: its spend is attributed to the launcher and its image was preflighted.
    Playbook { image: RunImage },
    /// An approved scope's loop run.
    #[cfg(feature = "autoresearch")]
    Loop {
        scope: i64,
        identity_digest: Option<String>,
    },
}

/// The issue a dispatch claims and the run it records for it.
#[derive(Debug, Clone)]
pub(crate) struct RunClaim<'a> {
    pub key: &'a str,
    /// The status the issue is dispatched from.
    pub from: Status,
    pub run_id: &'a str,
    pub origin: RunOrigin,
}

/// Where the claimed run is placed: decided by the dispatch, recorded with the claim.
pub(crate) struct RunPlacement<'a> {
    pub name: &'a RunName,
    pub dispatch: RunDispatch,
    pub location: &'a RunLocation,
    /// The pod's ledger row; `None` for a local run.
    pub work_pod: Option<&'a NewWorkPod>,
    /// Whether the run is stamped with the principal it is dispatched for.
    pub stamp_dispatcher: bool,
}

/// Context on a dispatch that failed after claiming its issue: the claim is released and the
/// failure is already on the issue's event log.
#[derive(Debug, thiserror::Error)]
#[error("released the run's claim")]
pub(crate) struct Released;

/// Record `e` on `key`'s event log as a dispatch from `from` that failed for `reason`, and hand it
/// back. A [`Released`] failure is already recorded and passes through untouched.
pub(crate) async fn record_dispatch_failure(
    db: &Db,
    key: &str,
    from: Status,
    reason: &str,
    e: anyhow::Error,
) -> anyhow::Error {
    if e.is::<Released>() {
        return e;
    }
    let _ = db
        .events()
        .append(&Event::now(
            key,
            from.as_str(),
            from.as_str(),
            Some(reason),
            Some(&crate::model::truncate_chain(&format!("{e:#}"))),
        ))
        .await;
    e
}

impl RunClaim<'_> {
    fn launched(&self, dispatch: RunDispatch) -> &'static str {
        match (&self.origin, dispatch) {
            (RunOrigin::Playbook { .. }, RunDispatch::Pod) => "playbook pod launched",
            (RunOrigin::Playbook { .. }, RunDispatch::Local) => "playbook running locally",
            #[cfg(feature = "autoresearch")]
            (RunOrigin::Loop { .. }, _) => "loop pod launched",
        }
    }

    /// The event reason a failed dispatch of this run records.
    pub(crate) fn failed(&self) -> &'static str {
        match self.origin {
            RunOrigin::Playbook { .. } => PLAYBOOK_DISPATCH_FAILED,
            #[cfg(feature = "autoresearch")]
            RunOrigin::Loop { .. } => LOOP_DISPATCH_FAILED,
        }
    }

    /// Record `e` on the issue's event log as a dispatch of this run that failed before claiming,
    /// and hand it back. A [`Released`] failure is already recorded and passes through untouched.
    pub(crate) async fn record_failure(&self, db: &Db, e: anyhow::Error) -> anyhow::Error {
        record_dispatch_failure(db, self.key, self.from, self.failed(), e).await
    }

    /// Claim `from` → `running` and record the run, its name, placement, and work-pod row in one
    /// transaction. Returns whether the claim won; a lost claim writes nothing.
    #[tracing::instrument(name = "db.claim_run", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %self.key, run_id = %self.run_id), err)]
    pub(crate) async fn claim(&self, db: &Db, placed: &RunPlacement<'_>) -> Result<bool> {
        let mut tx = db.pool().begin().await?;
        if !crate::issues::store::claim_issue(&mut *tx, self.key, self.from, Status::Running)
            .await?
        {
            return Ok(false);
        }
        let (scope, identity_digest, image) = match &self.origin {
            RunOrigin::Playbook { image } => (None, None, Some(image)),
            #[cfg(feature = "autoresearch")]
            RunOrigin::Loop {
                scope,
                identity_digest,
            } => (Some(*scope), identity_digest.clone(), None),
        };
        crate::runs::store::insert_run(
            &mut *tx,
            &NewRun {
                run_id: self.run_id.to_string(),
                scope,
                issue: Some(self.key.to_string()),
                identity_digest,
                status: "running".to_string(),
                pod: placed.work_pod.map(|w| w.pod_name.clone()),
                session_uri: None,
                best_score: None,
                cost_usd: None,
            },
        )
        .await?;
        if let Some(image) = image {
            crate::runs::store::attribute_run(&mut *tx, self.run_id).await?;
            crate::runs::store::set_run_image(&mut *tx, self.run_id, image).await?;
        }
        if placed.stamp_dispatcher {
            crate::runs::store::stamp_dispatcher(&mut *tx, self.run_id).await?;
        }
        crate::runs::store::set_run_dispatch(&mut *tx, self.run_id, placed.dispatch).await?;
        crate::runs::store::set_run_location(&mut *tx, self.run_id, placed.location).await?;
        crate::runs::names::set(&mut *tx, self.run_id, placed.name).await?;
        if let Some(work_pod) = placed.work_pod {
            crate::runs::work_pods::insert_work_pod(&mut *tx, work_pod).await?;
        }
        let ev = Event::now(
            self.key,
            self.from.as_str(),
            Status::Running.as_str(),
            Some(self.launched(placed.dispatch)),
            Some(self.run_id),
        );
        event_log::insert(&mut *tx, &ev).await?;
        tx.commit().await?;
        db.events().publish(&ev);
        Ok(true)
    }

    /// Undo a claim whose dispatch failed: the issue goes back to `from` with `e` on its event
    /// log, the run row goes, a dispatch request it consumed is restored, and its pod row (if
    /// any) is failed with the reason. An issue parked meanwhile stays parked and unparks to
    /// `from`. A release that cannot be written leaves the claim for the run completion edge, which settles
    /// a run whose pod is gone. Returns `e` marked [`Released`].
    #[tracing::instrument(name = "db.release_run", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %self.key, run_id = %self.run_id))]
    pub(crate) async fn release(
        &self,
        db: &Db,
        pod: Option<&str>,
        e: anyhow::Error,
    ) -> anyhow::Error {
        let evidence = crate::model::truncate_chain(&format!("{e:#}"));
        if let Err(release) = self.release_rows(db, pod, &evidence).await {
            tracing::warn!(
                issue_key = %self.key,
                run_id = %self.run_id,
                error = format!("{release:#}"),
                "releasing a failed dispatch's claim failed"
            );
        }
        e.context(Released)
    }

    async fn release_rows(&self, db: &Db, pod: Option<&str>, evidence: &str) -> Result<()> {
        let mut tx = db.pool().begin().await?;
        let released =
            crate::issues::store::claim_issue(&mut *tx, self.key, Status::Running, self.from)
                .await?;
        if !released {
            crate::issues::store::rewind_pre_park(&mut *tx, self.key, Status::Running, self.from)
                .await?;
        }
        crate::runs::store::delete_unstarted_run(&mut tx, self.run_id).await?;
        if let Some(pod) = pod {
            crate::runs::work_pods::set_work_pod_state(
                &mut *tx,
                pod,
                WorkPodState::Failed,
                None,
                Some(evidence),
            )
            .await?;
        }
        let ev = released.then(|| {
            Event::now(
                self.key,
                Status::Running.as_str(),
                self.from.as_str(),
                Some(self.failed()),
                Some(evidence),
            )
        });
        if let Some(ev) = &ev {
            event_log::insert(&mut *tx, ev).await?;
        }
        tx.commit().await?;
        if let Some(ev) = &ev {
            db.events().publish(ev);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::client::Db;
    use crate::model::{ParkedBy, Status};
    use crate::runs::claim::{Released, RunClaim, RunOrigin, RunPlacement};
    use crate::runs::model::{RunDispatch, RunImage, RunLocation};
    use crate::runs::workpod::{NewWorkPod, WorkPodState};
    use sqlx::PgPool;

    const KEY: &str = "playbook:survey:0199c0de-7c2c-71a5-8000-1";
    const POD: &str = "crucible-run-brave-otter";

    async fn issue_at(db: &Db, status: Status) {
        sqlx::query(
            "INSERT INTO issues (key, repo, status, input_kind, updated_at) \
             VALUES ($1, 'owner/repo', $2, 'playbook', '2026-01-01T00:00:00Z')",
        )
        .bind(KEY)
        .bind(status.as_str())
        .execute(db.pool())
        .await
        .expect("seed issue");
    }

    fn claim() -> RunClaim<'static> {
        RunClaim {
            key: KEY,
            from: Status::New,
            run_id: "run-1",
            origin: RunOrigin::Playbook {
                image: RunImage::default(),
            },
        }
    }

    async fn claim_pod(db: &Db) -> anyhow::Result<bool> {
        let name = crate::runs::names::reserve(db.pool()).await?;
        claim()
            .claim(
                db,
                &RunPlacement {
                    name: &name,
                    dispatch: RunDispatch::Pod,
                    location: &RunLocation::hub(),
                    work_pod: Some(&NewWorkPod {
                        pod_name: POD.to_string(),
                        kind: "run".to_string(),
                        issue_key: Some(KEY.to_string()),
                        state: WorkPodState::Running,
                        cost_tag: "run".to_string(),
                        cluster: "hub".to_string(),
                    }),
                    stamp_dispatcher: true,
                },
            )
            .await
    }

    async fn status(db: &Db) -> Status {
        crate::issues::store::get_issue(db.pool(), KEY)
            .await
            .expect("read")
            .expect("issue")
            .status
    }

    async fn runs(db: &Db) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM runs")
            .fetch_one(db.pool())
            .await
            .expect("count")
    }

    /// The claim is the issue at `running` with its run, pod row, and transition, all at once.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_claim_records_the_run_and_its_pod_row_with_the_transition(pool: PgPool) {
        let db = Db::new(pool);
        issue_at(&db, Status::New).await;
        assert!(claim_pod(&db).await.expect("claim"));

        assert_eq!(status(&db).await, Status::Running);
        let run = crate::runs::store::running_run_for_issue(db.pool(), KEY)
            .await
            .expect("read")
            .expect("the claimed run is the issue's live run");
        assert_eq!(run.run_id, "run-1");
        assert_eq!(run.pod.as_deref(), Some(POD));
        let row = crate::runs::work_pods::get_work_pod(db.pool(), POD)
            .await
            .expect("read")
            .expect("pod row");
        assert_eq!(row.state, WorkPodState::Running);
        let events = db.events().read_for_key(KEY).await.expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(
            (events[0].from.as_str(), events[0].to.as_str()),
            ("new", "running")
        );
        assert_eq!(events[0].evidence.as_deref(), Some("run-1"));
    }

    /// An issue that already left `new` is not claimed, and the lost claim writes nothing.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_lost_claim_writes_nothing(pool: PgPool) {
        let db = Db::new(pool);
        issue_at(&db, Status::Parked).await;
        assert!(!claim_pod(&db).await.expect("claim"));

        assert_eq!(status(&db).await, Status::Parked);
        assert_eq!(runs(&db).await, 0);
        assert!(
            crate::runs::work_pods::get_work_pod(db.pool(), POD)
                .await
                .expect("read")
                .is_none()
        );
        assert!(
            db.events()
                .read_for_key(KEY)
                .await
                .expect("events")
                .is_empty()
        );
    }

    /// A release puts the issue back at `new` with the failure on its log, drops the run, fails
    /// the pod row with the reason, and hands back the dispatch request the claim consumed.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_release_undoes_the_claim_and_hands_back_the_dispatch_request(pool: PgPool) {
        let db = Db::new(pool);
        issue_at(&db, Status::New).await;
        assert!(
            crate::issues::store::request_dispatch(db.pool(), KEY, "alice")
                .await
                .expect("request")
        );
        assert!(claim_pod(&db).await.expect("claim"));
        let dispatched_by: Option<String> =
            sqlx::query_scalar("SELECT dispatched_by FROM runs WHERE run_id = 'run-1'")
                .fetch_one(db.pool())
                .await
                .expect("read");
        assert_eq!(dispatched_by.as_deref(), Some("user:alice"));

        let e = claim()
            .release(
                &db,
                Some(POD),
                anyhow::Error::msg("creating the loop-run pod: refused"),
            )
            .await;
        assert!(e.is::<Released>(), "{e:#}");

        assert_eq!(status(&db).await, Status::New);
        assert_eq!(runs(&db).await, 0);
        let requested: Option<String> =
            sqlx::query_scalar("SELECT dispatch_requested_by FROM issues WHERE key = $1")
                .bind(KEY)
                .fetch_one(db.pool())
                .await
                .expect("read");
        assert_eq!(requested.as_deref(), Some("user:alice"));
        let row = crate::runs::work_pods::get_work_pod(db.pool(), POD)
            .await
            .expect("read")
            .expect("pod row");
        assert_eq!(row.state, WorkPodState::Failed);
        assert!(
            row.error.as_deref().is_some_and(|e| e.contains("refused")),
            "{:?}",
            row.error
        );
        let events = db.events().read_for_key(KEY).await.expect("events");
        let last = events.last().expect("the release event");
        assert_eq!((last.from.as_str(), last.to.as_str()), ("running", "new"));
        assert_eq!(
            last.reason.as_deref(),
            Some(crate::runs::claim::PLAYBOOK_DISPATCH_FAILED)
        );
        assert!(
            last.evidence
                .as_deref()
                .is_some_and(|e| e.contains("refused")),
            "{:?}",
            last.evidence
        );
        let recorded =
            crate::runs::claim::record_dispatch_failure(&db, KEY, Status::New, "x", e).await;
        assert!(recorded.is::<Released>());
        assert_eq!(
            db.events().read_for_key(KEY).await.expect("events").len(),
            events.len(),
            "a released failure is not recorded twice"
        );
    }

    /// A human park that lands mid-dispatch stands: the release drops the run and leaves the
    /// issue parked, and the unpark returns it to `new` to be dispatched again.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_release_after_a_human_park_keeps_the_park(pool: PgPool) {
        let db = Db::new(pool);
        issue_at(&db, Status::New).await;
        assert!(claim_pod(&db).await.expect("claim"));
        crate::issues::store::park_issue(
            db.pool(),
            KEY,
            "held by hand",
            ParkedBy::Human,
            &crate::clock::now_rfc3339(),
        )
        .await
        .expect("park");

        let _ = claim()
            .release(&db, Some(POD), anyhow::Error::msg("refused"))
            .await;

        assert_eq!(status(&db).await, Status::Parked);
        assert_eq!(runs(&db).await, 0);
        assert!(
            crate::issues::transitions::unpark(db.pool(), db.events(), KEY, None, None)
                .await
                .expect("unpark")
        );
        assert_eq!(status(&db).await, Status::New);
    }
}
