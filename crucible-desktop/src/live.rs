//! The live controller view: registered playbooks and every launch of them, read through the
//! `crux` client library.
//!
//! The controller and its key come from the profile in use; see [`crate::profile`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crucible_contract::session::SessionEvent;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde_json::Value;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use rune::Waker;
use rune::time::Instant;

use crate::captured::{Body, Captured, Opened};
use crate::draft::Working;
use crate::filter::LaunchFilter;
use crate::logline::LogLine;
use crate::model::Verdict;
use crate::net::{Connection, Connector, Down, Task, reaching, within};
use crate::pack::PackFiles;
use crate::revisions::{self, Revisions, Wanted};
use crate::session::RunFold;

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
/// How long a replay that stopped short of what it replaces waits before it is shown anyway.
const REPLAY_QUIET: Duration = Duration::from_secs(3);

#[derive(Clone, Default, PartialEq)]
pub enum LiveView {
    #[default]
    Loading,
    Unavailable(Down),
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
        rows.sort_by_key(|row| !row.status.is_running());
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
    /// The controller's own issue lane is open to the user.
    pub autoresearch: bool,
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
#[derive(Clone, Default, PartialEq)]
pub struct LivePlaybook {
    pub id: String,
    pub key: String,
    pub status: LaunchStatus,
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
        match self.status {
            LaunchStatus::Running => Verdict::Running,
            LaunchStatus::Done => Verdict::Valid,
            LaunchStatus::Error => Verdict::Error,
            LaunchStatus::Cancelled => Verdict::Cancelled,
            LaunchStatus::New | LaunchStatus::Other(_) => Verdict::Unfinished,
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
    /// The run shown, the launch's newest.
    pub run_id: Option<String>,
    /// The files its tasks captured, or why they could not be listed; `None` until read.
    pub files: Option<Result<Vec<Captured>, String>>,
}

/// The launch a run is compared against: its tasks as its stored session left them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Base {
    pub key: String,
    pub fold: RunFold,
    /// Why its run could not be read, when it could not.
    pub error: Option<String>,
}

/// How the controller's reason for parking a cancelled launch starts.
const CANCELLED: &str = "cancelled by ";

/// Where a launch stands, from the controller's status token.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LaunchStatus {
    #[default]
    New,
    Running,
    Done,
    Error,
    /// Stopped by a person.
    Cancelled,
    Other(String),
}

impl LaunchStatus {
    /// The controller's `parked` token is always a launch error on playbook runs, and a cancel
    /// is one it parks with a reason that starts `cancelled by`.
    pub fn parse(token: &str, why: Option<&str>) -> Self {
        match token {
            "new" => Self::New,
            "running" => Self::Running,
            "done" => Self::Done,
            "error" | "parked" if why.is_some_and(|why| why.starts_with(CANCELLED)) => {
                Self::Cancelled
            }
            "error" | "parked" => Self::Error,
            other => Self::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::New => "new",
            Self::Running => "running",
            Self::Done => "done",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
            Self::Other(token) => token,
        }
    }

    pub fn is_running(&self) -> bool {
        *self == Self::Running
    }
}

impl std::fmt::Display for LaunchStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the window, the session streams and finished actions tell the worker.
enum Msg {
    /// Which launch to keep current; `None` stops reading details.
    Select(Option<String>),
    /// A session stream delivered lines.
    Streamed,
    /// Read the launch list now.
    Refresh,
    /// Drop the connection and who it named, and read crux's config afresh.
    Reconnect,
    /// Which launch to read as the base of a comparison; `None` stops.
    Compare(Option<String>),
}

/// What the window asks for that runs as a task of its own.
enum Action {
    /// Read a target's params schema and the admin caps, for a launch form.
    Prepare(Target),
    /// Launch a target with this body (`params`, `max_cost`, `max_time`, maybe `rerun`).
    Launch {
        target: Target,
        body: Value,
    },
    /// What rerunning a finished run from these tasks would reuse.
    Rerun {
        run: String,
        from: Vec<String>,
    },
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
        dir: PathBuf,
        max_cost: f64,
        max_time: String,
    },
    /// Fetch one file a run captured, by its key. `size` is what the listing says it holds, and
    /// `settled` that its run has stopped, so its copy on disk can stand in for it.
    File {
        run: String,
        key: String,
        size: u64,
        settled: bool,
    },
    Revisions(Wanted),
}

/// What an [`Action`] came to.
enum Reply {
    Prepared {
        target: Target,
        result: Result<Form, String>,
    },
    Launched {
        playbook: String,
        result: Result<String, String>,
    },
    Reran {
        run: String,
        from: Vec<String>,
        result: Result<crux::dto::RerunPreview, String>,
    },
    Synced {
        draft: String,
        result: Result<(Working, Option<String>), String>,
    },
    Dropped {
        dir: PathBuf,
        draft: String,
        result: Result<DropOutcome, String>,
    },
    File(Opened),
    Revisions(Revisions),
}

impl Action {
    /// Whether the launch list changes when this lands.
    fn launches(&self) -> bool {
        matches!(self, Action::Launch { .. } | Action::Drop { .. })
    }

    async fn run(self, c: Connection, dirs: &Dirs) -> Reply {
        match self {
            Action::Prepare(target) => {
                let result = prepare(&c, &target).await;
                Reply::Prepared { target, result }
            }
            Action::Launch { target, body } => {
                let result = send_launch(&c, &target, &body).await;
                Reply::Launched {
                    playbook: target.id().to_string(),
                    result,
                }
            }
            Action::Rerun { run, from } => {
                let result = within("the rerun preview", c.client.rerun_preview(&run, &from)).await;
                Reply::Reran { run, from, result }
            }
            Action::Pull { draft, template } => {
                let result = pull(&c, dirs.drafts.as_deref(), &draft, template.as_deref()).await;
                Reply::Synced { draft, result }
            }
            Action::Save(working) => {
                let draft = working.draft.clone();
                let result = save(&c, dirs.drafts.as_deref(), working).await;
                Reply::Synced { draft, result }
            }
            Action::Drop {
                dir,
                max_cost,
                max_time,
            } => {
                let draft = draft_id(&dir).unwrap_or_default();
                let result = drop_pack(&c, &dir, &draft, max_cost, &max_time).await;
                Reply::Dropped { dir, draft, result }
            }
            Action::File {
                run,
                key,
                size,
                settled,
            } => {
                let files = dirs.files.as_deref();
                let kept =
                    file_on_disk(files, &run, &key).filter(|(_, bytes)| settled && *bytes == size);
                Reply::File(match kept {
                    Some((opened, _)) => opened,
                    None => fetch_file(&c, files, run, key).await,
                })
            }
            Action::Revisions(wanted) => Reply::Revisions(Revisions {
                result: read_revisions(&c, &wanted).await,
                wanted,
            }),
        }
    }

