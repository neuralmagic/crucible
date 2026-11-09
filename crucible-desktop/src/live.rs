//! The live controller view: registered playbooks and every launch of them, read through the
//! `crux` client library.
//!
//! The controller and its key come from the profile in use; see [`crate::profile`].

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crucible_contract::session::SessionEvent;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde_json::Value;
use tokio::sync::oneshot;

use rune::Waker;

use crate::draft::Working;
use crate::filter::LaunchFilter;
use crate::httpcache::{CachedTransport, Store};
use crate::logline::LogLine;
use crate::model::Verdict;
use crate::pack::PackFiles;
use crate::session::RunFold;

const TIMEOUT: Duration = Duration::from_secs(25);
/// How long streamed session lines gather before they are folded and shown, so a burst of agent
/// output is one redraw rather than dozens.
const STREAM_BATCH: Duration = Duration::from_millis(40);
/// Console lines kept per run.
const LOG_LINES: usize = 4000;
/// Finished launches kept for the window to notify about.
const FINISHED: usize = 32;
/// How long a live session follow that failed waits before it is tried again.
const FOLLOW_RETRY: Duration = Duration::from_secs(15);
/// How long a follow that ended cleanly, while the list still says running, waits before the
/// next one: the list catches up in that time.
const FOLLOW_REST: Duration = Duration::from_secs(4);

#[derive(Clone, Default, PartialEq)]
pub enum LiveView {
    #[default]
    Loading,
    Unavailable(String),
    Ready {
        endpoint: String,
        identity: Identity,
        now: i64,
        /// Every launch the controller returned, newest first.
        launches: Vec<LivePlaybook>,
        /// Why the newest refresh failed, while this list is the last one that did not.
        stale: Option<String>,
    },
}

impl LiveView {
    pub fn launches(&self) -> &[LivePlaybook] {
        match self {
            LiveView::Ready { launches, .. } => launches,
            _ => &[],
        }
    }

    pub fn launch(&self, key: &str) -> Option<&LivePlaybook> {
        self.launches().iter().find(|launch| launch.key == key)
    }

    pub fn launches_of<'a>(&'a self, playbook: &str) -> impl Iterator<Item = &'a LivePlaybook> {
        self.launches()
            .iter()
            .filter(move |launch| launch.id == playbook)
    }

    /// Who is looking, and when, for the owner and landed filters.
    fn viewer(&self) -> (&str, i64) {
        match self {
            LiveView::Ready { identity, now, .. } => (identity.user.as_str(), *now),
            _ => ("", 0),
        }
    }

    /// The launches the filter keeps, newest first.
    pub fn kept(&self, filter: &LaunchFilter) -> Vec<&LivePlaybook> {
        let (me, now) = self.viewer();
        self.launches()
            .iter()
            .filter(|launch| filter.keeps(launch, me, now))
            .collect()
    }

    /// Each playbook's newest launch the filter keeps: running ones first, then newest first.
    pub fn playbooks(&self, filter: &LaunchFilter) -> Vec<&LivePlaybook> {
        let mut seen = HashSet::new();
        let mut rows: Vec<&LivePlaybook> = self
            .kept(filter)
            .into_iter()
            .filter(|launch| seen.insert(launch.id.as_str()))
            .collect();
        rows.sort_by_key(|row| row.status != "running");
        rows
    }

    pub fn playbook_count(&self) -> usize {
        self.launches()
            .iter()
            .map(|launch| launch.id.as_str())
            .collect::<HashSet<_>>()
            .len()
    }

    /// The launch a selection shows: the pinned launch while the list has it, otherwise the
    /// playbook's newest launch the filter keeps, otherwise its newest launch at all.
    pub fn resolve(
        &self,
        playbook: &str,
        pinned: Option<&str>,
        filter: &LaunchFilter,
    ) -> Option<&LivePlaybook> {
        let (me, now) = self.viewer();
        pinned
            .and_then(|key| self.launch(key))
            .or_else(|| {
                self.launches_of(playbook)
                    .find(|launch| filter.keeps(launch, me, now))
            })
            .or_else(|| self.launches_of(playbook).next())
    }
}

/// The controller's answer to `crux whoami`: who the credential in use is.
#[derive(Clone, Default, PartialEq)]
pub struct Identity {
    pub user: String,
    pub role: String,
    /// What the user may own things as: themself, then each of their teams.
    pub owners: Vec<String>,
}

impl Identity {
    pub fn label(&self) -> String {
        if self.role.is_empty() {
            self.user.clone()
        } else {
            format!("{} · {}", self.user, self.role)
        }
    }
}

/// One launch of a registered playbook.
#[derive(Clone, PartialEq)]
pub struct LivePlaybook {
    pub id: String,
    pub key: String,
    pub status: String,
    pub cost: Option<f64>,
    pub cap: f64,
    pub by: String,
    pub agent: String,
    pub why: Option<String>,
    pub created: String,
    pub origin: String,
    pub max_time: String,
    pub runs: i64,
    pub description: Option<String>,
    pub landed: i64,
    pub landed_at: String,
}

impl LivePlaybook {
    /// A case-insensitive match on the fields a reader can see in the row.
    pub fn matches(&self, query: &str) -> bool {
        let fields = [
            self.id.as_str(),
            self.status.as_str(),
            self.by.as_str(),
            self.agent.as_str(),
            self.origin.as_str(),
        ];
        fields
            .iter()
            .any(|field| field.to_ascii_lowercase().contains(query))
    }

    pub fn verdict(&self) -> Verdict {
        match self.status.as_str() {
            "running" => Verdict::Running,
            "done" => Verdict::Valid,
            "error" => Verdict::Error,
            _ => Verdict::Unfinished,
        }
    }
}

/// What the newest run of one launch did.
#[derive(Clone, PartialEq)]
pub struct LiveDetail {
    pub key: String,
    /// The run's session is being streamed as it is written.
    pub streaming: bool,
    pub fold: RunFold,
    pub spent: f64,
    pub verdict: Verdict,
    pub passed: usize,
    pub failed: usize,
    pub log: Vec<LogLine>,
    /// Where the tasks came from: the stored or streamed session, or the graph standing in.
    pub source: &'static str,
    /// Why the run could not be read, when it could not.
    pub error: Option<String>,
    /// What it was launched with and where its engine went.
    pub launch: Option<crux::dto::PlaybookLaunch>,
    /// The workflow pack it ran, or why it could not be read.
    pub pack: Result<PackFiles, String>,
}

/// The launch a run is compared against: its tasks as its stored session left them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Base {
    pub key: String,
    pub fold: RunFold,
    /// Why its run could not be read, when it could not.
    pub error: Option<String>,
}

/// The controller's `parked` token is always a launch error on playbook runs; relabeled here
/// only, the wire token is unchanged.
fn shown_status(status: &str) -> &str {
    match status {
        "parked" => "error",
        status => status,
    }
}

/// What the window and the session streams tell the worker.
enum Msg {
    /// Which launch to keep current; `None` stops reading details.
    Select(Option<String>),
    /// A session stream delivered lines.
    Streamed,
    /// Read a target's params schema and the admin caps, for a launch form.
    Prepare(Target),
    /// Launch a target with this body (`params`, `max_cost`, `max_time`, maybe `rerun`).
    Launch { target: Target, body: Value },
    /// What rerunning a finished run from these tasks would reuse.
    Rerun { run: String, from: Vec<String> },
    /// Pull a draft's newest save into its working copy, creating the draft from `template`
    /// first when it does not exist.
    Pull {
        draft: String,
        template: Option<String>,
    },
    /// Save a working copy as its draft's next version.
    Save(Working),
    /// Push a pack folder as the next version of the draft named after it, creating the draft on
    /// first drop, and test-fire it under these ceilings.
    Drop {
        dir: std::path::PathBuf,
        max_cost: f64,
        max_time: String,
    },
    /// Drop the connection and who it named, and read crux's config afresh.
    Reconnect,
    /// Which launch to read as the base of a comparison; `None` stops.
    Compare(Option<String>),
}

