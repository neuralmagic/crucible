//! Shared per-cluster kube clients for hub-spoke dispatch.
//!
//! One client per target cluster, built on first use and cloned by every caller
//! (`kube::Client` clones share one connection stack). The hub is the ambient in-cluster
//! identity; spokes are kubeconfigs mounted under a clusters directory:
//!
//! ```text
//! <clusters_dir>/<cluster-name>/kubeconfig
//! ```
//!
//! A spoke kubeconfig must authenticate via `token_file`, not an inline `token`: kube-client
//! re-reads the file at least once a minute, which is required for a projected,
//! kubelet-rotated ServiceAccount token to keep working after rotation. The chart mounts the
//! projected token next to the kubeconfig. The kubeconfig's context namespace is also used as
//! the spoke's loop-pod namespace.

#![allow(clippy::disallowed_macros)]

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// The reserved cluster name for the controller's own cluster.
pub const HUB_CLUSTER: &str = "hub";

/// Resolves a personal dispatch target (`personal:<secret id>`) to its kubeconfig YAML.
///
/// The bytes come out of Vault and are handed straight to `kube`'s in-memory kubeconfig parser —
/// they are never written to the controller's disk, and never leave through a run API. `Ok(None)`
/// is a target that no longer exists, which is how a revocation reads once the registry row is
/// gone.
#[async_trait::async_trait]
pub trait PersonalKubeconfigs: Send + Sync {
    async fn kubeconfig(&self, target: &str) -> Result<Option<String>>;
}

/// The shared client registry; clients are built lazily. Shared via [`std::sync::Arc`].
pub struct ClusterClients {
    clusters_dir: Option<PathBuf>,
    cache: tokio::sync::RwLock<HashMap<String, kube::Client>>,
    /// How a `personal:` target's kubeconfig is fetched. `None` on a deployment with no secrets
    /// registry, where personal targets cannot exist at all.
    personal: Option<Arc<dyn PersonalKubeconfigs>>,
}

impl ClusterClients {
    pub fn new(clusters_dir: Option<PathBuf>) -> Self {
        Self {
            clusters_dir,
            cache: tokio::sync::RwLock::new(HashMap::new()),
            personal: None,
        }
    }

    /// Install the personal-target resolver. Separate from [`ClusterClients::new`] because the
    /// registry it reads needs a database pool and a Vault client, neither of which exists when the
    /// shared clients are first built.
    pub fn with_personal(mut self, personal: Arc<dyn PersonalKubeconfigs>) -> Self {
        self.personal = Some(personal);
        self
    }

    /// Forget a target's cached client, so the next caller rebuilds it. Called when a personal
    /// target is revoked or its kubeconfig rotated: without this, a cached client would keep
    /// working off credentials the registry no longer holds.
    pub async fn evict(&self, cluster: &str) {
        self.cache.write().await.remove(cluster);
    }

    /// The client for `cluster`, building and caching it on first use.
    pub async fn client(&self, cluster: &str) -> Result<kube::Client> {
        if let Some(c) = self.cache.read().await.get(cluster) {
            return Ok(c.clone());
        }
        crate::install_crypto_provider();
        let client = match personal_secret_id(cluster) {
            Some(_) => self.personal_client(cluster).await?,
            None => self
                .target(cluster)?
                .client()
                .await
                .with_context(|| format!("building the kube client for cluster `{cluster}`"))?,
        };
        let mut cache = self.cache.write().await;
        // Two tasks can build concurrently; the first insert wins so all callers share one client.
        Ok(cache.entry(cluster.to_string()).or_insert(client).clone())
    }

    /// Every connected cluster: `hub`, then each clusters-directory entry that carries a
    /// kubeconfig, sorted. Directory presence is the registration — the same rule `target`
    /// resolves by.
    pub fn names(&self) -> Vec<String> {
        let mut names = vec![HUB_CLUSTER.to_string()];
        if let Some(dir) = &self.clusters_dir
            && let Ok(entries) = std::fs::read_dir(dir)
        {
            let mut spokes: Vec<String> = entries
                .flatten()
                .filter(|e| e.path().join("kubeconfig").is_file())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect();
            spokes.sort();
            names.extend(spokes);
        }
        names
    }

    /// Build a client for a personal target from its Vault-held kubeconfig. The YAML is parsed in
    /// memory and the resolved context's namespace becomes the target's namespace, exactly as a
    /// mounted spoke's does — a personal target is scoped to the namespace its owner pointed at,
    /// never to a namespace a launch names.
    async fn personal_client(&self, cluster: &str) -> Result<kube::Client> {
        let Some(resolver) = &self.personal else {
            bail!(
                "dispatch target `{cluster}` is a personal cluster credential, and this deployment \
                 has no secrets registry to resolve one"
            );
        };
        let Some(yaml) = resolver.kubeconfig(cluster).await? else {
            bail!("dispatch target `{cluster}` is no longer registered");
        };
        let parsed = kube::config::Kubeconfig::from_yaml(&yaml)
            .with_context(|| format!("parsing the kubeconfig for `{cluster}`"))?;
        let config = kube::Config::from_custom_kubeconfig(parsed, &Default::default())
            .await
            .with_context(|| format!("resolving the kubeconfig for `{cluster}`"))?;
        kube::Client::try_from(config)
            .with_context(|| format!("building the kube client for `{cluster}`"))
    }

