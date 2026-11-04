use crate::client::Db;
#[cfg(feature = "autoresearch")]
use crate::config::ControllerCfg;
use crate::runs::workpod::*;
use anyhow::{Context, Result};
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
/// bounded by their completion edge ([`crate::runs::completion::ingest_completion`]), so they're
/// skipped. Returns the issue keys it freed, so the
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
            // A run's deadline is its completion edge's; never reaped here.
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
        queue: crate::daemon::queue::WorkQueue,
    ) -> crate::daemon::queue::BoxFuture<Result<()>> {
        let db = self.db.clone();
        let cfg = self.cfg.clone();
        Box::pin(async move {
            for key in sweep_timed_out_turns(&db, cfg.runtime.dispatcher.as_ref(), &cfg).await {
                queue.enqueue(crate::daemon::queue::IssueKey(key));
            }
            Ok(())
        })
    }
}

/// The issues behind `running` run pod rows, each once, in row order.
pub(crate) fn run_pod_resync_keys(rows: &[WorkPodRow]) -> Vec<String> {
    let run = WorkKind::Run.label_value();
    let mut seen = std::collections::BTreeSet::new();
    rows.iter()
        .filter(|row| row.state == WorkPodState::Running && row.kind == run)
        .filter_map(|row| row.issue_key.clone())
        .filter(|key| seen.insert(key.clone()))
        .collect()
}

/// A [`crate::daemon::queue::DiscoverySource`] that re-drives the issue of every `running` run pod
/// row on the discovery tick, whatever the issue's status, so the completion edge
/// ([`crate::runs::completion::ingest_completion`]) collects a finished pod, fails a vanished or
/// overrun one, and never depends on the pod watch having delivered an event.
pub struct RunPodResync {
    db: Db,
}

impl RunPodResync {
    pub(crate) fn new(db: Db) -> Self {
        RunPodResync { db }
    }
}