/// What a launch form starts: a registered playbook, or a draft's newest save.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Playbook(String),
    Draft(String),
}

impl Target {
    pub fn id(&self) -> &str {
        match self {
            Target::Playbook(id) | Target::Draft(id) => id,
        }
    }
}

/// What a launch form is filled against.
#[derive(Clone, Debug, PartialEq)]
pub struct Form {
    pub schema: Value,
    /// A draft's schema digest, so a save that lands meanwhile refuses the launch.
    pub digest: Option<String>,
    /// The admin caps: dollars and a duration such as `3h`.
    pub max_cost: f64,
    pub max_time: String,
}

/// A launch form's schema and the caps its ceilings must stay under.
#[derive(Clone, Debug, PartialEq)]
pub struct Prepared {
    pub seq: u64,
    pub target: Target,
    pub result: Result<Form, String>,
}

/// What a launch came back with: the new launch's key, or why it was refused.
#[derive(Clone, Debug, PartialEq)]
pub struct Launched {
    pub seq: u64,
    pub playbook: String,
    pub result: Result<String, String>,
}

/// What a pack folder dropped on the app came to: its draft, and the launch key or why not.
#[derive(Clone, Debug)]
pub struct Dropped {
    pub seq: u64,
    pub dir: std::path::PathBuf,
    pub draft: String,
    pub result: Result<String, String>,
}

/// What a rerun would do, for the form asking for one.
#[derive(Clone, Debug, PartialEq)]
pub struct Reran {
    pub seq: u64,
    pub run: String,
    pub from: Vec<String>,
    pub result: Result<crux::dto::RerunPreview, String>,
}

/// A working copy pulled or saved, and what the controller said of the save.
#[derive(Clone, Debug, PartialEq)]
pub struct Synced {
    pub seq: u64,
    pub draft: String,
    pub result: Result<(Working, Option<String>), String>,
}

/// A launch that stopped running, for the window to announce. `seq` only grows.
#[derive(Clone, Debug, PartialEq)]
pub struct Finished {
    pub seq: u64,
    /// The launch's key.
    pub key: String,
    pub playbook: String,
    pub status: String,
    pub by: String,
    pub why: Option<String>,
    pub cost: Option<f64>,
}

/// A folder's name as a draft id: lowercase letters, digits and single dashes.
pub fn draft_id(dir: &std::path::Path) -> Option<String> {
    let name = dir.file_name()?.to_string_lossy().to_lowercase();
    let mut id = String::new();
    for c in name.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            id.push(c);
        } else if !id.is_empty() && !id.ends_with('-') {
            id.push('-');
        }
    }
    let id = id.trim_end_matches('-').to_string();
    (!id.is_empty()).then_some(id)
}

/// The controller fetcher. One instance lives on its own worker thread, so a slow controller
/// query never delays local log tracking.
struct Fetcher {
    target: crate::profile::Target,
    connection: Option<Connection>,
    cache: Option<Cache>,
    identity: Option<(String, Identity)>,
    view: LiveView,
    detail: Option<LiveDetail>,
    base: Option<Base>,
    launches: HashMap<String, CachedLaunch>,
    packs: HashMap<String, PackFiles>,
    runs: HashMap<String, CachedRun>,
    finished: Vec<Finished>,
    finished_seq: u64,
    /// Handed to each session stream, so new lines wake the worker.
    wake: Sender<Msg>,
    /// Registered playbook ids, the ones a launch form can start, and when they were read.
    registered: Vec<String>,
    registered_at: Option<Instant>,
    prepared: Option<Prepared>,
    launched: Option<Launched>,
    reran: Option<Reran>,
    synced: Option<Synced>,
    dropped: Option<Dropped>,
    replies: u64,
    /// Where draft working copies go.
    drafts: Option<std::path::PathBuf>,
}

/// What the controller last answered, on disk, and whether answers are coming from it now.
pub(crate) struct Cache {
    pub(crate) store: Arc<Store>,
    offline: Arc<AtomicBool>,
}

impl Cache {
    pub(crate) fn open() -> Option<Cache> {
        let path = crate::httpcache::cache_root()?.join("controller.sqlite");
        Some(Cache {
            store: Arc::new(Store::open(&path).ok()?),
            offline: Arc::new(AtomicBool::new(false)),
        })
    }

    fn offline(&self) -> bool {
        self.offline.load(Ordering::Relaxed)
    }
}

/// The crux client and the runtime its calls block on.
pub(crate) struct Connection {
    pub(crate) client: crux::Client,
    runtime: tokio::runtime::Runtime,
}

impl Connection {
    /// The controller `target` names, read through `cache` when there is one.
    pub(crate) fn open(
        target: &crate::profile::Target,
        cache: Option<&Cache>,
    ) -> Result<Self, String> {
        target.ready()?;
        let config = target.config()?;
        let wire = crux::client::HttpTransport::with_client(&config, target.http()?)
            .map_err(|error| format!("{error:#}"))?;
        let transport: Arc<dyn crux::client::Transport> = match cache {
            Some(cache) => Arc::new(CachedTransport::new(
                Arc::new(wire),
                cache.store.clone(),
                cache.offline.clone(),
            )),
            None => Arc::new(wire),
        };
        let client = crux::Client::new(transport);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("crucible-playbooks-http")
            .enable_all()
            .build()
            .map_err(|error| format!("starting the controller client: {error}"))?;
        Ok(Connection { client, runtime })
    }

    /// Run one call to its end or [`TIMEOUT`], whichever comes first.
    pub(crate) fn call<T, E: std::fmt::Display>(
        &self,
        what: &str,
        call: impl Future<Output = Result<T, E>>,
    ) -> Result<T, String> {
        match self
            .runtime
            .block_on(async { tokio::time::timeout(TIMEOUT, call).await })
        {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(format!("{error:#}")),
            Err(_) => Err(format!("{what} timed out after {}s", TIMEOUT.as_secs())),
        }
    }
}

/// One launch's detail, as of the run count and status the launch list showed when it was read:
/// a dispatch or a status change is what invalidates it.
struct CachedLaunch {
    runs: i64,
    status: String,
    detail: crux::dto::PlaybookLaunch,
}

