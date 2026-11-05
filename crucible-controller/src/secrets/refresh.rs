//! Keeps a live run's minted GitHub App token fresh (ADR-0051). An installation token lasts an
//! hour and a run is minted one at dispatch, so on an interval shorter than half that the leader
//! re-mints the token of every running pod it recorded delivering one to, and merge-patches that
//! key of that pod's Secret alone. The token is mounted into `crucible-git` by directory, so the
//! kubelet carries the new bytes in, and the sidecar reads the file on every request.
//!
//! What to refresh comes only from `run_git_tokens`, written at dispatch. Each sweep first drops
//! the rows of pods no longer running and of runs whose binding is gone or now names another
//! secret, so revoking a binding stops the refresh at the next sweep rather than at the run's end.

use crate::secrets::deliver::GitToken;
use crate::secrets::provider::SecretProvider;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Secret;
use std::sync::Arc;
use std::time::Duration;

/// Shorter than half the token's hour, so a token handed out just before a sweep still has more
/// than half its life left at the next one, kubelet propagation included.
pub const INTERVAL: Duration = Duration::from_secs(25 * 60);

/// Record that `pod_name`'s Secret carries `token`, minted through its binding.
pub async fn record(
    pool: &sqlx::PgPool,
    pod_name: &str,
    cluster: &str,
    namespace: &str,
    token: &GitToken,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO run_git_tokens
             (pod_name, cluster, namespace, binding_id, secret_id, secret_key, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (pod_name) DO UPDATE SET binding_id = EXCLUDED.binding_id,
             secret_id = EXCLUDED.secret_id, secret_key = EXCLUDED.secret_key",
    )
    .bind(pod_name)
    .bind(cluster)
    .bind(namespace)
    .bind(&token.binding_id)
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
            tracing::warn!(error = %format!("{e:#}"), "GitHub token refresh sweep failed");
        }
    }
}

/// Drop the rows of finished pods and of revoked or repointed bindings; the count dropped.
async fn prune(pool: &sqlx::PgPool) -> Result<u64> {
    let done = sqlx::query(
        "DELETE FROM run_git_tokens t
         WHERE NOT EXISTS (SELECT 1 FROM work_pods w
                           WHERE w.pod_name = t.pod_name AND w.state = 'running')
            OR NOT EXISTS (SELECT 1 FROM secret_bindings b
                           WHERE b.id = t.binding_id AND b.secret_id = t.secret_id)",
    )
    .execute(pool)
    .await
    .context("pruning GitHub token refresh rows")?;
    Ok(done.rows_affected())
}

async fn sweep(
    db: &crate::client::Db,
    clusters: &crate::runs::clusters::ClusterClients,
    provider: &dyn SecretProvider,
) -> Result<()> {
    let dropped = prune(db.pool()).await?;
    if dropped > 0 {
        tracing::info!(
            dropped,
            "stopped refreshing GitHub tokens of finished runs or revoked bindings"
        );
    }
    let live: Vec<Live> = sqlx::query_as(
        "SELECT pod_name, cluster, namespace, secret_id, secret_key FROM run_git_tokens",
    )
    .fetch_all(db.pool())
    .await
    .context("reading the runs whose GitHub token to refresh")?;
    for run in live {
        match refresh(db, clusters, provider, &run).await {
            Ok(()) => {
                tracing::info!(pod = %run.pod_name, cluster = %run.cluster, "refreshed GitHub token")
            }
            Err(e) => tracing::warn!(
                pod = %run.pod_name,
                cluster = %run.cluster,
                error = %format!("{e:#}"),
                "GitHub token refresh failed"
            ),
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
        .with_context(|| format!("registry secret {} is gone", run.secret_id))?;
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

#[cfg(test)]
mod tests {
    use crate::secrets::refresh::*;

    async fn secret(pool: &sqlx::PgPool, id: &str) {
        sqlx::query(
            "INSERT INTO secrets (id, name, owner, kind, visibility, consumer, mode, vault_path,
                                  created_at, updated_at)
             VALUES ($1, $1, 'user:u', 'opaque', 'broker_only', 'run', 'minted',
                     'mint://github-app', 'now', 'now')",
        )
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn work_pod(pool: &sqlx::PgPool, pod: &str, state: &str) {
        sqlx::query(
            "INSERT INTO work_pods (pod_name, kind, state, cost_tag, created_at, updated_at)
             VALUES ($1, 'run', $2, 'test', 'now', 'now')",
        )
        .bind(pod)
        .bind(state)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn binding(pool: &sqlx::PgPool, id: &str, secret_id: &str) {
        sqlx::query(
            "INSERT INTO secret_bindings (id, secret_id, scope_kind, scope_id, projection_kind,
                                          projection, declared_name, created_at)
             VALUES ($1, $2, 'playbook', $1, 'file', '/t', 'github-app', 'now')",
        )
        .bind(id)
        .bind(secret_id)
        .execute(pool)
        .await
        .unwrap();
    }

    fn token(binding_id: &str, secret_id: &str) -> GitToken {
        GitToken {
            key: "github-app".into(),
            binding_id: binding_id.into(),
            secret_id: secret_id.into(),
            minted: true,
        }
    }

    /// A row survives only while its pod runs and its binding still names the same secret.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn prune_keeps_live_bound_runs_and_drops_finished_or_revoked_ones(pool: sqlx::PgPool) {
        secret(&pool, "s1").await;
        secret(&pool, "s2").await;
        for (pod, state) in [
            ("live", "running"),
            ("done", "succeeded"),
            ("revoked", "running"),
            ("repointed", "running"),
        ] {
            work_pod(&pool, pod, state).await;
        }
        binding(&pool, "b-live", "s1").await;
        binding(&pool, "b-done", "s1").await;
        binding(&pool, "b-repointed", "s2").await;
        for (pod, binding_id) in [
            ("live", "b-live"),
            ("done", "b-done"),
            ("revoked", "b-gone"),
            ("repointed", "b-repointed"),
        ] {
            record(&pool, pod, "hub", "ns", &token(binding_id, "s1"))
                .await
                .unwrap();
        }

        assert_eq!(prune(&pool).await.unwrap(), 3);
        let left: Vec<String> = sqlx::query_scalar("SELECT pod_name FROM run_git_tokens")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(left, ["live"]);
    }
}
