//! The window's handle to the live worker, and the worker's task.

use crate::captured::Captured;
use crate::draft::Working;
use crate::live::action::{Action, Msg, Reply, Reporting};
use crate::live::fetcher::Fetcher;
use crate::live::{
    Base, DraftPack, Dropped, Finished, Launched, LiveDetail, LiveView, Prepared, Reran, Snapshot,
    Synced, Target,
};
use crate::net::{Connector, Down, Task};
use crate::report::WantedRuns;
use crate::revisions::Wanted;
use rune::Waker;
use rune::time::Instant;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// How long streamed session lines gather before they are folded and shown, so a burst of agent
/// output is one redraw rather than dozens.
const STREAM_BATCH: Duration = Duration::from_millis(40);

/// What one pass of the poll loop read.
struct Polled {
    view: Arc<LiveView>,
    detail: Option<Arc<LiveDetail>>,
    base: Option<Base>,
    draft: Option<Arc<DraftPack>>,
    finished: Vec<Finished>,
    registered: Vec<String>,
}

/// The UI-side handle to the live worker. Dropping it ends the poll loop and its streams.
///
/// ```text
/// window ── select, compare ──▶ poll loop ──── list, detail ──▶ ┐
///        ── launch, save, … ──▶ a task each ── its reply ─────▶ ┴ snapshot ──▶ window
/// ```
pub struct Client {
    tx: UnboundedSender<Msg>,
    shared: Arc<Mutex<Shared>>,
    connector: Connector,
    dirs: Dirs,
    pub(in crate::live) reporting: Arc<Reporting>,
    /// The poll loop.
    _poll: Option<Task>,
}

/// Where the worker's actions keep files.
#[derive(Clone)]
pub(in crate::live) struct Dirs {
    /// Draft working copies.
    pub(in crate::live) drafts: Option<PathBuf>,
    /// Copies of fetched captured files.
    pub(in crate::live) files: Option<PathBuf>,
}

struct Shared {
    snapshot: Snapshot,
    version: u64,
    waker: Option<Waker>,
    /// Replies answered so far; each takes the next number as its `seq`.
    replies: u64,
}

impl Shared {
    fn polled(&mut self, polled: Polled) {
        self.snapshot.view = polled.view;
        self.snapshot.detail = polled.detail;
        self.snapshot.base = polled.base;
        self.snapshot.draft = polled.draft;
        self.snapshot.finished = polled.finished;
        self.snapshot.registered = polled.registered;
    }

    fn answered(&mut self, reply: Reply) {
        self.replies += 1;
        let seq = self.replies;
        match reply {
            Reply::Prepared { target, result } => {
                self.snapshot.prepared = Some(Prepared {
                    seq,
                    target,
                    result,
                });
            }
            Reply::Launched { playbook, result } => {
                self.snapshot.launched = Some(Launched {
                    seq,
                    playbook,
                    result,
                });
            }
            Reply::Reran { run, from, result } => {
                self.snapshot.reran = Some(Reran {
                    seq,
                    run,
                    from,
                    result,
                });
            }
            Reply::Synced { draft, result } => {
                self.snapshot.synced = Some(Synced { seq, draft, result });
            }
            Reply::Dropped { dir, draft, result } => {
                self.snapshot.dropped = Some(Dropped {
                    seq,
                    dir,
                    draft,
                    result,
                });
            }
            Reply::File(opened) => self.snapshot.file = Some(opened),
            Reply::Revisions(revisions) => self.snapshot.revisions = Some(Arc::new(revisions)),
            Reply::Report(built) => self.snapshot.report = Some(Arc::new(built)),
            Reply::RunsReport(built) => self.snapshot.runs_report = Some(Arc::new(built)),
        }
    }
}

impl Client {
    pub fn spawn(connector: Connector, drafts: Option<PathBuf>) -> Self {
        let dirs = Dirs {
            drafts,
            files: crate::captured::cache_dir(),
        };
        Client::spawn_with(connector, dirs)
    }

    /// Keep working copies under `drafts` from the next pull or save on.
    pub fn set_drafts(&mut self, drafts: Option<PathBuf>) {
        self.dirs.drafts = drafts;
    }

