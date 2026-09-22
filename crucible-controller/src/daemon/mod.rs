//! The resident shell: `select!`s a discovery timer against a kube pod watch,
//! both of which only ever call [`Enqueue::enqueue`] (sources never touch the
//! database or act directly), and drives the [`WorkQueue`] worker loop alongside them.
//!
//! `crucible-controller autopilot` (no `--once`) builds a runtime, assembles the [`Wiring`], and calls
//! [`run`]; `--once` ([`crate::daemon::autopilot::run_once`]) enqueues non-terminal rows and drains
//! without a resident loop — same reconcile core, no sources needed.

#![allow(clippy::disallowed_macros)]

pub(crate) mod api;
pub mod autopilot;
#[cfg(feature = "autoresearch")]
pub mod autopilot_flag;
pub mod import_sqlite;
pub mod leader;
pub mod overrides;
pub mod overrides_store;
pub mod queue;
pub mod rebuild;
pub mod store;

use crate::daemon::queue::DiscoverySource;
use crate::daemon::queue::TestSyncMarker;
use crate::daemon::queue::{
    BoxFuture, Enqueue, IssueKey, ParkFn, QueueConfig, ReconcileFn, WorkQueue,
};
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Pod;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{MissedTickBehavior, interval};

/// A stream of completed-loop-pod issue keys the daemon enqueues (the pod-completion edge). Boxed so
/// `run` doesn't name a concrete stream type: production is [`kube_completion_stream`], the test
/// harness feeds a channel-backed one so the daemon runs for real without a cluster.
pub type CompletionStream = Pin<Box<dyn futures_util::Stream<Item = IssueKey> + Send>>;

/// The periodic drift check: rebuild into a temp DB and diff it against the live one. `run`
/// invokes it on its own long interval and logs the outcome; the callable itself (`verify`)
/// lives in [`crate::daemon::rebuild`].
pub type VerifyFn = Arc<dyn Fn() -> BoxFuture<Result<()>> + Send + Sync>;

/// A live fetch of the non-terminal issue keys (`Db::non_terminal_keys`), for the manual
/// reconcile-now pass — the startup re-enqueue's semantics, re-runnable at trigger time.
pub type NonTerminalFn = Arc<dyn Fn() -> BoxFuture<Result<Vec<String>>> + Send + Sync>;

/// Everything [`run`] plugs into its `select!` loop beyond the queue config: the reconcile core, the
/// park callback, the discovery sources, the pod-completion stream, and the optional drift-verify
/// hook. Bundled into one struct so `run` stays a five-parameter call as the wiring grows.
pub struct Wiring {
    /// The work queue the worker drains. Passed in (not created inside [`run`]) so the caller can
    /// also hand its [`Enqueue`] handle to the override sink — human overrides enqueue into the very
    /// queue the worker serves, composing with the sources instead of a second, disconnected one.
    queue: WorkQueue,
    reconcile: ReconcileFn,
    park: ParkFn,
    discovery: Arc<dyn DiscoverySource>,
    completions: CompletionStream,
    verify: Option<VerifyFn>,
    /// Fresh non-terminal keys for the manual reconcile-now pass (the API's kick-it-now button):
    /// the pass runs a discovery poll AND a full re-enqueue, so stuck rows re-drive immediately
    /// instead of waiting out the cadence.
    reenqueue_all: NonTerminalFn,
}

/// The label the launch path sets on every loop pod it renders, mapping a pod back to
/// the issue key that spawned it. Recorded here as the source of truth for the launch path to
/// consume.
pub(crate) const ISSUE_KEY_LABEL: &str = "crucible.dev/issue-key";
/// The exact issue key (`owner/repo#N`, verbatim) — annotation because label values can't hold
/// `/` or `#`. The launch path stamps both; the watch reads this one.
pub const ISSUE_KEY_ANNOTATION: &str = "crucible.dev/issue-key";
/// The controller-minted run id a loop-run pod carries (`crate::runs::workpod::stamp_run_pod` stamps it),
/// mapping the pod back to its `runs` row + stored session. Distinct from the work-pod name.
pub const RUN_ID_ANNOTATION: &str = "crucible.dev/run-id";

/// The selector every rendered loop pod carries. The shared `key=value` literal lives in
/// `crucible-contract` (the `crucible` binary renders pods with it, this daemon watches on it).
pub use crucible_contract::MANAGED_BY_SELECTOR;

