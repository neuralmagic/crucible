use crate::client::Db;
#[cfg(feature = "autoresearch")]
use crate::config::ControllerCfg;
use crate::runs::workpod::*;
use anyhow::{Context, Result};
#[cfg(feature = "autoresearch")]
use std::sync::Arc;
use std::time::Duration;

/// The retained-terminal pod names past the count cap: rows sorted newest-terminal first, everything
/// after the first `keep` returned for sweeping. A row with no `terminal_at` sorts oldest (the time
/// sweep already treats it as unretainable). `keep = 0` sweeps them all. RFC3339 `…Z` stamps compare
/// chronologically as strings.
pub(crate) fn failed_pod_overflow(rows: &[WorkPodRow], keep: u32) -> Vec<(String, String)> {
    let mut retained: Vec<&WorkPodRow> = rows.iter().collect();
    retained.sort_by(|a, b| {
        b.terminal_at
            .cmp(&a.terminal_at)
            .then_with(|| a.pod_name.cmp(&b.pod_name))
    });
    retained
        .into_iter()
        .skip(keep as usize)
        .map(|r| (r.pod_name.clone(), r.cluster.clone()))
        .collect()
}

/// Delete a pod ahead of closing its row. `true` = the pod is definitively gone (deleted, or
/// the API answered 404) and the row may advance. `false` = the API call failed (transient or
/// credential-rejected), so the row is kept for a later pass: the pod may still exist and
/// closing the row now would leak it. A non-kube error advances the row, matching the
/// pre-existing best-effort behavior.
pub(crate) async fn delete_for_sweep(
    dispatcher: &dyn PodDispatcher,
    cluster: &str,
    namespace: &str,
    pod_name: &str,
) -> bool {
    match dispatcher.delete(cluster, namespace, pod_name).await {
        Ok(()) => true,
        Err(e) => match crate::runs::workpod::retry::classify_chain(&e) {
            Some(crate::runs::workpod::retry::KubeFailure::Gone) | None => true,
            Some(_) => {
                tracing::warn!(
                    pod_name,
                    cluster,
                    error = format!("{e:#}"),
                    "sweep: cluster did not answer the pod delete; holding the row"
                );
                false
            }
        },
    }
}

/// Per-row namespace via [`PodDispatcher::pod_namespace`]. `None` (with a warning) when the
/// spoke credential can't even be loaded.
pub(crate) async fn row_namespace(
    dispatcher: &dyn PodDispatcher,
    cluster: &str,
    hub_namespace: &str,
) -> Option<String> {
    match dispatcher.pod_namespace(cluster, hub_namespace).await {
        Ok(ns) => Some(ns),
        Err(e) => {
            tracing::warn!(
                cluster,
                error = format!("{e:#}"),
                "sweep: cannot resolve the cluster namespace; skipping row"
            );
            None
        }
    }
}

/// The count-cap pass over the retained-terminal states (`succeeded`/`failed` — the rows whose pod
/// may still sit on the cluster): keep the newest `keep`, sweep the rest (pod deleted, row →
/// `swept`) even inside the time window. Global across kinds — the debugging budget is cluster
/// clutter, not per-kind fairness. Runs at startup and after every new retained failure, so a
/// failure storm can't bury `kubectl get pods`. Best-effort like the time sweep: a per-row error is
/// logged and the pass continues.
pub(crate) async fn sweep_failed_pod_overflow(
    db: &Db,
    dispatcher: &dyn PodDispatcher,
    namespace: &str,
    keep: u32,
) {
    let rows = match crate::runs::work_pods::work_pods_in_states(
        db.pool(),
        &[WorkPodState::Succeeded, WorkPodState::Failed],
    )
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(
                error = format!("{e:#}"),
                "failed-pod count sweep: reading rows failed"
            );
            return;
        }
    };
    for (name, cluster) in failed_pod_overflow(&rows, keep) {
        let Some(ns) = row_namespace(dispatcher, &cluster, namespace).await else {
            continue;
        };
        if !delete_for_sweep(dispatcher, &cluster, &ns, &name).await {
            continue;
        }
        if let Err(e) = crate::runs::work_pods::set_work_pod_state(
            db.pool(),
            &name,
            WorkPodState::Swept,
            None,
            None,
        )
        .await
        {
            tracing::warn!(
                pod_name = %name,
                error = format!("{e:#}"),
                "failed-pod count sweep: closing a row failed (continuing)"
            );
        }
    }
}