    pub(in crate::live) fn spawn_with(connector: Connector, dirs: Dirs) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Mutex::new(Shared {
            snapshot: Snapshot::default(),
            version: 0,
            waker: None,
            replies: 0,
        }));
        let poll = match connector.runtime() {
            Ok(runtime) => Some(Task::spawn(
                &runtime,
                run_client(connector.clone(), rx, tx.clone(), Arc::clone(&shared)),
            )),
            Err(why) => {
                publish(&shared, |shared| {
                    shared.snapshot.view = Arc::new(LiveView::Unavailable(Down::Unreached(why)));
                });
                None
            }
        };
        Client {
            tx,
            shared,
            connector,
            dirs,
            reporting: Arc::default(),
            _poll: poll,
        }
    }

    pub fn set_waker(&self, waker: Waker) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.waker = Some(waker);
        }
    }

    /// Run `action` as a task of its own and publish its reply when it lands.
    fn start(&self, action: Action) {
        let runtime = match self.connector.runtime() {
            Ok(runtime) => runtime,
            Err(why) => {
                let reply = action.failed(why, &self.dirs);
                publish(&self.shared, |shared| shared.answered(reply));
                return;
            }
        };
        let (connector, dirs) = (self.connector.clone(), self.dirs.clone());
        let (shared, tx) = (Arc::clone(&self.shared), self.tx.clone());
        let reporting = Arc::clone(&self.reporting);
        runtime.spawn(async move {
            let launches = action.launches();
            let reply = match connector.connect().await {
                Ok(connection) => action.run(connection, &dirs, &reporting).await,
                Err(why) => action.failed(why, &dirs),
            };
            publish(&shared, |shared| shared.answered(reply));
            if launches {
                let _ = tx.send(Msg::Refresh);
            }
        });
    }

    /// Read `target`'s params schema and the caps, for a launch form.
    pub fn prepare(&self, target: Target) {
        self.start(Action::Prepare(target));
    }

    /// Launch `target`; the result comes back in a snapshot.
    pub fn launch(&self, target: Target, body: Value) {
        self.start(Action::Launch { target, body });
    }

    pub fn drop_pack(&self, dir: PathBuf, max_cost: f64, max_time: String) {
        self.start(Action::Drop {
            dir,
            max_cost,
            max_time,
        });
    }

    /// Ask what rerunning `run` from `from` would reuse.
    pub fn preview_rerun(&self, run: &str, from: Vec<String>) {
        self.start(Action::Rerun {
            run: run.to_string(),
            from,
        });
    }

    /// Pull `draft` into its working copy, creating it from `template` when it is missing.
    pub fn pull(&self, draft: &str, template: Option<&str>) {
        self.start(Action::Pull {
            draft: draft.to_string(),
            template: template.map(ToString::to_string),
        });
    }

    /// Save `working` as its draft's next version.
    pub fn save(&self, working: Working) {
        self.start(Action::Save(working));
    }

    /// Connect again with whatever crux's config now holds.
    pub fn reconnect(&self) {
        let _ = self.tx.send(Msg::Reconnect);
    }

    /// Which live playbook's graph to keep current; `None` stops fetching graphs.
    pub fn select(&self, playbook: Option<&str>) {
        let _ = self.tx.send(Msg::Select(playbook.map(ToString::to_string)));
    }

    pub fn compare(&self, base: Option<&str>) {
        let _ = self.tx.send(Msg::Compare(base.map(ToString::to_string)));
    }

    /// Which draft save's pack to read; `None` stops.
    pub fn draft(&self, shown: Option<(String, i64)>) {
        let _ = self.tx.send(Msg::Draft(shown));
    }

    /// Fetch the file `run` captured under `key`. Its copy on disk is used only when the run
    /// has `settled` and the copy is the `size` the listing gives.
    pub fn file(&self, run: &str, key: &str, size: u64, settled: bool) {
        self.start(Action::File {
            run: run.to_string(),
            key: key.to_string(),
            size,
            settled,
        });
    }

    pub fn revisions(&self, wanted: Wanted) {
        self.start(Action::Revisions(wanted));
    }

    /// Build the report `wanted` names from `plugin`, the pack's plugin as its text, and the
    /// run's `listed` captured files.
    pub fn report(&self, wanted: crate::report::Wanted, plugin: String, listed: Vec<Captured>) {
        self.start(Action::Report {
            wanted,
            plugin,
            listed,
        });
    }

    /// Build the runs view `wanted` names from `plugin`, the newest launch's plugin as its text.
    pub fn runs_report(&self, wanted: WantedRuns, plugin: String) {
        self.start(Action::RunsReport { wanted, plugin });
    }

    pub fn take(&self, seen: u64) -> Option<(u64, Snapshot)> {
        let shared = self.shared.lock().ok()?;
        (shared.version > seen).then(|| (shared.version, shared.snapshot.clone()))
    }
}

