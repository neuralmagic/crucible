//! Cancelling a launch's runs: each `running` run on a pod has its pod deleted on the cluster it
//! ran on and is marked `cancelled`; a locally dispatched run has its engine signalled to stop
//! through the registry of live local runs, and settles `cancelled` when its supervisor folds it.

use anyhow::Result;

use crate::client::Db;
use crate::config::ControllerCfg;
use crate::runs::workpod::WorkPodState;

/// Cancel `key`'s running runs for `actor`: a pod run's pod is deleted on the cluster it ran on,
/// a local run's engine is signalled to stop. Returns how many it stopped; a run whose cluster did
/// not answer the delete, or whose engine could not be signalled, stays `running`, for a later
/// cancel or the resync.
pub async fn cancel_runs(
    db: &Db,
    cfg: &ControllerCfg,
    key: &str,
    actor: Option<&str>,
) -> Result<usize> {
    let dispatcher = cfg.runtime.dispatcher.clone();
    let runs: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT run_id, pod FROM runs WHERE issue = $1 AND status = 'running' ORDER BY seq",
    )
    .bind(key)
    .fetch_all(db.pool())
    .await?;
    let note = format!("cancelled by {}", actor.unwrap_or("an operator"));
    let mut cancelled = 0;
    for (run_id, pod) in runs {
        let Some(pod) = pod else {
            if !cfg.runtime.local_runs.cancel(&run_id) {
                tracing::warn!(issue_key = %key, run_id = %run_id, "cancel: a local run this process is not supervising is not stopped here");
                continue;
            }
            crate::runs::pod_token::revoke(
                db.pool(),
                &crate::runs::pod_token::Subject::Run(run_id.clone()),
            )
            .await?;
            tracing::info!(issue_key = %key, run_id = %run_id, "cancel: signalled a locally dispatched run's engine to stop");
            cancelled += 1;
            continue;
        };
        let row = crate::runs::work_pods::get_work_pod(db.pool(), &pod).await?;
        let cluster = row
            .as_ref()
            .map(|r| r.cluster.clone())
            .unwrap_or_else(|| crate::runs::clusters::HUB_CLUSTER.to_string());
        let Some(ns) =
            crate::runs::workpod::row_namespace(dispatcher.as_ref(), &cluster, &cfg.pod_namespace)
                .await
        else {
            continue;
        };
        if !crate::runs::workpod::delete_for_sweep(dispatcher.as_ref(), &cluster, &ns, &pod).await {
            continue;
        }
        if row.is_some() {
            crate::runs::work_pods::set_work_pod_state(
                db.pool(),
                &pod,
                WorkPodState::Collected,
                Some(&note),
                None,
            )
            .await?;
        }
        crate::runs::store::set_run_status(db.pool(), &run_id, "cancelled").await?;
        crate::runs::pod_token::revoke(
            db.pool(),
            &crate::runs::pod_token::Subject::Run(run_id.clone()),
        )
        .await?;
        tracing::info!(issue_key = %key, run_id = %run_id, pod_name = %pod, cluster = %cluster, "cancel: deleted the run's pod");
        cancelled += 1;
    }
    Ok(cancelled)
}