/// Assemble the production [`Wiring`] from an open ledger + config: the reconcile core with the
/// human-override drain in front, the machine park callback, the three approval polls + upstream
/// watermark as one [`MultiDiscovery`], and the scheduled drift check. The binary's daemon and the
/// integration harness both call this, so the daemon under test IS the assembled daemon — the
/// harness substitutes only the two cluster edges ([`CompletionStream`] and the
/// [`crate::runs::workpod::PodDispatcher`]).
pub fn assemble(
    db: &crate::client::Db,
    cfg: &crate::config::ControllerCfg,
    queue: WorkQueue,
    overrides: Arc<crate::daemon::overrides::OverrideStore>,
    completions: CompletionStream,
    policy: crate::authz::policy::ActivePolicy,
) -> Wiring {
    // Install the process-global queue handle so a collection that frees a slot can re-drive a
    // queued backlog row's issue back through reconcile (the drain can't thread a handle through the
    // frozen reconcile signature — same rationale as the dispatcher global). Assembly is the one
    // shared production + harness build point, so both get the real queue.
    crate::runs::workpod::install_enqueue(Arc::new(queue.clone()));

    // A failed reconcile that exhausts its retry budget parks the issue with the last error as the
    // reason (machine park — a human sweep or an upstream change can revive it).
    // `park_issue` is async; the `ParkFn` boundary is sync, so it's spawned.
    let park_db = db.clone();
    let park: ParkFn = Arc::new(move |key: IssueKey, reason: String| {
        let db = park_db.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::issues::transitions::park_and_purge(
                db.pool(),
                &key.0,
                &reason,
                crate::model::ParkedBy::Machine,
            )
            .await
            {
                tracing::error!(issue_key = %key.0, error = format!("{e:#}"), "daemon: failed to park issue");
            }
        });
    });

    // The reconcile core with the pending human override drained first, so an
    // operator's park/unpark/bump composes with claims instead of racing them.
    let rec_db = db.clone();
    let rec_cfg = cfg.clone();
    let reconcile: ReconcileFn = Arc::new(move |key: IssueKey| {
        let db = rec_db.clone();
        let cfg = rec_cfg.clone();
        let store = overrides.clone();
        Box::pin(async move {
            crate::daemon::overrides::apply_pending(&db, &store, &key.0).await?;
            crate::issues::reconcile::reconcile(&db, &cfg, &key.0).await
        })
    });

    // The fire-time group refresh's credential. A deployment with no issuer or no mounted key runs
    // the sweep exactly as it did before: on the schedule-row snapshot alone.
    let owner_refresh =
        match crate::identity::oidc::credentials::OwnerRefresh::from_env(db.pool().clone()) {
            Ok(refresh) => refresh,
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "schedules: no fire-time group refresh");
                None
            }
        };
    #[allow(unused_mut)]
    let mut sources: Vec<Arc<dyn DiscoverySource>> = vec![
        // The only source that writes: every launch trigger (a due one-shot, a due schedule, a
        // watch hit) claims its row and mints the launch in one transaction, then enqueues the key.
        Arc::new(crate::launches::standing::TriggerSweep::new(
            db.clone(),
            vec![
                Arc::new(crate::launches::one_shots::OneShotTrigger),
                Arc::new(crate::launches::schedules::ScheduleTrigger::new(
                    db.clone(),
                    cfg.overrides.clone(),
                )),
                Arc::new(crate::launches::watches::WatchTrigger::new(
                    crate::launches::jira::trackers(cfg.jira_config()),
                )),
            ],
            cfg.schedule_auto_disable_failures,
            std::time::Duration::from_secs(cfg.schedule_owner_ttl_secs),
            owner_refresh,
            Some(policy),
        )),
    ];
    #[cfg(feature = "autoresearch")]
    if cfg.autoresearch_enabled() {
        // The approvals: the upstream watermark poll re-drives changed rows through reconcile (so
        // the staleness + upstream-close edges fire), the `upstream_updated_at` backfill stamps
        // pre-migration-0007 rows (self-quiescing — zero GitHub calls once none are NULL), the
        // closed-upstream repair retires contaminated rows (self-quiescing per repo via
        // `closed_repaired_at`), the approval poll flips `awaiting-approval` rows on the approval
        // signal, and the review-comment poll reseeds the next run. All no-op without their config
        // (repos / `CONTROLLER_PACK_REPO` / `CONTROLLER_APPROVERS` / GitHub creds).
        let authz = crate::issues::github::Authz::from_env();
        let bot_user = std::env::var("CONTROLLER_BOT_USER").unwrap_or_default();
        let autoresearch: [Arc<dyn DiscoverySource>; 6] = [
            Arc::new(crate::issues::approvals::UpstreamPoll::new(db.clone())),
            Arc::new(crate::issues::approvals::BackfillPoll::new(db.clone())),
            Arc::new(crate::issues::approvals::ClosedRepairPoll::new(db.clone())),
            Arc::new(crate::issues::approvals::ApprovalPoll::new(
                db.clone(),
                cfg.clone(),
                authz.clone(),
            )),
            Arc::new(crate::issues::approvals::ReviewCommentPoll::new(
                db.clone(),
                authz,
                bot_user,
            )),
            // Out-of-band turn-deadline enforcement: non-blocking dispatch no longer awaits a turn, so a
            // hung pod is reaped here (and its issue re-driven) instead of wedging its row forever.
            Arc::new(crate::runs::workpod::TurnTimeoutPoll::new(
                db.clone(),
                cfg.clone(),
            )),
        ];
        sources.extend(autoresearch);
    }
    let discovery: Arc<dyn DiscoverySource> = Arc::new(MultiDiscovery::new(sources));

    // The periodic drift check: rebuild into a temp DB and diff against the live one.
    // Best-effort; a divergence is reported, never fatal.
    let verify_db = db.clone();
    let verify_cfg = cfg.clone();
    let verify: VerifyFn = Arc::new(move || {
        let db = verify_db.clone();
        let cfg = verify_cfg.clone();
        Box::pin(async move {
            let report = crate::daemon::rebuild::verify(&db, &cfg).await?;
            if !report.is_clean() {
                tracing::warn!(
                    divergences = report.diffs.len() + report.gaps.len(),
                    "daemon: drift check found divergences from evidence"
                );
            }
            Ok(())
        })
    });

    let reenq_db = db.clone();
    let reenqueue_all: NonTerminalFn = Arc::new(move || {
        let db = reenq_db.clone();
        Box::pin(async move { crate::issues::store::non_terminal_keys(db.pool()).await })
    });

    Wiring {
        queue,
        reconcile,
        park,
        discovery,
        completions,
        verify: Some(verify),
        reenqueue_all,
    }
}

