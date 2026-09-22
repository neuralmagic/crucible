//! The in-memory work queue: sources enqueue a key, one worker dequeues and reconciles it. The
//! frozen contract is just [`IssueKey`] + [`Enqueue`]; everything else here is the
//! client-go/controller-runtime workqueue shape (coalescing set + FIFO order) plus exponential-backoff
//! requeue and park-after-N-failures, adapted to run over a single mpsc-fed worker instead of a CRD
//! reconciler.
//!
//! A dequeued key never carries a payload — the worker re-reads state from the database inside
//! `reconcile`, so a stale or duplicate enqueue is harmless by construction.

#![allow(clippy::disallowed_macros)]

use anyhow::Result;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

/// An opaque unique `issues.key`, the queue's only unit of work (frozen): `owner/repo#N` for a
/// GitHub issue, `scenario:{id}` for an adopted scenario — see [`crate::issues::model::InputKind`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IssueKey(pub String);

/// Sources (the discovery timer, the approval signal, the pod watch, the review-comment poll)
/// only ever call this — they never touch the database or act directly (frozen).
pub trait Enqueue: Send + Sync {
    fn enqueue(&self, key: IssueKey);

    /// Put newly authorized or human-directed work ahead of discovery backlog. Implementations
    /// without priority support remain correct, merely FIFO.
    fn enqueue_urgent(&self, key: IssueKey) {
        self.enqueue(key);
    }
}

/// A boxed, owned, `Send` future — the reconcile function's return type without naming a crate
/// (`futures::future::BoxFuture` would do the same job; this avoids the extra dependency).
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// The reconcile function the daemon plugs in (the real `reconcile`, or a test closure). Cloned
/// cheaply behind an `Arc` so the worker loop can call it repeatedly without owning it.
pub type ReconcileFn = Arc<dyn Fn(IssueKey) -> BoxFuture<Result<()>> + Send + Sync>;

/// Called once a key has failed `park_after` consecutive reconciles, with the key and the last
/// error's message — the daemon wires this to `Db::park_issue` (`ParkedBy::Machine`).
pub type ParkFn = Arc<dyn Fn(IssueKey, String) + Send + Sync>;

/// Backoff and park thresholds. Fields, not constants, so tests use millisecond-scale values and
/// never sleep for the production schedule (base 30s, cap 1h, park after 5).
#[derive(Debug, Clone, Copy)]
pub struct QueueConfig {
    pub base_backoff: Duration,
    pub max_backoff: Duration,
    pub park_after: u32,
}

impl Default for QueueConfig {
    fn default() -> Self {
        QueueConfig {
            base_backoff: Duration::from_secs(30),
            max_backoff: Duration::from_secs(3600),
            park_after: 5,
        }
    }
}

impl QueueConfig {
    /// Exponential backoff for the `n`th consecutive failure (`n` >= 1): `base * 2^(n-1)`,
    /// capped at `max_backoff`. Saturates instead of overflowing for large `n`.
    fn backoff_for(&self, n: u32) -> Duration {
        let shift = n.saturating_sub(1).min(31);
        let scaled = self.base_backoff.saturating_mul(1u32 << shift);
        scaled.min(self.max_backoff)
    }
}

/// The client-go/controller-runtime workqueue shape: `order` is FIFO, `dirty` is every key
/// pending-or-in-flight (dedup), `processing` is the key(s) currently being reconciled (one, with
/// a single worker, but the set generalizes cleanly if workers are later sharded).
struct QueueState {
    order: VecDeque<String>,
    dirty: HashSet<String>,
    processing: HashSet<String>,
}

struct Inner {
    state: Mutex<QueueState>,
    notify: Notify,
    shutdown: AtomicBool,
}

