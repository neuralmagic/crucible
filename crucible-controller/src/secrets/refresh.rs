//! Keeps a live run's GitHub App token fresh (ADR-0051). An installation token lasts an hour and a
//! run is minted one at dispatch, so on an interval shorter than half that the leader re-mints
//! the token of every running pod the controller recorded delivering one to, and merge-patches
//! that key of that pod's Secret alone. What to refresh comes from the controller's own record,
//! never from labels or annotations a cluster object could carry. The token is mounted by
//! directory, so the kubelet carries the new bytes into the running `crucible-git` container,
//! which reads the file on every request. The run never asks for a token.

use crate::secrets::deliver::GitToken;
use crate::secrets::provider::SecretProvider;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Secret;
use std::sync::Arc;
use std::time::Duration;

/// Shorter than half the token's hour, so a token handed out just before a sweep still has more
/// than half its life left at the next one, kubelet propagation included.
pub const INTERVAL: Duration = Duration::from_secs(25 * 60);

/// Record that `pod_name`'s Secret carries `token`, minted from a registry secret.
pub async fn record(
    pool: &sqlx::PgPool,
    pod_name: &str,
    cluster: &str,
    namespace: &str,
    token: &GitToken,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO run_git_tokens (pod_name, cluster, namespace, secret_id, secret_key, created_at)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (pod_name) DO UPDATE SET secret_id = EXCLUDED.secret_id,
             secret_key = EXCLUDED.secret_key",
    )
    .bind(pod_name)
    .bind(cluster)
    .bind(namespace)
    .bind(&token.secret_id)
    .bind(&token.key)
    .bind(jiff::Timestamp::now().to_string())
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Live {
    pod_name: String,
    cluster: String,
    namespace: String,
    secret_id: String,
    secret_key: String,
}

pub async fn watch_loop(
    db: crate::client::Db,
    clusters: Arc<crate::runs::clusters::ClusterClients>,
    provider: Arc<dyn SecretProvider>,
) {
    let mut timer = tokio::time::interval(INTERVAL);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    timer.tick().await;
    loop {
        timer.tick().await;
        if let Err(e) = sweep(&db, &clusters, provider.as_ref()).await {
            tracing::warn!(error = %format!("{e:#}"), "refreshing run GitHub tokens");
        }
    }
}

async fn sweep(
    db: &crate::client::Db,
    clusters: &crate::runs::clusters::ClusterClients,
    provider: &dyn SecretProvider,
) -> Result<()> {
    let live: Vec<Live> = sqlx::query_as(
        "SELECT t.pod_name, t.cluster, t.namespace, t.secret_id, t.secret_key
         FROM run_git_tokens t JOIN work_pods w ON w.pod_name = t.pod_name
         WHERE w.state = 'running'",
    )
    .fetch_all(db.pool())
    .await
    .context("reading the running pods that carry a GitHub token")?;
    for run in live {
        let outcome = refresh(db, clusters, provider, &run).await;
        match outcome {
            Ok(()) => {
                tracing::info!(pod = %run.pod_name, cluster = %run.cluster, "refreshed the run's GitHub token")
            }
            Err(e) => {
                tracing::warn!(pod = %run.pod_name, cluster = %run.cluster, error = %format!("{e:#}"), "refreshing the run's GitHub token")
            }
        }
    }
    Ok(())
}

async fn refresh(
    db: &crate::client::Db,
    clusters: &crate::runs::clusters::ClusterClients,
    provider: &dyn SecretProvider,
    run: &Live,
) -> Result<()> {
    let row = crate::secrets::store::get(db.pool(), &run.secret_id)
        .await?
        .with_context(|| format!("no registry secret {}", run.secret_id))?;
    let token = provider.value_of(&row).await?;
    let client = clusters.client(&run.cluster).await?;
    let api: kube::Api<Secret> = kube::Api::namespaced(client, &run.namespace);
    let patch = serde_json::json!({ "stringData": { run.secret_key.clone(): token } });
    api.patch(
        &crate::secrets::deliver::secret_name(&run.pod_name),
        &kube::api::PatchParams::default(),
        &kube::api::Patch::Merge(&patch),
    )
    .await
    .context("patching the run Secret")?;
    Ok(())
}
