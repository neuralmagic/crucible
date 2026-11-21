//! The in-memory work queue: sources enqueue a key, one worker dequeues and reconciles it. The
//! frozen contract is just [`IssueKey`] + [`WorkQueue::enqueue`]; everything else here is the
//! client-go/controller-runtime workqueue shape (coalescing set + FIFO order) plus exponential-backoff
//! requeue and park-after-N-failures, adapted to run over a single mpsc-fed worker instead of a CRD
//! reconciler.
//!
//! A dequeued key never carries a payload — the worker re-reads state from the database inside
//! `reconcile`, so a stale or duplicate enqueue is harmless by construction.

#![allow(clippy::disallowed_macros)]

use anyhow::Result;
use futures_util::FutureExt;
use std::any::Any;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Notify, mpsc, oneshot};

/// An opaque unique `issues.key`, the queue's only unit of work (frozen): `owner/repo#N` for a
/// GitHub issue, `scenario:{id}` for an adopted scenario — see [`crate::issues::model::InputKind`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IssueKey(pub String);

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
#[derive(Default)]
struct QueueState {
    order: VecDeque<String>,
    /// How many keys at the front of `order` were added urgent. They run FIFO among themselves.
    urgent: usize,
    dirty: HashSet<String>,
    processing: HashSet<String>,
}

impl QueueState {
    /// `add`: if `key` is already dirty (queued or marked to re-run after the in-flight reconcile
    /// finishes), do nothing — this is the coalescing step. Otherwise mark it dirty and, unless
    /// it's currently processing, push it onto the FIFO.
    fn add(&mut self, key: String) {
        if !self.dirty.insert(key.clone()) {
            return;
        }
        if self.processing.contains(&key) {
            return;
        }
        self.order.push_back(key);
    }

    /// Add operator-directed work ahead of the ordinary discovery/startup backlog, behind any
    /// urgent key already waiting. If the key is already queued, promote that existing entry
    /// instead of letting FIFO dedup hide the human override behind every older key. An in-flight
    /// key keeps the normal dirty-bit follow-up semantics: it cannot be interrupted safely, so it
    /// reruns immediately after completion.
    fn add_urgent(&mut self, key: String) {
        if self.processing.contains(&key) {
            self.dirty.insert(key);
            return;
        }
        if !self.dirty.insert(key.clone()) {
            match self.order.iter().position(|queued| queued == &key) {
                Some(position) if position < self.urgent => return,
                Some(position) => {
                    self.order.remove(position);
                }
                None => return,
            }
        }
        let at = self.urgent;
        self.order.insert(at, key);
        self.urgent += 1;
    }

    /// The next key to reconcile, now processing.
    fn next(&mut self) -> Option<String> {
        let key = self.order.pop_front()?;
        self.urgent = self.urgent.saturating_sub(1);
        self.dirty.remove(&key);
        self.processing.insert(key.clone());
        Some(key)
    }

    /// The reconcile for `key` finished (success or a not-yet-parked failure): it's no longer
    /// processing, and if something re-`add`ed it meanwhile, it goes back on the FIFO now.
    fn done(&mut self, key: &str) {
        self.processing.remove(key);
        if self.dirty.contains(key) {
            self.order.push_back(key.to_string());
        }
    }

    /// The reconcile for `key` is being parked: no further requeue even if it was re-`add`ed
    /// while in flight — parking means "stop looking at this until something revives it."
    fn done_parking(&mut self, key: &str) {
        self.processing.remove(key);
        self.dirty.remove(key);
    }

    #[cfg(test)]
    fn pending(&self) -> Pending {
        let order: Vec<String> = self.order.iter().cloned().collect();
        let (urgent, plain) = order.split_at(self.urgent.min(order.len()));
        Pending {
            urgent: urgent.to_vec(),
            plain: plain.to_vec(),
        }
    }
}

type Panic = Box<dyn Any + Send>;

enum Msg {
    Add(String),
    AddUrgent(String),
    Run(Worker, oneshot::Sender<Panic>),
    ShutDown,
    #[cfg(test)]
    Pending(oneshot::Sender<Pending>),
}

struct Worker {
    reconcile: ReconcileFn,
    park: ParkFn,
    cfg: QueueConfig,
    sync: Option<Arc<TestSyncMarker>>,
}

/// A [`WorkQueue::run`] call the actor is serving.
struct Running {
    worker: Worker,
    panicked: oneshot::Sender<Panic>,
    failures: HashMap<String, u32>,
    in_flight: Option<(String, BoxFuture<std::thread::Result<Result<()>>>)>,
}

/// The task that owns the queue. It applies the handles' messages and, while a
/// [`WorkQueue::run`] call is waiting on it, reconciles one key at a time.
struct Actor {
    inbox: mpsc::UnboundedReceiver<Msg>,
    handle: mpsc::WeakUnboundedSender<Msg>,
    state: QueueState,
    shutdown: bool,
    running: Option<Running>,
}

