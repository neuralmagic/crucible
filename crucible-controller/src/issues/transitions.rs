//! The write+event lockstep: a state transition (or park/unpark) and its history row commit in
//! one transaction — no call site can reach the underlying CAS and skip the event except by
//! calling that CAS directly (`claim_issue` stays public for the ~40 sites that carry a custom
//! reason/evidence string these fixed-shape helpers can't reproduce).

use crate::event_log::{self, Event, EventLog};
use crate::model::{ParkReason, ParkedBy, Status};
use crate::runs::workpod::WorkPodState;
use anyhow::Result;
use sqlx::PgPool;

/// Atomically claim `from` → `to` and, only if the claim won, insert the matching event row in
/// the same transaction. A lost claim logs nothing. Returns whether it won.
#[tracing::instrument(name = "db.transition", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub async fn transition(
    pool: &PgPool,
    events: &EventLog,
    key: &str,
    from: Status,
    to: Status,
    reason: Option<&str>,
    evidence: Option<&str>,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let won = crate::issues::store::claim_issue(&mut *tx, key, from, to).await?;
    if !won {
        return Ok(false);
    }
    let ev = Event::now(key, from.as_str(), to.as_str(), reason, evidence);
    event_log::insert(&mut *tx, &ev).await?;
    tx.commit().await?;
    events.publish(&ev);
    Ok(true)
}

/// Claim `from` → `parked` inside `tx` and, only if the claim won, insert the event row and
/// purge the issue's queued work pods. Returns the event (for the caller to publish after commit)
/// and the purge count, or `None` when the CAS lost.
async fn claim_park_event<'a>(
    tx: &mut sqlx::PgConnection,
    key: &'a str,
    from: Status,
    rendered: &'a str,
    parked_by: ParkedBy,
    now: &str,
) -> Result<Option<(Event<'a>, u64)>> {
    let won =
        crate::issues::store::claim_park(&mut *tx, key, from, rendered, parked_by, now).await?;
    if !won {
        return Ok(None);
    }
    let ev = Event::now(
        key,
        from.as_str(),
        Status::Parked.as_str(),
        Some(rendered),
        None,
    );
    event_log::insert(&mut *tx, &ev).await?;
    // Only the CAS winner purges: the queued rows represent future spend this park rejects.
    let purged = crate::runs::work_pods::purge_queued_work_pods(&mut *tx, key).await?;
    Ok(Some((ev, purged)))
}

/// Claim `from` → `parked` with a reason + authority in one UPDATE and, only if the claim won,
/// insert the event row and purge the issue's queued work pods (future spend a park rejects) —
/// all in one transaction. `reason` is a [`ParkReason`] (not a bare string) so a call site can
/// never park with a reason downstream logic can't recognize — [`ParkReason::Display`] is the one
/// place this renders.
#[tracing::instrument(name = "db.park", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub async fn park(
    pool: &PgPool,
    events: &EventLog,
    key: &str,
    from: Status,
    reason: &ParkReason,
    parked_by: ParkedBy,
) -> Result<bool> {
    let rendered = reason.to_string();
    let now = crate::clock::now_rfc3339();
    let mut tx = pool.begin().await?;
    let Some((ev, purged)) =
        claim_park_event(&mut tx, key, from, &rendered, parked_by, &now).await?
    else {
        return Ok(false);
    };
    tx.commit().await?;
    events.publish(&ev);
    if purged > 0 {
        tracing::debug!(issue_key = %key, purged, "park deleted queued work pods");
    }
    Ok(true)
}

/// Atomically park a running issue and terminalize the run pod that supplied the failure
/// evidence. The issue, event, queued-work purge, run status, and optional work-pod status are one
/// state transition; observers cannot see a parked issue whose run still appears live.
#[tracing::instrument(name = "db.park_failed_run", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key, run_id = %run_id), err)]
pub(crate) async fn park_failed_run(
    pool: &PgPool,
    events: &EventLog,
    key: &str,
    reason: &ParkReason,
    run_id: &str,
    run_status: &str,
    pod: Option<&str>,
) -> Result<bool> {
    let rendered = reason.to_string();
    let now = crate::clock::now_rfc3339();
    let mut tx = pool.begin().await?;
    let Some((ev, purged)) = claim_park_event(
        &mut tx,
        key,
        Status::Running,
        &rendered,
        ParkedBy::Machine,
        &now,
    )
    .await?
    else {
        return Ok(false);
    };
    crate::runs::store::set_run_status(&mut *tx, run_id, run_status).await?;
    if let Some(pod) = pod {
        crate::runs::work_pods::set_work_pod_state(
            &mut *tx,
            pod,
            WorkPodState::Failed,
            None,
            Some(&rendered),
        )
        .await?;
    }
    tx.commit().await?;
    events.publish(&ev);
    if purged > 0 {
        tracing::debug!(issue_key = %key, purged, "park deleted queued work pods");
    }
    Ok(true)
}

/// Unpark (`parked` → the stored pre-park status) and, only if it took, insert the real
/// transition's event row in the same transaction, attributed to `actor` when known.
#[tracing::instrument(name = "db.unpark", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub async fn unpark(
    pool: &PgPool,
    events: &EventLog,
    key: &str,
    reason: Option<&str>,
    actor: Option<&str>,
) -> Result<bool> {
    let now = crate::clock::now_rfc3339();
    let mut tx = pool.begin().await?;
    let restored = crate::issues::store::unpark_issue(&mut *tx, key, &now).await?;
    let Some(status) = &restored else {
        return Ok(false);
    };
    let ev = Event::now(
        key,
        Status::Parked.as_str(),
        status,
        Some(reason.unwrap_or("unparked by human override")),
        None,
    )
    .by(actor);
    event_log::insert(&mut *tx, &ev).await?;
    tx.commit().await?;
    events.publish(&ev);
    Ok(true)
}

/// Park an issue unconditionally (no CAS, no event) and purge its queued work pods. Stamps
/// `updated_at`. The unconditional park verb used by crash-recovery and admin paths that already
/// know the issue isn't `parked` — distinct from [`park`], which CASes off a known `from` and
/// logs the transition.
#[tracing::instrument(name = "db.park_and_purge", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub async fn park_and_purge(
    ex: impl sqlx::PgExecutor<'_> + Copy,
    key: &str,
    reason: &str,
    parked_by: ParkedBy,
) -> Result<()> {
    let now = crate::clock::now_rfc3339();
    crate::issues::store::park_issue(ex, key, reason, parked_by, &now).await?;
    let purged = crate::runs::work_pods::purge_queued_work_pods(ex, key).await?;
    if purged > 0 {
        tracing::debug!(issue_key = %key, purged, "park deleted queued work pods");
    }
    Ok(())
}