    /// The reply when there is no connection to run on.
    fn failed(self, why: String, dirs: &Dirs) -> Reply {
        match self {
            Action::Prepare(target) => Reply::Prepared {
                target,
                result: Err(why),
            },
            Action::Launch { target, .. } => Reply::Launched {
                playbook: target.id().to_string(),
                result: Err(why),
            },
            Action::Rerun { run, from } => Reply::Reran {
                run,
                from,
                result: Err(why),
            },
            Action::Pull { draft, .. } => Reply::Synced {
                draft,
                result: Err(why),
            },
            Action::Save(working) => Reply::Synced {
                draft: working.draft,
                result: Err(why),
            },
            Action::Drop { dir, .. } => Reply::Dropped {
                draft: draft_id(&dir).unwrap_or_default(),
                dir,
                result: Err(why),
            },
            Action::File { run, key, .. } => {
                Reply::File(match file_on_disk(dirs.files.as_deref(), &run, &key) {
                    Some((opened, _)) => opened,
                    None => Opened {
                        body: Err(why),
                        run,
                        key,
                        disk: None,
                    },
                })
            }
            Action::Revisions(wanted) => Reply::Revisions(Revisions {
                wanted,
                result: Err(why),
            }),
        }
    }
}

/// A draft's saves and test-fires, and the diff of the range asked for, or of the one the pane
/// opens on when none was picked or the picked one names a save the draft no longer holds.
async fn read_revisions(c: &Connection, wanted: &Wanted) -> Result<revisions::Read, String> {
    let draft = wanted.draft.as_str();
    let (history, runs) = tokio::join!(
        within("the draft's history", c.client.draft_history(draft)),
        within(
            "the draft's test-fires",
            c.client.playbook_runs(None, Some(draft))
        ),
    );
    let history: Vec<crux::dto::DraftHistoryEntry> = history?;
    let fires = revisions::fires(runs.unwrap_or_default());
    let range = wanted
        .range
        .and_then(|range| revisions::held(&history, range))
        .or_else(|| revisions::opening(&history, &fires, wanted.ran))
        .ok_or_else(|| format!("draft {draft} has no saves"))?;
    let diff = within(
        "the diff",
        c.client.draft_diff(draft, range.from, Some(range.to)),
    )
    .await?;
    let zone = TimeZone::system();
    let landed = history
        .iter()
        .map(|entry| revisions::Landed {
            at: epoch(&entry.created_at),
            label: landed_label(&entry.created_at, &zone),
        })
        .collect();
    Ok(revisions::Read {
        history,
        landed,
        fires,
        range,
        diff,
    })
}

async fn prepare(c: &Connection, target: &Target) -> Result<Form, String> {
    let client = &c.client;
    let registered = match target {
        Target::Playbook(id) => Some(id.as_str()),
        Target::Draft(_) => None,
    };
    let schema = async {
        match target {
            Target::Playbook(id) => {
                let schema =
                    within("the params schema", client.playbook_schema::<Value>(id)).await?;
                Ok::<_, String>((schema, None))
            }
            Target::Draft(id) => {
                let head: crux::dto::DraftCompile =
                    within("the draft's schema", client.draft_preview(id, None)).await?;
                Ok((
                    head.params_schema.unwrap_or(Value::Null),
                    head.schema_digest,
                ))
            }
        }
    };
    let (schema, caps, targets, providers) = tokio::join!(
        schema,
        within("the launch caps", client.playbook_caps()),
        within("the dispatch targets", client.dispatch_targets(registered)),
        within("the providers", client.providers()),
    );
    let (schema, digest) = schema?;
    let caps: crux::dto::PlaybookCaps = caps?;
    Ok(Form {
        schema,
        digest,
        max_cost: caps.max_cost,
        max_time: caps.max_time,
        targets: targets?,
        providers: providers?,
    })
}

/// Launch `target` with `body`, for the new launch's key.
async fn send_launch(c: &Connection, target: &Target, body: &Value) -> Result<String, String> {
    let ack: crux::dto::LaunchAck = match target {
        Target::Playbook(id) => within("the launch", c.client.launch(id, body)).await?,
        Target::Draft(id) => within("the launch", c.client.launch_draft(id, body)).await?,
    };
    Ok(ack.key)
}

/// Pull `draft`'s newest save into its working copy under `drafts`, creating it from `template`
/// first when the controller has no such draft.
async fn pull(
    c: &Connection,
    drafts: Option<&Path>,
    draft: &str,
    template: Option<&str>,
) -> Result<(Working, Option<String>), String> {
    let root = drafts.ok_or("no home directory for working copies")?;
    let newest = within(
        "the draft",
        c.client.draft_files::<crux::dto::DraftFiles>(draft, None),
    )
    .await;
    let (files, note) = match (newest, template) {
        (Ok(files), _) => (files, None),
        (Err(_), Some(template)) => {
            let _: crux::dto::DraftCompile = within(
                "creating the draft",
                c.client
                    .create_draft(draft, &format!("Drafted from {template}"), Some(template)),
            )
            .await?;
            let files: crux::dto::DraftFiles =
                within("the new draft", c.client.draft_files(draft, None)).await?;
            (
                files,
                Some(format!("Created draft {draft} from {template}")),
            )
        }
        (Err(error), None) => return Err(error),
    };
    let working = Working::write(root, draft, files.version, &files.files)?;
    Ok((working, note))
}

