//! The controller fetcher: it polls the launch list, follows the selected launch's run and its
//! session stream, and settles the ones that finished.

use crate::captured::Captured;
use crate::live::action::Msg;
use crate::live::calls;
use crate::live::shape::{
    agent_label, build_detail, captured, captured_files, clock, console, epoch, graph_fold,
    identity, landed_label, owned, pin_label, typed_log, unix_now, unlaunched,
};
use crate::live::{
    Base, DraftPack, Finished, Identity, LaunchStatus, LiveDetail, LiveDraft, LivePlaybook,
    LiveView,
};
use crate::logline::LogLine;
use crate::net::{Connection, Connector, Down, reaching, within};
use crate::pack::PackFiles;
use crate::session::RunFold;
use jiff::tz::TimeZone;
use rune::time::Instant;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

/// Console lines kept per run.
const LOG_LINES: usize = 4000;
mod dispatched;

/// Finished launches kept for the window to notify about.
pub(in crate::live) const FINISHED: usize = 32;
/// How long a live session follow that failed waits before it is tried again.
const FOLLOW_RETRY: Duration = Duration::from_secs(15);
/// How long a follow that ended cleanly, while the list still says running, waits before the
/// next one: the list catches up in that time.
const FOLLOW_REST: Duration = Duration::from_secs(4);
/// How long a replay that stopped short of what it replaces waits before it is shown anyway.
pub(in crate::live) const REPLAY_QUIET: Duration = Duration::from_secs(3);

/// The controller fetcher. One instance lives on its own worker thread, so a slow controller
/// query never delays local log tracking.
pub(in crate::live) struct Fetcher {
    pub(in crate::live) connector: Connector,
    pub(in crate::live) identity: Option<(String, Identity)>,
    pub(in crate::live) view: LiveView,
    pub(in crate::live) detail: Option<Arc<LiveDetail>>,
    pub(in crate::live) base: Option<Base>,
    /// The viewer's drafts, as last listed.
    pub(in crate::live) drafts: Vec<LiveDraft>,
    pub(in crate::live) draft: Option<Arc<DraftPack>>,
    launches: HashMap<String, CachedLaunch>,
    packs: HashMap<String, PackFiles>,
    runs: HashMap<String, CachedRun>,
    pub(in crate::live) finished: Vec<Finished>,
    finished_seq: u64,
    /// Handed to each session stream, so new lines wake the worker.
    wake: UnboundedSender<Msg>,
    /// Registered playbook ids, the ones a launch form can start.
    pub(in crate::live) registered: Vec<String>,
    pub(in crate::live) shareable: Vec<crux::client::Shared>,
    pub(in crate::live) catalog_at: Option<Instant>,
    /// The launch each playbook target run belongs to, by run id.
    run_launches: HashMap<String, String>,
    /// The call each target launch was dispatched by, as the calling runs' calls named it.
    callers: HashMap<String, crux::dto::CalledBy>,
    /// The target node whose log the window shows.
    pub(in crate::live) node_ask: Option<calls::NodeAsk>,
    /// Each target node's log as last read, and whether the node had ended then.
    node_logs: HashMap<calls::NodeAsk, (Result<String, String>, bool)>,
    /// What each run's detail says called it, by run id; `None` when nothing did.
    parents: HashMap<String, Option<calls::Parent>>,
}

/// One launch's detail, as of the run count and status the launch list showed when it was read:
/// a dispatch or a status change is what invalidates it.
struct CachedLaunch {
    runs: i64,
    status: LaunchStatus,
    detail: crux::dto::PlaybookLaunch,
}

