//! The collect-tail drain: a collection that frees a slot re-drives the eldest queued issue back
//! through reconcile, which owns the spend gates and promotes the row.

#![cfg(feature = "autoresearch")]

use crate::client::Db;
use crate::daemon::queue::WorkQueue;
use crate::runs::workpod::*;

/// How many queued rows a single collect-tail drain scans while hunting for the eldest DRAINABLE
/// (non-parked) one. A collection frees exactly one slot, so it re-drives one issue; a run of
/// stale parked rows at the FIFO head is purged as the scan skips past them. A backlog of parked
/// rows deeper than this drains over successive collections and the timeout-sweep backstop, so the
/// bound just keeps one collection's scan cheap.
pub(crate) const QUEUE_DRAIN_SCAN: i64 = 256;

/// Collect up to `limit` DRAINABLE queued issue keys for `kind`, eldest-first: scan the FIFO head
/// (bounded by [`QUEUE_DRAIN_SCAN`]), skip + PURGE any row whose issue has since parked — that's
/// spend the park rejected, and purging backfills stale rows whose issue parked before the
/// park-purge existed — and return the live ones. Best-effort: a DB hiccup is logged and yields the
/// keys gathered so far (a scan failure yields none) rather than failing the caller.
pub(crate) async fn drain_queued(db: &Db, kind: WorkKind, limit: usize) -> Vec<String> {
    let mut keys = Vec::new();
    if limit == 0 {
        return keys;
    }
    let candidates = match crate::runs::work_pods::queued_candidates(
        db.pool(),
        kind.label_value(),
        QUEUE_DRAIN_SCAN,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                kind = kind.label_value(),
                error = format!("{e:#}"),
                "queue drain: scanning queued candidates failed (continuing)"
            );
            return keys;
        }
    };
    for cand in candidates {
        match cand.issue_key {
            Some(key) if !cand.parked => {
                keys.push(key);
                if keys.len() >= limit {
                    break;
                }
            }
            // Parked (or an orphan row with no issue key): purge it and keep scanning for a live one.
            _ => {
                if let Err(e) =
                    crate::runs::work_pods::delete_queued_work_pod(db.pool(), &cand.pod_name).await
                {
                    tracing::warn!(
                        pod_name = %cand.pod_name,
                        error = format!("{e:#}"),
                        "queue drain: purging a parked queued row failed (continuing)"
                    );
                }
            }
        }
    }
    keys
}

/// Re-drive one freed slot's worth of queued backlog for `kind`: find the eldest DRAINABLE queued
/// row (purging any whose issue has since parked) and enqueue its ISSUE so the next reconcile pass
/// promotes the row onto the slot this collection just freed. The tail never dispatches a pod itself
/// — the reconcile pass owns the spend gates and issue context; this only re-drives the key.
/// Best-effort: a DB hiccup or an unwired queue is logged and the collection it rides never fails.
pub(crate) async fn drain_freed_slot(db: &Db, queue: Option<&WorkQueue>, kind: WorkKind) {
    if let Some(key) = drain_queued(db, kind, 1).await.into_iter().next()
        && let Some(queue) = queue
    {
        queue.enqueue(crate::daemon::queue::IssueKey(key));
    }
}
