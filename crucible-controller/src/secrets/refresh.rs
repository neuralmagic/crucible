//! Keeps a live run's GitHub App token fresh (ADR-0051). An installation token lasts an hour and a
//! run is minted one at dispatch, so on an interval shorter than half that this re-mints the token
//! of every run Secret labelled as carrying one and merge-patches that key alone. The token is
//! mounted by directory, so the kubelet carries the new bytes into the running `crucible-git`
//! container, which reads the file on every request. The run never asks for a token.

use crate::secrets::deliver::{
    GIT_TOKEN_KEY_ANNOTATION, GIT_TOKEN_LABEL, GIT_TOKEN_SECRET_ANNOTATION,
};
use crate::secrets::provider::SecretProvider;
use k8s_openapi::api::core::v1::Secret;
use std::sync::Arc;
use std::time::Duration;

/// Shorter than half the token's hour, so a token handed out just before a sweep still has more
/// than half its life left at the next one, kubelet propagation included.
pub const INTERVAL: Duration = Duration::from_secs(25 * 60);

pub async fn watch_loop(
    db: crate::client::Db,
    clusters: Arc<crate::runs::clusters::ClusterClients>,
    hub_namespace: String,
    provider: Arc<dyn SecretProvider>,
) {
    let mut timer = tokio::time::interval(INTERVAL);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    timer.tick().await;
    loop {
        timer.tick().await;
        for cluster in clusters.names() {
            if let Err(e) = sweep(&db, &clusters, &cluster, &hub_namespace, provider.as_ref()).await
            {
                tracing::warn!(%cluster, error = %format!("{e:#}"), "refreshing run GitHub tokens");
            }
        }
    }
}

async fn sweep(
    db: &crate::client::Db,
    clusters: &crate::runs::clusters::ClusterClients,
    cluster: &str,
    hub_namespace: &str,
    provider: &dyn SecretProvider,
) -> anyhow::Result<()> {
    use anyhow::Context;
    let client = clusters.client(cluster).await?;
    let namespace = clusters.pod_namespace(cluster, hub_namespace).await?;
    let api: kube::Api<Secret> = kube::Api::namespaced(client, &namespace);
    let labelled = api
        .list(&kube::api::ListParams::default().labels(&format!("{GIT_TOKEN_LABEL}=true")))
        .await
        .context("listing run Secrets that carry a GitHub token")?;
    for secret in labelled {
        let name = secret.metadata.name.clone().unwrap_or_default();
        let note = |k: &str| {
            secret
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(k).cloned())
        };
        let (Some(key), Some(id)) = (
            note(GIT_TOKEN_KEY_ANNOTATION),
            note(GIT_TOKEN_SECRET_ANNOTATION),
        ) else {
            tracing::warn!(%cluster, secret = %name, "run Secret is labelled without its token annotations");
            continue;
        };
        let outcome = async {
            let row = crate::secrets::store::get(db.pool(), &id)
                .await?
                .with_context(|| format!("no registry secret {id}"))?;
            let token = provider.value_of(&row).await?;
            let patch = serde_json::json!({ "stringData": { key.clone(): token } });
            api.patch(
                &name,
                &kube::api::PatchParams::default(),
                &kube::api::Patch::Merge(&patch),
            )
            .await
            .context("patching the run Secret")?;
            anyhow::Ok(())
        }
        .await;
        match outcome {
            Ok(()) => tracing::info!(%cluster, secret = %name, "refreshed the run's GitHub token"),
            Err(e) => {
                tracing::warn!(%cluster, secret = %name, error = %format!("{e:#}"), "refreshing the run's GitHub token")
            }
        }
    }
    Ok(())
}