/// One run's session fold and console output, keyed by run id. A settled run never changes, so
/// switching back to it costs no API call.
#[derive(Default)]
pub(in crate::live) struct CachedRun {
    pub(in crate::live) fold: RunFold,
    /// Session lines a stream has folded into `fold`.
    lines: usize,
    pub(in crate::live) log: Vec<LogLine>,
    settled: bool,
    source: &'static str,
    error: Option<String>,
    follow: Option<SessionFollow>,
    retry_at: Option<Instant>,
    /// A new stream replays the session and the log from their starts; what each replaces stays
    /// on screen until its replay has caught up.
    replay: Option<Replay<RunFold>>,
    log_replay: Option<Replay<Vec<LogLine>>>,
    /// Its captured files, and how many results had settled when they were listed.
    files: Option<Result<Vec<Captured>, String>>,
    files_at: usize,
}

/// What a restarted stream has replayed so far, held off screen.
struct Replay<T> {
    held: T,
    lines: usize,
    landed: Instant,
}

impl<T: Default> Replay<T> {
    fn starting(now: Instant) -> Self {
        Replay {
            held: T::default(),
            lines: 0,
            landed: now,
        }
    }

    /// Whether the replay replaces `shown` lines yet: it has reached them, its stream finished,
    /// or it went quiet short of them. A stream that failed part way replaces nothing.
    fn caught_up(&self, shown: usize, ended: Option<bool>, now: Instant) -> bool {
        if self.lines == 0 {
            return false;
        }
        self.lines >= shown
            || match ended {
                Some(finished) => finished,
                None => now.saturating_duration_since(self.landed) >= REPLAY_QUIET,
            }
    }
}

impl CachedRun {
    pub(in crate::live) fn restart(&mut self, now: Instant) {
        self.replay = Some(Replay::starting(now));
        self.log_replay = Some(Replay::starting(now));
    }

    /// Fold what a stream delivered. `ended` is whether it finished cleanly, once it has ended.
    pub(in crate::live) fn take(
        &mut self,
        lines: &[String],
        logs: &[String],
        ended: Option<bool>,
        now: Instant,
    ) {
        match self.replay.take() {
            Some(mut replay) => {
                for line in lines {
                    replay.held.apply_line(line);
                }
                if !lines.is_empty() {
                    replay.lines += lines.len();
                    replay.landed = now;
                }
                if replay.caught_up(self.lines, ended, now) {
                    self.fold = replay.held;
                    self.lines = replay.lines;
                } else {
                    self.replay = Some(replay);
                }
            }
            None => {
                for line in lines {
                    self.fold.apply_line(line);
                }
                self.lines += lines.len();
            }
        }
        let parsed = logs.iter().map(|raw| LogLine::parse(raw));
        match self.log_replay.take() {
            Some(mut replay) => {
                replay.held.extend(parsed);
                trim_log(&mut replay.held);
                if !logs.is_empty() {
                    replay.lines += logs.len();
                    replay.landed = now;
                }
                if replay.caught_up(self.log.len(), ended, now) {
                    self.log = replay.held;
                } else {
                    self.log_replay = Some(replay);
                }
            }
            None => {
                self.log.extend(parsed);
                trim_log(&mut self.log);
            }
        }
    }
}

fn trim_log(log: &mut Vec<LogLine>) {
    let excess = log.len().saturating_sub(LOG_LINES);
    log.drain(..excess);
}

/// A running run's session and pod log, streamed by [`crux::Client::follow_run`] on its own
/// thread and collected for the worker to fold. Dropping it ends the stream.
struct SessionFollow {
    lines: Arc<Mutex<Vec<String>>>,
    logs: Arc<Mutex<Vec<String>>>,
    ended: Arc<Mutex<Option<Result<(), String>>>>,
    stop: Option<oneshot::Sender<()>>,
}