/// Poll several sources in sequence on each timer tick (the upstream watermark, the approval gate,
/// the review-comment reseed). A source's error is logged and the rest still run — one flaky forge
/// call must not starve the others.
pub struct MultiDiscovery {
    sources: Vec<Arc<dyn DiscoverySource>>,
}

impl MultiDiscovery {
    fn new(sources: Vec<Arc<dyn DiscoverySource>>) -> Self {
        MultiDiscovery { sources }
    }
}

impl DiscoverySource for MultiDiscovery {
    fn poll(&self, enqueue: Arc<dyn Enqueue>) -> BoxFuture<Result<()>> {
        let sources = self.sources.clone();
        Box::pin(async move {
            for src in &sources {
                if let Err(e) = src.poll(enqueue.clone()).await {
                    tracing::warn!(
                        error = format!("{e:#}"),
                        "daemon: a discovery source failed (continuing)"
                    );
                }
            }
            Ok(())
        })
    }
}

/// A live source of the current discovery cadence (Lane O2): a runtime override can retune it
/// without a restart, so the loop re-reads it each tick rather than freezing the startup value.
pub type DiscoveryIntervalFn = Arc<dyn Fn() -> Duration + Send + Sync>;

/// Everything [`run`] needs beyond the queue/discovery/reconcile plumbing.
pub struct DaemonConfig {
    pub queue: QueueConfig,
    /// The initial discovery cadence (the first timer period).
    pub discovery_interval: Duration,
    /// A live source of the current discovery cadence, re-read each tick so a runtime override to
    /// `discovery_secs` takes effect without a restart. `None` keeps `discovery_interval` fixed.
    pub discovery_interval_fn: Option<DiscoveryIntervalFn>,
    /// How often the drift check ([`Wiring::verify`]) runs — a long, staggered cadence so the
    /// rebuild-and-diff never competes with discovery. Ignored when no verify hook is wired.
    pub verify_interval: Duration,
    /// The manual reconcile trigger (`POST /api/reconcile` fires it with `notify_one`). Each fire
    /// runs one discovery poll plus a full non-terminal re-enqueue; the permit semantics coalesce
    /// clicks that land while a pass is running. `None` (tests without an API) never fires.
    pub reconcile_now: Option<Arc<tokio::sync::Notify>>,
}

/// Project a watched `Pod` event into the issue key to reconcile, or `None` if it isn't a
/// completion (still running) or carries no [`ISSUE_KEY_ANNOTATION`]. Pure and unit-testable
/// without a cluster — the same discipline `crucible ps`'s `pod_to_row` uses.
///
/// The exact key rides an ANNOTATION, not the label: real keys (`owner/repo#42`) contain
/// characters that are illegal in label values, so the label carries only a lossy sanitized
/// hint for humans/selectors and can never round-trip. Annotation values are unrestricted.
fn pod_completion_key(pod: &Pod) -> Option<IssueKey> {
    let phase = pod.status.as_ref()?.phase.as_deref()?;
    if phase != "Succeeded" && phase != "Failed" {
        return None;
    }
    let key = pod
        .metadata
        .annotations
        .as_ref()?
        .get(ISSUE_KEY_ANNOTATION)?;
    Some(IssueKey(key.clone()))
}

/// One cluster's completion watch: an endless stream of that cluster's completed pods' issue keys.
pub type ClusterCompletions = Pin<Box<dyn futures_util::Stream<Item = IssueKey> + Send>>;

/// The cluster edge of the completion watch: which clusters to watch, and how to open one of them.
/// Production is [`KubeWatchSource`] (a namespaced kube pod watch per cluster); tests substitute
/// channel-backed streams so the supervisor runs for real without a cluster.
pub trait ClusterWatchSource: Send + Sync + 'static {
    /// The clusters that should currently be watched. Re-read on every rescan, so a spoke
    /// registered after boot is picked up without a restart.
    fn clusters(&self) -> Vec<String>;
    /// Open one cluster's watch. An `Err` leaves that cluster unwatched until the next rescan.
    fn open(&self, cluster: String) -> BoxFuture<Result<ClusterCompletions>>;
}

/// How often [`watch_completions`] re-reads the cluster set and retries the clusters it has no
/// live watch on.
pub const CLUSTER_RESCAN_INTERVAL: Duration = Duration::from_secs(60);

/// The production [`ClusterWatchSource`]: every cluster the controller dispatches to, each watched
/// in its own loop-pod namespace.
pub struct KubeWatchSource {
    clusters: Arc<crate::runs::clusters::ClusterClients>,
    hub_namespace: String,
}

impl KubeWatchSource {
    pub fn new(
        clusters: Arc<crate::runs::clusters::ClusterClients>,
        hub_namespace: String,
    ) -> Self {
        KubeWatchSource {
            clusters,
            hub_namespace,
        }
    }
}

impl ClusterWatchSource for KubeWatchSource {
    fn clusters(&self) -> Vec<String> {
        self.clusters.names()
    }

    fn open(&self, cluster: String) -> BoxFuture<Result<ClusterCompletions>> {
        let clusters = self.clusters.clone();
        let hub_namespace = self.hub_namespace.clone();
        Box::pin(async move {
            let client = clusters
                .client(&cluster)
                .await
                .with_context(|| format!("connecting to cluster `{cluster}` for the pod watch"))?;
            let namespace = clusters
                .pod_namespace(&cluster, &hub_namespace)
                .await
                .with_context(|| format!("resolving the loop namespace on cluster `{cluster}`"))?;
            Ok(pod_completion_watch(client, &namespace, &cluster))
        })
    }
}