impl crate::daemon::queue::DiscoverySource for RunPodResync {
    fn poll(
        &self,
        queue: crate::daemon::queue::WorkQueue,
    ) -> crate::daemon::queue::BoxFuture<Result<()>> {
        let db = self.db.clone();
        Box::pin(async move {
            let rows =
                crate::runs::work_pods::work_pods_in_states(db.pool(), &[WorkPodState::Running])
                    .await
                    .context("reading running work pods for the run pod resync")?;
            for key in run_pod_resync_keys(&rows) {
                queue.enqueue(crate::daemon::queue::IssueKey(key));
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

/// Reconcile the WorkPod ledger against the cluster at controller startup, so a restart wastes no
/// paid turn and leaks no pod. The passes over the `work_pods` rows:
///   * a `running` TURN row is left for the level-triggered adoption path — re-driving the issue
///     (the completion watch's initial list + the non-terminal re-enqueue) collects it on the one
///     shared tail (verdict or scope-report, cost booked once by the CAS, a vanished pod recorded as
///     a lost turn). See [`reconcile_turn_on_startup`].
///   * a `running` RUN row whose pod is observed FAILED → converge onto the failed-pod policy (its
///     `session.jsonl` completion is the out-of-band edge; see [`reconcile_run_on_startup`]).
///   * a `succeeded`/`failed` row past its retention → sweep the pod + close the row.
///
/// A still-running pod is left alone (the in-flight reconcile owns it, or the next reconcile
/// re-drives and re-adopts it). Best-effort: a per-row error is logged and the sweep continues.
/// The count-cap pass ([`sweep_failed_pod_overflow`], keep the newest `failed_pod_keep`) runs
/// after the per-row passes, over whatever they left retained.
pub async fn reconcile_on_startup(
    db: &Db,
    dispatcher: &dyn PodDispatcher,
    namespace: &str,
    failed_pod_keep: u32,
) {
    let rows = match crate::runs::work_pods::work_pods_in_states(
        db.pool(),
        &[
            WorkPodState::Running,
            WorkPodState::Succeeded,
            WorkPodState::Failed,
        ],
    )
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(
                error = format!("{e:#}"),
                "workpod startup sweep: reading rows failed"
            );
            return;
        }
    };
    let now = crate::clock::now_rfc3339();
    for row in rows {
        let Some(ns) =
            crate::runs::workpod::row_namespace(dispatcher, &row.cluster, namespace).await
        else {
            continue;
        };
        if let Err(e) = reconcile_one_on_startup(db, dispatcher, &ns, &row, &now).await {
            tracing::warn!(
                pod_name = %row.pod_name,
                error = format!("{e:#}"),
                "workpod startup sweep: reconciling a row failed (continuing)"
            );
        }
    }
    sweep_failed_pod_overflow(db, dispatcher, namespace, failed_pod_keep).await;
}

async fn reconcile_one_on_startup(
    db: &Db,
    dispatcher: &dyn PodDispatcher,
    namespace: &str,
    row: &WorkPodRow,
    now: &str,
) -> Result<()> {
    // Collection is per-kind: a turn is watched + ingested in-band here (its verdict rides its logs),
    // a run is watched out-of-band (the shared pod watch + the reconcile completion edge ingest its
    // `session.jsonl`), so its startup pass only converges a dead pod onto the GC policy.
    match WorkKind::parse_label(&row.kind)? {
        WorkKind::AgentTurn(_) => {
            reconcile_turn_on_startup(db, dispatcher, namespace, row, now).await
        }
        WorkKind::Run => reconcile_run_on_startup(db, dispatcher, namespace, row, now).await,
    }
}

/// The turn kind's startup pass. A `running` turn is re-adopted OUT-OF-BAND now, exactly like a run:
/// the pod-completion watch's initial list (turn pods carry the managed-by selector, so a terminal
/// one is emitted at startup) plus the startup non-terminal re-enqueue both re-drive the issue
/// through reconcile, where [`dispatch_grounded_rank`]/[`dispatch_scope`] ADOPT the existing pod and
/// collect it on the single shared tail — verdict AND scope-report, both kinds, cost booked exactly
/// once by the CAS. So this pass does not scrape verdicts itself (a scope turn's logs carry a
/// `CRUCIBLE_SCOPE_REPORT:` marker, not `CRUCIBLE_VERDICT:`, so a verdict scrape here would
/// mis-mark it failed + drop the pack). It only converges a RETAINED terminal turn onto the
/// GC policy — a still-`running` row is left for adoption (a vanished pod surfaces there as a lost
/// turn, so it never hangs).
async fn reconcile_turn_on_startup(
    db: &Db,
    dispatcher: &dyn PodDispatcher,
    namespace: &str,
    row: &WorkPodRow,
    now: &str,
) -> Result<()> {
    match row.state {
        // Left for the level-triggered adoption path (the one collection tail): re-driving the issue
        // through reconcile collects it, so a second parse here would only fight the CAS.
        WorkPodState::Running => {}
        WorkPodState::Succeeded | WorkPodState::Failed
            if failed_pod_should_sweep(row.terminal_at.as_deref(), now, FAILED_POD_RETENTION) =>
        {
            sweep_retained_pod(db, dispatcher, namespace, row).await?;
        }
        // A terminal pod still inside its retention window, or a queued/collected/swept row, needs
        // no cluster reconciliation.
        _ => {}
    }
    Ok(())
}

/// The run kind's startup pass. A run is re-adopted out-of-band: the issue-row re-enqueue + the
/// shared pod watch ingest a still-live or already-succeeded run's `session.jsonl` (which then
/// [`collect_run_pod`]s it), so this pass leaves those alone and only converges a pod that DEFINITELY
/// FAILED onto the failed-pod retention policy — so its row doesn't hang at `running` and its pod
/// eventually gets swept. (A vanished/unreachable pod is deliberately left for the completion edge:
/// the run may have finished and published its log before the pod was reaped, and marking it failed
/// here would race that ingest.)
async fn reconcile_run_on_startup(
    db: &Db,
    dispatcher: &dyn PodDispatcher,
    namespace: &str,
    row: &WorkPodRow,
    now: &str,
) -> Result<()> {
    match row.state {
        WorkPodState::Running => {
            if let Ok(TerminalState {
                phase: TurnPhase::Failed,
                ..
            }) = dispatcher
                .await_terminal(
                    &row.cluster,
                    namespace,
                    &row.pod_name,
                    Duration::from_secs(1),
                )
                .await
            {
                crate::runs::work_pods::set_work_pod_state(
                    db.pool(),
                    &row.pod_name,
                    WorkPodState::Failed,
                    None,
                    Some("loop-run pod failed (observed at controller startup)"),
                )
                .await?;
            }
        }
        WorkPodState::Succeeded | WorkPodState::Failed
            if failed_pod_should_sweep(row.terminal_at.as_deref(), now, FAILED_POD_RETENTION) =>
        {
            sweep_retained_pod(db, dispatcher, namespace, row).await?;
        }
        _ => {}
    }
    Ok(())
}

/// Delete a retained terminal pod whose retention window has elapsed and mark its row `swept`. A
/// delete that reports the pod as still needed leaves the row alone for the next pass.
async fn sweep_retained_pod(
    db: &Db,
    dispatcher: &dyn PodDispatcher,
    namespace: &str,
    row: &WorkPodRow,
) -> Result<()> {
    if !crate::runs::workpod::delete_for_sweep(dispatcher, &row.cluster, namespace, &row.pod_name)
        .await
    {
        return Ok(());
    }
    crate::runs::work_pods::set_work_pod_state(
        db.pool(),
        &row.pod_name,
        WorkPodState::Swept,
        None,
        None,
    )
    .await
}