impl SessionFollow {
    /// Start the stream as a task on the connection's runtime.
    fn spawn(connection: &Connection, run_id: &str, wake: UnboundedSender<Msg>) -> Self {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let logs = Arc::new(Mutex::new(Vec::new()));
        let ended = Arc::new(Mutex::new(None));
        let (stop, stopped) = oneshot::channel::<()>();
        let (sink, outcome) = (Arc::clone(&lines), Arc::clone(&ended));
        let log_sink = Arc::clone(&logs);
        let log_wake = wake.clone();
        let (client, run_id) = (connection.client.clone(), run_id.to_string());
        connection.spawn(async move {
            let mut collect = move |line: &str| {
                if let Ok(mut lines) = sink.lock() {
                    if lines.is_empty() {
                        let _ = wake.send(Msg::Streamed);
                    }
                    lines.push(line.to_string());
                }
                Ok(())
            };
            let mut collect_log = move |line: &str| {
                if let Ok(mut logs) = log_sink.lock() {
                    if logs.is_empty() {
                        let _ = log_wake.send(Msg::Streamed);
                    }
                    logs.push(line.to_string());
                }
                Ok(())
            };
            let result = tokio::select! {
                followed = client.follow_run(&run_id, &mut collect, &mut collect_log) => {
                    followed.map_err(|error| format!("{error:#}"))
                }
                _ = stopped => Ok(()),
            };
            if let Ok(mut ended) = outcome.lock() {
                *ended = Some(result);
            }
        });
        SessionFollow {
            lines,
            logs,
            ended,
            stop: Some(stop),
        }
    }

    fn drain(&self) -> Vec<String> {
        self.lines
            .lock()
            .map(|mut lines| std::mem::take(&mut *lines))
            .unwrap_or_default()
    }

    fn drain_logs(&self) -> Vec<String> {
        self.logs
            .lock()
            .map(|mut logs| std::mem::take(&mut *logs))
            .unwrap_or_default()
    }

    fn ended(&self) -> Option<Result<(), String>> {
        self.ended.lock().ok().and_then(|ended| ended.clone())
    }
}

impl Drop for SessionFollow {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

impl Fetcher {
    pub(in crate::live) fn new(connector: Connector, wake: UnboundedSender<Msg>) -> Self {
        Fetcher {
            connector,
            identity: None,
            view: LiveView::Loading,
            detail: None,
            base: None,
            drafts: Vec::new(),
            draft: None,
            launches: HashMap::new(),
            packs: HashMap::new(),
            runs: HashMap::new(),
            finished: Vec::new(),
            finished_seq: 0,
            wake,
            registered: Vec::new(),
            shareable: Vec::new(),
            catalog_at: None,
            run_launches: HashMap::new(),
            callers: HashMap::new(),
            node_ask: None,
            node_logs: HashMap::new(),
            parents: HashMap::new(),
        }
    }

    /// Re-reads the playbook and draft catalogs at most once a minute.
    pub(in crate::live) async fn refresh_catalog(&mut self) -> bool {
        if self
            .catalog_at
            .is_some_and(|at| at.elapsed() < Duration::from_secs(60))
        {
            return false;
        }
        self.catalog_at = Some(Instant::now());
        let Ok(connection) = self.connection().await else {
            return false;
        };
        let Ok(playbooks) = within(
            "the playbooks",
            connection.client.playbooks::<Vec<crux::dto::Playbook>>(),
        )
        .await
        else {
            return false;
        };
        let mut ids: Vec<String> = playbooks.into_iter().map(|p| p.id).collect();
        ids.sort();
        #[derive(serde::Deserialize)]
        struct Draft {
            id: String,
        }
        let drafts = within(
            "the drafts",
            connection.client.get::<Vec<Draft>>("/api/playbook-drafts"),
        )
        .await;
        let mut shareable: Vec<_> = ids
            .iter()
            .cloned()
            .map(crux::client::Shared::Playbook)
            .collect();
        if let Ok(mut drafts) = drafts {
            drafts.sort_by(|a, b| a.id.cmp(&b.id));
            shareable.extend(
                drafts
                    .into_iter()
                    .map(|d| crux::client::Shared::Draft(d.id)),
            );
        }
        let changed = ids != self.registered || shareable != self.shareable;
        self.registered = ids;
        self.shareable = shareable;
        changed
    }