impl Actor {
    async fn run(mut self) {
        loop {
            self.start_next();
            let msg = match self.running.as_mut().and_then(|r| r.in_flight.as_mut()) {
                Some((_, reconcile)) => tokio::select! {
                    biased;
                    msg = self.inbox.recv() => msg,
                    result = reconcile => {
                        self.settle(result);
                        continue;
                    }
                },
                None => self.inbox.recv().await,
            };
            match msg {
                Some(msg) => self.apply(msg),
                None => return,
            }
        }
    }

    fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Add(key) => self.state.add(key),
            Msg::AddUrgent(key) => self.state.add_urgent(key),
            Msg::Run(..) if self.running.is_some() => {}
            Msg::Run(worker, panicked) => {
                self.running = Some(Running {
                    worker,
                    panicked,
                    failures: HashMap::new(),
                    in_flight: None,
                });
            }
            Msg::ShutDown => self.shutdown = true,
            #[cfg(test)]
            Msg::Pending(reply) => {
                let _ = reply.send(self.state.pending());
            }
        }
    }

    fn start_next(&mut self) {
        let Some(running) = self.running.as_mut() else {
            return;
        };
        if running.in_flight.is_some() {
            return;
        }
        match self.state.next() {
            Some(key) => {
                let reconcile = running.worker.reconcile.clone();
                let issue = IssueKey(key.clone());
                let attempt =
                    AssertUnwindSafe(async move { reconcile(issue).await }).catch_unwind();
                running.in_flight = Some((key, Box::pin(attempt)));
            }
            None if self.shutdown => self.running = None,
            None => {}
        }
    }

    /// Clear the reconciled key, requeue it after backoff, or park it after
    /// `cfg.park_after` consecutive failures.
    fn settle(&mut self, result: std::thread::Result<Result<()>>) {
        let Some(mut running) = self.running.take() else {
            return;
        };
        let Some((key, _)) = running.in_flight.take() else {
            self.running = Some(running);
            return;
        };
        let result = match result {
            Ok(result) => result,
            Err(panic) => {
                self.state.done(&key);
                let _ = running.panicked.send(panic);
                return;
            }
        };
        match result {
            Ok(()) => {
                running.failures.remove(&key);
                self.state.done(&key);
            }
            Err(err) => {
                let n = {
                    let count = running.failures.entry(key.clone()).or_insert(0);
                    *count += 1;
                    *count
                };
                if n >= running.worker.cfg.park_after {
                    running.failures.remove(&key);
                    self.state.done_parking(&key);
                    (running.worker.park)(
                        IssueKey(key.clone()),
                        crate::model::truncate_chain(&format!("{err:#}")),
                    );
                } else {
                    self.state.done(&key);
                    let backoff = running.worker.cfg.backoff_for(n);
                    let handle = self.handle.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(backoff).await;
                        if let Some(inbox) = handle.upgrade() {
                            let _ = inbox.send(Msg::Add(key));
                        }
                    });
                }
            }
        }
        if let Some(marker) = &running.worker.sync {
            marker.mark();
        }
        self.running = Some(running);
    }
}

/// The queue handle: what every source enqueues through, and what drives the worker loop via
/// [`WorkQueue::run`]. Cheap to clone (a channel sender inside).
#[derive(Clone)]
pub struct WorkQueue {
    inbox: mpsc::UnboundedSender<Msg>,
}

impl Default for WorkQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkQueue {
    /// Spawns the queue's task, so it must be called inside a Tokio runtime.
    pub fn new() -> Self {
        let (inbox, messages) = mpsc::unbounded_channel();
        tokio::spawn(
            Actor {
                inbox: messages,
                handle: inbox.downgrade(),
                state: QueueState::default(),
                shutdown: false,
                running: None,
            }
            .run(),
        );
        WorkQueue { inbox }
    }

    fn send(&self, msg: Msg) {
        let _ = self.inbox.send(msg);
    }

    /// Startup re-enqueue: the caller fetches `Db::non_terminal_keys` and hands
    /// them here before the worker starts, so a crash or a dropped event costs latency, never
    /// work.
    pub(crate) fn reenqueue_startup(&self, keys: Vec<String>) {
        for key in keys {
            self.send(Msg::Add(key));
        }
    }

    /// Sources (the discovery timer, the approval signal, the pod watch, the review-comment poll)
    /// only ever call this — they never touch the database or act directly (frozen).
    pub fn enqueue(&self, key: IssueKey) {
        self.send(Msg::Add(key.0));
    }

    /// Put newly authorized or human-directed work ahead of discovery backlog.
    pub fn enqueue_urgent(&self, key: IssueKey) {
        self.send(Msg::AddUrgent(key.0));
    }