    /// The loop-pod namespace on `cluster`: the hub uses the controller's configured
    /// `pod_namespace`; a spoke uses the namespace declared by its kubeconfig's context.
    pub async fn pod_namespace(&self, cluster: &str, hub_namespace: &str) -> Result<String> {
        if cluster == HUB_CLUSTER {
            return Ok(hub_namespace.to_string());
        }
        Ok(self.client(cluster).await?.default_namespace().to_string())
    }

    fn target(&self, cluster: &str) -> Result<forge::kube::KubeTarget> {
        if cluster == HUB_CLUSTER {
            return Ok(forge::kube::KubeTarget::Ambient);
        }
        let Some(dir) = &self.clusters_dir else {
            bail!(
                "cluster `{cluster}` requested but no clusters directory is configured \
                 (set CONTROLLER_CLUSTERS_DIR)"
            );
        };
        let path = dir.join(cluster).join("kubeconfig");
        if !path.is_file() {
            bail!(
                "cluster `{cluster}` has no kubeconfig at {} — every non-hub cluster needs one \
                 mounted there",
                path.display()
            );
        }
        Ok(forge::kube::KubeTarget::Kubeconfig {
            path: Some(path),
            context: None,
            proxy_url: None,
        })
    }
}

/// The prefix every personal target's name carries. It is not a legal cluster directory entry, so
/// a personal name can never collide with a mounted spoke, and a stored `issues.dispatch_target`
/// says which resolver owns it by inspection.
pub const PERSONAL_PREFIX: &str = "personal:";

/// The personal target name for a registered kubeconfig secret. The secret id, not the secret name:
/// ids are stable across a rename and say nothing about who registered them.
pub fn personal_name(secret_id: &str) -> String {
    format!("{PERSONAL_PREFIX}{secret_id}")
}

/// The secret id inside a personal target name, or `None` for a shared cluster.
pub fn personal_secret_id(target: &str) -> Option<&str> {
    target.strip_prefix(PERSONAL_PREFIX)
}

#[cfg(test)]
mod tests {
    use crate::runs::clusters::*;

    #[tokio::test]
    async fn unknown_cluster_without_dir_errors() {
        let cc = ClusterClients::new(None);
        let err = cc
            .client("wharf")
            .await
            .err()
            .expect("no client")
            .to_string();
        assert!(err.contains("no clusters directory"), "{err}");
    }

    #[tokio::test]
    async fn unknown_cluster_with_dir_names_the_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let cc = ClusterClients::new(Some(dir.path().to_path_buf()));
        let err = cc
            .client("wharf")
            .await
            .err()
            .expect("no client")
            .to_string();
        assert!(err.contains("wharf/kubeconfig"), "{err}");
    }

    #[tokio::test]
    async fn names_lists_hub_then_sorted_spokes_with_kubeconfigs() {
        let dir = tempfile::tempdir().unwrap();
        for spoke in ["wharf", "pike"] {
            let d = dir.path().join(spoke);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("kubeconfig"), "x").unwrap();
        }
        // A directory without a kubeconfig is not a cluster.
        std::fs::create_dir_all(dir.path().join("scratch")).unwrap();
        let cc = ClusterClients::new(Some(dir.path().to_path_buf()));
        assert_eq!(cc.names(), vec!["hub", "pike", "wharf"]);
        assert_eq!(ClusterClients::new(None).names(), vec!["hub"]);
    }

    #[tokio::test]
    async fn hub_namespace_passes_through() {
        let cc = ClusterClients::new(None);
        let ns = cc.pod_namespace(HUB_CLUSTER, "autoresearch").await.unwrap();
        assert_eq!(ns, "autoresearch");
    }

    #[tokio::test]
    async fn spoke_namespace_comes_from_the_kubeconfig_context() {
        let dir = tempfile::tempdir().unwrap();
        let spoke = dir.path().join("wharf");
        std::fs::create_dir_all(&spoke).unwrap();
        // kube-client reads the token file when it builds the client, so the fixture needs real
        // bytes there — the same projected-token file the chart mounts next to the kubeconfig.
        let token = spoke.join("token");
        std::fs::write(&token, "fake-projected-token").unwrap();
        std::fs::write(
            spoke.join("kubeconfig"),
            format!(
                r#"
apiVersion: v1
kind: Config
clusters:
  - name: wharf
    cluster: {{ server: "https://wharf.example:6443" }}
users:
  - name: fed
    user: {{ tokenFile: {} }}
contexts:
  - name: wharf
    context: {{ cluster: wharf, user: fed, namespace: crucible-loops }}
current-context: wharf
"#,
                token.display()
            ),
        )
        .unwrap();
        let cc = ClusterClients::new(Some(dir.path().to_path_buf()));
        let ns = cc.pod_namespace("wharf", "autoresearch").await.unwrap();
        assert_eq!(ns, "crucible-loops");
    }
}
