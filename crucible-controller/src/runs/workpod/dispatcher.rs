use crate::runs::workpod::*;
#[cfg(feature = "autoresearch")]
use anyhow::Context;
use anyhow::Result;
#[cfg(feature = "autoresearch")]
use crucible::deploy::{DeployProfile, DigestResolver, render_turn};
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Secret};
#[cfg(feature = "autoresearch")]
use std::path::Path;
#[cfg(feature = "autoresearch")]
use std::sync::Arc;
use std::time::Duration;

/// The cluster boundary every work kind is dispatched over: create a pod, wait for it to go terminal,
/// read its logs, delete it. Async end to end — every call is awaited on the daemon's runtime (the
/// trait is `dyn`, so `async_trait` boxes the futures). Dispatch of an [`WorkKind::AgentTurn`] now
/// drives only `create` (non-blocking); the out-of-band COLLECTION (the adopt path + the timeout
/// sweep) drives `await_terminal`/`logs`/`delete`. A [`WorkKind::Run`] likewise only `create`s at
/// launch (watched out-of-band by the shared pod watch) and `delete`s on collection. Production is
/// [`KubePodDispatcher`]; a test installs a fake so the accounting + state machine run without a cluster.
#[async_trait::async_trait]
pub trait PodDispatcher: Send + Sync {
    /// Create the pod, returning it as the API server stored it — its `metadata.uid` is populated, so
    /// the caller can owner-ref a dependent object (the pack ConfigMap) to it for k8s cascade GC.
    async fn create(&self, cluster: &str, namespace: &str, pod: Pod) -> Result<Pod>;
    /// Create a ConfigMap (the pack payload a loop-run pod mounts). Separate from [`Self::create`] so
    /// only the run path pays for it; the default no-ops so a fake that never dispatches a run needn't
    /// implement it. Production ([`KubePodDispatcher`]) overrides it with a real kube create.
    async fn create_configmap(
        &self,
        _cluster: &str,
        _namespace: &str,
        _cm: ConfigMap,
    ) -> Result<()> {
        Ok(())
    }
    /// Create the run's Secret (the bound values a pod mounts, see [`ADR-0036`]). Same shape as
    /// [`Self::create_configmap`]: created after the pod so it can be owner-referenced to it, and
    /// collected by the cluster when the pod goes.
    async fn create_secret(&self, _cluster: &str, _namespace: &str, _secret: Secret) -> Result<()> {
        Ok(())
    }
    async fn await_terminal(
        &self,
        cluster: &str,
        namespace: &str,
        name: &str,
        timeout: Duration,
    ) -> Result<TerminalState>;
    async fn logs(&self, cluster: &str, namespace: &str, name: &str) -> Result<String>;
    async fn delete(&self, cluster: &str, namespace: &str, name: &str) -> Result<()>;
    /// The loop-pod namespace on `cluster`: the hub uses the caller's configured namespace, a
    /// spoke the namespace declared by its kubeconfig's context. The default returns
    /// `hub_namespace`, so fakes that only dispatch to the hub need no override; production
    /// delegates to the cluster registry.
    async fn pod_namespace(&self, _cluster: &str, hub_namespace: &str) -> Result<String> {
        Ok(hub_namespace.to_string())
    }
}

/// Whether an orphaned turn pod can be adopted NOW without blocking the single queue worker: already
/// terminal (collect the result), or gone (adopt anyway — the collection records the lost turn so the
/// row never wedges at `running`). A still-running pod says no: dispatch defers it (a `Launched`
/// outcome) and the completion watch re-drives on its terminal edge. The 1s poll is the whole point —
/// it never parks the worker on a live 5-120 minute turn. Shared by the reconcile adopt-first pre-pass
/// and the dispatch-side adopt guard.
///
/// A transient or auth-rejected API failure also defers adoption: an unanswered call does not
/// establish that the pod is gone, and adopting then would record a possibly-live turn as lost.
/// Only a definitive 404 (or a non-kube error, matching pre-existing behavior) adopts.
#[cfg(feature = "autoresearch")]
pub(crate) async fn turn_pod_adoptable(
    dispatcher: &dyn PodDispatcher,
    cluster: &str,
    namespace: &str,
    pod_name: &str,
) -> bool {
    match dispatcher
        .await_terminal(cluster, namespace, pod_name, Duration::from_secs(1))
        .await
    {
        Ok(t) => t.phase != TurnPhase::TimedOut,
        Err(e) => !matches!(
            crate::runs::workpod::retry::classify_chain(&e),
            Some(crate::runs::workpod::retry::KubeFailure::Transient)
                | Some(crate::runs::workpod::retry::KubeFailure::AuthRejected(_))
        ),
    }
}

/// Render one turn pod through the linked engine (`crucible::deploy::render_turn`, the library form
/// of `deploy render-turn`), returning the parsed, unstamped [`Pod`]. Blocking (profile read, image
/// pinning), so the caller runs it under `spawn_blocking`.
#[cfg(feature = "autoresearch")]
pub(crate) fn render_turn_pod(
    spec: &WorkPodSpec,
    profile_path: &Path,
    digests: Option<Arc<dyn DigestResolver>>,
) -> Result<Pod> {
    let opts = spec.turn_opts(digests)?;
    let profile = DeployProfile::load(profile_path)?;
    let yaml = render_turn(&profile, &opts)?;
    serde_norway::from_str::<Pod>(&yaml).context("decoding the rendered turn Pod")
}
