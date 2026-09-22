use crate::client::Db;
use crate::runs::workpod::*;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{ConfigMap, Pod};
use std::time::Duration;

/// The production cluster boundary: create/watch/logs/delete a work pod over the kube API,
/// cluster-aware — every call goes through the shared per-cluster client registry
/// ([`crate::runs::clusters::ClusterClients`]). Reached only in-cluster; a test installs a fake, so
/// this is compile-tested + covered by an `#[ignore]`d live test.
pub struct KubePodDispatcher {
    clusters: std::sync::Arc<crate::runs::clusters::ClusterClients>,
}

impl KubePodDispatcher {
    pub fn new(clusters: std::sync::Arc<crate::runs::clusters::ClusterClients>) -> Self {
        Self { clusters }
    }

    async fn api(&self, cluster: &str, namespace: &str) -> Result<kube::Api<Pod>> {
        let client = self.clusters.client(cluster).await?;
        Ok(kube::Api::namespaced(client, namespace))
    }
}

#[async_trait::async_trait]
impl PodDispatcher for KubePodDispatcher {
    async fn create(&self, cluster: &str, namespace: &str, pod: Pod) -> Result<Pod> {
        let api = self.api(cluster, namespace).await?;
        api.create(&kube::api::PostParams::default(), &pod)
            .await
            .context("creating the turn pod")
    }

    async fn create_configmap(&self, cluster: &str, namespace: &str, cm: ConfigMap) -> Result<()> {
        let client = self.clusters.client(cluster).await?;
        let api: kube::Api<ConfigMap> = kube::Api::namespaced(client, namespace);
        api.create(&kube::api::PostParams::default(), &cm)
            .await
            .context("creating the pack ConfigMap")?;
        Ok(())
    }

    async fn create_secret(
        &self,
        cluster: &str,
        namespace: &str,
        secret: k8s_openapi::api::core::v1::Secret,
    ) -> Result<()> {
        let client = self.clusters.client(cluster).await?;
        let api: kube::Api<k8s_openapi::api::core::v1::Secret> =
            kube::Api::namespaced(client, namespace);
        api.create(&kube::api::PostParams::default(), &secret)
            .await
            .context("creating the run Secret")?;
        Ok(())
    }

    async fn await_terminal(
        &self,
        cluster: &str,
        namespace: &str,
        name: &str,
        timeout: Duration,
    ) -> Result<TerminalState> {
        let api = self.api(cluster, namespace).await?;
        let deadline = std::time::Instant::now() + timeout;
        let mut backoff = crate::runs::workpod::retry::POLL_BACKOFF_START;
        loop {
            match api.get(name).await {
                Ok(pod) => {
                    backoff = crate::runs::workpod::retry::POLL_BACKOFF_START;
                    let status = pod.status.as_ref();
                    match status.and_then(|s| s.phase.as_deref()) {
                        Some("Succeeded") => {
                            return Ok(TerminalState {
                                phase: TurnPhase::Succeeded,
                                message: terminated_message(status),
                            });
                        }
                        Some("Failed") => {
                            return Ok(TerminalState {
                                phase: TurnPhase::Failed,
                                message: terminated_message(status),
                            });
                        }
                        _ => {}
                    }
                    if std::time::Instant::now() >= deadline {
                        return Ok(TerminalState {
                            phase: TurnPhase::TimedOut,
                            message: status.and_then(waiting_message),
                        });
                    }
                }
                // Transient failures re-poll with backoff until the caller's deadline;
                // 401/403 and 404 are definitive and return an error immediately.
                Err(e) => match crate::runs::workpod::retry::classify(&e) {
                    crate::runs::workpod::retry::KubeFailure::Transient => {
                        if std::time::Instant::now() + backoff >= deadline {
                            return Err(anyhow::Error::from(e).context(format!(
                                "cluster `{cluster}` unreachable polling the turn pod"
                            )));
                        }
                        tracing::warn!(cluster, pod = name, error = %e, "turn-pod poll failed (transient), backing off");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(crate::runs::workpod::retry::POLL_BACKOFF_MAX);
                        continue;
                    }
                    _ => return Err(anyhow::Error::from(e).context("polling the turn pod")),
                },
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    async fn logs(&self, cluster: &str, namespace: &str, name: &str) -> Result<String> {
        let api = self.api(cluster, namespace).await?;
        api.logs(name, &kube::api::LogParams::default())
            .await
            .context("reading the turn pod's logs")
    }

    async fn delete(&self, cluster: &str, namespace: &str, name: &str) -> Result<()> {
        let api = self.api(cluster, namespace).await?;
        api.delete(name, &kube::api::DeleteParams::default())
            .await
            .context("deleting the turn pod")?;
        Ok(())
    }

    async fn pod_namespace(&self, cluster: &str, hub_namespace: &str) -> Result<String> {
        self.clusters.pod_namespace(cluster, hub_namespace).await
    }
}

fn waiting_message(status: &k8s_openapi::api::core::v1::PodStatus) -> Option<String> {
    status
        .container_statuses
        .as_ref()?
        .iter()
        .find_map(|container| {
            let waiting = container.state.as_ref()?.waiting.as_ref()?;
            let reason = waiting.reason.as_deref().unwrap_or("waiting");
            Some(
                match waiting
                    .message
                    .as_deref()
                    .filter(|message| !message.is_empty())
                {
                    Some(message) => format!("{reason}: {message}"),
                    None => reason.to_string(),
                },
            )
        })
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