/// Push pack folder `dir` as the next version of `draft`, creating the draft on first drop, and
/// test-fire that version under `max_cost` and `max_time` where the draft's newest run went.
async fn drop_pack(
    c: &Connection,
    dir: &Path,
    draft: &str,
    max_cost: f64,
    max_time: &str,
) -> Result<DropOutcome, String> {
    if draft.is_empty() {
        return Err(format!(
            "{} names no draft: its name has no letters or digits",
            dir.display()
        ));
    }
    let (files, _) = crate::draft::read(dir)?;
    if files.is_empty() {
        return Err(format!("{} holds no pack files", dir.display()));
    }
    let newest = within(
        "the draft",
        c.client.draft_files::<crux::dto::DraftFiles>(draft, None),
    )
    .await;
    let base = match newest {
        Ok(newest) => newest.version,
        Err(_) => {
            let created: crux::dto::DraftCompile = within(
                "creating the draft",
                c.client.create_draft(
                    draft,
                    &format!("Dropped on Crucible Playbooks from {}", dir.display()),
                    None,
                ),
            )
            .await?;
            created.version
        }
    };
    match within("the save", c.client.save_draft(draft, &files, base)).await? {
        crux::client::DraftSave::Saved(_) => {}
        crux::client::DraftSave::Stale(stale) => return Err(stale.error),
    }
    let launches: Vec<crux::dto::PlaybookRun> = within(
        "the draft's launches",
        c.client.playbook_runs(None, Some(draft)),
    )
    .await?;
    let Some(newest) = crate::runs_on::newest_dispatched(&launches) else {
        return Ok(DropOutcome::Saved);
    };
    let newest: crux::dto::PlaybookLaunch =
        within("the draft's newest launch", c.client.playbook_run(newest)).await?;
    let Ok(runs_on) = crate::runs_on::RunsOn::try_from(&newest) else {
        return Ok(DropOutcome::Saved);
    };
    let mut body = serde_json::json!({
        "params": {},
        "max_cost": max_cost,
        "max_time": max_time,
    });
    runs_on.pin(&mut body);
    let ack: crux::dto::LaunchAck =
        within("the launch", c.client.launch_draft(draft, &body)).await?;
    Ok(DropOutcome::Launched(ack.key))
}