/// Reap turn pods that overran their deadline. Non-blocking dispatch no longer awaits a turn, so a
/// pod that never terminates (a wedged agent, a stuck sandbox pull) would hold its `running` row —
/// and its slot under the per-kind concurrency cap — forever. This out-of-band sweep, hooked into the
/// daemon's discovery tick ([`TurnTimeoutPoll`]), finds `running` AGENT-TURN rows older than their
/// kind's deadline, deletes the pod, and CAS-fails the row (the CAS keeps the single-booking honest
/// against a re-drive that reached the pod first). Row age is `created_at`; the grounded deadline is
/// flat ([`GROUNDED_RANK_TIMEOUT`]), the scope deadline scales with the effective gaming allowance
/// ([`scope_deadline`]) — the same allowance the pod's argv was rendered with. Run pods are
/// hours-long and watched out-of-band, so they're skipped. Returns the issue keys it freed, so the
/// caller re-drives them (a freed slot must re-dispatch). Best-effort: a per-row error is logged and
/// the sweep continues.
///
/// It ALSO backstops the collection-edge queue drain ([`drain_freed_slot`]): after reaping, it fills
/// the grounded-rank free slots from the queued backlog (the eldest DRAINABLE rows up to the slot
/// count, purging any whose issue has since parked), so a missed completion edge can't strand the
/// queue forever. Only grounded-rank carries a queue (`dispatch_scope` has no admission/queue step),
/// so the backstop is one extra query per tick. Those drained keys ride the same returned vec.
#[cfg(feature = "autoresearch")]
pub(crate) async fn sweep_timed_out_turns(
    db: &Db,
    dispatcher: &dyn PodDispatcher,
    cfg: &ControllerCfg,
) -> Vec<String> {
    let rows = match crate::runs::work_pods::work_pods_in_states(
        db.pool(),
        &[WorkPodState::Running],
    )
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(
                error = format!("{e:#}"),
                "turn timeout sweep: reading running rows failed"
            );
            return Vec::new();
        }
    };
    let namespace = &cfg.pod_namespace;
    // The scope deadline reads the CURRENT effective gaming allowance — the same knob the render used;
    // an operator who raised it mid-flight grants the running turn the longer leash too.
    let scope_gaming_rounds = cfg.effective().scope_gaming_rounds;
    let mut redrive = Vec::new();
    for row in rows {
        let Ok(kind) = WorkKind::parse_label(&row.kind) else {
            continue;
        };
        // This match stays a direct kind dispatch rather than going through `TurnSpec::deadline` —
        // routing it through a spec would mean constructing (or looking up) a spec instance per row
        // just to call one already-kind-generic getter that reads the same two consts either way.
        // No behavior differs and no future drift is prevented by the indirection, so the trait hook
        // isn't warranted here.
        let deadline = match kind {
            WorkKind::AgentTurn(TurnKind::GroundedRank) => GROUNDED_RANK_TIMEOUT,
            WorkKind::AgentTurn(TurnKind::Scope) => scope_deadline(scope_gaming_rounds),
            // A run is watched out-of-band (the shared pod watch ingests its session); never reaped here.
            WorkKind::Run => continue,
        };
        if !turn_row_overran(&row.created_at, deadline) {
            continue;
        }
        // Delete the pod, then CAS-fail the row so its slot frees and the issue re-drives.
        // A lost CAS means a collector already terminalized it; leave it unchanged. If the
        // delete gets no answer from the cluster, keep the row until the next tick: the pod
        // may have finished and its verdict is still collectable.
        let Some(ns) = row_namespace(dispatcher, &row.cluster, namespace).await else {
            continue;
        };
        if !delete_for_sweep(dispatcher, &row.cluster, &ns, &row.pod_name).await {
            continue;
        }
        let reason = format!(
            "turn pod {} exceeded its {}s deadline; reaped by the timeout sweep",
            row.pod_name,
            deadline.as_secs()
        );
        match crate::runs::work_pods::try_finish_running_work_pod(
            db.pool(),
            &row.pod_name,
            WorkPodState::Failed,
            None,
            Some(&reason),
        )
        .await
        {
            Ok(true) => {
                if let Some(m) = db.metrics() {
                    m.record_turn(kind.label_value(), "timed-out");
                }
                if let Some(key) = row.issue_key {
                    redrive.push(key);
                }
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(
                pod_name = %row.pod_name,
                error = format!("{e:#}"),
                "turn timeout sweep: failing a row failed (continuing)"
            ),
        }
    }
    // Backstop the collection-edge drain: fill the grounded-rank free slots from the queued backlog
    // (the reap loop above may have just freed some), so a missed completion edge can't strand it.
    redrive.extend(drain_free_slots_backstop(db, cfg).await);
    redrive
}

