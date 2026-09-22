//! The concrete [`OverrideSink`] the daemon mounts under the API/UI, plus the worker-side apply
//! step. The sink itself is kept abstract as a trait; this module supplies the concrete impl.
//!
//! An override does **not** race the reconcile worker: the API handler only builds an [`Override`]
//! and hands it here; [`QueueOverrideSink::submit`] stashes the intent by key and enqueues the key
//! through the same [`WorkQueue`] every source uses. When the single worker picks that key up, the
//! daemon's reconcile step first drains the pending override ([`apply_pending`]) and *then* runs the
//! normal [`crate::issues::reconcile::reconcile`] — so a human park/unpark/bump composes with claims
//! instead of racing them, and reconcile stays the only writer of `parked_by = human` stickiness.
//!
//! Stickiness itself is structural: a human park sets `parked_by = human`, and nothing in the
//! engine auto-unparks a human park (only the machine-park auto-unpark edge does, and it checks the
//! authority). A human *unpark* is always honoured — it's the operator explicitly taking it back.

use crate::client::Db;
use crate::daemon::queue::{Override, OverrideKind, OverrideSink, WorkQueue};
use crate::event_log::Event;
use crate::model::{ParkedBy, Status};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The pending-override store: at most one intent per key (a later submit for the same key wins —
/// the human's latest instruction is the one that matters). Shared (`Arc`) between the sink that
/// fills it and the worker step that drains it.
#[derive(Default)]
pub struct OverrideStore {
    pending: Mutex<HashMap<String, Override>>,
}

impl OverrideStore {
    pub fn new() -> Self {
        OverrideStore::default()
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, Override>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn stash(&self, ov: Override) {
        self.pending().insert(ov.key.0.clone(), ov);
    }

    fn take(&self, key: &str) -> Option<Override> {
        self.pending().remove(key)
    }
}

/// The [`OverrideSink`] the API/UI posts to: stash the intent, enqueue the key. It never touches the
/// database (the frozen contract: POSTs don't write the DB — reconcile does).
pub struct QueueOverrideSink {
    store: Arc<OverrideStore>,
    queue: WorkQueue,
}

impl QueueOverrideSink {
    pub fn new(store: Arc<OverrideStore>, queue: WorkQueue) -> Self {
        QueueOverrideSink { store, queue }
    }
}

impl OverrideSink for QueueOverrideSink {
    fn submit(&self, ov: Override) {
        let key = ov.key.clone();
        self.store.stash(ov);
        self.queue.enqueue_urgent(key);
    }
}

/// Drain and apply any pending override for `key` before reconcile runs (called by the daemon's
/// reconcile step). A no-op when nothing is pending, so it's cheap on every dequeue.
pub async fn apply_pending(db: &Db, store: &OverrideStore, key: &str) -> Result<()> {
    let Some(ov) = store.take(key) else {
        return Ok(());
    };
    let actor = ov.actor.as_deref();
    match ov.kind {
        OverrideKind::Park => apply_park(db, key, ov.reason.as_deref(), actor).await,
        OverrideKind::Unpark => {
            crate::issues::transitions::unpark(
                db.pool(),
                db.events(),
                key,
                ov.reason.as_deref(),
                actor,
            )
            .await?;
            Ok(())
        }
        OverrideKind::Bump => apply_bump(db, key, ov.priority, ov.reason.as_deref(), actor).await,
        OverrideKind::ScopeNow {
            ref justification,
            max_cost,
        } => apply_scope_now(db, key, justification, max_cost, actor).await,
        OverrideKind::Redispatch { ref justification } => {
            apply_redispatch(db, key, justification, actor).await
        }
    }
}

/// A human park: force `status → parked` with `parked_by = human` from
/// whatever the row is now, logging the transition. Untracked key → nothing to park.
async fn apply_park(db: &Db, key: &str, reason: Option<&str>, actor: Option<&str>) -> Result<()> {
    let Some(issue) = crate::issues::store::get_issue(db.pool(), key).await? else {
        return Ok(());
    };
    if issue.status == Status::Parked && issue.parked_by == Some(ParkedBy::Human) {
        return Ok(()); // already a human park — nothing to change, no duplicate event
    }
    let reason = reason.unwrap_or("parked by human override");
    let now = crate::clock::now_rfc3339();
    let mut tx = db.pool().begin().await?;
    crate::issues::store::park_issue(&mut *tx, key, reason, ParkedBy::Human, &now).await?;
    let purged = crate::runs::work_pods::purge_queued_work_pods(&mut *tx, key).await?;
    let ev = Event::now(
        key,
        issue.status.as_str(),
        Status::Parked.as_str(),
        Some(reason),
        None,
    )
    .by(actor);
    crate::event_log::insert(&mut *tx, &ev).await?;
    tx.commit().await?;
    db.events().publish(&ev);
    if purged > 0 {
        tracing::debug!(issue_key = %key, purged, "park deleted queued work pods");
    }
    Ok(())
}

/// A human bump: reweight the priority without touching the lifecycle, and log a same-status
/// event so the reweight shows up in the audit trail (it used to be invisible outside the row).
async fn apply_bump(
    db: &Db,
    key: &str,
    priority: Option<i64>,
    reason: Option<&str>,
    actor: Option<&str>,
) -> Result<()> {
    let Some(priority) = priority else {
        return Ok(());
    };
    let Some(issue) = crate::issues::store::get_issue(db.pool(), key).await? else {
        return Ok(());
    };
    if !crate::issues::store::set_priority(db.pool(), key, priority).await? {
        return Ok(());
    }
    let note = format!(
        "priority {} -> {}{}",
        issue.priority,
        priority,
        reason.map(|r| format!(": {r}")).unwrap_or_default()
    );
    let status = issue.status.as_str();
    db.events()
        .append(&Event::now(key, status, status, Some(&note), None).by(actor))
        .await?;
    Ok(())
}

/// A human-triggered ScopeNow: unpark the issue if parked, set status to `new` (if not already),
/// and stash the justification + max_cost on the issue so `reconcile_new` can detect and honor
/// the ScopeNow bypass. The reconcile step reads the stashed fields and skips ALL gates.
async fn apply_scope_now(
    db: &Db,
    key: &str,
    justification: &str,
    max_cost: Option<f64>,
    actor: Option<&str>,
) -> Result<()> {
    let Some(issue) = crate::issues::store::get_issue(db.pool(), key).await? else {
        return Ok(());
    };
    // Unpark if parked (human or machine). Unpark restores the pre-park status, but ScopeNow's
    // contract is a fresh scope from the top — force `new` when the restore landed elsewhere.
    if issue.status == Status::Parked {
        crate::issues::transitions::unpark(db.pool(), db.events(), key, Some(justification), actor)
            .await?;
        if let Some(unparked) = crate::issues::store::get_issue(db.pool(), key).await?
            && unparked.status != Status::New
        {
            let mut tx = db.pool().begin().await?;
            if crate::issues::store::claim_issue(&mut *tx, key, unparked.status, Status::New)
                .await?
            {
                let reason = format!("ScopeNow: {justification}");
                let ev = Event::now(
                    key,
                    unparked.status.as_str(),
                    Status::New.as_str(),
                    Some(&reason),
                    None,
                )
                .by(actor);
                crate::event_log::insert(&mut *tx, &ev).await?;
                tx.commit().await?;
                db.events().publish(&ev);
            }
        }
    }
    // Stash the ScopeNow intent on the issue row so reconcile_new can read it.
    crate::runs::work_pods::set_scope_now(db.pool(), key, justification, max_cost).await?;
    let status = if issue.status == Status::Parked {
        "new"
    } else {
        issue.status.as_str()
    };
    db.events()
        .append(
            &Event::now(
                key,
                status,
                status,
                Some(&format!("ScopeNow: {justification}")),
                None,
            )
            .by(actor),
        )
        .await?;
    Ok(())
}

/// A human-triggered redispatch: re-run an already-approved pack. Legal only from a finished run
/// (`done` kept nothing, `pr-open` landed a draft PR); a live status is declined so we never race an
/// in-flight run. The approval gate is re-verified against `scopes.approved_at` (never trusted from
/// the request) — a never-scoped or unapproved issue is declined. On success the issue is routed back
/// to `awaiting-approval` and the justification stashed, where [`crate::issues::reconcile::reconcile_awaiting`]
/// re-verifies approval, mints a FRESH run id, and re-dispatches the SAME stored pack through the
/// normal `dispatch_run` path (caps, ledger, park-on-error, run knobs all apply). The stash exempts
/// that one dispatch from the autopilot pause and is cleared once the run launches.
async fn apply_redispatch(
    db: &Db,
    key: &str,
    justification: &str,
    actor: Option<&str>,
) -> Result<()> {
    let Some(issue) = crate::issues::store::get_issue(db.pool(), key).await? else {
        return Ok(());
    };
    let decline = async |reason: &str| -> Result<()> {
        db.events()
            .append(
                &Event::now(
                    key,
                    issue.status.as_str(),
                    issue.status.as_str(),
                    Some(reason),
                    None,
                )
                .by(actor),
            )
            .await
    };
    if !matches!(issue.status, Status::Done | Status::PrOpen) {
        return decline(&format!(
            "redispatch declined: not a finished run (status {})",
            issue.status.as_str()
        ))
        .await;
    }
    // Re-verify the approval gate rather than trusting the request: a never-scoped or unapproved
    // pack can never be re-run.
    let approved = match crate::issues::store::latest_scope_for_issue(db.pool(), key).await? {
        Some(scope) => scope.is_approved(),
        None => false,
    };
    if !approved {
        return decline("redispatch declined: no approved pack to re-run").await;
    }
    let from = issue.status;
    let mut tx = db.pool().begin().await?;
    if crate::issues::store::claim_issue(&mut *tx, key, from, Status::AwaitingApproval).await? {
        crate::runs::work_pods::set_redispatch(&mut *tx, key, justification).await?;
        let reason = format!("redispatch: {justification}");
        let ev = Event::now(
            key,
            from.as_str(),
            Status::AwaitingApproval.as_str(),
            Some(&reason),
            None,
        )
        .by(actor);
        crate::event_log::insert(&mut *tx, &ev).await?;
        tx.commit().await?;
        db.events().publish(&ev);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::queue::IssueKey;
    use crate::issues::model::NewIssue;
    use crate::model::ParkReason;
    use sqlx::PgPool;

    fn db_with(pool: PgPool) -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        (Db::new(pool), dir)
    }

    fn issue(key: &str) -> NewIssue {
        NewIssue {
            key: key.to_string(),
            repo: "owner/repo".to_string(),
            priority: 0,
            evidence_url: None,
            title: None,
            author: None,
            body: None,
            labels: Vec::new(),
            upstream_updated_at: None,
        }
    }

    #[test]
    fn submit_stashes_and_enqueues() {
        let store = Arc::new(OverrideStore::new());
        let queue = WorkQueue::new();
        let sink = QueueOverrideSink::new(store.clone(), queue.clone());
        sink.submit(Override {
            key: IssueKey("owner/repo#1".into()),
            kind: OverrideKind::Bump,
            reason: None,
            priority: Some(9),
            actor: None,
        });
        // The key is dirty in the queue, and the intent is stashed for the worker to drain.
        assert!(store.take("owner/repo#1").is_some());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn park_override_sets_human_authority_and_logs(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        crate::issues::store::upsert_issue(db.pool(), &issue("owner/repo#1")).await?;
        let store = OverrideStore::new();
        store.stash(Override {
            key: IssueKey("owner/repo#1".into()),
            kind: OverrideKind::Park,
            reason: Some("don't touch this".into()),
            priority: None,
            actor: None,
        });

        apply_pending(&db, &store, "owner/repo#1").await?;

        let got = crate::issues::store::get_issue(db.pool(), "owner/repo#1")
            .await?
            .expect("issue");
        assert_eq!(got.status, Status::Parked);
        assert_eq!(got.parked_by, Some(ParkedBy::Human));
        assert_eq!(got.parked_reason.as_deref(), Some("don't touch this"));
        let events = crate::event_log::export_string(db.pool()).await?;
        assert_eq!(events.lines().count(), 1, "one park event logged");
        // Draining is one-shot: a second apply finds nothing pending.
        apply_pending(&db, &store, "owner/repo#1").await?;
        assert_eq!(
            crate::event_log::export_string(db.pool())
                .await?
                .lines()
                .count(),
            1
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn park_override_attributes_the_event_to_the_actor(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        crate::issues::store::upsert_issue(db.pool(), &issue("owner/repo#1")).await?;
        let store = OverrideStore::new();
        store.stash(Override {
            key: IssueKey("owner/repo#1".into()),
            kind: OverrideKind::Park,
            reason: Some("not worth a run".into()),
            priority: None,
            actor: Some("wren".into()),
        });

        apply_pending(&db, &store, "owner/repo#1").await?;

        let events = db.events().read_for_key("owner/repo#1").await?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].actor.as_deref(), Some("wren"));
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn bump_override_logs_an_attributed_audit_event(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        crate::issues::store::upsert_issue(db.pool(), &issue("owner/repo#1")).await?;
        let store = OverrideStore::new();
        store.stash(Override {
            key: IssueKey("owner/repo#1".into()),
            kind: OverrideKind::Bump,
            reason: Some("customer escalation".into()),
            priority: Some(42),
            actor: Some("wren".into()),
        });

        apply_pending(&db, &store, "owner/repo#1").await?;

        let got = crate::issues::store::get_issue(db.pool(), "owner/repo#1")
            .await?
            .expect("issue");
        assert_eq!(got.priority, 42);
        let events = db.events().read_for_key("owner/repo#1").await?;
        assert_eq!(events.len(), 1, "the reweight lands in the audit trail");
        assert_eq!(events[0].from, "new");
        assert_eq!(events[0].to, "new", "a bump is not a lifecycle transition");
        assert_eq!(
            events[0].reason.as_deref(),
            Some("priority 0 -> 42: customer escalation")
        );
        assert_eq!(events[0].actor.as_deref(), Some("wren"));
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn unpark_override_returns_a_human_park_to_new(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        crate::issues::store::upsert_issue(db.pool(), &issue("owner/repo#1")).await?;
        crate::issues::transitions::park_and_purge(
            db.pool(),
            "owner/repo#1",
            "sticky",
            ParkedBy::Human,
        )
        .await?;

        let store = OverrideStore::new();
        store.stash(Override {
            key: IssueKey("owner/repo#1".into()),
            kind: OverrideKind::Unpark,
            reason: Some("operator revived it".into()),
            priority: None,
            actor: None,
        });
        apply_pending(&db, &store, "owner/repo#1").await?;

        let got = crate::issues::store::get_issue(db.pool(), "owner/repo#1")
            .await?
            .expect("issue");
        assert_eq!(got.status, Status::New);
        assert!(got.parked_by.is_none());
        assert!(got.parked_reason.is_none());
        Ok(())
    }

    /// Park/unpark round-trips the issue's pipeline position: a dispatch-failure park from
    /// `awaiting-approval` must not demote the issue to `new` on unpark (live failure: an approved
    /// pack was orphaned because unpark hard-coded `new`). The event line reports the real
    /// transition.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn unpark_restores_the_pre_park_status(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        crate::issues::store::upsert_issue(db.pool(), &issue("owner/repo#1")).await?;
        crate::issues::store::claim_issue(
            db.pool(),
            "owner/repo#1",
            Status::New,
            Status::AwaitingApproval,
        )
        .await?;
        crate::issues::transitions::park_and_purge(
            db.pool(),
            "owner/repo#1",
            "dispatching the loop run",
            ParkedBy::Machine,
        )
        .await?;
        // A re-park while already parked must not overwrite the original stamp.
        crate::issues::transitions::park_and_purge(
            db.pool(),
            "owner/repo#1",
            "parked again",
            ParkedBy::Machine,
        )
        .await?;

        assert!(
            crate::issues::transitions::unpark(
                db.pool(),
                db.events(),
                "owner/repo#1",
                Some("retry dispatch"),
                None
            )
            .await?
        );
        let got = crate::issues::store::get_issue(db.pool(), "owner/repo#1")
            .await?
            .expect("issue");
        assert_eq!(
            got.status,
            Status::AwaitingApproval,
            "unpark restores the pre-park status"
        );
        let events = db.events().read_for_key("owner/repo#1").await?;
        let last = events.last().expect("unpark event");
        assert_eq!(last.from, "parked");
        assert_eq!(
            last.to, "awaiting-approval",
            "the event reports the real transition"
        );
        Ok(())
    }

    /// The CAS park verb stamps the pre-park status too, and a legacy row (parked before the
    /// column existed, stamp NULL) still unparks to `new`.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn claim_park_stamps_and_legacy_null_falls_back_to_new(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        crate::issues::store::upsert_issue(db.pool(), &issue("owner/repo#1")).await?;
        crate::issues::store::claim_issue(db.pool(), "owner/repo#1", Status::New, Status::Scoped)
            .await?;
        assert!(
            crate::issues::transitions::park(
                db.pool(),
                db.events(),
                "owner/repo#1",
                Status::Scoped,
                &ParkReason::Legacy("proposal died".to_string()),
                ParkedBy::Machine
            )
            .await?
        );
        assert!(
            crate::issues::transitions::unpark(db.pool(), db.events(), "owner/repo#1", None, None)
                .await?
        );
        assert_eq!(
            crate::issues::store::get_issue(db.pool(), "owner/repo#1")
                .await?
                .expect("issue")
                .status,
            Status::Scoped,
            "the CAS park verb round-trips too"
        );

        // Legacy shape: force the stamp NULL under an active park, as pre-migration rows have.
        crate::issues::transitions::park_and_purge(
            db.pool(),
            "owner/repo#1",
            "old park",
            ParkedBy::Machine,
        )
        .await?;
        sqlx::query("UPDATE issues SET pre_park_status = NULL WHERE key = 'owner/repo#1'")
            .execute(db.pool())
            .await?;
        assert!(
            crate::issues::transitions::unpark(db.pool(), db.events(), "owner/repo#1", None, None)
                .await?
        );
        assert_eq!(
            crate::issues::store::get_issue(db.pool(), "owner/repo#1")
                .await?
                .expect("issue")
                .status,
            Status::New,
            "a legacy NULL stamp falls back to new"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn bump_override_reweights_priority_without_touching_status(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        crate::issues::store::upsert_issue(db.pool(), &issue("owner/repo#1")).await?;
        assert!(
            crate::issues::store::claim_issue(
                db.pool(),
                "owner/repo#1",
                Status::New,
                Status::Scoped
            )
            .await?
        );
        let store = OverrideStore::new();
        store.stash(Override {
            key: IssueKey("owner/repo#1".into()),
            kind: OverrideKind::Bump,
            reason: None,
            priority: Some(42),
            actor: None,
        });
        apply_pending(&db, &store, "owner/repo#1").await?;

        let got = crate::issues::store::get_issue(db.pool(), "owner/repo#1")
            .await?
            .expect("issue");
        assert_eq!(got.priority, 42);
        assert_eq!(
            got.status,
            Status::Scoped,
            "bump leaves the lifecycle alone"
        );
        Ok(())
    }

    /// A ScopeNow on a parked issue whose pre-park status wasn't `new` forces it back to `new`,
    /// and the forced transition lands in the event history (rebuild replays events; a silent
    /// status flip would reconstruct the wrong status).
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn scope_now_on_a_parked_issue_logs_the_forced_return_to_new(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        crate::issues::store::upsert_issue(db.pool(), &issue("owner/repo#1")).await?;
        crate::issues::store::claim_issue(
            db.pool(),
            "owner/repo#1",
            Status::New,
            Status::AwaitingApproval,
        )
        .await?;
        crate::issues::transitions::park_and_purge(
            db.pool(),
            "owner/repo#1",
            "wedged",
            ParkedBy::Machine,
        )
        .await?;

        let store = OverrideStore::new();
        store.stash(Override {
            key: IssueKey("owner/repo#1".into()),
            kind: OverrideKind::ScopeNow {
                justification: "customer escalation".into(),
                max_cost: None,
            },
            reason: None,
            priority: None,
            actor: Some("wren".into()),
        });
        apply_pending(&db, &store, "owner/repo#1").await?;

        let got = crate::issues::store::get_issue(db.pool(), "owner/repo#1")
            .await?
            .expect("issue");
        assert_eq!(got.status, Status::New, "forced back to new");
        assert_eq!(
            got.scope_now_justification.as_deref(),
            Some("customer escalation")
        );
        let events = db.events().read_for_key("owner/repo#1").await?;
        let forced = events
            .iter()
            .find(|e| e.from == "awaiting-approval" && e.to == "new")
            .expect("the forced transition is in the history");
        assert_eq!(
            forced.reason.as_deref(),
            Some("ScopeNow: customer escalation")
        );
        assert_eq!(forced.actor.as_deref(), Some("wren"));
        Ok(())
    }

    /// Seed an issue at a finished status with a recorded, approved scope — the post-run state a
    /// redispatch acts on. Returns nothing; the issue key + approval gate are the fixture.
    async fn seed_finished_with_approval(db: &Db, key: &str, from: Status) -> Result<()> {
        crate::issues::store::upsert_issue(db.pool(), &issue(key)).await?;
        assert!(crate::issues::store::claim_issue(db.pool(), key, Status::New, from).await?);
        let scope_id = crate::issues::store::insert_scope(
            db.pool(),
            &crate::issues::model::NewScope {
                issue: key.into(),
                pack_digest: Some("v1:abc".into()),
                check_outcome: Some("PASS".into()),
            },
        )
        .await?;
        sqlx::query(
            "UPDATE scopes SET approved_by='alice', approved_at='2026-07-02T00:00:00Z' WHERE id=$1",
        )
        .bind(scope_id)
        .execute(db.pool())
        .await?;
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn redispatch_reopens_the_approval_approval_from_done(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        seed_finished_with_approval(&db, "owner/repo#1", Status::Done).await?;
        let store = OverrideStore::new();
        store.stash(Override {
            key: IssueKey("owner/repo#1".into()),
            kind: OverrideKind::Redispatch {
                justification: "re-run: prior run was degenerate".into(),
            },
            reason: Some("re-run: prior run was degenerate".into()),
            priority: None,
            actor: Some("wren".into()),
        });

        apply_pending(&db, &store, "owner/repo#1").await?;

        let got = crate::issues::store::get_issue(db.pool(), "owner/repo#1")
            .await?
            .expect("issue");
        assert_eq!(
            got.status,
            Status::AwaitingApproval,
            "a redispatch routes the finished run back to the approval gate"
        );
        assert_eq!(
            got.redispatch_justification.as_deref(),
            Some("re-run: prior run was degenerate"),
            "the one-shot stash is set for the awaiting-approval reconcile"
        );
        let events = db.events().read_for_key("owner/repo#1").await?;
        let last = events.last().expect("redispatch event");
        assert_eq!(last.from, "done");
        assert_eq!(last.to, "awaiting-approval");
        assert_eq!(last.actor.as_deref(), Some("wren"));
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn redispatch_reopens_the_approval_from_pr_open(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        seed_finished_with_approval(&db, "owner/repo#2", Status::PrOpen).await?;
        let store = OverrideStore::new();
        store.stash(Override {
            key: IssueKey("owner/repo#2".into()),
            kind: OverrideKind::Redispatch {
                justification: "another pass".into(),
            },
            reason: None,
            priority: None,
            actor: None,
        });

        apply_pending(&db, &store, "owner/repo#2").await?;

        let got = crate::issues::store::get_issue(db.pool(), "owner/repo#2")
            .await?
            .expect("issue");
        assert_eq!(got.status, Status::AwaitingApproval);
        assert!(got.redispatch_justification.is_some());
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn redispatch_declined_when_never_scoped(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        crate::issues::store::upsert_issue(db.pool(), &issue("owner/repo#3")).await?;
        assert!(
            crate::issues::store::claim_issue(db.pool(), "owner/repo#3", Status::New, Status::Done)
                .await?
        );
        let store = OverrideStore::new();
        store.stash(Override {
            key: IssueKey("owner/repo#3".into()),
            kind: OverrideKind::Redispatch {
                justification: "no pack exists".into(),
            },
            reason: None,
            priority: None,
            actor: Some("wren".into()),
        });

        apply_pending(&db, &store, "owner/repo#3").await?;

        let got = crate::issues::store::get_issue(db.pool(), "owner/repo#3")
            .await?
            .expect("issue");
        assert_eq!(
            got.status,
            Status::Done,
            "a never-scoped issue can't be re-run — it stays put"
        );
        assert!(got.redispatch_justification.is_none());
        let events = db.events().read_for_key("owner/repo#3").await?;
        let last = events.last().expect("decline event");
        assert_eq!(last.from, "done");
        assert_eq!(last.to, "done", "a decline is not a transition");
        assert!(
            last.reason
                .as_deref()
                .unwrap_or_default()
                .contains("declined")
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn redispatch_declined_from_a_live_status(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        // Approved but still running: redispatch must not race the in-flight run.
        seed_finished_with_approval(&db, "owner/repo#4", Status::Running).await?;
        let store = OverrideStore::new();
        store.stash(Override {
            key: IssueKey("owner/repo#4".into()),
            kind: OverrideKind::Redispatch {
                justification: "too eager".into(),
            },
            reason: None,
            priority: None,
            actor: None,
        });

        apply_pending(&db, &store, "owner/repo#4").await?;

        let got = crate::issues::store::get_issue(db.pool(), "owner/repo#4")
            .await?
            .expect("issue");
        assert_eq!(
            got.status,
            Status::Running,
            "a live run is never re-dispatched from under itself"
        );
        assert!(got.redispatch_justification.is_none());
        Ok(())
    }
}