    /// Records each launch the new list shows stopped that the old list showed running.
    pub(in crate::live) fn note_finished(&mut self, next: &[LivePlaybook]) {
        let was_running: HashSet<&str> = self
            .view
            .launches()
            .iter()
            .filter(|launch| launch.status.is_running())
            .map(|launch| launch.key.as_str())
            .collect();
        for launch in next {
            if !launch.status.is_running() && was_running.contains(launch.key.as_str()) {
                self.finished_seq += 1;
                self.finished.push(Finished {
                    seq: self.finished_seq,
                    key: launch.key.clone(),
                    playbook: launch.id.clone(),
                    status: launch.status.clone(),
                    by: launch.by.clone(),
                    why: launch.why.clone(),
                    cost: launch.cost,
                });
            }
        }
        let excess = self.finished.len().saturating_sub(FINISHED);
        self.finished.drain(..excess);
    }

    pub(in crate::live) fn view(&self) -> &LiveView {
        &self.view
    }

    /// Read launch `key`'s run as the comparison base, once: a stopped run never changes, so a
    /// base already read stays. A launch the list does not show yet is tried again later.
    pub(in crate::live) async fn refresh_base(&mut self, key: Option<&str>) -> bool {
        let Some(key) = key else {
            let changed = self.base.is_some();
            self.base = None;
            return changed;
        };
        if self
            .base
            .as_ref()
            .is_some_and(|base| base.key == key && base.error.is_none())
        {
            return false;
        }
        let Some(launch) = self.view.launch(key).cloned() else {
            return false;
        };
        let run_id = self
            .launch_detail(&launch)
            .await
            .and_then(|record| record.runs.first().map(|run| run.run_id.clone()));
        let connection = self.connection().await.ok();
        let next = match (&run_id, &connection) {
            (Some(run_id), Some(connection)) => {
                let run = self.runs.entry(run_id.clone()).or_default();
                if !run.settled {
                    let session =
                        within("the session", connection.client.run_session(run_id)).await;
                    settle(run, connection, run_id, session).await;
                }
                Base {
                    key: key.to_string(),
                    fold: run.fold.clone(),
                    error: run.error.clone(),
                }
            }
            (None, _) => Base {
                key: key.to_string(),
                fold: RunFold::default(),
                error: Some("that launch never dispatched a run".to_string()),
            },
            (Some(_), None) => return false,
        };
        let changed = self.base.as_ref() != Some(&next);
        self.base = Some(next);
        changed
    }

    pub(in crate::live) fn any_running(&self) -> bool {
        self.view
            .launches()
            .iter()
            .any(|launch| launch.status.is_running())
    }

    /// Whether the selected launch's run can still change: it is running, or it stopped and its
    /// stored session has not been read yet.
    pub(in crate::live) fn selected_live(&self, selected: Option<&str>) -> bool {
        let Some(launch) = self.launch(selected) else {
            return false;
        };
        launch.status.is_running()
            || self
                .launches
                .get(&launch.key)
                .and_then(|cached| cached.detail.runs.first())
                .and_then(|newest| self.runs.get(&newest.run_id))
                .is_some_and(|run| !run.settled && run.error.is_none())
    }

    fn launch(&self, selected: Option<&str>) -> Option<&LivePlaybook> {
        selected.and_then(|key| self.view.launch(key))
    }

    /// A failed refresh keeps the last list it had and says so; with no list yet, the failure
    /// is all there is to show.
    pub(in crate::live) fn unavailable(&mut self, error: Down) -> bool {
        match &mut self.view {
            LiveView::Ready { stale, .. } => {
                let said = format!("Refresh failed; showing the last list. {}", error.why());
                let changed = stale.as_deref() != Some(said.as_str());
                *stale = Some(said);
                changed
            }
            view => {
                *view = LiveView::Unavailable(error);
                self.detail = None;
                true
            }
        }
    }

    pub(in crate::live) async fn connection(&self) -> Result<Connection, String> {
        self.connector.connect().await
    }