    /// Stop accepting new work and wake the worker so it exits `run` once the queue drains: drop
    /// every queue handle / source, then call this, then join the worker task.
    pub(crate) fn shut_down(&self) {
        self.send(Msg::ShutDown);
    }

    /// The keys waiting to be dequeued, urgent ones first, in the order they will run.
    #[cfg(test)]
    pub(crate) async fn pending(&self) -> Pending {
        let (reply, pending) = oneshot::channel();
        self.send(Msg::Pending(reply));
        pending.await.unwrap_or_default()
    }

    /// The worker loop: dequeue a key, reconcile it, and either clear it, requeue it after
    /// backoff, or park it after `cfg.park_after` consecutive failures. Runs until [`shut_down`]
    /// is called and the queue is empty. `sync` marks completion of each processed item so tests
    /// can await real drain instead of sleeping. One call at a time: a second returns at once.
    pub(crate) async fn run(
        &self,
        reconcile: ReconcileFn,
        park: ParkFn,
        cfg: QueueConfig,
        sync: Option<Arc<TestSyncMarker>>,
    ) {
        let (panicked, worker_panicked) = oneshot::channel();
        self.send(Msg::Run(
            Worker {
                reconcile,
                park,
                cfg,
                sync,
            },
            panicked,
        ));
        if let Ok(panic) = worker_panicked.await {
            std::panic::resume_unwind(panic);
        }
    }
}

/// What a queue holds, split the way it will dequeue.
#[cfg(test)]
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Pending {
    pub(crate) urgent: Vec<String>,
    pub(crate) plain: Vec<String>,
}

/// A human override submitted through the API/UI: park/unpark/bump plus
/// the intent a bare [`WorkQueue::enqueue`] can't carry — a reason (park) or a new priority
/// (bump). Accepted as a contract addition alongside the frozen [`IssueKey`];
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
    /// Stop a launch: park it as a human would, then delete its running runs' pods and mark the
    /// runs `cancelled`.
    Cancel,
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

/// A discovery source: polls whatever it polls (the upstream watermark, the approval PRs, the
/// review comments) and enqueues changed keys. Handed an owned [`WorkQueue`] (not a borrow) so a
/// source that enqueues *after* an async fetch can move it into its future — the handle is a
/// cheap `Arc` clone, so this costs nothing. Most sources only enqueue; the one-shot and schedule
/// sweeps also write, claiming a due row and minting its launch before enqueueing the key.
pub trait DiscoverySource: Send + Sync {
    fn poll(&self, queue: WorkQueue) -> BoxFuture<anyhow::Result<()>>;
}

/// A source whose work has a due time. The launch loop asks it for the earliest one and runs a
/// pass once that time has come. `held` names the rows an earlier pass deferred; both calls leave
/// them out.
pub trait DueSource: Send + Sync {
    fn next_due(&self, held: Vec<String>) -> BoxFuture<anyhow::Result<Option<jiff::Timestamp>>>;
    fn pass(&self, queue: WorkQueue, held: Vec<String>) -> BoxFuture<anyhow::Result<Pass>>;
}

/// What one [`DueSource::pass`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Pass {
    /// Rows the pass claimed, whatever became of them.
    pub claimed: usize,
    /// Rows still due that the pass deferred or skipped.
    pub held: Vec<String>,
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
    async fn urgent_keys_run_first_in_the_order_they_arrived() {
        let queue = WorkQueue::new();
        let seen: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let seen_cl = seen.clone();
        let sync = Arc::new(TestSyncMarker::new());

        queue.reenqueue_startup(vec!["old-a".into(), "old-b".into()]);
        queue.enqueue_urgent(IssueKey("launch-1".into()));
        queue.enqueue_urgent(IssueKey("launch-2".into()));
        // Already urgent: keeps its place instead of jumping ahead of launch-2.
        queue.enqueue_urgent(IssueKey("launch-1".into()));
        // A backlog key promoted now lands behind the urgent keys already waiting.
        queue.enqueue_urgent(IssueKey("old-b".into()));
        queue.enqueue_urgent(IssueKey("launch-3".into()));
        queue.enqueue(IssueKey("new-plain".into()));

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

        sync.wait_for_count(6).await;
        queue.shut_down();
        handle.await.expect("worker joined");
        assert_eq!(
            *seen.lock().expect("lock"),
            vec![
                "launch-1",
                "launch-2",
                "old-b",
                "launch-3",
                "old-a",
                "new-plain"
            ]
        );
    }

    #[test]
    fn the_urgent_count_drains_so_later_urgent_keys_still_go_first() {
        let mut state = QueueState::default();
        state.add_urgent("u1".into());
        assert_eq!(state.next().as_deref(), Some("u1"));
        state.done("u1");
        state.add("plain".into());
        state.add_urgent("u2".into());
        assert_eq!(state.next().as_deref(), Some("u2"));
        assert_eq!(state.next().as_deref(), Some("plain"));
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