/// The queued keys that fill grounded-rank's currently-free slots (`grounded_rank_pod_cap` minus the
/// running count), eldest-first and up to the free count, purging any candidate whose issue has
/// since parked. One query for the candidate scan (the sweep's "cheap per tick" budget), bounded by
/// [`QUEUE_DRAIN_SCAN`] so a run of parked head rows can't hide the live rows behind it. A DB error
/// yields no keys rather than failing the whole sweep. Grounded-rank is the only queuing kind, so
/// this is the whole backstop.
#[cfg(feature = "autoresearch")]
async fn drain_free_slots_backstop(db: &Db, cfg: &ControllerCfg) -> Vec<String> {
    let kind = WorkKind::AgentTurn(TurnKind::GroundedRank);
    let cap = cfg.effective().grounded_rank_pod_cap;
    let active = match crate::runs::work_pods::count_active_work_pods(db.pool(), kind.label_value())
        .await
    {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(
                error = format!("{e:#}"),
                "turn timeout sweep: counting active grounded turns failed (skipping the drain backstop)"
            );
            return Vec::new();
        }
    };
    let free = usize::try_from(cap.saturating_sub(active)).unwrap_or(0);
    drain_queued(db, kind, free).await
}

/// Whether a `running` turn row has outlived `deadline`, from its `created_at` dispatch stamp. An
/// unparseable stamp counts as overran — a row with no usable clock can't be trusted to still be
/// live, and the pod delete + CAS-fail are both safe if it wasn't.
#[cfg(feature = "autoresearch")]
fn turn_row_overran(created_at: &str, deadline: Duration) -> bool {
    match elapsed_secs_since(created_at) {
        Ok(secs) => secs >= deadline.as_secs_f64(),
        Err(_) => true,
    }
}

/// A [`crate::daemon::queue::DiscoverySource`] that runs [`sweep_timed_out_turns`] on the daemon's discovery
/// tick — out-of-band deadline enforcement for non-blocking turn dispatch. Enqueues each issue whose
/// row it reaped so it re-dispatches on the freed slot.
#[cfg(feature = "autoresearch")]
pub struct TurnTimeoutPoll {
    db: Db,
    cfg: ControllerCfg,
}

#[cfg(feature = "autoresearch")]
impl TurnTimeoutPoll {
    pub(crate) fn new(db: Db, cfg: ControllerCfg) -> Self {
        TurnTimeoutPoll { db, cfg }
    }
}

#[cfg(feature = "autoresearch")]
impl crate::daemon::queue::DiscoverySource for TurnTimeoutPoll {
    fn poll(
        &self,
        enqueue: Arc<dyn crate::daemon::queue::Enqueue>,
    ) -> crate::daemon::queue::BoxFuture<Result<()>> {
        let db = self.db.clone();
        let cfg = self.cfg.clone();
        Box::pin(async move {
            let dispatcher = active_dispatcher();
            for key in sweep_timed_out_turns(&db, dispatcher.as_ref(), &cfg).await {
                enqueue.enqueue(crate::daemon::queue::IssueKey(key));
            }
            Ok(())
        })
    }
}

/// Whether a terminal pod is past its retention window and should be swept. A missing/unparseable
/// `terminal_at` sweeps immediately (a row with no clock can't be retained meaningfully). RFC3339
/// timestamps (`…Z`), the ledger's stamp format.
pub(crate) fn failed_pod_should_sweep(
    terminal_at: Option<&str>,
    now: &str,
    retention: Duration,
) -> bool {
    let (Some(t), Ok(now_ts)) = (terminal_at, parse_ts(now)) else {
        return true;
    };
    match parse_ts(t) {
        Ok(t_ts) => now_ts.duration_since(t_ts).unwrap_or(Duration::ZERO) >= retention,
        Err(_) => true,
    }
}

/// Parse an RFC3339 `…Z` stamp to a `SystemTime` for retention math (the ledger's format).
pub(crate) fn parse_ts(s: &str) -> Result<std::time::SystemTime> {
    let ts: jiff::Timestamp = s
        .parse()
        .with_context(|| format!("parsing timestamp {s:?}"))?;
    Ok(std::time::UNIX_EPOCH + Duration::from_secs(ts.as_second().max(0) as u64))
}