/// A namespaced pod watch over [`MANAGED_BY_SELECTOR`], mapped to completed pods' issue keys via
/// [`pod_completion_key`]. Endless: `default_backoff` reconnects internally, so a watch error is
/// logged with its cluster and the stream continues.
fn pod_completion_watch(
    client: kube::Client,
    namespace: &str,
    cluster: &str,
) -> ClusterCompletions {
    use futures_util::StreamExt;
    use kube::Api;
    use kube::runtime::{WatchStreamExt, watcher};

    let api: Api<Pod> = Api::namespaced(client, namespace);
    let cfg = watcher::Config::default().labels(MANAGED_BY_SELECTOR);
    let cluster = cluster.to_string();
    Box::pin(
        watcher(api, cfg)
            .default_backoff()
            .applied_objects()
            .filter_map(move |res| {
                let cluster = cluster.clone();
                async move {
                    match res {
                        Ok(pod) => pod_completion_key(&pod),
                        Err(e) => {
                            tracing::warn!(
                                %cluster,
                                error = format!("{e:#}"),
                                "daemon: pod watch error (reconnecting)"
                            );
                            None
                        }
                    }
                }
            }),
    )
}

/// The production [`CompletionStream`] the binary hands [`run`]: one pod watch per cluster the
/// controller dispatches to, merged, so a spoke turn's terminal edge re-drives its issue exactly
/// like a hub turn's.
pub fn kube_completion_stream(
    clusters: Arc<crate::runs::clusters::ClusterClients>,
    hub_namespace: &str,
) -> CompletionStream {
    watch_completions(
        Arc::new(KubeWatchSource::new(clusters, hub_namespace.to_string())),
        CLUSTER_RESCAN_INTERVAL,
    )
}

/// One cluster's completion stream tagged with the cluster it came from.
type TaggedCompletions = Pin<Box<dyn futures_util::Stream<Item = (String, ClusterEvent)> + Send>>;

/// What one watched cluster's tagged stream yields.
enum ClusterEvent {
    Completed(IssueKey),
    /// The cluster's watch stream finished; the supervisor reopens it on the next rescan.
    Ended,
}

/// Merge every cluster's completion watch into one [`CompletionStream`], rescanning on `rescan` so
/// a cluster that is unregistered-at-boot, unreachable, or whose watch ended is picked up (or
/// retried) without a restart. Per-cluster failures are isolated: opening one cluster's watch can
/// fail without disturbing the others.
pub fn watch_completions(
    source: Arc<dyn ClusterWatchSource>,
    rescan: Duration,
) -> CompletionStream {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(supervise_watches(source, rescan, tx));
    Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|key| (key, rx))
    }))
}

/// The supervisor behind [`watch_completions`]. Exits when the receiver is dropped.
async fn supervise_watches(
    source: Arc<dyn ClusterWatchSource>,
    rescan: Duration,
    tx: tokio::sync::mpsc::Sender<IssueKey>,
) {
    use futures_util::StreamExt;
    use std::collections::HashSet;

    let mut merged: futures_util::stream::SelectAll<TaggedCompletions> =
        futures_util::stream::SelectAll::new();
    let mut watched: HashSet<String> = HashSet::new();
    let mut failing: HashSet<String> = HashSet::new();
    let mut rescan_timer = interval(rescan.max(Duration::from_millis(1)));
    rescan_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = rescan_timer.tick() => {
                for cluster in source.clusters() {
                    if watched.contains(&cluster) {
                        continue;
                    }
                    match source.open(cluster.clone()).await {
                        Ok(stream) => {
                            if failing.remove(&cluster) {
                                tracing::info!(%cluster, "daemon: pod-completion watch recovered");
                            }
                            tracing::info!(%cluster, "daemon: watching pod completions");
                            watched.insert(cluster.clone());
                            merged.push(tag_cluster(cluster, stream));
                        }
                        Err(e) => {
                            if failing.insert(cluster.clone()) {
                                tracing::warn!(
                                    %cluster,
                                    error = format!("{e:#}"),
                                    "daemon: cannot open the pod-completion watch; retrying on the next rescan"
                                );
                            }
                        }
                    }
                }
            }
            Some((cluster, event)) = merged.next(), if !merged.is_empty() => {
                match event {
                    ClusterEvent::Completed(key) => {
                        if tx.send(key).await.is_err() {
                            return;
                        }
                    }
                    ClusterEvent::Ended => {
                        watched.remove(&cluster);
                        tracing::warn!(
                            %cluster,
                            "daemon: pod-completion watch ended; reopening on the next rescan"
                        );
                    }
                }
            }
            _ = tx.closed() => return,
        }
    }
}

/// Tag one cluster's keys with its name and append the [`ClusterEvent::Ended`] sentinel, so the
/// supervisor learns which cluster's watch it has to reopen.
fn tag_cluster(cluster: String, stream: ClusterCompletions) -> TaggedCompletions {
    use futures_util::StreamExt;
    let ended = cluster.clone();
    Box::pin(
        stream
            .map(move |key| (cluster.clone(), ClusterEvent::Completed(key)))
            .chain(futures_util::stream::once(async move {
                (ended, ClusterEvent::Ended)
            })),
    )
}