/// One run's session fold and console output, keyed by run id. A settled run never changes, so
/// switching back to it costs no API call.
#[derive(Default)]
struct CachedRun {
    fold: RunFold,
    log: Vec<LogLine>,
    settled: bool,
    source: &'static str,
    error: Option<String>,
    follow: Option<SessionFollow>,
    retry_at: Option<Instant>,
    /// A new stream replays the session and the log from their starts; what each replaces stays
    /// on screen until that replay's first lines land.
    replaying: bool,
    replaying_log: bool,
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
    /// Start the stream as a task on the connection's runtime, the one that drives every pooled
    /// connection the client holds.
    fn spawn(connection: &Connection, run_id: &str, wake: Sender<Msg>) -> Self {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let logs = Arc::new(Mutex::new(Vec::new()));
        let ended = Arc::new(Mutex::new(None));
        let (stop, stopped) = oneshot::channel::<()>();
        let (sink, outcome) = (Arc::clone(&lines), Arc::clone(&ended));
        let log_sink = Arc::clone(&logs);
        let log_wake = wake.clone();
        let (client, run_id) = (connection.client.clone(), run_id.to_string());
        connection.runtime.spawn(async move {
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
    fn new(target: crate::profile::Target, wake: Sender<Msg>) -> Self {
        Fetcher {
            target,
            connection: None,
            cache: Cache::open(),
            identity: None,
            view: LiveView::Loading,
            detail: None,
            base: None,
            launches: HashMap::new(),
            packs: HashMap::new(),
            runs: HashMap::new(),
            finished: Vec::new(),
            finished_seq: 0,
            wake,
            registered: Vec::new(),
            registered_at: None,
            prepared: None,
            launched: None,
            reran: None,
            synced: None,
            dropped: None,
            replies: 0,
            drafts: crate::draft::root(),
        }
    }

    /// Re-reads the registered playbooks at most once a minute.
    fn refresh_registered(&mut self) -> bool {
        if self
            .registered_at
            .is_some_and(|at| at.elapsed() < Duration::from_secs(60))
        {
            return false;
        }
        self.registered_at = Some(Instant::now());
        let Ok(connection) = self.connection() else {
            return false;
        };
        let Ok(playbooks) = connection.call(
            "the playbooks",
            connection.client.playbooks::<Vec<crux::dto::Playbook>>(),
        ) else {
            return false;
        };
        let mut ids: Vec<String> = playbooks.into_iter().map(|p| p.id).collect();
        ids.sort();
        let changed = ids != self.registered;
        self.registered = ids;
        changed
    }

    fn prepare(&mut self, target: Target) {
        self.replies += 1;
        let seq = self.replies;
        let result = self.connection().and_then(|connection| {
            let (schema, digest) = match &target {
                Target::Playbook(id) => (
                    connection.call(
                        "the params schema",
                        connection.client.playbook_schema::<Value>(id),
                    )?,
                    None,
                ),
                Target::Draft(id) => {
                    let head: crux::dto::DraftCompile = connection.call(
                        "the draft's schema",
                        connection.client.draft_preview(id, None),
                    )?;
                    (
                        head.params_schema.unwrap_or(Value::Null),
                        head.schema_digest,
                    )
                }
            };
            let caps: crux::dto::PlaybookCaps =
                connection.call("the launch caps", connection.client.playbook_caps())?;
            Ok(Form {
                schema,
                digest,
                max_cost: caps.max_cost,
                max_time: caps.max_time,
            })
        });
        self.prepared = Some(Prepared {
            seq,
            target,
            result,
        });
    }

    fn send_launch(&mut self, target: Target, body: Value) {
        self.replies += 1;
        let seq = self.replies;
        let result = self.connection().and_then(|connection| {
            let ack: crux::dto::LaunchAck = match &target {
                Target::Playbook(id) => {
                    connection.call("the launch", connection.client.launch(id, &body))?
                }
                Target::Draft(id) => {
                    connection.call("the launch", connection.client.launch_draft(id, &body))?
                }
            };
            Ok(ack.key)
        });
        self.launched = Some(Launched {
            seq,
            playbook: target.id().to_string(),
            result,
        });
    }

    fn preview_rerun(&mut self, run: String, from: Vec<String>) {
        self.replies += 1;
        let seq = self.replies;
        let result = self.connection().and_then(|connection| {
            connection.call(
                "the rerun preview",
                connection.client.rerun_preview(&run, &from),
            )
        });
        self.reran = Some(Reran {
            seq,
            run,
            from,
            result,
        });
    }

    /// Pull `draft`'s newest save into its working copy, creating it from `template` first when
    /// the controller has no such draft.
    fn pull(&mut self, draft: String, template: Option<String>) {
        self.replies += 1;
        let seq = self.replies;
        let drafts = self.drafts.clone();
        let result = self.connection().and_then(|connection| {
            let root = drafts
                .as_deref()
                .ok_or("no home directory for working copies")?;
            let newest = connection.call(
                "the draft",
                connection
                    .client
                    .draft_files::<crux::dto::DraftFiles>(&draft, None),
            );
            let (files, note) = match (newest, &template) {
                (Ok(files), _) => (files, None),
                (Err(_), Some(template)) => {
                    let _: crux::dto::DraftCompile = connection.call(
                        "creating the draft",
                        connection.client.create_draft(
                            &draft,
                            &format!("Drafted from {template}"),
                            Some(template),
                        ),
                    )?;
                    let files = connection
                        .call("the new draft", connection.client.draft_files(&draft, None))?;
                    (
                        files,
                        Some(format!("Created draft {draft} from {template}")),
                    )
                }
                (Err(error), None) => return Err(error),
            };
            let working = Working::write(root, &draft, files.version, &files.files)?;
            Ok((working, note))
        });
        self.synced = Some(Synced { seq, draft, result });
    }

    /// Push pack folder `dir` as the next version of the draft named after it, creating the draft
    /// on first drop, and test-fire that version under `max_cost` and `max_time`.
    fn drop_pack(&mut self, dir: std::path::PathBuf, max_cost: f64, max_time: &str) {
        self.replies += 1;
        let seq = self.replies;
        let draft = draft_id(&dir).unwrap_or_default();
        let result = self.connection().and_then(|connection| {
            if draft.is_empty() {
                return Err(format!(
                    "{} names no draft: its name has no letters or digits",
                    dir.display()
                ));
            }
            let (files, _) = crate::draft::read(&dir)?;
            if files.is_empty() {
                return Err(format!("{} holds no pack files", dir.display()));
            }
            let base = match connection.call(
                "the draft",
                connection
                    .client
                    .draft_files::<crux::dto::DraftFiles>(&draft, None),
            ) {
                Ok(newest) => newest.version,
                Err(_) => {
                    let created: crux::dto::DraftCompile = connection.call(
                        "creating the draft",
                        connection.client.create_draft(
                            &draft,
                            &format!("Dropped on Crucible Playbooks from {}", dir.display()),
                            None,
                        ),
                    )?;
                    created.version
                }
            };
            match connection.call(
                "the save",
                connection.client.save_draft(&draft, &files, base),
            )? {
                crux::client::DraftSave::Saved(_) => {}
                crux::client::DraftSave::Stale(stale) => return Err(stale.error),
            }
            let body = serde_json::json!({
                "params": {},
                "max_cost": max_cost,
                "max_time": max_time,
            });
            let ack: crux::dto::LaunchAck =
                connection.call("the launch", connection.client.launch_draft(&draft, &body))?;
            Ok(ack.key)
        });
        self.dropped = Some(Dropped {
            seq,
            dir,
            draft,
            result,
        });
    }

    /// Save a working copy as its draft's next version.
    fn save(&mut self, mut working: Working) {
        self.replies += 1;
        let seq = self.replies;
        let drafts = self.drafts.clone();
        let draft = working.draft.clone();
        let result = self.connection().and_then(|connection| {
            let root = drafts
                .as_deref()
                .ok_or("no home directory for working copies")?;
            let (files, digest) = crate::draft::read(&working.dir)?;
            if files.is_empty() {
                return Err("The working copy holds no files".to_string());
            }
            match connection.call(
                "the save",
                connection.client.save_draft(&draft, &files, working.base),
            )? {
                crux::client::DraftSave::Saved(raw) => {
                    let saved: crux::dto::DraftCompile = serde_json::from_value(raw)
                        .map_err(|e| format!("reading the save: {e}"))?;
                    working.saved(root, saved.version, digest)?;
                    let errors: Vec<&str> = saved
                        .diagnostics
                        .iter()
                        .map(|d| d.message.as_str())
                        .collect();
                    let note = if saved.schema_digest.is_none() || !errors.is_empty() {
                        format!(
                            "Saved version {}, which does not compile: {}",
                            saved.version,
                            errors.join("; ")
                        )
                    } else {
                        format!("Saved version {}", saved.version)
                    };
                    Ok((working, Some(note)))
                }
                crux::client::DraftSave::Stale(stale) => Err(stale.error),
            }
        });
        self.synced = Some(Synced { seq, draft, result });
    }

    /// Records each launch the new list shows stopped that the old list showed running.
    fn note_finished(&mut self, next: &[LivePlaybook]) {
        let was_running: HashSet<&str> = self
            .view
            .launches()
            .iter()
            .filter(|launch| launch.status == "running")
            .map(|launch| launch.key.as_str())
            .collect();
        for launch in next {
            if launch.status != "running" && was_running.contains(launch.key.as_str()) {
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

    fn view(&self) -> &LiveView {
        &self.view
    }

    fn detail(&self) -> Option<&LiveDetail> {
        self.detail.as_ref()
    }

    /// Read launch `key`'s run as the comparison base, once: a stopped run never changes, so a
    /// base already read stays. A launch the list does not show yet is tried again later.
    fn refresh_base(&mut self, key: Option<&str>) -> bool {
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
            .and_then(|record| record.runs.first().map(|run| run.run_id.clone()));
        let next = match (&run_id, self.connection.as_ref()) {
            (Some(run_id), Some(connection)) => {
                let run = self.runs.entry(run_id.clone()).or_default();
                if !run.settled {
                    settle(run, connection, run_id);
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

    fn any_running(&self) -> bool {
        self.view
            .launches()
            .iter()
            .any(|launch| launch.status == "running")
    }

    /// Whether the selected launch's run can still change: it is running, or it stopped and its
    /// stored session has not been read yet.
    fn selected_live(&self, selected: Option<&str>) -> bool {
        let Some(launch) = self.launch(selected) else {
            return false;
        };
        launch.status == "running"
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
    fn unavailable(&mut self, error: String) -> bool {
        match &mut self.view {
            LiveView::Ready { stale, .. } => {
                let said = format!("Refresh failed; showing the last list. {error}");
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

    fn connection(&mut self) -> Result<&Connection, String> {
        if self.connection.is_none() {
            self.connection = Some(Connection::open(&self.target, self.cache.as_ref())?);
        }
        self.connection
            .as_ref()
            .ok_or_else(|| "no controller connection".to_string())
    }

    fn refresh_list(&mut self) -> Result<bool, String> {
        if self.identity.is_none() {
            let connection = self.connection()?;
            let whoami = connection.call("whoami", connection.client.whoami())?;
            let endpoint = connection.client.base().to_string();
            self.identity = Some((endpoint, identity(whoami)));
        }
        let (endpoint, identity) = self.identity.clone().unwrap_or_default();

        let connection = self.connection()?;
        let launches: Vec<crux::dto::PlaybookRun> = connection.call(
            "the launch list",
            connection.client.playbook_runs(None, None),
        )?;
        let mut launches: Vec<LivePlaybook> = launches
            .into_iter()
            .map(|launch| {
                let agent = agent_label(&launch);
                LivePlaybook {
                    id: launch.playbook,
                    key: launch.key,
                    status: shown_status(&launch.status).to_string(),
                    cost: launch.cost_usd,
                    cap: launch.max_cost,
                    by: launch.created_by.unwrap_or_default(),
                    agent,
                    why: launch
                        .parked_reason
                        .or(launch.secrets_refusal)
                        .filter(|why| !why.is_empty()),
                    created: clock(&launch.created_at),
                    landed: epoch(&launch.created_at).unwrap_or_default(),
                    landed_at: landed_label(&launch.created_at),
                    origin: launch.origin,
                    max_time: launch.max_time,
                    runs: launch.runs,
                    description: launch.description.filter(|text| !text.is_empty()),
                }
            })
            .collect();
        launches.sort_by_key(|launch| std::cmp::Reverse(launch.landed));

        self.note_finished(&launches);
        let now = unix_now();
        let next = LiveView::Ready {
            endpoint,
            identity,
            now: now - now % 60,
            launches,
            stale: self
                .cache
                .as_ref()
                .filter(|cache| cache.offline())
                .map(|_| "Offline; showing what the controller last answered".to_string()),
        };
        let changed = next != self.view;
        self.view = next;
        Ok(changed)
    }

    /// The launch's own record, re-read when its run count or status moved.
    fn launch_detail(&mut self, launch: &LivePlaybook) -> Option<crux::dto::PlaybookLaunch> {
        if let Some(cached) = self.launches.get(&launch.key)
            && cached.runs == launch.runs
            && cached.status == launch.status
        {
            return Some(cached.detail.clone());
        }
        let connection = self.connection().ok()?;
        let detail: crux::dto::PlaybookLaunch = connection
            .call("the launch", connection.client.playbook_run(&launch.key))
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
    fn pack(&mut self, playbook: &str, draft_version: Option<i64>) -> Result<PackFiles, String> {
        let key = match draft_version {
            Some(version) => format!("{playbook}@draft-{version}"),
            None => format!("{playbook}@pin"),
        };
        if let Some(pack) = self.packs.get(&key) {
            return Ok(pack.clone());
        }
        let connection = self.connection()?;
        let mut pack = match draft_version {
            Some(version) => {
                let draft: crux::dto::DraftFiles = connection.call(
                    "the draft",
                    connection.client.draft_files(playbook, Some(version)),
                )?;
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
                    connection.call("the playbook", connection.client.playbook(playbook))?;
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
            let _ = pack.copy_to(&dir);
        }
        self.packs.insert(key, pack.clone());
        Ok(pack)
    }

    /// Rebuild the detail pane for the selected launch. A running run streams its session and
    /// re-reads its console; a stopped one reads its stored session once and is then settled.
    /// `poll` also re-reads the console; a streamed batch only folds the new session lines.
    fn refresh_detail(&mut self, selected: Option<&str>, poll: bool) -> bool {
        let Some(launch) = self.launch(selected).cloned() else {
            let changed = self.detail.is_some();
            self.detail = None;
            return changed;
        };
        let running = launch.status == "running";
        let record = match (poll, &self.detail) {
            (false, Some(detail)) if detail.key == launch.key => detail.launch.clone(),
            _ => self.launch_detail(&launch),
        };
        let run_id = record
            .as_ref()
            .and_then(|record| record.runs.first())
            .map(|run| run.run_id.clone());
        let pack = match (poll, &self.detail) {
            (false, Some(detail)) if detail.key == launch.key => detail.pack.clone(),
            _ => self.pack(
                &launch.id,
                record
                    .as_ref()
                    .and_then(|record| record.launch.draft_version),
            ),
        };
        for (id, run) in &mut self.runs {
            if Some(id) != run_id.as_ref() {
                run.follow = None;
            }
        }

        if self.connection().is_err() {
            return false;
        }
        let wake = self.wake.clone();
        let (fold, log, source, error, streaming) = match &run_id {
            Some(run_id) => {
                let Some(connection) = self.connection.as_ref() else {
                    return false;
                };
                let run = self.runs.entry(run_id.clone()).or_default();
                if running {
                    follow_session(run, connection, run_id, wake);
                    if poll
                        && let Ok(log) = console(connection, run_id)
                        && log.len() >= run.log.len().min(1)
                    {
                        run.log = log;
                    }
                } else if !run.settled {
                    run.follow = None;
                    settle(run, connection, run_id);
                    if let Ok(log) = console(connection, run_id)
                        && log.len() >= run.log.len().min(1)
                    {
                        run.log = log;
                    }
                }
                (
                    run.fold.clone(),
                    run.log.clone(),
                    run.source,
                    run.error.clone(),
                    run.follow.is_some(),
                )
            }
            None => (RunFold::default(), Vec::new(), "", None, false),
        };
        let mut next = build_detail(&launch, fold, log);
        next.source = source;
        next.error = error;
        next.launch = record;
        next.pack = pack;
        next.streaming = streaming;
        let changed = self.detail.as_ref() != Some(&next);
        self.detail = Some(next);
        changed
    }
}

/// Keep a session stream on a running run and fold what it has delivered. A stream that ended
/// cleanly is started again on the next read; one that failed waits [`FOLLOW_RETRY`].
fn follow_session(run: &mut CachedRun, connection: &Connection, run_id: &str, wake: Sender<Msg>) {
    if let Some(follow) = &run.follow {
        let (lines, logs) = (follow.drain(), follow.drain_logs());
        // A new stream replays from the start, so its first lines replace what the last one left.
        if run.replaying && !lines.is_empty() {
            run.fold = RunFold::default();
            run.replaying = false;
        }
        if run.replaying_log && !logs.is_empty() {
            run.log.clear();
            run.replaying_log = false;
        }
        for line in lines {
            run.fold.apply_line(&line);
        }
        run.log.extend(logs.iter().map(|raw| LogLine::parse(raw)));
        let excess = run.log.len().saturating_sub(LOG_LINES);
        run.log.drain(..excess);
        match follow.ended() {
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
    run.replaying = true;
    run.replaying_log = true;
    run.source = "session";
    run.error = None;
    run.follow = Some(SessionFollow::spawn(connection, run_id, wake));
}

/// Read a stopped run once: its stored session, or the graph the controller keeps when no
/// session was stored.
fn settle(run: &mut CachedRun, connection: &Connection, run_id: &str) {
    match connection.call("the session", connection.client.run_session(run_id)) {
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
        Err(session) => {
            match connection.call("the graph", connection.client.graph::<Value>(run_id)) {
                Ok(graph) => {
                    run.fold = graph_fold(&graph);
                    run.source = "graph";
                    run.settled = true;
                    run.error = None;
                }
                Err(_) => run.error = Some(session),
            }
        }
    }
}

/// A run's graph read as the session it was built from: its plan, then each result. The
/// controller writes absent numbers as `null`, which the session's wire types do not take.
fn graph_fold(graph: &Value) -> RunFold {
    let mut fold = RunFold::default();
    if graph
        .get("tasks")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        return fold;
    }
    let mut plan = serde_json::json!({
        "kind": "plan_admitted",
        "plan_version": graph.get("plan_version").and_then(Value::as_u64).unwrap_or(1),
        "budget_usd": 0.0,
        "tasks": graph.get("tasks").cloned().unwrap_or(Value::Array(Vec::new())),
    });
    strip_nulls(&mut plan);
    if let Ok(event) = serde_json::from_value::<SessionEvent>(plan) {
        fold.apply(event);
    }
    for result in graph
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let mut result = result.clone();
        strip_nulls(&mut result);
        if let Value::Object(fields) = &mut result {
            fields.insert("kind".into(), Value::String("task_result".into()));
        }
        if let Ok(event) = serde_json::from_value::<SessionEvent>(result) {
            fold.apply(event);
        }
    }
    fold
}

fn strip_nulls(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.retain(|_, field| !field.is_null());
            fields.values_mut().for_each(strip_nulls);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_nulls),
        _ => {}
    }
}

/// `repo@rev path`, or the draft a registered playbook was published from.
fn pin_label(playbook: &crux::dto::Playbook) -> String {
    let rev: String = playbook.rev.chars().take(12).collect();
    match &playbook.source {
        crux::dto::PlaybookSource::Git { repo, path, .. } => {
            let path = if path.is_empty() {
                String::new()
            } else {
                format!(" {path}")
            };
            format!("{repo}@{rev}{path}")
        }
        crux::dto::PlaybookSource::Draft { draft, version } => {
            format!("published from draft {draft} v{version}")
        }
    }
}

fn identity(whoami: crux::dto::Whoami) -> Identity {
    let user = whoami.user.unwrap_or_default();
    let owners = (!user.is_empty())
        .then(|| format!("user:{user}"))
        .into_iter()
        .chain(whoami.teams.iter().map(|t| format!("team:{}", t.team)))
        .collect();
    Identity {
        user,
        role: whoami.role,
        owners,
    }
}

fn agent_label(launch: &crux::dto::PlaybookRun) -> String {
    match (
        launch.agent_provider.as_deref(),
        launch.agent_model.as_deref(),
    ) {
        (Some(provider), Some(model)) => format!("{provider} · {model}"),
        (None, Some(model)) => model.to_string(),
        (Some(provider), None) => provider.to_string(),
        (None, None) => String::new(),
    }
}

/// The run's whole console output as the controller serves it, typed.
fn console(connection: &Connection, run_id: &str) -> Result<Vec<LogLine>, String> {
    let log: crux::dto::RunLog =
        connection.call("the console", connection.client.run_log(run_id))?;
    Ok(log
        .text
        .map(|text| text.lines().map(LogLine::parse).collect())
        .unwrap_or_default())
}

fn build_detail(launch: &LivePlaybook, fold: RunFold, log: Vec<LogLine>) -> LiveDetail {
    let settled = !fold.tasks.is_empty() && fold.required_settled();
    let verdict = match launch.status.as_str() {
        "running" => Verdict::Running,
        "done" if !settled => Verdict::Unfinished,
        "done" if fold.required_passed() => Verdict::Valid,
        "done" => Verdict::Invalid,
        "error" => Verdict::Error,
        "new" => Verdict::Unfinished,
        _ if settled && fold.required_passed() => Verdict::Valid,
        _ => Verdict::Unfinished,
    };
    let spent = if fold.tasks.iter().all(|task| task.result.is_none()) {
        launch.cost.unwrap_or(0.0)
    } else {
        fold.spent()
    };
    LiveDetail {
        streaming: false,
        key: launch.key.clone(),
        spent,
        verdict,
        passed: fold.passed(),
        failed: fold.failed(),
        fold,
        log,
        source: "",
        error: None,
        launch: None,
        pack: Ok(PackFiles::default()),
    }
}

/// `18:00`, the UTC time of day of an RFC 3339 stamp; empty when it does not parse.
fn clock(stamp: &str) -> String {
    stamp
        .parse::<Timestamp>()
        .map(|at| at.to_zoned(TimeZone::UTC).strftime("%H:%M").to_string())
        .unwrap_or_default()
}

fn unix_now() -> i64 {
    Timestamp::now().as_second()
}

/// Whole Unix seconds from an RFC 3339 stamp.
fn epoch(stamp: &str) -> Option<i64> {
    stamp.parse::<Timestamp>().ok().map(|at| at.as_second())
}

/// `Nov 6 18:00` in UTC; the raw stamp when it does not parse.
fn landed_label(stamp: &str) -> String {
    stamp
        .parse::<Timestamp>()
        .map(|at| {
            at.to_zoned(TimeZone::UTC)
                .strftime("%b %-d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| stamp.to_string())
}

/// What the live worker last read, for the window.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub view: LiveView,
    pub detail: Option<LiveDetail>,
    /// The launch the selected one is compared against.
    pub base: Option<Base>,
    /// Recently finished launches, oldest first.
    pub finished: Vec<Finished>,
    /// Registered playbook ids: the ones the app can launch.
    pub registered: Vec<String>,
    pub prepared: Option<Prepared>,
    pub launched: Option<Launched>,
    pub reran: Option<Reran>,
    pub synced: Option<Synced>,
    pub dropped: Option<Dropped>,
}

/// The UI-side handle to the live worker. Dropping it stops the thread.
pub struct Client {
    tx: Sender<Msg>,
    shared: Arc<Mutex<Shared>>,
}

struct Shared {
    snapshot: Snapshot,
    version: u64,
    waker: Option<Waker>,
}

impl Client {
    pub fn spawn(target: crate::profile::Target) -> Self {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Shared {
            snapshot: Snapshot::default(),
            version: 0,
            waker: None,
        }));
        let worker = Arc::clone(&shared);
        let wake = tx.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("crucible-playbooks-live".into())
            .spawn(move || run_client(target, rx, wake, worker))
        {
            publish(
                &shared,
                Snapshot {
                    view: LiveView::Unavailable(format!("starting the live worker: {error}")),
                    detail: None,
                    base: None,
                    finished: Vec::new(),
                    registered: Vec::new(),
                    prepared: None,
                    launched: None,
                    reran: None,
                    synced: None,
                    dropped: None,
                },
            );
        }
        Client { tx, shared }
    }

    pub fn set_waker(&self, waker: Waker) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.waker = Some(waker);
        }
    }

    /// Read `target`'s params schema and the caps, for a launch form.
    pub fn prepare(&self, target: Target) {
        let _ = self.tx.send(Msg::Prepare(target));
    }

    /// Launch `target`; the result comes back in a snapshot.
    pub fn launch(&self, target: Target, body: Value) {
        let _ = self.tx.send(Msg::Launch { target, body });
    }

    pub fn drop_pack(&self, dir: std::path::PathBuf, max_cost: f64, max_time: String) {
        let _ = self.tx.send(Msg::Drop {
            dir,
            max_cost,
            max_time,
        });
    }

    /// Ask what rerunning `run` from `from` would reuse.
    pub fn preview_rerun(&self, run: &str, from: Vec<String>) {
        let _ = self.tx.send(Msg::Rerun {
            run: run.to_string(),
            from,
        });
    }

    /// Pull `draft` into its working copy, creating it from `template` when it is missing.
    pub fn pull(&self, draft: &str, template: Option<&str>) {
        let _ = self.tx.send(Msg::Pull {
            draft: draft.to_string(),
            template: template.map(ToString::to_string),
        });
    }

    /// Save `working` as its draft's next version.
    pub fn save(&self, working: Working) {
        let _ = self.tx.send(Msg::Save(working));
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

    pub fn take(&self, seen: u64) -> Option<(u64, Snapshot)> {
        let shared = self.shared.lock().ok()?;
        (shared.version > seen).then(|| (shared.version, shared.snapshot.clone()))
    }
}

fn publish(shared: &Mutex<Shared>, snapshot: Snapshot) {
    let waker = match shared.lock() {
        Ok(mut shared) => {
            shared.snapshot = snapshot;
            shared.version += 1;
            shared.waker.clone()
        }
        Err(_) => return,
    };
    if let Some(waker) = waker {
        waker();
    }
}

fn run_client(
    target: crate::profile::Target,
    rx: Receiver<Msg>,
    wake: Sender<Msg>,
    shared: Arc<Mutex<Shared>>,
) {
    let mut fetcher = Fetcher::new(target, wake);
    let mut selected: Option<String> = None;
    let mut compare: Option<String> = None;
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
        match rx.recv_timeout(wait) {
            Ok(Msg::Select(playbook)) => {
                selected = playbook;
                next_detail = Instant::now();
            }
            Ok(Msg::Streamed) => {
                next_stream.get_or_insert_with(|| Instant::now() + STREAM_BATCH);
            }
            Ok(Msg::Prepare(target)) => {
                fetcher.prepare(target);
                force = true;
            }
            Ok(Msg::Launch { target, body }) => {
                fetcher.send_launch(target, body);
                next_list = Instant::now();
                force = true;
            }
            Ok(Msg::Rerun { run, from }) => {
                fetcher.preview_rerun(run, from);
                force = true;
            }
            Ok(Msg::Pull { draft, template }) => {
                fetcher.pull(draft, template);
                force = true;
            }
            Ok(Msg::Save(working)) => {
                fetcher.save(working);
                force = true;
            }
            Ok(Msg::Drop {
                dir,
                max_cost,
                max_time,
            }) => {
                fetcher.drop_pack(dir, max_cost, &max_time);
                next_list = Instant::now();
                force = true;
            }
            Ok(Msg::Compare(base)) => {
                compare = base;
                force = fetcher.refresh_base(compare.as_deref());
            }
            Ok(Msg::Reconnect) => {
                fetcher.connection = None;
                fetcher.identity = None;
                fetcher.registered_at = None;
                fetcher.view = LiveView::Loading;
                next_list = Instant::now();
                force = true;
            }
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }

        let now = Instant::now();
        let mut changed = force;
        if now >= next_list {
            match fetcher.refresh_list() {
                Ok(list_changed) => changed |= list_changed,
                Err(error) => changed |= fetcher.unavailable(error),
            }
            changed |= fetcher.refresh_registered();
            changed |= fetcher.refresh_base(compare.as_deref());
            next_list = now
                + if fetcher.any_running() {
                    Duration::from_secs(2)
                } else {
                    Duration::from_secs(8)
                };
            next_detail = now;
        }
        if now >= next_detail {
            changed |= fetcher.refresh_detail(selected.as_deref(), true);
            next_detail = now
                + if fetcher.selected_live(selected.as_deref()) {
                    Duration::from_secs(2)
                } else {
                    Duration::from_secs(60)
                };
            next_stream = None;
        } else if next_stream.is_some_and(|at| now >= at) {
            changed |= fetcher.refresh_detail(selected.as_deref(), false);
            next_stream = None;
        }
        if changed {
            publish(
                &shared,
                Snapshot {
                    view: fetcher.view().clone(),
                    detail: fetcher.detail().cloned(),
                    base: fetcher.base.clone(),
                    finished: fetcher.finished.clone(),
                    registered: fetcher.registered.clone(),
                    prepared: fetcher.prepared.clone(),
                    launched: fetcher.launched.clone(),
                    reran: fetcher.reran.clone(),
                    synced: fetcher.synced.clone(),
                    dropped: fetcher.dropped.clone(),
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::filter::{LandedFilter, LaunchFilter, OriginFilter, OwnerFilter};
    use crate::live::*;

    fn launch(status: &str) -> LivePlaybook {
        LivePlaybook {
            id: "demo".into(),
            key: "playbook:demo:1".into(),
            status: status.into(),
            cost: Some(0.5),
            cap: 2.0,
            by: "wren".into(),
            agent: "openai · gpt".into(),
            why: None,
            created: "10:00".into(),
            origin: "draft".into(),
            max_time: "25m".into(),
            runs: 1,
            description: None,
            landed: 1_760_000_000,
            landed_at: "Nov 6 10:00".into(),
        }
    }

    const NOW: i64 = 1_760_010_000;

    fn firing(playbook: &str, n: i64, origin: &str, status: &str) -> LivePlaybook {
        LivePlaybook {
            id: playbook.into(),
            key: format!("playbook:{playbook}:{n}"),
            origin: origin.into(),
            landed: 1_760_000_000 + n * 60,
            ..launch(status)
        }
    }

    /// Newest first, as the controller returns them.
    fn ready(mut launches: Vec<LivePlaybook>) -> LiveView {
        launches.sort_by_key(|launch| std::cmp::Reverse(launch.landed));
        LiveView::Ready {
            endpoint: String::new(),
            identity: Identity {
                user: "wren".into(),
                role: String::new(),
                owners: vec!["user:wren".into()],
            },
            now: NOW,
            launches,
            stale: None,
        }
    }

    #[test]
    fn a_launch_that_stops_running_is_noted_once_and_the_list_is_bounded() {
        let (wake, _rx) = mpsc::channel();
        let mut fetcher = Fetcher::new(crate::profile::Target::unreachable(), wake);
        let first = vec![
            firing("a", 1, "draft", "running"),
            firing("b", 2, "draft", "done"),
        ];
        fetcher.note_finished(&first);
        assert!(
            fetcher.finished.is_empty(),
            "a first list has nothing to compare"
        );
        fetcher.view = ready(first);

        let mut stopped = firing("a", 1, "draft", "error");
        stopped.why = Some("budget ran out".into());
        let second = vec![stopped, firing("b", 2, "draft", "done")];
        fetcher.note_finished(&second);
        assert_eq!(fetcher.finished.len(), 1);
        let done = &fetcher.finished[0];
        assert_eq!((done.seq, done.playbook.as_str()), (1, "a"));
        assert_eq!((done.status.as_str(), done.by.as_str()), ("error", "wren"));
        assert_eq!(done.why.as_deref(), Some("budget ran out"));

        fetcher.view = ready(second.clone());
        fetcher.note_finished(&second);
        assert_eq!(fetcher.finished.len(), 1, "a stopped launch is noted once");

        for n in 0..(FINISHED as i64 + 5) {
            fetcher.view = ready(vec![firing("c", 10 + n, "schedule", "running")]);
            fetcher.note_finished(&[firing("c", 10 + n, "schedule", "done")]);
        }
        assert_eq!(fetcher.finished.len(), FINISHED);
        assert!(fetcher.finished.windows(2).all(|w| w[0].seq < w[1].seq));
    }

    fn everything() -> LaunchFilter {
        LaunchFilter {
            query: String::new(),
            owner: OwnerFilter::All,
            origin: OriginFilter::All,
            landed: LandedFilter::Any,
        }
    }

    fn only(origin: OriginFilter) -> LaunchFilter {
        LaunchFilter {
            origin,
            ..everything()
        }
    }

    fn keys(launches: &[&LivePlaybook]) -> Vec<String> {
        launches.iter().map(|launch| launch.key.clone()).collect()
    }

    fn resolved(
        view: &LiveView,
        playbook: &str,
        pinned: Option<&str>,
        filter: &LaunchFilter,
    ) -> Option<String> {
        view.resolve(playbook, pinned, filter)
            .map(|launch| launch.key.clone())
    }

    #[test]
    fn a_failed_refresh_keeps_the_last_list_and_says_why() {
        let (wake, _rx) = mpsc::channel();
        let mut fetcher = Fetcher::new(crate::profile::Target::unreachable(), wake);
        assert!(fetcher.unavailable("dial refused".into()));
        assert!(matches!(fetcher.view, LiveView::Unavailable(_)));

        fetcher.view = ready(vec![firing("nightly", 1, "schedule", "done")]);
        assert!(fetcher.unavailable("replica b: 502".into()));
        let LiveView::Ready {
            launches, stale, ..
        } = &fetcher.view
        else {
            panic!("a failed refresh dropped the list");
        };
        assert_eq!(launches.len(), 1);
        assert_eq!(
            stale.as_deref(),
            Some("Refresh failed; showing the last list. replica b: 502")
        );
        assert!(
            !fetcher.unavailable("replica b: 502".into()),
            "the same failure again is no change to draw"
        );
    }

    #[test]
    fn a_followed_playbook_moves_to_each_new_firing() {
        let before = ready(vec![
            firing("nightly", 1, "schedule", "done"),
            firing("nightly", 2, "schedule", "done"),
        ]);
        assert_eq!(
            resolved(&before, "nightly", None, &everything()).as_deref(),
            Some("playbook:nightly:2")
        );
        let after = ready(vec![
            firing("nightly", 1, "schedule", "done"),
            firing("nightly", 2, "schedule", "done"),
            firing("nightly", 3, "schedule", "running"),
        ]);
        assert_eq!(
            resolved(&after, "nightly", None, &everything()).as_deref(),
            Some("playbook:nightly:3")
        );
    }

    #[test]
    fn a_pinned_firing_stays_put_when_the_schedule_fires_again() {
        let view = ready(vec![
            firing("nightly", 1, "schedule", "done"),
            firing("nightly", 2, "schedule", "done"),
            firing("nightly", 3, "schedule", "running"),
        ]);
        assert_eq!(
            resolved(&view, "nightly", Some("playbook:nightly:1"), &everything()).as_deref(),
            Some("playbook:nightly:1")
        );
    }

    #[test]
    fn a_pinned_firing_that_aged_out_falls_back_to_the_same_playbook() {
        let view = ready(vec![
            firing("other", 9, "manual", "running"),
            firing("nightly", 4, "schedule", "done"),
            firing("nightly", 5, "schedule", "done"),
        ]);
        assert_eq!(
            resolved(&view, "nightly", Some("playbook:nightly:1"), &everything()).as_deref(),
            Some("playbook:nightly:5")
        );
    }

    #[test]
    fn a_followed_playbook_shows_its_newest_launch_the_filter_keeps() {
        let view = ready(vec![
            firing("nightly", 1, "schedule", "done"),
            firing("nightly", 2, "manual", "done"),
        ]);
        assert_eq!(
            resolved(&view, "nightly", None, &only(OriginFilter::Schedules)).as_deref(),
            Some("playbook:nightly:1")
        );
        assert_eq!(
            resolved(&view, "nightly", None, &only(OriginFilter::Drafts)).as_deref(),
            Some("playbook:nightly:2"),
            "a playbook the filter hides still shows its newest launch"
        );
        assert_eq!(resolved(&view, "gone", None, &everything()), None);
    }

    #[test]
    fn the_schedules_filter_keeps_a_playbook_whose_newest_launch_was_manual() {
        let view = ready(vec![
            firing("nightly", 1, "schedule", "done"),
            firing("nightly", 2, "manual", "done"),
            firing("adhoc", 3, "manual", "done"),
        ]);
        assert_eq!(
            keys(&view.playbooks(&only(OriginFilter::Schedules))),
            ["playbook:nightly:1"]
        );
        assert_eq!(
            keys(&view.kept(&only(OriginFilter::Schedules))),
            ["playbook:nightly:1"]
        );
    }

    #[test]
    fn the_sidebar_lists_one_row_per_playbook_running_first_then_newest() {
        let view = ready(vec![
            firing("nightly", 1, "schedule", "running"),
            firing("nightly", 2, "schedule", "done"),
            firing("adhoc", 3, "manual", "done"),
            firing("weekly", 4, "schedule", "done"),
            firing("hourly", 0, "schedule", "running"),
        ]);
        assert_eq!(
            keys(&view.playbooks(&everything())),
            [
                "playbook:hourly:0",
                "playbook:weekly:4",
                "playbook:adhoc:3",
                "playbook:nightly:2",
            ]
        );
        assert_eq!(view.playbook_count(), 4);
    }

    #[test]
    fn the_owner_and_landed_filters_read_the_viewer_from_the_view() {
        let mut theirs = firing("nightly", 2, "schedule", "done");
        theirs.by = "lwilkins".into();
        let view = ready(vec![firing("nightly", 1, "schedule", "done"), theirs]);
        let mine = LaunchFilter {
            owner: OwnerFilter::Mine,
            ..everything()
        };
        assert_eq!(keys(&view.kept(&mine)), ["playbook:nightly:1"]);
        let hour = LaunchFilter {
            landed: LandedFilter::Hour,
            ..everything()
        };
        assert!(view.kept(&hour).is_empty(), "both landed hours before NOW");
        assert!(LiveView::Loading.kept(&mine).is_empty());
    }

    #[test]
    fn a_graph_without_a_stored_session_still_reads_as_tasks_and_results() {
        let graph: Value = serde_json::from_str(
            r##"{"plan_version":2,"decisions":[],"fanout":[],"outputs":{},
                "tasks":[{"name":"seed","kind":"command","depends_on":[],"required":true,"over":"","when":"","keyed":[],"needs":"all","max_fanout":0,"session":""},
                         {"name":"edit","kind":"agent","depends_on":["seed"],"required":true,"over":"seed.kernels","when":"","keyed":[],"needs":"all","max_fanout":0,"session":""}],
                "results":[{"iter":0,"task":"seed","status":"pass","note":"","cost_usd":null,"secs":null,"blocked":null,"links":[],"agent":null,"repairs":[],"output":{"kernels":["a"]}},
                           {"iter":0,"task":"edit[a]","status":"fail","note":"broke","cost_usd":0.5,"secs":12.0,"blocked":null,"links":[{"url":"https://github.com/o/r/pull/9","provider":"github","kind":"pull_request","label":"#9"}],"agent":{"harness":"claude","model":"m","effort":"low"},"repairs":[],"output":null}]}"##,
        )
        .unwrap();
        let fold = graph_fold(&graph);
        assert_eq!(fold.plan_version, 2);
        assert_eq!(fold.tasks.len(), 2);
        assert_eq!(fold.passed(), 1);
        let edit = &fold.tasks[1];
        assert_eq!(edit.over, "seed.kernels");
        assert_eq!(edit.instances.len(), 1);
        let instance = edit.instances[0].result.as_ref().unwrap();
        assert_eq!(instance.note, "broke");
        assert!((instance.secs - 12.0).abs() < 1e-9);
        assert_eq!(edit.links().len(), 1);
        assert_eq!(edit.agent().map(|a| a.harness.as_str()), Some("claude"));
    }

    #[test]
    fn a_graph_with_nothing_in_it_folds_to_an_empty_run() {
        assert_eq!(graph_fold(&serde_json::json!({})), RunFold::default());
    }

    #[test]
    fn a_utc_stamp_becomes_unix_seconds_and_a_short_label() {
        assert_eq!(epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch("2026-11-06T18:00:00Z"), Some(1_793_988_000));
        assert_eq!(landed_label("2026-11-06T18:00:00Z"), "Nov 6 18:00");
        assert_eq!(landed_label("not a stamp"), "not a stamp");
    }

    #[test]
    fn a_search_matches_the_fields_the_row_shows() {
        let playbook = launch("done");
        assert!(playbook.matches("demo"));
        assert!(playbook.matches("draft"));
        assert!(playbook.matches("wren"));
        assert!(playbook.matches("openai"));
        assert!(!playbook.matches("schedule"));
    }

    #[test]
    fn a_whoami_answer_names_the_user_and_role_the_sidebar_shows() {
        let named: crux::dto::Whoami =
            serde_json::from_str(r#"{"user":"wynn","role":"admin","admin":true}"#).unwrap();
        assert_eq!(identity(named).label(), "wynn \u{b7} admin");
        let anonymous: crux::dto::Whoami = serde_json::from_str(r#"{"role":""}"#).unwrap();
        assert_eq!(identity(anonymous).label(), "");
    }

    #[test]
    fn a_playbook_keeps_every_firing_while_a_launch_is_selected_by_its_key() {
        let mut newest = launch("error");
        newest.key = "playbook:demo:2".into();
        newest.runs = 0;
        let mut older = launch("done");
        older.key = "playbook:demo:1".into();
        let view = LiveView::Ready {
            endpoint: String::new(),
            identity: Identity::default(),
            now: 0,
            launches: vec![newest, older],
            stale: None,
        };
        assert_eq!(view.launches_of("demo").count(), 2);
        assert_eq!(
            view.launch("playbook:demo:1").map(|l| l.status.as_str()),
            Some("done")
        );
        assert_eq!(view.launch("playbook:demo:2").map(|l| l.runs), Some(0));
    }

    const SESSION: &str = r#"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":2.0,"tasks":[{"name":"draft","kind":"agent","depends_on":[],"required":true,"session":"scribe"},{"name":"audit","kind":"agent","depends_on":["draft"],"required":false}]}
{"v":1,"kind":"task_result","task":"draft","status":"pass","attempts":1,"cost_usd":0.25,"output":{"entries":3}}
{"v":1,"kind":"task_result","task":"audit","status":"fail","attempts":1,"cost_usd":0.25,"note":"nothing to grade"}"#;

    fn session() -> RunFold {
        let mut fold = RunFold::default();
        for line in SESSION.lines() {
            fold.apply_line(line);
        }
        fold
    }

    #[test]
    fn a_settled_session_with_its_required_tasks_passed_reads_valid() {
        let detail = build_detail(&launch("done"), session(), Vec::new());
        assert_eq!(detail.verdict, Verdict::Valid);
        assert_eq!(detail.fold.tasks.len(), 2);
        assert_eq!(detail.passed, 1);
        assert_eq!(detail.failed, 1);
        assert!((detail.spent - 0.5).abs() < 1e-9);
        assert!(!detail.fold.tasks[1].required);
    }

    #[test]
    fn a_running_launch_reads_as_running_whatever_the_session_says() {
        let detail = build_detail(&launch("running"), session(), Vec::new());
        assert_eq!(detail.verdict, Verdict::Running);
    }

    #[test]
    fn a_done_launch_whose_session_is_not_settled_reads_unfinished() {
        let detail = build_detail(&launch("done"), RunFold::default(), Vec::new());
        assert_eq!(detail.verdict, Verdict::Unfinished);
        assert_eq!(detail.spent, 0.5, "the launch's own cost stands in");
    }

    #[test]
    fn a_parked_launch_without_a_graph_is_an_error_and_keeps_nothing() {
        let parked = launch(shown_status("parked"));
        assert_eq!(parked.verdict(), Verdict::Error);
        let detail = build_detail(&parked, RunFold::default(), Vec::new());
        assert_eq!(detail.verdict, Verdict::Error);
        assert!(detail.fold.tasks.is_empty());
        assert_eq!(detail.spent, 0.5);
    }

    /// Against the controller `crux` is configured for: read a registered pack with its Git
    /// links, pull a draft into a working copy, and preview rerunning the draft's newest finished
    /// run. None of it writes to the controller. `CRUCIBLE_E2E_PLAYBOOK` and `CRUCIBLE_E2E_DRAFT`
    /// name the two; run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs a live controller"]
    fn a_live_draft_pulls_into_a_working_copy_and_its_run_previews_a_rerun() {
        let draft =
            std::env::var("CRUCIBLE_E2E_DRAFT").unwrap_or_else(|_| "vllm-agent-fanout".into());
        let (wake, _rx) = std::sync::mpsc::channel();
        let mut fetcher = Fetcher::new(crate::profile::Target::unreachable(), wake);
        let root = tempfile::tempdir().unwrap();
        fetcher.drafts = Some(root.path().to_path_buf());

        fetcher.pull(draft.clone(), None);
        let synced = fetcher.synced.clone().expect("a pull reply");
        let (working, note) = synced.result.expect("the draft pulls");
        assert_eq!(note, None, "an existing draft is pulled, not created");
        assert!(working.base > 0);
        assert!(working.dir.join("crucible.toml").is_file());
        assert!(!working.edited());
        assert_eq!(
            crate::draft::Working::open(root.path(), &draft).as_ref(),
            Some(&working)
        );

        let registered = std::env::var("CRUCIBLE_E2E_PLAYBOOK").unwrap_or_else(|_| "triage".into());
        let pinned = fetcher
            .pack(&registered, None)
            .expect("the registered pack reads");
        assert!(pinned.file("crucible.toml").is_some());
        assert!(
            pinned
                .web_url("crucible.toml", 1)
                .is_some_and(|url| url.starts_with("https://") && url.ends_with("crucible.toml#L1")),
            "{:?}",
            pinned.web
        );

        fetcher.prepare(Target::Draft(draft.clone()));
        let form = fetcher
            .prepared
            .clone()
            .unwrap()
            .result
            .expect("the draft's form");
        assert!(
            form.digest.is_some(),
            "a compiling draft has a schema digest"
        );
        assert!(form.max_cost > 0.0);

        fetcher.refresh_list().expect("the launch list");
        let launch = fetcher
            .view()
            .launches_of(&draft)
            .find(|launch| launch.status != "running" && launch.runs > 0)
            .cloned()
            .expect("a finished launch of the draft");
        let record = fetcher.launch_detail(&launch).expect("the launch detail");
        let run = record.runs.first().expect("a run").run_id.clone();
        fetcher.preview_rerun(run.clone(), Vec::new());
        let reran = fetcher.reran.clone().unwrap();
        assert_eq!(reran.run, run);
        match reran.result {
            Ok(preview) => {
                assert_eq!(preview.run_id, run);
                assert_eq!(preview.playbook, draft);
                assert!(preview.draft_version.is_some());
            }
            Err(error) => assert!(
                error.contains("git memory") || error.contains("rerun"),
                "a refusal names why: {error}"
            ),
        }
    }
}
