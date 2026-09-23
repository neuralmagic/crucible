//! The WorkPod dispatch primitive: the controller never runs an agent turn in-process. It
//! dispatches a **controller-owned work pod** per unit of paid agent work — NON-BLOCKING: it creates
//! the pod and returns, so the single serial queue worker never parks on a 5-120 minute turn and the
//! per-kind concurrency cap can actually fan out. Collection is out-of-band: the shared pod-completion
//! watch re-drives the issue on the pod's terminal edge, and the reconcile adopt-first pre-pass scrapes
//! the result, folds it into the ledger (single-booking CAS), and GCs the pod. A hung pod that never
//! terminates is reaped by [`sweep_timed_out_turns`]. Grounded ranking, scope proposal, and loop runs
//! all fold onto this one primitive.
//!
//! The pieces here split by testability, deliberately:
//!   * The **pure core** — [`WorkKind`]/[`WorkPodState`] vocabulary, the turn render options
//!     [`WorkPodSpec::turn_opts`], label/ownerRef [`stamp_pod`], the [`admit`] cap/budget
//!     accounting, the [`parse_verdict_logs`] marker scrape, and the GC decision — is deterministic
//!     and unit-tested below.
//!   * The **cluster boundary** is the [`PodDispatcher`] trait: production is [`KubePodDispatcher`]
//!     (create/watch/logs/delete over the kube API), a test installs a fake. The orchestration
//!     [`dispatch_grounded_rank`] drives the pure core against whatever dispatcher it's handed, so
//!     the accounting + state-machine + fallback logic is exercised without a cluster — the same
//!     boundary discipline the [`PodDispatcher`] draws for every work kind.
//!
//! Both the turn pod and the loop pod are rendered in-process through the linked `crucible`
//! library (`deploy::render_turn`, `deploy::render_yaml`), so the engine's renderers stay the single
//! source of truth for the pod shape.
//!
//! [`markers`] holds the marker/timeout constants, [`core`] the pure vocabulary + admission + pod
//! spec described above, [`scrape`] the log/marker scraping, [`dispatcher`] the [`PodDispatcher`]
//! trait boundary, [`run`] the `WorkKind::Run` section (the full autoresearch loop pod folded onto
//! the primitive), [`kube_dispatcher`] the [`KubePodDispatcher`], [`sweep`] the failed/timed-out
//! pod GC, and [`globals`] the installable dispatcher/enqueue statics.

mod core;
mod dispatcher;
mod globals;
mod kube_dispatcher;
mod markers;
pub(crate) mod retry;
mod run;
mod scrape;
#[cfg(feature = "autoresearch")]
mod spec;
mod sweep;
#[cfg(test)]
mod tests;
mod trace;

/// The cluster this issue's next pod dispatches onto, in precedence order: the target its launch
/// pinned, else the cluster its GPU-measured contract routes to, else the controller's configured
/// default. Resolved at each dispatch site rather than carried on the spec, so repointing the
/// default moves everything that never asked for a specific cluster.
pub(crate) async fn issue_dispatch_cluster(
    db: &crate::client::Db,
    cfg: &crate::config::ControllerCfg,
    issue_key: &str,
) -> anyhow::Result<String> {
    let routing = crate::issues::store::dispatch_target(db.pool(), issue_key).await?;
    if let Some(target) = routing.target {
        return Ok(target);
    }
    if let Some(contract) = routing.contract
        && let Some(cluster) = cfg.contract_clusters()?.get(&contract)
    {
        return Ok(cluster.clone());
    }
    Ok(cfg.dispatch_cluster.clone())
}

pub(crate) use core::*;
#[cfg(feature = "autoresearch")]
pub(crate) use dispatcher::*;
pub(crate) use globals::*;
pub(crate) use markers::*;
#[cfg(feature = "autoresearch")]
pub(crate) use run::*;
pub(crate) use scrape::*;
pub(crate) use sweep::*;

pub use core::{WorkKind, WorkPodState};
pub use dispatcher::PodDispatcher;
pub use globals::{
    install_contracts, install_dispatcher, install_enqueue, reset_contracts, reset_dispatcher,
    reset_enqueue,
};
pub use kube_dispatcher::{KubePodDispatcher, reconcile_on_startup};
pub use run::{
    LaunchSecrets, RunAdmission, RunDisposition, RunRenderOpts, collect_run_pod, dispatch_run,
    render_run_docs, run_pod_name, stamp_run_pod,
};
#[cfg(feature = "autoresearch")]
pub use run::{ScopeOutcome, dispatch_scope};
pub use scrape::TerminalState;