    pub(in crate::live) async fn refresh_list(&mut self) -> Result<bool, Down> {
        if self.identity.is_none() {
            let connection = self.connection().await?;
            let whoami = reaching("whoami", connection.client.whoami()).await?;
            let endpoint = connection.client.base().to_string();
            self.identity = Some((endpoint, identity(whoami)));
        }
        let (endpoint, identity) = self.identity.clone().unwrap_or_default();

        let connection = self.connection().await?;
        let (launches, drafts) = tokio::join!(
            reaching(
                "the launch list",
                connection.client.playbook_runs(None, None),
            ),
            within("the drafts", connection.client.playbook_drafts()),
        );
        let launches: Vec<crux::dto::PlaybookRun> = launches?;
        if let Ok(drafts) = drafts {
            self.drafts = owned(drafts, &identity);
        }
        let zone = TimeZone::system();
        let mut launches: Vec<LivePlaybook> = launches
            .into_iter()
            .map(|launch| {
                let agent = agent_label(&launch);
                LivePlaybook {
                    id: launch.playbook,
                    key: launch.key,
                    status: LaunchStatus::parse(&launch.status, launch.parked_reason.as_deref()),
                    cost: launch.cost_usd,
                    cap: launch.max_cost,
                    by: launch.created_by.unwrap_or_default(),
                    agent,
                    why: launch
                        .parked_reason
                        .or(launch.secrets_refusal)
                        .filter(|why| !why.is_empty()),
                    created: clock(&launch.created_at, &zone),
                    landed: epoch(&launch.created_at).unwrap_or_default(),
                    landed_at: landed_label(&launch.created_at, &zone),
                    origin: launch.origin,
                    max_time: launch.max_time,
                    runs: launch.runs,
                    description: launch.description.filter(|text| !text.is_empty()),
                    called_by: launch.called_by,
                }
            })
            .collect();
        for launch in &mut launches {
            if launch.called_by.is_none() {
                launch.called_by = self.callers.get(&launch.key).cloned();
            }
        }
        launches.sort_by_key(|launch| std::cmp::Reverse(launch.landed));

        self.note_finished(&launches);
        let now = unix_now();
        let drafts = unlaunched(&self.drafts, &launches);
        let next = LiveView::Ready {
            endpoint,
            identity,
            now: now - now % 60,
            launches,
            drafts,
            stale: self
                .connector
                .offline()
                .then(|| "Offline; showing what the controller last answered".to_string()),
        };
        let changed = next != self.view;
        self.view = next;
        Ok(changed)
    }

    /// Read `shown`'s pack, once: a draft save never changes. A read that failed is tried again.
    pub(in crate::live) async fn refresh_draft(&mut self, shown: Option<&(String, i64)>) -> bool {
        let Some((draft, version)) = shown else {
            return self.draft.take().is_some();
        };
        if self.draft.as_ref().is_some_and(|read| {
            read.draft == *draft && read.version == *version && read.pack.is_ok()
        }) {
            return false;
        }
        let next = DraftPack {
            draft: draft.clone(),
            version: *version,
            pack: self.pack(draft, Some(*version)).await,
        };
        let changed = self.draft.as_deref() != Some(&next);
        self.draft = Some(Arc::new(next));
        changed
    }

    /// The launch's own record, re-read when its run count or status moved.
    pub(in crate::live) async fn launch_detail(
        &mut self,
        launch: &LivePlaybook,
    ) -> Option<crux::dto::PlaybookLaunch> {
        if let Some(cached) = self.launches.get(&launch.key)
            && cached.runs == launch.runs
            && cached.status == launch.status
        {
            return Some(cached.detail.clone());
        }
        let connection = self.connection().await.ok()?;
        let detail: crux::dto::PlaybookLaunch =
            within("the launch", connection.client.playbook_run(&launch.key))
                .await
                .ok()?;
        self.launches.insert(
            launch.key.clone(),
            CachedLaunch {
                runs: launch.runs,
                status: launch.status.clone(),
                detail: detail.clone(),
            },
        );
        Some(detail)
    }

