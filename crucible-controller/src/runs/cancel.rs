//! Cancelling a launch's runs: each `running` run on a pod has its engine log kept, its pod and
//! its gateway deleted on the cluster it ran on, and is marked `cancelled`. A local run's engine
//! is interrupted, and its supervisor folds whatever session it wrote. Every open call the
//! launch's runs dispatched is cancelled with them.

use anyhow::Result;

use crate::client::Db;
use crate::config::ControllerCfg;
use crate::runs::workpod::WorkPodState;

/// Cancel `key`'s running pod runs for `actor`. Returns how many it cancelled; a run whose cluster
/// did not answer the delete stays `running`, for a later cancel or the resync.
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
    match crate::calls::api::cancel_for_launch(db, cfg, key).await {
        Ok(0) => {}
        Ok(calls) => {
            tracing::info!(issue_key = %key, calls, "cancel: cancelled the launch's open calls")
        }
        Err(e) => {
            tracing::warn!(issue_key = %key, error = %format!("{e:#}"), "cancel: cancelling the launch's calls failed")
        }
    }
    let profile = crate::runs::completion::collection_profile(cfg).await;
    let mut cancelled = 0;
    for (run_id, pod) in runs {
        let Some(pod) = pod else {
            if crate::runs::local_run::interrupt(cfg.scratch_root(), &run_id) {
                tracing::info!(issue_key = %key, run_id = %run_id, "cancel: interrupted the local run");
                cancelled += 1;
            } else {
                tracing::warn!(issue_key = %key, run_id = %run_id, "cancel: a run with no pod and no local engine is not stopped here");
            }
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
        crate::runs::completion::keep_engine_log(
            db,
            dispatcher.as_ref(),
            &cluster,
            &ns,
            &pod,
            &run_id,
        )
        .await;
        if !crate::runs::workpod::delete_for_sweep(dispatcher.as_ref(), &cluster, &ns, &pod).await {
            continue;
        }
        crate::runs::workpod::teardown_run_gateway(
            dispatcher.as_ref(),
            &cluster,
            profile.as_deref(),
            &pod,
            &ns,
        )
        .await;
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