/// The resident daemon loop. Fetches `non_terminal_keys` from `db` for the startup re-enqueue,
/// then `select!`s the discovery timer against the pod watch until `shutdown` fires, driving the
/// queue's worker loop alongside them. Every source only ever calls `queue.enqueue`.
///
/// `sync` is `None` in production; tests pass a [`TestSyncMarker`] to await real drain instead of
/// sleeping. `shutdown` lets tests (and, eventually, a signal handler) request a graceful stop —
/// dropping sources, draining the queue, and returning once the worker has joined.
pub async fn run(
    non_terminal_keys: Vec<String>,
    cfg: DaemonConfig,
    wiring: Wiring,
    shutdown: Arc<tokio::sync::Notify>,
    sync: Option<Arc<TestSyncMarker>>,
) -> Result<()> {
    run_led(non_terminal_keys, cfg, wiring, shutdown, sync, None).await
}

/// [`run`] under leadership (ADR-0049 §1): the daemon holds the advisory-lock fence and the
/// lease through `leadership`, and losing either is an immediate step-down — the loop returns
/// an error, the process exits, and the kubelet restart rejoins as a standby.
pub async fn run_led(
    non_terminal_keys: Vec<String>,
    cfg: DaemonConfig,
    wiring: Wiring,
    shutdown: Arc<tokio::sync::Notify>,
    sync: Option<Arc<TestSyncMarker>>,
    leadership: Option<crate::daemon::leader::Leadership>,
) -> Result<()> {
    let Wiring {
        queue,
        reconcile,
        park,
        discovery,
        mut completions,
        verify,
        reenqueue_all,
    } = wiring;

    queue.reenqueue_startup(non_terminal_keys);

    let worker_queue = queue.clone();
    let worker = tokio::spawn(async move {
        worker_queue.run(reconcile, park, cfg.queue, sync).await;
    });

    let mut discovery_period = cfg.discovery_interval;
    let discovery_interval_fn = cfg.discovery_interval_fn.clone();
    let mut discovery_timer = interval(discovery_period);
    discovery_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // Stagger the first tick so a fleet of daemons (or several sources within one) don't all
    // fire together and hammer the same endpoints at once.
    discovery_timer.reset_after(stagger_offset(discovery_period));

    // The drift check runs on its own long, heavily-staggered cadence so a rebuild-and-diff never
    // lands on a discovery tick. Its first fire is delayed a full interval — a fresh daemon has
    // nothing to have drifted from yet.
    let mut verify_timer = interval(cfg.verify_interval);
    verify_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    verify_timer.reset_after(cfg.verify_interval);

    let enqueue: Arc<dyn Enqueue> = Arc::new(queue.clone());

    // One pinned, eagerly-enabled shutdown future for the whole loop. A fresh `notified()` per
    // select iteration loses any `notify_waiters` that fires while an arm body runs (a discovery
    // poll can occupy most of the tick on a slow machine) — the daemon would then wait forever.
    let shutdown_signal = shutdown.notified();
    tokio::pin!(shutdown_signal);
    shutdown_signal.as_mut().enable();

    // Leadership loss is a select source like any other; `pending()` when the daemon runs
    // unelected (tests, `--once`, local dev without a cluster).
    let mut leadership = leadership;
    let stepped_down = async {
        match leadership.as_mut() {
            Some(l) => l.lost().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(stepped_down);

    loop {
        tokio::select! {
            reason = stepped_down.as_mut() => {
                shutdown.notify_waiters();
                queue.shut_down();
                let _ = worker.await;
                anyhow::bail!("stepping down: {reason}");
            }
            _ = discovery_timer.tick() => {
                discovery.poll(enqueue.clone()).await.context("discovery poll")?;
                // Re-read the effective cadence: a runtime override to `discovery_secs` retunes the
                // timer live (rebuilt only when the period actually changed, so a stable config is
                // a cheap comparison).
                if let Some(f) = &discovery_interval_fn {
                    let want = f();
                    if want != discovery_period && !want.is_zero() {
                        discovery_period = want;
                        discovery_timer = interval(discovery_period);
                        discovery_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
                        discovery_timer.reset_after(discovery_period);
                    }
                }
            }
            _ = verify_timer.tick(), if verify.is_some() => {
                if let Some(v) = &verify
                    && let Err(e) = v().await {
                        tracing::warn!(error = format!("{e:#}"), "daemon: scheduled drift check failed (continuing)");
                    }
            }
            maybe_key = pod_next(&mut completions) => {
                if let Some(key) = maybe_key {
                    queue.enqueue(key);
                }
            }
            // The manual pass: a discovery poll (what a tick does) plus a full non-terminal
            // re-enqueue (what startup does), so stuck rows re-drive without waiting out the
            // cadence. Best-effort — a button press must never take the daemon down. A fresh
            // `notified()` each iteration is safe here (unlike shutdown's `notify_waiters`):
            // `notify_one` stores a permit, so a fire mid-arm is consumed next iteration, and
            // several fires while a pass runs coalesce into one.
            _ = manual_trigger(cfg.reconcile_now.as_ref()) => {
                tracing::info!("daemon: manual reconcile triggered");
                if let Err(e) = discovery.poll(enqueue.clone()).await {
                    tracing::warn!(error = format!("{e:#}"), "daemon: manual reconcile discovery poll failed");
                }
                match (reenqueue_all)().await {
                    Ok(keys) => queue.reenqueue_startup(keys),
                    Err(e) => tracing::warn!(error = format!("{e:#}"), "daemon: manual reconcile re-enqueue failed"),
                }
                // The manual pass just did a tick's work — push the next scheduled one a full
                // period out instead of letting it double-fire right behind us.
                discovery_timer.reset();
            }
            _ = shutdown_signal.as_mut() => break,
        }
    }

    // Drop the sources (the loop above already exited its select — `completions` and
    // `discovery_timer` go out of scope at the end of this fn), signal the queue to drain, and
    // join the worker.
    queue.shut_down();
    worker.await.context("joining the queue worker")?;
    Ok(())
}

/// Await the manual reconcile trigger, or never when no API is wired (`None` keeps the select arm
/// permanently pending instead of needing an `if` guard).
async fn manual_trigger(trigger: Option<&Arc<tokio::sync::Notify>>) {
    match trigger {
        Some(n) => n.notified().await,
        None => std::future::pending().await,
    }
}

async fn pod_next(
    stream: &mut std::pin::Pin<Box<dyn futures_util::Stream<Item = IssueKey> + Send>>,
) -> Option<IssueKey> {
    use futures_util::StreamExt;
    stream.next().await
}

/// A small deterministic stagger (a quarter of the interval, floored at 1s) rather than a fixed
/// constant — scales down for fast test intervals and up for the real multi-minute cadence.
fn stagger_offset(interval: Duration) -> Duration {
    (interval / 4).max(Duration::from_secs(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::PodStatus;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn pod(phase: Option<&str>, key: Option<&str>) -> Pod {
        let mut labels = BTreeMap::new();
        let mut annotations = BTreeMap::new();
        if let Some(k) = key {
            // The launch path stamps both: exact key as annotation, lossy hint as label.
            annotations.insert(ISSUE_KEY_ANNOTATION.to_string(), k.to_string());
            labels.insert(
                ISSUE_KEY_LABEL.to_string(),
                crate::runs::engine::issue_key_label_value(k),
            );
        }
        Pod {
            metadata: ObjectMeta {
                labels: Some(labels),
                annotations: Some(annotations),
                ..Default::default()
            },
            status: Some(PodStatus {
                phase: phase.map(str::to_string),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn completion_key_fires_on_succeeded_or_failed_with_the_exact_annotation() {
        assert_eq!(
            pod_completion_key(&pod(Some("Succeeded"), Some("owner/repo#1"))),
            Some(IssueKey("owner/repo#1".into())),
            "the annotation round-trips the real key, slashes and hash intact"
        );
        assert_eq!(
            pod_completion_key(&pod(Some("Failed"), Some("owner/repo#2"))),
            Some(IssueKey("owner/repo#2".into()))
        );
    }

    #[test]
    fn completion_key_ignores_a_label_only_pod() {
        // Label values are lossy (`owner-repo-1`) and must never be mistaken for a key.
        let mut p = pod(Some("Succeeded"), None);
        p.metadata
            .labels
            .get_or_insert_with(BTreeMap::new)
            .insert(ISSUE_KEY_LABEL.to_string(), "owner-repo-1".to_string());
        assert_eq!(pod_completion_key(&p), None);
    }

    #[test]
    fn completion_key_is_none_while_running_or_pending() {
        assert_eq!(
            pod_completion_key(&pod(Some("Running"), Some("owner/repo#1"))),
            None
        );
        assert_eq!(
            pod_completion_key(&pod(Some("Pending"), Some("owner/repo#1"))),
            None
        );
        assert_eq!(pod_completion_key(&pod(None, Some("owner/repo#1"))), None);
    }

    #[test]
    fn completion_key_is_none_without_the_label() {
        assert_eq!(pod_completion_key(&pod(Some("Succeeded"), None)), None);
    }

    #[test]
    fn stagger_offset_scales_with_interval_and_floors_at_one_second() {
        assert_eq!(
            stagger_offset(Duration::from_secs(300)),
            Duration::from_secs(75)
        );
        assert_eq!(
            stagger_offset(Duration::from_millis(40)),
            Duration::from_secs(1)
        );
    }

    /// A [`ClusterWatchSource`] backed by real channels — the same seam the daemon harness uses for
    /// the kube watch. Each cluster gets a scripted list of openings, replayed in order: `Some(rx)`
    /// hands out a live stream, `None` fails the open. An exhausted list keeps failing.
    struct TestWatchSource {
        clusters: Mutex<Vec<String>>,
        openings: Mutex<
            std::collections::HashMap<
                String,
                std::collections::VecDeque<Option<tokio::sync::mpsc::Receiver<IssueKey>>>,
            >,
        >,
        opened: Arc<Mutex<Vec<String>>>,
    }

    impl TestWatchSource {
        fn new(clusters: &[&str]) -> Self {
            TestWatchSource {
                clusters: Mutex::new(clusters.iter().map(|c| c.to_string()).collect()),
                openings: Mutex::new(std::collections::HashMap::new()),
                opened: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// Script one opening for `cluster`: a live stream whose sender is returned.
        fn script_stream(&self, cluster: &str) -> tokio::sync::mpsc::Sender<IssueKey> {
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            self.openings
                .lock()
                .expect("lock")
                .entry(cluster.to_string())
                .or_default()
                .push_back(Some(rx));
            tx
        }

        /// Script one opening for `cluster` that fails.
        fn script_failure(&self, cluster: &str) {
            self.openings
                .lock()
                .expect("lock")
                .entry(cluster.to_string())
                .or_default()
                .push_back(None);
        }
    }

    impl ClusterWatchSource for TestWatchSource {
        fn clusters(&self) -> Vec<String> {
            self.clusters.lock().expect("lock").clone()
        }

        fn open(&self, cluster: String) -> BoxFuture<Result<ClusterCompletions>> {
            self.opened.lock().expect("lock").push(cluster.clone());
            let next = self
                .openings
                .lock()
                .expect("lock")
                .get_mut(&cluster)
                .and_then(|q| q.pop_front())
                .flatten();
            Box::pin(async move {
                let rx = next.ok_or_else(|| {
                    anyhow::anyhow!("cluster `{cluster}` is unreachable in this test")
                })?;
                let stream: ClusterCompletions =
                    Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
                        rx.recv().await.map(|key| (key, rx))
                    }));
                Ok(stream)
            })
        }
    }

    /// The rescan cadence every supervisor test runs at: short enough that a retry lands inside the
    /// test's timeout, long enough that a pass never depends on a single tick's timing.
    const TEST_RESCAN: Duration = Duration::from_millis(20);

    /// The next key off the merged stream, with a bound so a regression fails instead of hanging.
    async fn next_key(stream: &mut CompletionStream) -> IssueKey {
        tokio::time::timeout(Duration::from_secs(5), pod_next(stream))
            .await
            .expect("a completion arrived")
            .expect("the merged stream is still open")
    }

    #[tokio::test]
    async fn every_cluster_s_completions_reach_the_merged_stream() {
        let source = Arc::new(TestWatchSource::new(&["hub", "wharf"]));
        let hub = source.script_stream("hub");
        let wharf = source.script_stream("wharf");
        let mut merged = watch_completions(source.clone(), TEST_RESCAN);

        wharf
            .send(IssueKey("owner/repo#1".into()))
            .await
            .expect("send");
        assert_eq!(next_key(&mut merged).await.0, "owner/repo#1");
        hub.send(IssueKey("owner/repo#2".into()))
            .await
            .expect("send");
        assert_eq!(next_key(&mut merged).await.0, "owner/repo#2");
    }

    #[tokio::test]
    async fn an_unreachable_cluster_leaves_the_others_watched_and_is_retried() {
        let source = Arc::new(TestWatchSource::new(&["hub", "wharf"]));
        let hub = source.script_stream("hub");
        source.script_failure("wharf");
        let wharf = source.script_stream("wharf");
        let mut merged = watch_completions(source.clone(), TEST_RESCAN);

        // The hub's watch is live even though the spoke's first open failed.
        hub.send(IssueKey("owner/repo#1".into()))
            .await
            .expect("send");
        assert_eq!(next_key(&mut merged).await.0, "owner/repo#1");

        // The retry on a later rescan picks the spoke up without a restart.
        wharf
            .send(IssueKey("owner/repo#2".into()))
            .await
            .expect("send");
        assert_eq!(next_key(&mut merged).await.0, "owner/repo#2");
        assert!(
            source
                .opened
                .lock()
                .expect("lock")
                .iter()
                .filter(|c| *c == "wharf")
                .count()
                >= 2,
            "the failed spoke was reopened"
        );
    }

    #[tokio::test]
    async fn a_watch_that_ends_is_reopened_on_the_next_rescan() {
        let source = Arc::new(TestWatchSource::new(&["wharf"]));
        let first = source.script_stream("wharf");
        let second = source.script_stream("wharf");
        let mut merged = watch_completions(source.clone(), TEST_RESCAN);

        first
            .send(IssueKey("owner/repo#1".into()))
            .await
            .expect("send");
        assert_eq!(next_key(&mut merged).await.0, "owner/repo#1");
        // Ending the first watch must not end the merged stream.
        drop(first);

        second
            .send(IssueKey("owner/repo#2".into()))
            .await
            .expect("send");
        assert_eq!(next_key(&mut merged).await.0, "owner/repo#2");
    }

    #[tokio::test]
    async fn a_cluster_registered_after_boot_is_watched_on_the_next_rescan() {
        let source = Arc::new(TestWatchSource::new(&["hub"]));
        let hub = source.script_stream("hub");
        let wharf = source.script_stream("wharf");
        let mut merged = watch_completions(source.clone(), TEST_RESCAN);

        hub.send(IssueKey("owner/repo#1".into()))
            .await
            .expect("send");
        assert_eq!(next_key(&mut merged).await.0, "owner/repo#1");

        source.clusters.lock().expect("lock").push("wharf".into());
        wharf
            .send(IssueKey("owner/repo#2".into()))
            .await
            .expect("send");
        assert_eq!(next_key(&mut merged).await.0, "owner/repo#2");
    }

    #[tokio::test]
    async fn dropping_the_merged_stream_stops_the_supervisor() {
        // Nothing is scripted, so every rescan retries the open and the count keeps growing until
        // the supervisor stops.
        let source = Arc::new(TestWatchSource::new(&["hub"]));
        let merged = watch_completions(source.clone(), TEST_RESCAN);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            source.opened.lock().expect("lock").len() > 1,
            "the failing open is retried while the stream lives"
        );
        drop(merged);
        tokio::time::sleep(Duration::from_millis(60)).await;
        let opens = source.opened.lock().expect("lock").len();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            source.opened.lock().expect("lock").len(),
            opens,
            "the supervisor exited with its receiver"
        );
    }

    /// The daemon shell without a live cluster: swap the kube pod watch for a channel-backed
    /// discovery source, drive one full cycle, then request shutdown and assert the worker
    /// joined (the leak-detecting finish pattern) — everything except the actual kube watch
    /// wiring, which compiles but needs a cluster to exercise.
    struct CountingDiscovery {
        polls: Arc<AtomicU32>,
        key: IssueKey,
    }

    impl DiscoverySource for CountingDiscovery {
        fn poll(&self, enqueue: Arc<dyn Enqueue>) -> BoxFuture<Result<()>> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            enqueue.enqueue(self.key.clone());
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn discovery_timer_enqueues_and_the_worker_reconciles_it() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_cl = seen.clone();
        let reconcile: ReconcileFn = Arc::new(move |key: IssueKey| {
            let seen = seen_cl.clone();
            Box::pin(async move {
                seen.lock().expect("lock").push(key.0);
                Ok(())
            })
        });
        let park: ParkFn = Arc::new(|_key, _reason| {});
        let discovery = Arc::new(CountingDiscovery {
            polls: Arc::new(AtomicU32::new(0)),
            key: IssueKey("owner/repo#9".into()),
        });
        let sync = Arc::new(TestSyncMarker::new());

        // No pod watch in this test — drive the reconcile side directly through the queue
        // instead of `run` (which requires a live/reachable kube API for the watch stream; the
        // watch wiring itself is exercised by compile + `pod_completion_key`'s unit tests above).
        let queue = WorkQueue::new();
        let worker_queue = queue.clone();
        let sync_cl = sync.clone();
        let worker = tokio::spawn(async move {
            worker_queue
                .run(reconcile, park, QueueConfig::default(), Some(sync_cl))
                .await;
        });

        let enqueue: Arc<dyn Enqueue> = Arc::new(queue.clone());
        discovery.poll(enqueue).await.expect("poll");
        sync.wait_for_count(1).await;
        queue.shut_down();
        worker.await.expect("worker joined");

        assert_eq!(*seen.lock().expect("lock"), vec!["owner/repo#9"]);
        assert_eq!(discovery.polls.load(Ordering::SeqCst), 1);
    }

    /// A full `run` with intervals parked in the far future, a pending completions stream, and a
    /// recording reconcile — only the manual trigger can drive work.
    struct ManualRig {
        seen: Arc<Mutex<Vec<String>>>,
        polls: Arc<AtomicU32>,
        sync: Arc<TestSyncMarker>,
        shutdown: Arc<tokio::sync::Notify>,
        handle: tokio::task::JoinHandle<Result<()>>,
    }

    /// `notify` is passed in (not minted here) so a test can fire it BEFORE the loop starts
    /// listening — the deterministic way to prove stacked fires coalesce into one permit.
    fn manual_rig(reenqueue_keys: Vec<String>, notify: Arc<tokio::sync::Notify>) -> ManualRig {
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_cl = seen.clone();
        let reconcile: ReconcileFn = Arc::new(move |key: IssueKey| {
            let seen = seen_cl.clone();
            Box::pin(async move {
                seen.lock().expect("lock").push(key.0);
                Ok(())
            })
        });
        let park: ParkFn = Arc::new(|_key, _reason| {});
        let polls = Arc::new(AtomicU32::new(0));
        let discovery = Arc::new(CountingDiscovery {
            polls: polls.clone(),
            key: IssueKey("owner/repo#1".into()),
        });
        let wiring = Wiring {
            queue: WorkQueue::new(),
            reconcile,
            park,
            discovery,
            completions: Box::pin(futures_util::stream::pending()),
            verify: None,
            reenqueue_all: Arc::new(move || {
                let keys = reenqueue_keys.clone();
                Box::pin(async move { Ok(keys) })
            }),
        };
        // An hour-scale cadence (first tick ~15min out after the stagger): the scheduled timers
        // can never fire inside the test — every observed pass came from the trigger.
        let cfg = DaemonConfig {
            queue: QueueConfig::default(),
            discovery_interval: Duration::from_secs(3600),
            discovery_interval_fn: None,
            verify_interval: Duration::from_secs(3600),
            reconcile_now: Some(notify),
        };
        let sync = Arc::new(TestSyncMarker::new());
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let handle = tokio::spawn(run(
            Vec::new(),
            cfg,
            wiring,
            shutdown.clone(),
            Some(sync.clone()),
        ));
        ManualRig {
            seen,
            polls,
            sync,
            shutdown,
            handle,
        }
    }

    #[tokio::test]
    async fn manual_trigger_runs_a_discovery_poll_plus_a_full_reenqueue() {
        let notify = Arc::new(tokio::sync::Notify::new());
        let rig = manual_rig(
            vec!["owner/repo#2".into(), "owner/repo#3".into()],
            notify.clone(),
        );
        notify.notify_one();
        rig.sync.wait_for_count(3).await;
        rig.shutdown.notify_waiters();
        rig.handle.await.expect("join").expect("run");

        let mut seen = rig.seen.lock().expect("lock").clone();
        seen.sort();
        assert_eq!(seen, vec!["owner/repo#1", "owner/repo#2", "owner/repo#3"]);
        assert_eq!(rig.polls.load(Ordering::SeqCst), 1, "exactly one pass");
    }

    #[tokio::test]
    async fn manual_trigger_coalesces_stacked_fires_and_refires_cleanly() {
        // Three clicks before the loop is even listening: `notify_one` stores at most one permit,
        // so they collapse into a single pass.
        let notify = Arc::new(tokio::sync::Notify::new());
        notify.notify_one();
        notify.notify_one();
        notify.notify_one();
        let rig = manual_rig(Vec::new(), notify.clone());
        rig.sync.wait_for_count(1).await;
        assert_eq!(
            rig.polls.load(Ordering::SeqCst),
            1,
            "stacked fires coalesce"
        );

        // A later click still works — the permit was consumed, not the trigger.
        notify.notify_one();
        rig.sync.wait_for_count(2).await;
        assert_eq!(rig.polls.load(Ordering::SeqCst), 2);

        rig.shutdown.notify_waiters();
        rig.handle.await.expect("join").expect("run");
    }
}