    /// The pack a launch ran: the draft version it was launched from, or the registered pin. A
    /// draft version never changes; a pin is read once per session.
    pub(in crate::live) async fn pack(
        &mut self,
        playbook: &str,
        draft_version: Option<i64>,
    ) -> Result<PackFiles, String> {
        let key = match draft_version {
            Some(version) => format!("{playbook}@draft-{version}"),
            None => format!("{playbook}@pin"),
        };
        if let Some(pack) = self.packs.get(&key) {
            return Ok(pack.clone());
        }
        let connection = self.connection().await?;
        let mut pack = match draft_version {
            Some(version) => {
                let draft: crux::dto::DraftFiles = within(
                    "the draft",
                    connection.client.draft_files(playbook, Some(version)),
                )
                .await?;
                let by = draft
                    .saved_by
                    .map(|by| format!(", saved by {by}"))
                    .unwrap_or_default();
                PackFiles::new(
                    format!("draft {playbook} v{version}{by}"),
                    draft
                        .files
                        .into_iter()
                        .map(|(path, text)| (path, Some(text))),
                )
            }
            None => {
                let registered: crux::dto::PlaybookDetail =
                    within("the playbook", connection.client.playbook(playbook)).await?;
                let mut pack = PackFiles::new(
                    pin_label(&registered.playbook),
                    registered
                        .files
                        .into_iter()
                        .map(|(path, text)| (path, Some(text))),
                );
                if let crux::dto::PlaybookSource::Git { repo, path, .. } =
                    &registered.playbook.source
                {
                    pack.on_git(repo, &registered.playbook.rev, path);
                }
                pack
            }
        };
        if let Some(dir) = crate::pack::cache_dir(&key) {
            let copied = crate::net::blocking(move || {
                let _ = pack.copy_to(&dir);
                pack
            });
            pack = copied
                .await
                .ok_or_else(|| "copying the pack stopped early".to_string())?;
        }
        self.packs.insert(key, pack.clone());
        Ok(pack)
    }

