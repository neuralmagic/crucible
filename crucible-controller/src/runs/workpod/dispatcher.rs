use anyhow::Result;
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Secret, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use std::collections::BTreeSet;
use std::time::Duration;

/// A create the dispatcher has no implementation for.
#[derive(Debug, thiserror::Error)]
pub enum Unsupported {
    #[error("this dispatcher creates no Services")]
    Services,
    #[error("this dispatcher creates no NetworkPolicies")]
    NetworkPolicies,
    #[error("this dispatcher rewrites no Secrets")]
    SecretValues,
}

/// The cluster boundary every work kind is dispatched over: create a pod, wait for it to go terminal,
/// read its logs, delete it. Async end to end — every call is awaited on the daemon's runtime (the
/// trait is `dyn`, so `async_trait` boxes the futures). Dispatch of an [`WorkKind::AgentTurn`] now
/// drives only `create` (non-blocking); the out-of-band COLLECTION (the adopt path + the timeout
/// sweep) drives `await_terminal`/`logs`/`delete`. A [`WorkKind::Run`] likewise only `create`s at
/// launch (collected out-of-band by the run completion edge) and `delete`s on collection. Production is
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
    /// Replace one key of an existing Secret, leaving its other keys as they are. Only the refresh
    /// of a live run's minted token needs it, so the default refuses.
    async fn set_secret_value(
        &self,
        _cluster: &str,
        _namespace: &str,
        _secret: &str,
        _key: &str,
        _value: &str,
    ) -> Result<()> {
        Err(Unsupported::SecretValues.into())
    }
    /// Create a Service, returning it as the API server stored it, its `spec.clusterIP` assigned.
    /// Only an isolated run gateway needs one, so the default refuses.
    async fn create_service(
        &self,
        _cluster: &str,
        _namespace: &str,
        _service: Service,
    ) -> Result<Service> {
        Err(Unsupported::Services.into())
    }
    /// Create a NetworkPolicy. Only an isolated run gateway needs one, so the default refuses.
    async fn create_network_policy(
        &self,
        _cluster: &str,
        _namespace: &str,
        _policy: NetworkPolicy,
    ) -> Result<()> {
        Err(Unsupported::NetworkPolicies.into())
    }
    /// Delete the Pods, Services, Secrets, and NetworkPolicies in `namespace` that match the label
    /// `selector`. Nothing matching is success. The default no-ops.
    async fn delete_selected(
        &self,
        _cluster: &str,
        _namespace: &str,
        _selector: &str,
    ) -> Result<()> {
        Ok(())
    }
    /// Delete the Services in `namespace` that match the label `selector`, and nothing else: a run
    /// gateway leaves only its loop Service in the loop namespace, where the controller may touch
    /// Services alone. Nothing matching is success. The default no-ops.
    async fn delete_services(
        &self,
        _cluster: &str,
        _namespace: &str,
        _selector: &str,
    ) -> Result<()> {
        Ok(())
    }
    /// Delete the OpenShell `Sandbox` resources in `namespace` that match the label `selector`.
    /// Nothing matching is success. The default no-ops.
    async fn delete_sandboxes(
        &self,
        _cluster: &str,
        _namespace: &str,
        _selector: &str,
    ) -> Result<()> {
        Ok(())
    }
    /// The values of `label` on the Pods, Services, Secrets, and NetworkPolicies in `namespace`
    /// that match the label `selector`. The default finds none.
    async fn selected_label_values(
        &self,
        _cluster: &str,
        _namespace: &str,
        _selector: &str,
        _label: &str,
    ) -> Result<BTreeSet<String>> {
        Ok(BTreeSet::new())
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

/// A turn pod's terminal phase, as the dispatcher observed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnPhase {
    Succeeded,
    Failed,
    TimedOut,
}

/// The terminal outcome [`PodDispatcher::await_terminal`] observed: the pod's [`TurnPhase`] plus the
/// kubelet-captured termination message when present. Turn pods are single-container, so `message` is
/// that container's `state.terminated.message`; it is `None` on a timeout, on an OOM/eviction that
/// wrote nothing, or from an old engine image that only emits the marker (the controller then falls
/// back to the marker scrape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalState {
    pub phase: TurnPhase,
    pub message: Option<String>,
}

impl TerminalState {
    /// The kubelet termination envelope, available only for a terminal pod.
    #[cfg(feature = "autoresearch")]
    pub(crate) fn termination_message(&self) -> Option<&str> {
        match self.phase {
            TurnPhase::TimedOut => None,
            TurnPhase::Succeeded | TurnPhase::Failed => self.message.as_deref(),
        }
    }

    /// The current container-waiting reason, available only while the pod is non-terminal.
    pub(crate) fn waiting_detail(&self) -> Option<&str> {
        match self.phase {
            TurnPhase::TimedOut => self.message.as_deref(),
            TurnPhase::Succeeded | TurnPhase::Failed => None,
        }
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