impl Inner {
    fn state(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `add`: if `key` is already dirty (queued or marked to re-run after the in-flight reconcile
    /// finishes), do nothing — this is the coalescing step. Otherwise mark it dirty and, unless
    /// it's currently processing, push it onto the FIFO.
    fn add(&self, key: String) {
        let mut st = self.state();
        if !st.dirty.insert(key.clone()) {
            return;
        }
        if st.processing.contains(&key) {
            return;
        }
        st.order.push_back(key);
        drop(st);
        self.notify.notify_one();
    }

    /// Add operator-directed work ahead of the ordinary discovery/startup backlog. If the key is
    /// already queued, promote that existing entry instead of letting FIFO dedup hide the human
    /// override behind every older key. An in-flight key keeps the normal dirty-bit follow-up
    /// semantics: it cannot be interrupted safely, so it reruns immediately after completion.
    fn add_urgent(&self, key: String) {
        let mut st = self.state();
        if st.processing.contains(&key) {
            st.dirty.insert(key);
            return;
        }
        if st.dirty.insert(key.clone()) {
            st.order.push_front(key);
        } else if let Some(position) = st.order.iter().position(|queued| queued == &key) {
            st.order.remove(position);
            st.order.push_front(key);
        }
        drop(st);
        self.notify.notify_one();
    }

    /// Block until a key is ready or the queue is shut down. Registers the `Notified` future
    /// before re-checking state so a concurrent `add`/`shutdown` between the check and the await
    /// can't be missed (the standard Tokio `Notify` wait-loop pattern).
    async fn get_next(&self) -> Option<String> {
        loop {
            let notified = self.notify.notified();
            {
                let mut st = self.state();
                if let Some(key) = st.order.pop_front() {
                    st.dirty.remove(&key);
                    st.processing.insert(key.clone());
                    return Some(key);
                }
                if self.shutdown.load(Ordering::SeqCst) {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// The reconcile for `key` finished (success or a not-yet-parked failure): it's no longer
    /// processing, and if something re-`add`ed it meanwhile, it goes back on the FIFO now.
    fn done(&self, key: &str) {
        let mut st = self.state();
        st.processing.remove(key);
        if st.dirty.contains(key) {
            st.order.push_back(key.to_string());
        }
        drop(st);
        self.notify.notify_one();
    }

    /// The reconcile for `key` is being parked: no further requeue even if it was re-`add`ed
    /// while in flight — parking means "stop looking at this until something revives it."
    fn done_parking(&self, key: &str) {
        let mut st = self.state();
        st.processing.remove(key);
        st.dirty.remove(key);
    }

    fn shut_down(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

/// The queue handle: implements [`Enqueue`] for sources, and drives the worker loop via
/// [`WorkQueue::run`]. Cheap to clone (an `Arc` inside).
#[derive(Clone)]
pub struct WorkQueue {
    inner: Arc<Inner>,
}

impl Default for WorkQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkQueue {
    pub fn new() -> Self {
        WorkQueue {
            inner: Arc::new(Inner {
                state: Mutex::new(QueueState {
                    order: VecDeque::new(),
                    dirty: HashSet::new(),
                    processing: HashSet::new(),
                }),
                notify: Notify::new(),
                shutdown: AtomicBool::new(false),
            }),
        }
    }

    /// Startup re-enqueue: the caller fetches `Db::non_terminal_keys` and hands
    /// them here before the worker starts, so a crash or a dropped event costs latency, never
    /// work.
    pub(crate) fn reenqueue_startup(&self, keys: Vec<String>) {
        for key in keys {
            self.inner.add(key);
        }
    }

    /// Enqueue a human override ahead of machine-discovered work. Kept off the frozen [`Enqueue`]
    /// trait so ordinary producers cannot accidentally bypass FIFO fairness.
    pub(crate) fn enqueue_urgent(&self, key: IssueKey) {
        self.inner.add_urgent(key.0);
    }

    /// Stop accepting new work and wake the worker so it exits `run` once the queue drains: drop
    /// every `Enqueue` handle / source, then call this, then join the worker task.
    pub(crate) fn shut_down(&self) {
        self.inner.shut_down();
    }

    /// The worker loop: dequeue a key, reconcile it, and either clear it, requeue it after
    /// backoff, or park it after `cfg.park_after` consecutive failures. Runs until [`shut_down`]
    /// is called and the queue is empty. `sync` marks completion of each processed item so tests
    /// can await real drain instead of sleeping.
    pub(crate) async fn run(
        &self,
        reconcile: ReconcileFn,
        park: ParkFn,
        cfg: QueueConfig,
        sync: Option<Arc<TestSyncMarker>>,
    ) {
        let mut failures: HashMap<String, u32> = HashMap::new();
        loop {
            let key = match self.inner.get_next().await {
                Some(k) => k,
                None => break,
            };
            let result = reconcile(IssueKey(key.clone())).await;
            match result {
                Ok(()) => {
                    failures.remove(&key);
                    self.inner.done(&key);
                }
                Err(err) => {
                    let n = {
                        let count = failures.entry(key.clone()).or_insert(0);
                        *count += 1;
                        *count
                    };
                    if n >= cfg.park_after {
                        failures.remove(&key);
                        self.inner.done_parking(&key);
                        park(
                            IssueKey(key.clone()),
                            crate::model::truncate_chain(&format!("{err:#}")),
                        );
                    } else {
                        self.inner.done(&key);
                        let backoff = cfg.backoff_for(n);
                        let inner = self.inner.clone();
                        let requeue_key = key.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(backoff).await;
                            inner.add(requeue_key);
                        });
                    }
                }
            }
            if let Some(marker) = &sync {
                marker.mark();
            }
        }
    }
}

impl Enqueue for WorkQueue {
    fn enqueue(&self, key: IssueKey) {
        self.inner.add(key.0);
    }

    fn enqueue_urgent(&self, key: IssueKey) {
        WorkQueue::enqueue_urgent(self, key);
    }
}

/// A human override submitted through the API/UI: park/unpark/bump plus
/// the intent the queue-only [`Enqueue`] trait can't carry — a reason (park) or a new priority
/// (bump). Accepted as a contract addition alongside the frozen [`IssueKey`]/[`Enqueue`] pair;
/// the daemon folds submissions into the reconcile flow so they compose with claims
/// instead of racing them.
#[derive(Debug, Clone, PartialEq)]
pub struct Override {
    pub(crate) key: IssueKey,
    pub(crate) kind: OverrideKind,
    /// Free-text reason (park's `parked_reason`; optionally carried on bump/unpark for the audit
    /// trail).
    pub(crate) reason: Option<String>,
    /// The new priority (bump only).
    pub(crate) priority: Option<i64>,
    /// Who asked ([`crate::identity::session::Identity`] at the API edge) — lands on the event-log line so an
    /// override traces back to a human, not just to "the API".
    pub(crate) actor: Option<String>,
}

/// What an override asks the reconciler to do.
#[derive(Debug, Clone, PartialEq)]
pub enum OverrideKind {
    Park,
    Unpark,
    Bump,
    /// Trigger an immediate scope-propose for an issue, bypassing ALL autopilot gates (rank horizon,
    /// tier, grounded prescope, daily ceiling, scopes/day, daily turn budget, per-kind concurrency
    /// cap). The human is the authorization; costs are still booked in the ledger.
    ScopeNow {
        justification: String,
        max_cost: Option<f64>,
    },
    /// Re-run an already-approved pack: mint a fresh loop run off the SAME stored pack, no new scope
    /// turn or approval gate. Legal only from a finished run (`done`/`pr-open`) whose approval gate
    /// still stands (re-verified in the apply step). The human is the authorization; costs are still
    /// booked in the ledger.
    Redispatch {
        justification: String,
    },
}

/// Where the API/UI hands an [`Override`] off. Reconcile is the only writer of `parked_by =
/// human` stickiness; this sink just queues the intent — it never touches the DB itself,
/// matching [`Enqueue`]'s shape.
pub trait OverrideSink: Send + Sync {
    fn submit(&self, ov: Override);
}

/// A discovery source: polls whatever it polls (the upstream watermark, the approval PRs, the
/// review comments) and enqueues changed keys. Handed an owned `Arc<dyn Enqueue>` (not a borrow) so
/// a source that enqueues *after* an async fetch can move it into its future — the queue handle is a
/// cheap `Arc` clone, so this costs nothing. Most sources only enqueue; the one-shot and schedule
/// sweeps also write, claiming a due row and minting its launch before enqueueing the key.
pub trait DiscoverySource: Send + Sync {
    fn poll(&self, enqueue: Arc<dyn Enqueue>) -> BoxFuture<anyhow::Result<()>>;
}

/// A monotonically increasing counter plus a `Notify`. [`WorkQueue::run`](crate::daemon::queue::WorkQueue::run)
/// calls [`mark`](TestSyncMarker::mark) once per processed item (success, backoff-requeue, or
/// park all count as "processed" — the point is "the worker looked at this item and moved on").
#[derive(Debug, Default)]
pub struct TestSyncMarker {
    count: AtomicU64,
    notify: Notify,
}

impl TestSyncMarker {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        TestSyncMarker {
            count: AtomicU64::new(0),
            notify: Notify::new(),
        }
    }

    /// Record one processed item and wake every waiter.
    pub(crate) fn mark(&self) {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// The number of items processed so far.
    #[cfg(test)]
    fn count(&self) -> u64 {
        self.count.load(Ordering::SeqCst)
    }

    /// Block until at least `n` items have been processed. Registers the `Notified` future
    /// before re-checking the count so a `mark` between the check and the await is never missed.
    #[cfg(test)]
    pub(crate) async fn wait_for_count(&self, n: u64) {
        loop {
            let notified = self.notify.notified();
            if self.count() >= n {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicU32;

    fn ok_reconcile() -> ReconcileFn {
        Arc::new(|_key: IssueKey| -> BoxFuture<Result<()>> { Box::pin(async { Ok(()) }) })
    }

    fn noop_park() -> ParkFn {
        Arc::new(|_key, _reason| {})
    }

    #[tokio::test]
    async fn fifo_order_is_preserved() {
        let queue = WorkQueue::new();
        let seen: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let seen_cl = seen.clone();
        let sync = Arc::new(TestSyncMarker::new());

        queue.enqueue(IssueKey("a".into()));
        queue.enqueue(IssueKey("b".into()));
        queue.enqueue(IssueKey("c".into()));

        let reconcile: ReconcileFn = Arc::new(move |key: IssueKey| {
            let seen = seen_cl.clone();
            Box::pin(async move {
                seen.lock().expect("lock").push(key.0);
                Ok(())
            })
        });

        let q = queue.clone();
        let sync_cl = sync.clone();
        let handle = tokio::spawn(async move {
            q.run(
                reconcile,
                noop_park(),
                QueueConfig::default(),
                Some(sync_cl),
            )
            .await;
        });

        // Real completion, not a sleep: the sync marker fires once per processed item.
        sync.wait_for_count(3).await;
        queue.shut_down();
        handle.await.expect("worker joined");

        assert_eq!(*seen.lock().expect("lock"), vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn urgent_enqueue_promotes_an_existing_backlog_key() {
        let queue = WorkQueue::new();
        let seen: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let seen_cl = seen.clone();
        let sync = Arc::new(TestSyncMarker::new());

        queue.reenqueue_startup(vec!["old-a".into(), "old-b".into(), "human".into()]);
        queue.enqueue_urgent(IssueKey("human".into()));

        let reconcile: ReconcileFn = Arc::new(move |key: IssueKey| {
            let seen = seen_cl.clone();
            Box::pin(async move {
                seen.lock().expect("lock").push(key.0);
                Ok(())
            })
        });
        let q = queue.clone();
        let sync_cl = sync.clone();
        let handle = tokio::spawn(async move {
            q.run(
                reconcile,
                noop_park(),
                QueueConfig::default(),
                Some(sync_cl),
            )
            .await;
        });

        sync.wait_for_count(3).await;
        queue.shut_down();
        handle.await.expect("worker joined");
        assert_eq!(*seen.lock().expect("lock"), vec!["human", "old-a", "old-b"]);
    }

    #[tokio::test]
    async fn coalescing_collapses_n_enqueues_while_busy_into_one_more_reconcile() {
        let queue = WorkQueue::new();
        let calls: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
        let calls_cl = calls.clone();
        let release: Arc<Notify> = Arc::new(Notify::new());
        let release_cl = release.clone();
        let sync = Arc::new(TestSyncMarker::new());

        // The first reconcile blocks on `release` so we can enqueue the same key repeatedly
        // while it's in flight — that's what "busy" means here.
        let first_call_started = Arc::new(Notify::new());
        let first_call_started_cl = first_call_started.clone();
        let reconcile: ReconcileFn = Arc::new(move |key: IssueKey| {
            let calls = calls_cl.clone();
            let release = release_cl.clone();
            let started = first_call_started_cl.clone();
            Box::pin(async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    started.notify_one();
                    release.notified().await;
                }
                assert_eq!(key.0, "busy-key");
                Ok(())
            })
        });

        let q = queue.clone();
        let sync_cl = sync.clone();
        let handle = tokio::spawn(async move {
            q.run(
                reconcile,
                noop_park(),
                QueueConfig::default(),
                Some(sync_cl),
            )
            .await;
        });

        queue.enqueue(IssueKey("busy-key".into()));
        first_call_started.notified().await;
        // Ten enqueues while the first reconcile is in flight: coalesce into exactly one more.
        for _ in 0..10 {
            queue.enqueue(IssueKey("busy-key".into()));
        }
        release.notify_one();

        // Two total reconciles: the initial one plus exactly one coalesced follow-up. Draining
        // the queue (no more work after the second item) proves nothing further was pending.
        sync.wait_for_count(2).await;
        queue.shut_down();
        handle.await.expect("worker joined");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "excess enqueues coalesced");
    }

    #[tokio::test]
    async fn backoff_schedule_doubles_and_caps() {
        let cfg = QueueConfig {
            base_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(40),
            park_after: 10,
        };
        assert_eq!(cfg.backoff_for(1), Duration::from_millis(5));
        assert_eq!(cfg.backoff_for(2), Duration::from_millis(10));
        assert_eq!(cfg.backoff_for(3), Duration::from_millis(20));
        assert_eq!(
            cfg.backoff_for(4),
            Duration::from_millis(40),
            "capped at max"
        );
        assert_eq!(
            cfg.backoff_for(20),
            Duration::from_millis(40),
            "stays capped"
        );
    }

    #[tokio::test]
    async fn failed_reconcile_is_requeued_after_backoff_then_succeeds() {
        let queue = WorkQueue::new();
        let attempts: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
        let attempts_cl = attempts.clone();
        let cfg = QueueConfig {
            base_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(50),
            park_after: 10,
        };
        let sync = Arc::new(TestSyncMarker::new());

        let reconcile: ReconcileFn = Arc::new(move |_key: IssueKey| {
            let attempts = attempts_cl.clone();
            Box::pin(async move {
                let n = attempts.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    anyhow::bail!("transient failure");
                }
                Ok(())
            })
        });

        let q = queue.clone();
        let sync_cl = sync.clone();
        let handle = tokio::spawn(async move {
            q.run(reconcile, noop_park(), cfg, Some(sync_cl)).await;
        });

        queue.enqueue(IssueKey("retry-key".into()));
        // Two processed items: the failing attempt and the successful retry.
        sync.wait_for_count(2).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 2);

        queue.shut_down();
        handle.await.expect("worker joined");
    }

    #[tokio::test]
    async fn parks_after_park_after_consecutive_failures_with_the_key_and_reason() {
        let queue = WorkQueue::new();
        let cfg = QueueConfig {
            base_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(4),
            park_after: 3,
        };
        let parked: Arc<StdMutex<Vec<(String, String)>>> = Arc::new(StdMutex::new(Vec::new()));
        let parked_cl = parked.clone();
        let sync = Arc::new(TestSyncMarker::new());

        let reconcile: ReconcileFn = Arc::new(|_key: IssueKey| -> BoxFuture<Result<()>> {
            Box::pin(async { anyhow::bail!("always fails") })
        });
        let park: ParkFn = Arc::new(move |key: IssueKey, reason: String| {
            parked_cl.lock().expect("lock").push((key.0, reason));
        });

        let q = queue.clone();
        let sync_cl = sync.clone();
        let handle = tokio::spawn(async move {
            q.run(reconcile, park, cfg, Some(sync_cl)).await;
        });

        queue.enqueue(IssueKey("doomed".into()));
        // Three processed items: two backoff-requeued failures, then the park.
        sync.wait_for_count(3).await;

        queue.shut_down();
        handle.await.expect("worker joined");

        let got = parked.lock().expect("lock");
        assert_eq!(got.len(), 1, "parked exactly once");
        assert_eq!(got[0].0, "doomed");
        assert_eq!(got[0].1, "always fails");
    }

    /// The live failure: a park read `insert_work_pod` because the reason was rendered from the
    /// outermost context alone. Every layer of the chain has to reach the park reason.
    #[tokio::test]
    async fn a_park_reason_carries_the_whole_error_chain() {
        let queue = WorkQueue::new();
        let cfg = QueueConfig {
            base_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(4),
            park_after: 1,
        };
        let parked: Arc<StdMutex<Vec<(String, String)>>> = Arc::new(StdMutex::new(Vec::new()));
        let parked_cl = parked.clone();
        let sync = Arc::new(TestSyncMarker::new());

        let reconcile: ReconcileFn = Arc::new(|_key: IssueKey| -> BoxFuture<Result<()>> {
            Box::pin(async {
                Err(
                    anyhow::anyhow!("duplicate key value violates unique constraint")
                        .context("recording the work-pod ledger row for crucible-run-x")
                        .context("dispatching the loop run"),
                )
            })
        });
        let park: ParkFn = Arc::new(move |key: IssueKey, reason: String| {
            parked_cl.lock().expect("lock").push((key.0, reason));
        });

        let q = queue.clone();
        let sync_cl = sync.clone();
        let handle = tokio::spawn(async move {
            q.run(reconcile, park, cfg, Some(sync_cl)).await;
        });

        queue.enqueue(IssueKey("doomed".into()));
        sync.wait_for_count(1).await;

        queue.shut_down();
        handle.await.expect("worker joined");

        let got = parked.lock().expect("lock");
        assert_eq!(got.len(), 1, "parked exactly once");
        assert_eq!(
            got[0].1,
            "dispatching the loop run: recording the work-pod ledger row for crucible-run-x: \
             duplicate key value violates unique constraint"
        );
    }

    /// A chain longer than the evidence cap is cut with a marker rather than dumping a page into
    /// the parked reason.
    #[tokio::test]
    async fn a_park_reason_caps_a_runaway_chain() {
        let queue = WorkQueue::new();
        let cfg = QueueConfig {
            base_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(4),
            park_after: 1,
        };
        let parked: Arc<StdMutex<Vec<(String, String)>>> = Arc::new(StdMutex::new(Vec::new()));
        let parked_cl = parked.clone();
        let sync = Arc::new(TestSyncMarker::new());

        let reconcile: ReconcileFn = Arc::new(|_key: IssueKey| -> BoxFuture<Result<()>> {
            Box::pin(async { Err(anyhow::anyhow!("{}", "x".repeat(5000))) })
        });
        let park: ParkFn = Arc::new(move |key: IssueKey, reason: String| {
            parked_cl.lock().expect("lock").push((key.0, reason));
        });

        let q = queue.clone();
        let sync_cl = sync.clone();
        let handle = tokio::spawn(async move {
            q.run(reconcile, park, cfg, Some(sync_cl)).await;
        });

        queue.enqueue(IssueKey("doomed".into()));
        sync.wait_for_count(1).await;

        queue.shut_down();
        handle.await.expect("worker joined");

        let got = parked.lock().expect("lock");
        assert_eq!(got[0].1.chars().count(), 2001, "capped, plus the marker");
        assert!(got[0].1.ends_with('…'));
    }

    #[tokio::test]
    async fn startup_reenqueue_drains_non_terminal_keys() {
        let queue = WorkQueue::new();
        let seen: Arc<StdMutex<HashSet<String>>> = Arc::new(StdMutex::new(HashSet::new()));
        let seen_cl = seen.clone();
        let sync = Arc::new(TestSyncMarker::new());

        queue.reenqueue_startup(vec!["a".into(), "b".into(), "c".into()]);

        let reconcile: ReconcileFn = Arc::new(move |key: IssueKey| {
            let seen = seen_cl.clone();
            Box::pin(async move {
                seen.lock().expect("lock").insert(key.0);
                Ok(())
            })
        });

        let q = queue.clone();
        let sync_cl = sync.clone();
        let handle = tokio::spawn(async move {
            q.run(
                reconcile,
                noop_park(),
                QueueConfig::default(),
                Some(sync_cl),
            )
            .await;
        });

        sync.wait_for_count(3).await;
        queue.shut_down();
        handle.await.expect("worker joined");

        let got = seen.lock().expect("lock");
        assert_eq!(got.len(), 3);
        for k in ["a", "b", "c"] {
            assert!(got.contains(k));
        }
    }

    #[tokio::test]
    async fn shut_down_lets_an_idle_worker_exit_within_timeout() {
        let queue = WorkQueue::new();
        let q = queue.clone();
        let handle = tokio::spawn(async move {
            q.run(ok_reconcile(), noop_park(), QueueConfig::default(), None)
                .await;
        });
        // Nothing was ever enqueued: shut_down alone must wake and end the loop.
        queue.shut_down();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("worker exited before the leak-check timeout")
            .expect("worker joined");
    }

    #[tokio::test]
    async fn wait_for_count_returns_once_enough_marks_landed() {
        let marker = Arc::new(TestSyncMarker::new());
        let m = marker.clone();
        tokio::spawn(async move {
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_millis(1)).await;
                m.mark();
            }
        });

        tokio::time::timeout(Duration::from_secs(2), marker.wait_for_count(3))
            .await
            .expect("wait_for_count returned before the timeout");
        assert_eq!(marker.count(), 3);
    }

    #[tokio::test]
    async fn wait_for_count_returns_immediately_if_already_satisfied() {
        let marker = TestSyncMarker::new();
        marker.mark();
        marker.mark();
        tokio::time::timeout(Duration::from_millis(50), marker.wait_for_count(1))
            .await
            .expect("already-satisfied wait returns without blocking");
    }
}