    /// Rebuild the detail pane for the selected launch. A running run streams its session and
    /// re-reads its console; a stopped one reads its stored session once and is then settled.
    /// `poll` also re-reads the console; a streamed batch only folds the new session lines.
    pub(in crate::live) async fn refresh_detail(
        &mut self,
        selected: Option<&str>,
        poll: bool,
    ) -> bool {
        let Some(launch) = self.launch(selected).cloned() else {
            let changed = self.detail.is_some();
            self.detail = None;
            return changed;
        };
        let running = launch.status.is_running();
        let record = match (poll, &self.detail) {
            (false, Some(detail)) if detail.key == launch.key => detail.launch.clone(),
            _ => self.launch_detail(&launch).await,
        };
        let run_id = record
            .as_ref()
            .and_then(|record| record.runs.first())
            .map(|run| run.run_id.clone());
        let pack = match (poll, &self.detail) {
            (false, Some(detail)) if detail.key == launch.key => detail.pack.clone(),
            _ => {
                let version = record
                    .as_ref()
                    .and_then(|record| record.launch.draft_version);
                self.pack(&launch.id, version).await
            }
        };
        for (id, run) in &mut self.runs {
            if Some(id) != run_id.as_ref() {
                run.follow = None;
            }
        }

        let Ok(connection) = self.connection().await else {
            return false;
        };
        let connection = &connection;
        let wake = self.wake.clone();
        let (fold, log, source, error, streaming, files) = match &run_id {
            Some(run_id) => {
                let run = self.runs.entry(run_id.clone()).or_default();
                let mut read_files = false;
                if running {
                    follow_session(run, connection, run_id, wake);
                    if poll
                        && run.log_replay.is_some()
                        && let Ok(log) = console(connection, run_id).await
                        && log.len() >= run.log.len().min(1)
                    {
                        run.log = log;
                    }
                } else if !run.settled {
                    run.follow = None;
                    let client = &connection.client;
                    let (session, log, files) = tokio::join!(
                        within("the session", client.run_session(run_id)),
                        within("the console", client.run_log(run_id)),
                        within("the captured files", client.run_files(run_id)),
                    );
                    settle(run, connection, run_id, session).await;
                    if let Ok(log) = log.map(typed_log)
                        && log.len() >= run.log.len().min(1)
                    {
                        run.log = log;
                    }
                    run.files = Some(files.map(captured));
                    run.files_at = settled_results(&run.fold);
                    read_files = true;
                }
                let results = settled_results(&run.fold);
                let retry = poll && matches!(run.files, Some(Err(_)));
                if !read_files && (run.files.is_none() || run.files_at != results || retry) {
                    run.files = Some(captured_files(connection, run_id).await);
                    run.files_at = results;
                }
                (
                    run.fold.clone(),
                    run.log.clone(),
                    run.source,
                    run.error.clone(),
                    run.follow.is_some(),
                    run.files.clone(),
                )
            }
            None => (RunFold::default(), Vec::new(), "", None, false, None),
        };
        let calls = self
            .calls(connection, &launch, run_id.as_deref(), &fold, poll)
            .await;
        let mut next = build_detail(&launch, fold, log);
        next.source = source;
        next.error = error;
        next.launch = record;
        next.pack = pack;
        next.streaming = streaming;
        next.run_id = run_id;
        next.files = files;
        next.log.extend(calls::lifecycle(&next.fold.tasks, &calls));
        next.calls = calls;
        next.node_log = self.node_log(connection, &next, poll).await;
        let learned = self
            .learn_caller(connection, &launch, next.run_id.as_deref())
            .await;
        let changed = learned || self.detail.as_deref() != Some(&next);
        self.detail = Some(Arc::new(next));
        changed
    }
}

/// Keep a session stream on a running run and fold what it has delivered. A stream that ended
/// cleanly is started again on the next read; one that failed waits [`FOLLOW_RETRY`].
fn follow_session(
    run: &mut CachedRun,
    connection: &Connection,
    run_id: &str,
    wake: UnboundedSender<Msg>,
) {
    if let Some(follow) = &run.follow {
        let ended = follow.ended();
        let (lines, logs) = (follow.drain(), follow.drain_logs());
        let finished = ended.as_ref().map(Result::is_ok);
        run.take(&lines, &logs, finished, Instant::now());
        match ended {
            None => return,
            Some(Ok(())) => run.retry_at = Some(Instant::now() + FOLLOW_REST),
            Some(Err(error)) => {
                run.error = Some(error);
                run.retry_at = Some(Instant::now() + FOLLOW_RETRY);
            }
        }
        run.follow = None;
        return;
    }
    if run.retry_at.is_some_and(|at| Instant::now() < at) {
        return;
    }
    run.restart(Instant::now());
    run.source = "session";
    run.error = None;
    run.follow = Some(SessionFollow::spawn(connection, run_id, wake));
}

/// Fold a stopped run once from its stored `session`, or from the graph the controller keeps
/// when no session was stored.
async fn settle(
    run: &mut CachedRun,
    connection: &Connection,
    run_id: &str,
    session: Result<String, String>,
) {
    match session {
        Ok(text) => {
            let mut fold = RunFold::default();
            for line in text.lines() {
                fold.apply_line(line);
            }
            run.fold = fold;
            run.source = "session";
            run.settled = true;
            run.error = None;
        }
        Err(session) => match within("the graph", connection.client.graph::<Value>(run_id)).await {
            Ok(graph) => {
                run.fold = graph_fold(&graph);
                run.source = "graph";
                run.settled = true;
                run.error = None;
            }
            Err(_) => run.error = Some(session),
        },
    }
}

/// Results settled so far, tasks and fan-out instances: each can add captured files.
fn settled_results(fold: &RunFold) -> usize {
    fold.tasks
        .iter()
        .map(|task| {
            usize::from(task.result.is_some())
                + task
                    .instances
                    .iter()
                    .filter(|instance| instance.result.is_some())
                    .count()
        })
        .sum()
}