fn publish(shared: &Mutex<Shared>, change: impl FnOnce(&mut Shared)) {
    let waker = match shared.lock() {
        Ok(mut shared) => {
            change(&mut shared);
            shared.version += 1;
            shared.waker.clone()
        }
        Err(_) => return,
    };
    if let Some(waker) = waker {
        waker();
    }
}

async fn run_client(
    connector: Connector,
    mut rx: UnboundedReceiver<Msg>,
    wake: UnboundedSender<Msg>,
    shared: Arc<Mutex<Shared>>,
) {
    let mut fetcher = Fetcher::new(connector, wake);
    let mut selected: Option<String> = None;
    let mut view = Arc::new(LiveView::default());
    let mut compare: Option<String> = None;
    let mut draft: Option<(String, i64)> = None;
    let mut next_list = Instant::now();
    let mut next_detail = Instant::now();
    let mut next_stream: Option<Instant> = None;
    loop {
        let mut force = false;
        let due = next_stream.map_or(next_list.min(next_detail), |at| {
            at.min(next_list).min(next_detail)
        });
        let wait = due
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(500));
        match crate::net::timeout(wait, rx.recv()).await {
            Ok(Some(Msg::Select(playbook))) => {
                selected = playbook;
                next_detail = Instant::now();
            }
            Ok(Some(Msg::Streamed)) => {
                next_stream.get_or_insert_with(|| Instant::now() + STREAM_BATCH);
            }
            Ok(Some(Msg::Refresh)) => next_list = Instant::now(),
            Ok(Some(Msg::Compare(base))) => {
                compare = base;
                force = fetcher.refresh_base(compare.as_deref()).await;
            }
            Ok(Some(Msg::Draft(shown))) => {
                draft = shown;
                force = fetcher.refresh_draft(draft.as_ref()).await;
            }
            Ok(Some(Msg::Reconnect)) => {
                fetcher.connector.reset();
                fetcher.identity = None;
                fetcher.drafts.clear();
                fetcher.registered_at = None;
                fetcher.view = LiveView::Loading;
                next_list = Instant::now();
                force = true;
            }
            Ok(None) => return,
            Err(_) => {}
        }

        let now = Instant::now();
        let mut changed = force;
        if now >= next_list {
            match fetcher.refresh_list().await {
                Ok(list_changed) => changed |= list_changed,
                Err(error) => changed |= fetcher.unavailable(error),
            }
            changed |= fetcher.refresh_registered().await;
            changed |= fetcher.refresh_base(compare.as_deref()).await;
            changed |= fetcher.refresh_draft(draft.as_ref()).await;
            next_list = now
                + if fetcher.any_running() || !matches!(fetcher.view, LiveView::Ready { .. }) {
                    Duration::from_secs(2)
                } else {
                    Duration::from_secs(8)
                };
            next_detail = now;
        }
        if now >= next_detail {
            changed |= fetcher.refresh_detail(selected.as_deref(), true).await;
            next_detail = now
                + if fetcher.selected_live(selected.as_deref()) {
                    Duration::from_secs(2)
                } else {
                    Duration::from_secs(60)
                };
            next_stream = None;
        } else if next_stream.is_some_and(|at| now >= at) {
            changed |= fetcher.refresh_detail(selected.as_deref(), false).await;
            next_stream = None;
        }
        if changed {
            if *view != *fetcher.view() {
                view = Arc::new(fetcher.view().clone());
            }
            let polled = Polled {
                view: Arc::clone(&view),
                detail: fetcher.detail.clone(),
                base: fetcher.base.clone(),
                draft: fetcher.draft.clone(),
                finished: fetcher.finished.clone(),
                registered: fetcher.registered.clone(),
            };
            publish(&shared, |shared| shared.polled(polled));
        }
    }
}