/// Save a working copy as its draft's next version.
async fn save(
    c: &Connection,
    drafts: Option<&Path>,
    mut working: Working,
) -> Result<(Working, Option<String>), String> {
    let root = drafts.ok_or("no home directory for working copies")?;
    let (files, digest) = crate::draft::read(&working.dir)?;
    if files.is_empty() {
        return Err("The working copy holds no files".to_string());
    }
    let saved = within(
        "the save",
        c.client.save_draft(&working.draft, &files, working.base),
    )
    .await?;
    match saved {
        crux::client::DraftSave::Saved(raw) => {
            let saved: crux::dto::DraftCompile =
                serde_json::from_value(raw).map_err(|e| format!("reading the save: {e}"))?;
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
}

/// A captured file's copy on disk and its length, when it has been fetched before.
fn file_on_disk(files: Option<&Path>, run: &str, key: &str) -> Option<(Opened, u64)> {
    let disk = crate::captured::cache_path(files?, run, key);
    let bytes = std::fs::read(&disk).ok()?;
    let length = bytes.len() as u64;
    let opened = Opened {
        body: Ok(Body::from_bytes(bytes)),
        run: run.to_string(),
        key: key.to_string(),
        disk: Some(disk),
    };
    Some((opened, length))
}

/// Fetch one captured file and keep a copy under `files`.
async fn fetch_file(c: &Connection, files: Option<&Path>, run: String, key: String) -> Opened {
    let bytes = within("the file", c.client.run_file(&run, &key)).await;
    let path = files.map(|files| crate::captured::cache_path(files, &run, &key));
    let disk = match (&bytes, path) {
        (Ok(bytes), Some(path)) => path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, bytes))
            .ok()
            .map(|()| path),
        _ => None,
    };
    Opened {
        body: bytes.map(Body::from_bytes),
        run,
        key,
        disk,
    }
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
    /// Where the caller may dispatch it, default first.
    pub targets: crux::dto::DispatchTargets,
    /// The provider registrations the caller may read.
    pub providers: Vec<crux::dto::Provider>,
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

/// What a pack folder dropped on the app came to: its draft, and what became of it or why not.
#[derive(Clone, Debug)]
pub struct Dropped {
    pub seq: u64,
    pub dir: std::path::PathBuf,
    pub draft: String,
    pub result: Result<DropOutcome, String>,
}

/// A drop launches the draft where its newest run went, with the same agent; a draft that never
/// ran is only saved, for a launch form to say where it runs.
#[derive(Clone, Debug, PartialEq)]
pub enum DropOutcome {
    Launched(String),
    Saved,
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
    pub status: LaunchStatus,
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
    connector: Connector,
    identity: Option<(String, Identity)>,
    view: LiveView,
    detail: Option<Arc<LiveDetail>>,
    base: Option<Base>,
    launches: HashMap<String, CachedLaunch>,
    packs: HashMap<String, PackFiles>,
    runs: HashMap<String, CachedRun>,
    finished: Vec<Finished>,
    finished_seq: u64,
    /// Handed to each session stream, so new lines wake the worker.
    wake: UnboundedSender<Msg>,
    /// Registered playbook ids, the ones a launch form can start, and when they were read.
    registered: Vec<String>,
    registered_at: Option<Instant>,
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
struct CachedRun {
    fold: RunFold,
    /// Session lines a stream has folded into `fold`.
    lines: usize,
    log: Vec<LogLine>,
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
    fn restart(&mut self, now: Instant) {
        self.replay = Some(Replay::starting(now));
        self.log_replay = Some(Replay::starting(now));
    }

    /// Fold what a stream delivered. `ended` is whether it finished cleanly, once it has ended.
    fn take(&mut self, lines: &[String], logs: &[String], ended: Option<bool>, now: Instant) {
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
    fn new(connector: Connector, wake: UnboundedSender<Msg>) -> Self {
        Fetcher {
            connector,
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
        }
    }

    /// Re-reads the registered playbooks at most once a minute.
    async fn refresh_registered(&mut self) -> bool {
        if self
            .registered_at
            .is_some_and(|at| at.elapsed() < Duration::from_secs(60))
        {
            return false;
        }
        self.registered_at = Some(Instant::now());
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
        let changed = ids != self.registered;
        self.registered = ids;
        changed
    }

    /// Records each launch the new list shows stopped that the old list showed running.
    fn note_finished(&mut self, next: &[LivePlaybook]) {
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

    fn view(&self) -> &LiveView {
        &self.view
    }

    /// Read launch `key`'s run as the comparison base, once: a stopped run never changes, so a
    /// base already read stays. A launch the list does not show yet is tried again later.
    async fn refresh_base(&mut self, key: Option<&str>) -> bool {
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

    fn any_running(&self) -> bool {
        self.view
            .launches()
            .iter()
            .any(|launch| launch.status.is_running())
    }

    /// Whether the selected launch's run can still change: it is running, or it stopped and its
    /// stored session has not been read yet.
    fn selected_live(&self, selected: Option<&str>) -> bool {
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
    fn unavailable(&mut self, error: Down) -> bool {
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

    async fn connection(&self) -> Result<Connection, String> {
        self.connector.connect().await
    }

    async fn refresh_list(&mut self) -> Result<bool, Down> {
        if self.identity.is_none() {
            let connection = self.connection().await?;
            let whoami = reaching("whoami", connection.client.whoami()).await?;
            let endpoint = connection.client.base().to_string();
            self.identity = Some((endpoint, identity(whoami)));
        }
        let (endpoint, identity) = self.identity.clone().unwrap_or_default();

        let connection = self.connection().await?;
        let launches: Vec<crux::dto::PlaybookRun> = reaching(
            "the launch list",
            connection.client.playbook_runs(None, None),
        )
        .await?;
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
                .connector
                .offline()
                .then(|| "Offline; showing what the controller last answered".to_string()),
        };
        let changed = next != self.view;
        self.view = next;
        Ok(changed)
    }

    /// The launch's own record, re-read when its run count or status moved.
    async fn launch_detail(&mut self, launch: &LivePlaybook) -> Option<crux::dto::PlaybookLaunch> {
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
    async fn pack(
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
    async fn refresh_detail(&mut self, selected: Option<&str>, poll: bool) -> bool {
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
        let mut next = build_detail(&launch, fold, log);
        next.source = source;
        next.error = error;
        next.launch = record;
        next.pack = pack;
        next.streaming = streaming;
        next.run_id = run_id;
        next.files = files;
        let changed = self.detail.as_deref() != Some(&next);
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

async fn captured_files(connection: &Connection, run_id: &str) -> Result<Vec<Captured>, String> {
    within("the captured files", connection.client.run_files(run_id))
        .await
        .map(captured)
}

fn captured(listing: crux::dto::RunFiles) -> Vec<Captured> {
    listing.files.into_iter().map(Captured::from).collect()
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
        autoresearch: whoami.entitlements.iter().any(|e| e == "autoresearch"),
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
async fn console(connection: &Connection, run_id: &str) -> Result<Vec<LogLine>, String> {
    within("the console", connection.client.run_log(run_id))
        .await
        .map(typed_log)
}

fn typed_log(log: crux::dto::RunLog) -> Vec<LogLine> {
    log.text
        .map(|text| text.lines().map(LogLine::parse).collect())
        .unwrap_or_default()
}

fn build_detail(launch: &LivePlaybook, fold: RunFold, log: Vec<LogLine>) -> LiveDetail {
    let settled = !fold.tasks.is_empty() && fold.required_settled();
    let verdict = match launch.status {
        LaunchStatus::Running => Verdict::Running,
        LaunchStatus::Done if !settled => Verdict::Unfinished,
        LaunchStatus::Done if fold.required_passed() => Verdict::Valid,
        LaunchStatus::Done => Verdict::Invalid,
        LaunchStatus::Error => Verdict::Error,
        LaunchStatus::Cancelled => Verdict::Cancelled,
        LaunchStatus::New => Verdict::Unfinished,
        LaunchStatus::Other(_) if settled && fold.required_passed() => Verdict::Valid,
        LaunchStatus::Other(_) => Verdict::Unfinished,
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
        run_id: None,
        files: None,
    }
}

/// `18:00`, the time of day of an RFC 3339 stamp in `zone`; empty when it does not parse.
fn clock(stamp: &str, zone: &TimeZone) -> String {
    stamp
        .parse::<Timestamp>()
        .map(|at| at.to_zoned(zone.clone()).strftime("%H:%M").to_string())
        .unwrap_or_default()
}

fn unix_now() -> i64 {
    Timestamp::now().as_second()
}

/// Whole Unix seconds from an RFC 3339 stamp.
fn epoch(stamp: &str) -> Option<i64> {
    stamp.parse::<Timestamp>().ok().map(|at| at.as_second())
}

/// `Nov 6 18:00` in `zone`; the raw stamp when it does not parse.
fn landed_label(stamp: &str, zone: &TimeZone) -> String {
    stamp.parse::<Timestamp>().map_or_else(
        |_| stamp.to_string(),
        |at| {
            at.to_zoned(zone.clone())
                .strftime("%b %-d %H:%M")
                .to_string()
        },
    )
}

/// What the live worker last read and what the window's actions last came to.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub view: Arc<LiveView>,
    pub detail: Option<Arc<LiveDetail>>,
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
    /// The captured file last fetched for the Files pane.
    pub file: Option<Opened>,
    pub revisions: Option<Arc<Revisions>>,
}

/// What one pass of the poll loop read.
struct Polled {
    view: Arc<LiveView>,
    detail: Option<Arc<LiveDetail>>,
    base: Option<Base>,
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
    /// The poll loop.
    _poll: Option<Task>,
}

/// Where the worker's actions keep files.
#[derive(Clone)]
struct Dirs {
    /// Draft working copies.
    drafts: Option<PathBuf>,
    /// Copies of fetched captured files.
    files: Option<PathBuf>,
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

    fn spawn_with(connector: Connector, dirs: Dirs) -> Self {
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
        runtime.spawn(async move {
            let launches = action.launches();
            let reply = match connector.connect().await {
                Ok(connection) => action.run(connection, &dirs).await,
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
            Ok(Some(Msg::Reconnect)) => {
                fetcher.connector.reset();
                fetcher.identity = None;
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
                finished: fetcher.finished.clone(),
                registered: fetcher.registered.clone(),
            };
            publish(&shared, |shared| shared.polled(polled));
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
            status: LaunchStatus::parse(status, None),
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
                autoresearch: false,
            },
            now: NOW,
            launches,
            stale: None,
        }
    }

    #[test]
    fn a_launch_that_stops_running_is_noted_once_and_the_list_is_bounded() {
        let (wake, _rx) = mpsc::unbounded_channel();
        let net = crate::net::Net::uncached_for_tests();
        let mut fetcher = Fetcher::new(net.connector(crate::profile::Target::unreachable()), wake);
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
        let (wake, _rx) = mpsc::unbounded_channel();
        let net = crate::net::Net::uncached_for_tests();
        let mut fetcher = Fetcher::new(net.connector(crate::profile::Target::unreachable()), wake);
        assert!(fetcher.unavailable(Down::Unreached("dial refused".into())));
        assert!(matches!(fetcher.view, LiveView::Unavailable(_)));

        fetcher.view = ready(vec![firing("nightly", 1, "schedule", "done")]);
        assert!(fetcher.unavailable(Down::Refused("replica b: 502".into())));
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
            !fetcher.unavailable(Down::Refused("replica b: 502".into())),
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
        let tokyo = TimeZone::get("Asia/Tokyo").unwrap();
        assert_eq!(
            landed_label("2026-11-06T18:00:00Z", &TimeZone::UTC),
            "Nov 6 18:00"
        );
        assert_eq!(landed_label("2026-11-06T18:00:00Z", &tokyo), "Nov 7 03:00");
        assert_eq!(landed_label("not a stamp", &tokyo), "not a stamp");
        assert_eq!(clock("2026-11-06T18:00:00Z", &tokyo), "03:00");
        assert_eq!(clock("not a stamp", &tokyo), "");
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

    fn session_lines() -> Vec<String> {
        SESSION.lines().map(str::to_string).collect()
    }

    /// A run whose first stream delivered the whole of [`SESSION`] and three console lines.
    fn streamed(now: Instant) -> CachedRun {
        let mut run = CachedRun::default();
        run.restart(now);
        let logs = ["one", "two", "three"].map(str::to_string);
        run.take(&session_lines(), &logs, None, now);
        run
    }

    fn raw(log: &[LogLine]) -> Vec<&str> {
        log.iter().map(|line| line.raw.as_str()).collect()
    }

    #[test]
    fn a_first_stream_shows_its_lines_as_they_land() {
        let now = Instant::now();
        let lines = session_lines();
        let mut run = CachedRun::default();
        run.restart(now);
        run.take(&lines[..1], &["one".to_string()], None, now);
        assert_eq!(run.fold.tasks.len(), 2);
        assert!(run.fold.tasks.iter().all(|task| task.result.is_none()));
        assert_eq!(raw(&run.log), ["one"]);
        run.take(&lines[1..], &["two".to_string()], None, now);
        assert_eq!(run.fold, session());
        assert_eq!(raw(&run.log), ["one", "two"]);
    }

    #[test]
    fn a_restarted_stream_stays_off_screen_until_it_reaches_what_is_shown() {
        let now = Instant::now();
        let lines = session_lines();
        let mut run = streamed(now);
        run.restart(now);
        run.take(&lines[..1], &["one".to_string()], None, now);
        assert_eq!(run.fold, session(), "a partial session replay is held");
        assert_eq!(raw(&run.log), ["one", "two", "three"]);
        run.take(&lines[1..2], &["two".to_string()], None, now);
        assert_eq!(run.fold, session());
        assert_eq!(raw(&run.log), ["one", "two", "three"]);

        let note = r#"{"v":1,"kind":"note","msg":"hi"}"#.to_string();
        run.take(&[lines[2].clone(), note], &["three".to_string()], None, now);
        assert_eq!(run.fold.tasks, session().tasks);
        assert_eq!(
            run.fold.notes,
            ["hi"],
            "the replay is shown once it has caught up"
        );
        assert_eq!(raw(&run.log), ["one", "two", "three"]);

        let later = r#"{"v":1,"kind":"note","msg":"later"}"#.to_string();
        run.take(&[later], &["four".to_string()], None, now);
        assert_eq!(
            run.fold.notes,
            ["hi", "later"],
            "then lines fold in as they land"
        );
        assert_eq!(raw(&run.log), ["one", "two", "three", "four"]);
    }

    #[test]
    fn a_replay_that_goes_quiet_short_of_what_is_shown_replaces_it() {
        let now = Instant::now();
        let mut run = streamed(now);
        run.restart(now);
        run.take(&session_lines()[..1], &["one".to_string()], None, now);
        let almost = now + REPLAY_QUIET - Duration::from_millis(1);
        run.take(&[], &[], None, almost);
        assert_eq!(run.fold, session());
        assert_eq!(raw(&run.log), ["one", "two", "three"]);
        run.take(&[], &[], None, now + REPLAY_QUIET);
        assert!(run.fold.tasks.iter().all(|task| task.result.is_none()));
        assert_eq!(raw(&run.log), ["one"]);
    }

    #[test]
    fn a_stream_that_finishes_short_of_what_is_shown_replaces_it() {
        let now = Instant::now();
        let mut run = streamed(now);
        run.restart(now);
        run.take(&session_lines()[..1], &["one".to_string()], Some(true), now);
        assert!(run.fold.tasks.iter().all(|task| task.result.is_none()));
        assert_eq!(raw(&run.log), ["one"]);
    }

    #[test]
    fn a_stream_that_fails_part_way_through_its_replay_replaces_nothing() {
        let now = Instant::now();
        let mut run = streamed(now);
        run.restart(now);
        let late = now + REPLAY_QUIET * 4;
        run.take(
            &session_lines()[..1],
            &["one".to_string()],
            Some(false),
            late,
        );
        assert_eq!(run.fold, session());
        assert_eq!(raw(&run.log), ["one", "two", "three"]);
    }

    #[test]
    fn a_restarted_stream_that_delivers_nothing_never_blanks_the_run() {
        let now = Instant::now();
        let mut run = streamed(now);
        run.restart(now);
        run.take(&[], &[], None, now + REPLAY_QUIET * 4);
        run.take(&[], &[], Some(true), now + REPLAY_QUIET * 8);
        assert_eq!(run.fold, session());
        assert_eq!(raw(&run.log), ["one", "two", "three"]);
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
    fn a_launch_parked_by_a_cancel_is_cancelled_not_an_error() {
        let parse = LaunchStatus::parse;
        assert_eq!(
            parse("parked", Some("cancelled by weaton")),
            LaunchStatus::Cancelled
        );
        assert_eq!(
            parse("error", Some("cancelled by an operator")),
            LaunchStatus::Cancelled
        );
        assert_eq!(
            parse("parked", Some("image preflight refused")),
            LaunchStatus::Error
        );
        assert_eq!(parse("parked", None), LaunchStatus::Error);
        assert_eq!(
            parse("done", Some("cancelled by weaton")),
            LaunchStatus::Done
        );
        assert_eq!(
            parse("running", Some("cancelled by weaton")),
            LaunchStatus::Running
        );
        assert_eq!(parse("held", None), LaunchStatus::Other("held".into()));
        let cancelled = LivePlaybook {
            status: LaunchStatus::Cancelled,
            ..LivePlaybook::default()
        };
        assert_eq!(cancelled.verdict(), Verdict::Cancelled);
        assert_eq!(cancelled.status.to_string(), "cancelled");
        let detail = build_detail(&cancelled, RunFold::default(), Vec::new());
        assert_eq!(detail.verdict, Verdict::Cancelled);
    }

    #[test]
    fn a_parked_launch_without_a_graph_is_an_error_and_keeps_nothing() {
        let parked = launch("parked");
        assert_eq!(parked.verdict(), Verdict::Error);
        let detail = build_detail(&parked, RunFold::default(), Vec::new());
        assert_eq!(detail.verdict, Verdict::Error);
        assert!(detail.fold.tasks.is_empty());
        assert_eq!(detail.spent, 0.5);
    }

    /// A controller that takes `slow` to answer the launch list and answers everything else at
    /// once: a launch with its key, `whoami` with a user. `listing` is set once a list read is in.
    fn slow_list_server(slow: Duration, listing: Arc<std::sync::atomic::AtomicBool>) -> String {
        let (url, _) = crate::testserver::serve(move |request| {
            if request.starts_with("POST ") {
                return r#"{"key":"playbook:demo:7"}"#.into();
            }
            if request.starts_with("GET /api/whoami") {
                return r#"{"user":"wren","role":"user"}"#.into();
            }
            if request.starts_with("GET /api/playbook-runs") {
                listing.store(true, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(slow);
            }
            "[]".into()
        });
        url
    }

    fn no_dirs() -> Dirs {
        Dirs {
            drafts: None,
            files: None,
        }
    }

    #[test]
    fn a_captured_files_copy_stands_in_only_for_a_settled_run_at_the_listed_size() {
        let fetched = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&fetched);
        let (url, _) = crate::testserver::serve(move |request| {
            if request.contains("/files/") || request.contains("/file") {
                let n = counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return ["v1", "v2", "third"].get(n).unwrap_or(&"late").to_string();
            }
            "[]".into()
        });
        let (keys, files) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let net = crate::net::Net::uncached_for_tests();
        let client = Client::spawn_with(
            net.connector(crate::testserver::target("t", &url, keys.path())),
            Dirs {
                drafts: None,
                files: Some(files.path().to_path_buf()),
            },
        );
        let count = || fetched.load(std::sync::atomic::Ordering::SeqCst);
        let shows = |text: &'static str| {
            let snapshot = snapshot_when(&client, Duration::from_secs(5), |s| {
                s.file
                    .as_ref()
                    .is_some_and(|f| f.body == Ok(Body::Text(text.into())))
            });
            snapshot.file.unwrap()
        };

        client.file("r1", "report/out.md", 2, false);
        let first = shows("v1");
        let disk = first.disk.expect("a copy on disk");
        assert_eq!(std::fs::read_to_string(&disk).unwrap(), "v1");

        client.file("r1", "report/out.md", 2, false);
        shows("v2");
        assert_eq!(count(), 2, "a running run's file is fetched again");
        assert_eq!(std::fs::read_to_string(&disk).unwrap(), "v2");

        client.file("r1", "report/out.md", 2, true);
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(
            count(),
            2,
            "a settled run's copy at the listed size stands in"
        );
        shows("v2");

        client.file("r1", "report/out.md", 5, true);
        shows("third");
        assert_eq!(count(), 3, "a copy of another size is stale");
    }

    /// The first snapshot `ready` accepts, within `within`.
    fn snapshot_when(
        client: &Client,
        within: Duration,
        ready: impl Fn(&Snapshot) -> bool,
    ) -> Snapshot {
        let until = Instant::now() + within;
        let mut seen = 0;
        loop {
            if let Some((version, snapshot)) = client.take(seen) {
                seen = version;
                if ready(&snapshot) {
                    return snapshot;
                }
            }
            assert!(Instant::now() < until, "no such snapshot within {within:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A controller holding draft `studio` at three saves, test-fired at v1 (passed) and v3
    /// (failed), that records each diff query it is asked.
    fn history_server(asked: Arc<Mutex<Vec<String>>>) -> String {
        let save = |version: i64, previous: Option<i64>| {
            serde_json::json!({
                "version": version, "previous_version": previous, "created_by": "wren",
                "created_at": format!("2026-08-22T09:0{version}:00Z"),
                "schema_digest": "sha256:aa", "diagnostics": 0,
                "changes": [{"path": "wf.star", "change": "modified"}],
            })
        };
        let (url, _) = crate::testserver::serve(move |request| {
            let path = request.split(' ').nth(1).unwrap_or_default();
            if path == "/api/playbook-drafts/studio/history" {
                return serde_json::json!([save(3, Some(2)), save(2, Some(1)), save(1, None)])
                    .to_string();
            }
            if path == "/api/playbook-runs?playbook=studio" {
                return serde_json::json!([
                    {"key": "playbook:studio:b", "status": "parked",
                     "parked_reason": "run errored", "draft_version": 3},
                    {"key": "playbook:studio:a", "status": "done", "draft_version": 1},
                    {"key": "playbook:survey:z", "status": "done"},
                ])
                .to_string();
            }
            if let Some(query) = path.strip_prefix("/api/playbook-drafts/studio/diff?") {
                asked.lock().unwrap().push(query.to_string());
                let end = |name: &str| {
                    query
                        .split('&')
                        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
                        .and_then(|value| value.parse::<i64>().ok())
                };
                return serde_json::json!({
                    "from": end("from"), "to": end("to"),
                    "files": [{
                        "path": "wf.star", "change": "modified", "additions": 1, "deletions": 1,
                        "unified": "--- a/wf.star\n+++ b/wf.star\n@@ -1 +1 @@\n-a\n+b\n",
                    }],
                })
                .to_string();
            }
            "[]".into()
        });
        url
    }

    #[test]
    fn a_drafts_history_reads_its_saves_its_test_fires_and_the_diff_of_one_range() {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let url = history_server(Arc::clone(&asked));
        let keys = tempfile::tempdir().unwrap();
        let net = crate::net::Net::uncached_for_tests();
        let client = Client::spawn_with(
            net.connector(crate::testserver::target("t", &url, keys.path())),
            no_dirs(),
        );
        let range = |from: i64, to: i64| revisions::Range {
            from: Some(from),
            to,
        };
        let read = |draft: &str, picked: Option<revisions::Range>| {
            let wanted = Wanted {
                draft: draft.into(),
                range: picked,
                ran: Some(3),
            };
            client.revisions(wanted.clone());
            snapshot_when(&client, Duration::from_secs(5), |s| {
                s.revisions.as_ref().is_some_and(|r| r.wanted == wanted)
            })
            .revisions
            .unwrap()
        };

        let opened = read("studio", None);
        let history = opened.result.as_ref().expect("the draft reads");
        assert_eq!(
            history.range,
            range(1, 3),
            "the failed run opens on everything saved since v1 passed"
        );
        assert_eq!(history.history.len(), 3);
        assert_eq!(
            history
                .fires
                .iter()
                .map(|fire| (fire.version, fire.status.clone()))
                .collect::<Vec<_>>(),
            vec![(3, LaunchStatus::Error), (1, LaunchStatus::Done)],
            "a registered playbook's launch is no test-fire"
        );
        assert_eq!(history.landed[0].at, epoch("2026-08-22T09:03:00Z"));
        assert_eq!(revisions::totals(&history.diff), (1, 1));

        let picked = read("studio", Some(range(3, 2)));
        assert_eq!(picked.result.as_ref().expect("reads").range, range(3, 2));

        let lost = read("studio", Some(range(9, 3)));
        assert_eq!(
            lost.result.as_ref().expect("reads").range,
            range(1, 3),
            "a range naming a save the draft does not hold opens afresh"
        );
        assert_eq!(
            *asked.lock().unwrap(),
            ["from=1&to=3", "from=3&to=2", "from=1&to=3"]
        );

        assert_eq!(
            read("empty", None)
                .result
                .as_ref()
                .err()
                .map(String::as_str),
            Some("draft empty has no saves")
        );
    }

    #[test]
    fn a_launch_lands_while_the_launch_list_is_still_being_read() {
        let listing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let url = slow_list_server(Duration::from_secs(4), Arc::clone(&listing));
        let keys = tempfile::tempdir().unwrap();
        let net = crate::net::Net::uncached_for_tests();
        let client = Client::spawn_with(
            net.connector(crate::testserver::target("t", &url, keys.path())),
            no_dirs(),
        );
        let until = Instant::now() + Duration::from_secs(5);
        while !listing.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(Instant::now() < until, "the worker never read the list");
            std::thread::sleep(Duration::from_millis(10));
        }

        let asked = Instant::now();
        client.launch(
            Target::Playbook("demo".into()),
            serde_json::json!({"params": {}}),
        );
        client.prepare(Target::Playbook("demo".into()));
        let snapshot = snapshot_when(&client, Duration::from_secs(2), |s| {
            s.launched.is_some() && s.prepared.is_some()
        });
        assert!(asked.elapsed() < Duration::from_secs(2));
        assert!(
            matches!(*snapshot.view, LiveView::Loading),
            "the list is still being read"
        );
        let launched = snapshot.launched.unwrap();
        assert_eq!(launched.playbook, "demo");
        assert_eq!(launched.result, Ok("playbook:demo:7".to_string()));
        let prepared = snapshot.prepared.unwrap();
        assert_eq!(prepared.target, Target::Playbook("demo".into()));
        let mut seqs = [launched.seq, prepared.seq];
        seqs.sort();
        assert_eq!(seqs, [1, 2], "each reply takes the next number");

        let listed = snapshot_when(&client, Duration::from_secs(8), |s| {
            matches!(*s.view, LiveView::Ready { .. })
        });
        assert_eq!(
            listed.launched.map(|l| l.seq),
            Some(launched.seq),
            "a poll leaves the replies in place"
        );
    }

    #[test]
    fn dropping_the_client_ends_its_polling() {
        let lists = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&lists);
        let (url, _) = crate::testserver::serve(move |request| {
            if request.starts_with("GET /api/whoami") {
                return r#"{"user":"wren","role":"user"}"#.into();
            }
            if request.starts_with("GET /api/playbook-runs") {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return r#"[{"key":"playbook:demo:1","playbook":"demo","status":"running",
                    "runs":0,"max_cost":2.0,"max_time":"30m","created_at":"2026-11-09T01:00:00Z"}]"#
                    .into();
            }
            "[]".into()
        });
        let keys = tempfile::tempdir().unwrap();
        let net = crate::net::Net::uncached_for_tests();
        let client = Client::spawn_with(
            net.connector(crate::testserver::target("t", &url, keys.path())),
            no_dirs(),
        );
        let listed = snapshot_when(&client, Duration::from_secs(5), |s| {
            matches!(*s.view, LiveView::Ready { .. })
        });
        assert_eq!(listed.view.launches().len(), 1);
        let until = Instant::now() + Duration::from_secs(8);
        while lists.load(std::sync::atomic::Ordering::SeqCst) < 2 {
            assert!(Instant::now() < until, "a running launch is polled again");
            std::thread::sleep(Duration::from_millis(20));
        }

        drop(client);
        std::thread::sleep(Duration::from_millis(300));
        let at_drop = lists.load(std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(Duration::from_secs(5));
        assert_eq!(
            lists.load(std::sync::atomic::Ordering::SeqCst),
            at_drop,
            "nothing reads the list once the client is gone"
        );
    }

    #[test]
    fn an_action_with_no_connection_to_run_on_is_answered_with_why() {
        let keys = tempfile::tempdir().unwrap();
        let mut profile = crate::profile::Profile {
            url: "http://127.0.0.1:9".into(),
            ..Default::default()
        };
        profile.tunnel = Some("exit 7".into());
        let target =
            crate::profile::Target::new("t", profile, &crate::keys::Store::Dir(keys.path().into()));
        let _keeper = target.start(keys.path(), || Err(std::io::Error::other("no supervisor")));
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !matches!(target.readiness(), crate::profile::Readiness::Failed(_)) {
            assert!(std::time::Instant::now() < until, "the start never failed");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let net = crate::net::Net::uncached_for_tests();
        let client = Client::spawn_with(net.connector(target), no_dirs());
        client.preview_rerun("run-1", vec!["draft".into()]);
        client.pull("demo", None);
        let snapshot = snapshot_when(&client, Duration::from_secs(5), |s| {
            s.reran.is_some() && s.synced.is_some()
        });
        let reran = snapshot.reran.unwrap();
        assert_eq!((reran.run.as_str(), reran.from.len()), ("run-1", 1));
        assert!(reran.result.unwrap_err().contains("no supervisor"));
        let synced = snapshot.synced.unwrap();
        assert_eq!(synced.draft, "demo");
        assert!(synced.result.unwrap_err().contains("no supervisor"));
    }

    /// Against the controller `crux` is configured for: read a registered pack with its Git
    /// links, pull a draft into a working copy, read its launch form with the dispatch targets
    /// and providers, and preview rerunning the draft's newest finished run. None of it writes to the controller. `CRUCIBLE_E2E_PLAYBOOK` and `CRUCIBLE_E2E_DRAFT`
    /// name the two; run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs a live controller"]
    fn a_live_draft_pulls_into_a_working_copy_and_its_run_previews_a_rerun() {
        let draft =
            std::env::var("CRUCIBLE_E2E_DRAFT").unwrap_or_else(|_| "vllm-agent-fanout".into());
        let (wake, _rx) = mpsc::unbounded_channel();
        let crux = crate::profile::or_default(&std::collections::BTreeMap::new())
            .remove("default")
            .expect("crux's own profile");
        let target = crate::profile::Target::new(
            "default",
            crux,
            &crate::keys::Store::Dir("/nonexistent".into()),
        );
        let net = crate::net::Net::uncached_for_tests();
        let mut fetcher = Fetcher::new(net.connector(target), wake);
        let root = tempfile::tempdir().unwrap();
        let runtime = net.handle().expect("a runtime");
        let c = runtime
            .block_on(fetcher.connection())
            .expect("a connection");

        let (working, note) = c
            .wait(pull(&c, Some(root.path()), &draft, None))
            .expect("the draft pulls");
        assert_eq!(note, None, "an existing draft is pulled, not created");
        assert!(working.base > 0);
        assert!(working.dir.join("crucible.toml").is_file());
        assert!(!working.edited());
        assert_eq!(
            crate::draft::Working::open(root.path(), &draft).as_ref(),
            Some(&working)
        );

        let registered = std::env::var("CRUCIBLE_E2E_PLAYBOOK").unwrap_or_else(|_| "triage".into());
        let pinned = runtime
            .block_on(fetcher.pack(&registered, None))
            .expect("the registered pack reads");
        assert!(pinned.file("crucible.toml").is_some());
        assert!(
            pinned
                .web_url("crucible.toml", 1)
                .is_some_and(|url| url.starts_with("https://") && url.ends_with("crucible.toml#L1")),
            "{:?}",
            pinned.web
        );

        let form = c
            .wait(prepare(&c, &Target::Draft(draft.clone())))
            .expect("the draft's form");
        assert!(
            form.digest.is_some(),
            "a compiling draft has a schema digest"
        );
        assert!(form.max_cost > 0.0);
        assert!(
            form.targets.targets.iter().filter(|t| t.default).count() == 1,
            "exactly one offered target is the default: {:?}",
            form.targets
        );
        assert!(
            crate::runs_on::RunsOnForm::new(&form.targets, &form.providers, Default::default())
                .providers
                .len()
                > 1,
            "an enabled agent provider is offered besides the default"
        );

        runtime
            .block_on(fetcher.refresh_list())
            .expect("the launch list");
        let launch = fetcher
            .view()
            .launches_of(&draft)
            .find(|launch| !launch.status.is_running() && launch.runs > 0)
            .cloned()
            .expect("a finished launch of the draft");
        let record = runtime
            .block_on(fetcher.launch_detail(&launch))
            .expect("the launch detail");
        let run = record.runs.first().expect("a run").run_id.clone();
        let preview = c.wait(within(
            "the rerun preview",
            c.client
                .rerun_preview::<crux::dto::RerunPreview>(&run, &Vec::new()),
        ));
        match preview {
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
