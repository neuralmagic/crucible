//! The boundaries the daemon assembles and the reconcile chain reads off its config: where work
//! pods are created, what admits an engine, what runs a declared build, and the queue a freed
//! slot re-drives.

use crate::authz::policy::{ActivePolicy, DEFAULT_POLICY, Engine, PolicyError};
use crate::daemon::queue::WorkQueue;
use crate::runs::contract::{AdmitFailure, ContractRegistry, DispatchTarget, RequestKind};
use crate::runs::workpod::dispatcher::PodDispatcher;
use crate::runs::workpod::kube_dispatcher::KubePodDispatcher;
use std::sync::Arc;

#[derive(Clone)]
pub struct Runtime {
    /// Creates, watches, and deletes work pods on whichever cluster a row names.
    pub dispatcher: Arc<dyn PodDispatcher>,
    /// The engine contract gate. `None` admits every target: a test, or a command that never
    /// dispatches.
    pub contracts: Option<Arc<ContractRegistry>>,
    /// What runs a declared `[build]`; the not-installed backend refuses with a typed reason.
    #[cfg(feature = "autoresearch")]
    pub builds: Arc<dyn crate::builds::backend::BuildBackend>,
    /// The reconcile queue a collection re-drives a queued issue through once it frees a slot.
    /// `None` leaves the backlog to the timeout sweep and the next pass.
    pub queue: Option<WorkQueue>,
    /// The policy set in force, shared with the API so an activation reaches dispatch-time
    /// decisions. `None` decides with the shipped default: a test, or a command that never
    /// dispatches.
    pub policy: Option<ActivePolicy>,
}

impl Default for Runtime {
    /// A hub-only dispatcher with no spoke credentials, no contract gate, no build backend, and no
    /// queue.
    fn default() -> Self {
        Runtime {
            dispatcher: Arc::new(KubePodDispatcher::new(Arc::new(
                crate::runs::clusters::ClusterClients::new(None),
            ))),
            contracts: None,
            #[cfg(feature = "autoresearch")]
            builds: Arc::new(crate::builds::backend::StubBuildBackend),
            queue: None,
            policy: None,
        }
    }
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("contracts", &self.contracts.is_some())
            .field("queue", &self.queue.is_some())
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl Runtime {
    /// Gate one launch: `Ok` when no registry is installed or every target matches this
    /// controller; the first failure otherwise.
    pub(crate) async fn admit_contract(
        &self,
        request: RequestKind,
        targets: &[DispatchTarget],
    ) -> Result<(), AdmitFailure> {
        match &self.contracts {
            Some(registry) => registry.admit_all(request, targets).await,
            None => Ok(()),
        }
    }

    /// The engine a decision made off a request is taken with, such as a dispatch resolving the
    /// secrets its scope binds.
    pub(crate) fn policy(&self) -> Result<Arc<Engine>, PolicyError> {
        match &self.policy {
            Some(active) => Ok(active.current()),
            None => Engine::load(DEFAULT_POLICY).map(Arc::new),
        }
    }

    /// The engine version the registry last recorded for `target`, or `None` without a registry.
    /// Names an engine in a refusal; the gate itself is [`Runtime::admit_contract`].
    #[cfg(feature = "autoresearch")]
    pub(crate) async fn recorded_engine_version(&self, target: &DispatchTarget) -> Option<String> {
        let registry = self.contracts.as_ref()?;
        Some(registry.record(target).await.engine_version)
    }
}
