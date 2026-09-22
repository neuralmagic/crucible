use crate::client::Db;
use crate::runs::workpod::*;
use std::sync::Arc;

/// Install the process-global pod dispatcher (production [`KubePodDispatcher`], or a test's fake).
/// The dispatch fires inside the pure reconcile step (a grounded turn) and the reconcile-awaiting
/// launch (a run), neither of which can thread a dispatcher argument through the frozen reconcile
/// signature — so it lives here, the one sanctioned process-global for the cluster boundary.
static ACTIVE_DISPATCHER: std::sync::RwLock<Option<Arc<dyn PodDispatcher>>> =
    std::sync::RwLock::new(None);

/// Install the active turn dispatcher (daemon assembly, or a test's fake).
pub fn install_dispatcher(dispatcher: Arc<dyn PodDispatcher>) {
    *ACTIVE_DISPATCHER
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(dispatcher);
}

/// Drop the installed dispatcher (a test's teardown).
pub fn reset_dispatcher() {
    *ACTIVE_DISPATCHER
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// The dispatcher a dispatch resolves to: the installed one, else a default [`KubePodDispatcher`]
/// with no spoke credentials (hub-only; no cluster registry was installed).
pub(crate) fn active_dispatcher() -> Arc<dyn PodDispatcher> {
    ACTIVE_DISPATCHER
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .unwrap_or_else(|| {
            Arc::new(KubePodDispatcher::new(Arc::new(
                crate::runs::clusters::ClusterClients::new(None),
            )))
        })
}

/// The contract registry every launch is gated on. Installed by the daemon assembly next to the
/// dispatcher, for the same reason: the gate fires inside the reconcile step and the launch paths,
/// which thread no registry handle. `None` (a unit test not exercising the check) gates nothing.
static ACTIVE_CONTRACTS: std::sync::RwLock<Option<Arc<crate::runs::contract::ContractRegistry>>> =
    std::sync::RwLock::new(None);

/// Install the active contract registry (daemon assembly, or a test's table-backed one).
pub fn install_contracts(registry: Arc<crate::runs::contract::ContractRegistry>) {
    *ACTIVE_CONTRACTS
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(registry);
}

/// Drop the installed contract registry (a test's teardown).
pub fn reset_contracts() {
    *ACTIVE_CONTRACTS
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// Gate one launch on the installed registry: `Ok` when no registry is installed or every target
/// matches this controller; the first failure otherwise.
pub(crate) async fn admit_contract(
    request: crate::runs::contract::RequestKind,
    targets: &[crate::runs::contract::DispatchTarget],
) -> Result<(), crate::runs::contract::AdmitFailure> {
    let registry = ACTIVE_CONTRACTS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let Some(registry) = registry else {
        return Ok(());
    };
    for target in targets {
        registry.admit(request, target).await?;
    }
    Ok(())
}

/// The engine version the installed registry last recorded for `target`, or `None` when no
/// registry is installed. Names an engine in a refusal; the gate itself is [`admit_contract`].
pub(crate) async fn recorded_engine_version(
    target: &crate::runs::contract::DispatchTarget,
) -> Option<String> {
    let registry = ACTIVE_CONTRACTS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()?;
    Some(registry.record(target).await.engine_version)
}

/// Install the process-global reconcile-queue handle, so a collection that frees a slot can re-drive
/// a queued backlog row's ISSUE back through reconcile (which owns the spend gates + issue context)
/// to promote it. Same sanctioned-process-global rationale as [`install_dispatcher`]: the drain
/// fires deep inside the collection tail (called from the pure reconcile step and its adopt-first
/// pre-pass), neither of which can thread a queue handle through the frozen reconcile signature. The
/// daemon assembly installs the real [`crate::daemon::queue::WorkQueue`]; unlike the dispatcher there is NO
/// default — a process with no queue wired (a unit test not exercising the drain) makes the drain a
/// safe no-op, since the timeout-sweep backstop and the next reconcile pass still drain the queue.
static ACTIVE_ENQUEUE: std::sync::RwLock<Option<Arc<dyn crate::daemon::queue::Enqueue>>> =
    std::sync::RwLock::new(None);

/// Install the active reconcile-queue handle (daemon assembly, or a test's recorder).
pub fn install_enqueue(enqueue: Arc<dyn crate::daemon::queue::Enqueue>) {
    *ACTIVE_ENQUEUE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(enqueue);
}

/// Drop the installed queue handle (a test's teardown).
pub fn reset_enqueue() {
    *ACTIVE_ENQUEUE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// The installed reconcile-queue handle, or `None` when no queue is wired (drain is then a no-op).
fn active_enqueue() -> Option<Arc<dyn crate::daemon::queue::Enqueue>> {
    ACTIVE_ENQUEUE
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

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
pub(crate) async fn drain_freed_slot(db: &Db, kind: WorkKind) {
    if let Some(key) = drain_queued(db, kind, 1).await.into_iter().next()
        && let Some(enqueue) = active_enqueue()
    {
        enqueue.enqueue(crate::daemon::queue::IssueKey(key));
    }
}
