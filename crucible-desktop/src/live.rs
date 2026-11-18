//! The live controller view: registered playbooks and every launch of them, read through the
//! `crux` client library.
//!
//! The controller and its key come from the profile in use; see [`crate::profile`].

mod action;
pub mod client;
mod fetcher;
mod reports;
mod shape;

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::Value;

use crate::captured::{Captured, Opened};
use crate::draft::Working;
use crate::filter::LaunchFilter;
use crate::logline::LogLine;
use crate::model::Verdict;
use crate::net::Down;
use crate::pack::PackFiles;
use crate::report::{Built, WantedRuns};
use crate::revisions::Revisions;
use crate::session::RunFold;

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
    /// The report last built for the Report pane.
    pub report: Option<Arc<Built<crate::report::Wanted>>>,
    /// The runs view last built for the runs page.
    pub runs_report: Option<Arc<Built<WantedRuns>>>,
}

#[cfg(test)]
mod tests {
    use crate::captured::Body;
    use crate::filter::{LandedFilter, LaunchFilter, OriginFilter, OwnerFilter};
    use crate::live::action::prepare;
    use crate::live::action::pull;
    use crate::live::client::Client;
    use crate::live::client::Dirs;
    use crate::live::fetcher::CachedRun;
    use crate::live::fetcher::FINISHED;
    use crate::live::fetcher::Fetcher;
    use crate::live::fetcher::REPLAY_QUIET;
    use crate::live::shape::build_detail;
    use crate::live::shape::clock;
    use crate::live::shape::epoch;
    use crate::live::shape::graph_fold;
    use crate::live::shape::identity;
    use crate::live::shape::landed_label;
    use crate::live::*;
    use crate::net::within;
    use crate::revisions;
    use crate::revisions::Wanted;
    use jiff::tz::TimeZone;
    use rune::time::Instant;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::mpsc;

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

    #[test]
    fn a_report_is_built_from_the_plugin_and_the_captured_files_it_asks_for() {
        use base64::Engine;
        let pack = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/report");
        let wasm = std::fs::read(pack.join("ui/report.wasm"))
            .expect("build it with `just example-report-view`");
        let encoded = base64::engine::general_purpose::STANDARD.encode(&wasm);
        let plugin: String = encoded
            .as_bytes()
            .chunks(76)
            .map(|line| format!("{}\n", String::from_utf8_lossy(line)))
            .collect();
        let asked = Arc::new(Mutex::new(Vec::<String>::new()));
        let (seen, lanes) = (Arc::clone(&asked), pack.clone());
        let (url, _) = crate::testserver::serve(move |line| {
            if !line.contains("/files/") {
                return "[]".into();
            }
            seen.lock().unwrap().push(line.to_string());
            let lane = if line.contains("triage-go") {
                "go"
            } else {
                "python"
            };
            std::fs::read_to_string(lanes.join(format!("inbox/{lane}.json"))).unwrap()
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
        let captured = |task: &str, path: &str, size: u64| Captured {
            task: task.into(),
            instance: None,
            path: path.into(),
            size,
            key: format!("{task}/{path}"),
        };
        let size = |lane: &str| {
            std::fs::metadata(pack.join(format!("inbox/{lane}.json")))
                .unwrap()
                .len()
        };
        let listed = vec![
            captured("triage-go", "verdicts.json", size("go")),
            captured("triage-go", "notes.md", 3),
            captured("triage-python", "verdicts.json", size("python")),
        ];
        let wanted = |run: &str, settled: bool| crate::report::Wanted {
            asked: Default::default(),
            run: run.into(),
            plugin: "ui/report.wasm.b64".into(),
            listed: listed.iter().map(|f| (f.key.clone(), f.size)).collect(),
            settled,
        };
        let built = |wanted: &crate::report::Wanted| {
            let snapshot = snapshot_when(&client, Duration::from_secs(30), |s| {
                s.report.as_ref().is_some_and(|b| &b.wanted == wanted)
            });
            snapshot.report.unwrap()
        };
        let triaged = |built: &Built<crate::report::Wanted>| {
            let view = built.report.as_ref().unwrap().view.as_ref().unwrap();
            let crucible_view::Node::Stack { children } = &view.body else {
                panic!("the report is a stack");
            };
            let crucible_view::Node::Stats { items } = &children[0] else {
                panic!("the report starts with its stats");
            };
            items[0].value.clone()
        };

        client.report(wanted("r1", false), plugin.clone(), listed.clone());
        let first = built(&wanted("r1", false));
        assert_eq!(triaged(&first), "64");
        let held = |client: &Client| client.reporting.plugins.lock().unwrap().held().cloned();
        let loaded = held(&client).expect("the worker keeps the plugin it loaded");
        let fetched = asked.lock().unwrap().clone();
        assert_eq!(fetched.len(), 2, "{fetched:?}");
        assert!(fetched.iter().all(|line| line.contains("verdicts.json")));

        client.report(wanted("r1", true), plugin.clone(), listed.clone());
        assert_eq!(triaged(&built(&wanted("r1", true))), "64");
        assert_eq!(
            asked.lock().unwrap().len(),
            2,
            "a settled run's copies stand in"
        );

        client.report(wanted("r2", true), "not base64!".into(), listed.clone());
        let refused = built(&wanted("r2", true));
        let error = refused.report.as_ref().unwrap().view.clone().unwrap_err();
        assert!(error.starts_with("the plugin is not base64"), "{error}");
        assert_eq!(asked.lock().unwrap().len(), 2, "nothing is fetched for it");

        client.report(wanted("r3", false), plugin.clone(), Vec::new());
        assert_eq!(
            built(&wanted("r3", false)).report.as_ref().unwrap().view,
            Err("No task has captured verdicts.json yet.".into())
        );

        let laned = crate::report::Wanted {
            asked: crate::report::Asked {
                params: [(
                    "task".to_string(),
                    crucible_view::Param::Select("triage-go".into()),
                )]
                .into(),
                ..Default::default()
            },
            ..wanted("r1", true)
        };
        assert_ne!(
            laned,
            wanted("r1", true),
            "params tell one report from another"
        );
        client.report(laned.clone(), plugin.clone(), listed.clone());
        let built_laned = built(&laned);
        assert_eq!(triaged(&built_laned), "34");
        assert_eq!(built_laned.report.as_ref().unwrap().asked, laned.asked);
        assert!(
            held(&client).is_some_and(|held| Arc::ptr_eq(&loaded, &held)),
            "the plugin is loaded once while its text stays the same"
        );

        let fetches = || asked.lock().unwrap().len();
        client.report(wanted("r1", false), plugin.clone(), listed.clone());
        built(&wanted("r1", false));
        assert_eq!(fetches(), 4, "a running run's files are fetched again");
        let event = |turn: u64, wanted: crate::report::Wanted| crate::report::Wanted {
            asked: crate::report::Asked {
                turn,
                state: Some("s".into()),
                event: Some(crucible_view::Event::Tick),
                ..Default::default()
            },
            ..wanted
        };
        let ticked = event(1, wanted("r1", false));
        client.report(ticked.clone(), plugin.clone(), listed.clone());
        let answered = built(&ticked);
        assert_eq!(triaged(&answered), "64");
        assert_eq!(
            answered.report.as_ref().unwrap().asked.event,
            Some(crucible_view::Event::Tick)
        );
        assert_eq!(fetches(), 4, "an event reads nothing again");

        let mut grown = listed.clone();
        grown[1].size = 4;
        let moved = crate::report::Wanted {
            listed: grown.iter().map(|f| (f.key.clone(), f.size)).collect(),
            ..event(2, wanted("r1", false))
        };
        client.report(moved.clone(), plugin.clone(), grown);
        assert_eq!(triaged(&built(&moved)), "64");
        assert_eq!(fetches(), 6, "but an event for other files reads them");
    }

    #[test]
    fn a_runs_view_is_built_from_what_each_launch_captured() {
        const PLUGIN: &str = r#"
            export const inputs = ["verdicts.json"];
            export function view() { return { title: "One", body: { stats: { items: [] } } }; }
            export const runsInputs = ["verdicts.json"];
            export function runs({ runs, total, params }) {
                const items = runs.map((run) => ({
                    label: `${run.id} ${run.status} ${run.params?.profile} v${run.pack_version} ${run.cost_usd ?? "-"}`,
                    value: run.files.map((file) => `${file.task}:${file.text}`).join(","),
                }));
                items.push({ label: "total", value: `${total} ${params.lane ?? "all"}` });
                return { title: "History", body: { stats: { items } } };
            }
        "#;
        let asked = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = Arc::clone(&asked);
        let (url, _) = crate::testserver::serve(move |line| {
            seen.lock().unwrap().push(line.to_string());
            let launch = ["PB-0", "PB-1", "PB-2", "PB-3"]
                .into_iter()
                .find(|key| line.contains(key))
                .unwrap_or("none");
            if line.contains("/files/") {
                format!("[\"{launch}\"]")
            } else if line.contains("/files") && launch == "PB-0" {
                let part = |at: usize| {
                    serde_json::json!({ "task": format!("part-{at}"), "path": "verdicts.json",
                        "size_bytes": crate::report::FILE_BYTES,
                        "key": format!("part-{at}/verdicts.json") })
                };
                serde_json::json!({ "files": (0..5).map(part).collect::<Vec<_>>() }).to_string()
            } else if line.contains("/files") {
                serde_json::json!({ "files": [
                    { "task": "apply", "path": "verdicts.json", "size_bytes": 8,
                      "key": "apply/verdicts.json" },
                    { "task": "triage", "instance": "go", "path": "verdicts.json",
                      "size_bytes": 8, "key": "triage-0/verdicts.json" },
                    { "task": "apply", "path": "notes.md", "size_bytes": 3,
                      "key": "apply/notes.md" },
                    { "task": "huge", "path": "verdicts.json",
                      "size_bytes": crate::report::FILE_BYTES + 1, "key": "huge/verdicts.json" },
                ] })
                .to_string()
            } else {
                let runs = if launch == "PB-3" {
                    serde_json::json!([])
                } else {
                    serde_json::json!([{ "run_id": format!("run-{launch}") }])
                };
                serde_json::json!({
                    "launch": { "params": { "profile": "demo" }, "draft_version": 4 },
                    "dispatch": { "state": "dispatched" },
                    "runs": runs,
                })
                .to_string()
            }
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
        let launched = |key: &str, status: crucible_view::Status| crate::report::Launched {
            key: key.into(),
            status,
            runs: 1,
            started: 1_790_000_000,
            cost: (status == crucible_view::Status::Done).then_some(0.5),
            origin: "manual".into(),
        };
        let wanted = |lane: Option<&str>| WantedRuns {
            playbook: "triage".into(),
            plugin: "ui/report.js".into(),
            launches: vec![
                launched("PB-3", crucible_view::Status::Unfinished),
                launched("PB-2", crucible_view::Status::Running),
                launched("PB-1", crucible_view::Status::Done),
            ],
            total: 9,
            asked: crate::report::Asked {
                params: lane
                    .map(|lane| {
                        (
                            "lane".to_string(),
                            crucible_view::Param::Select(lane.into()),
                        )
                    })
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        };
        let built = |wanted: &WantedRuns| {
            let snapshot = snapshot_when(&client, Duration::from_secs(60), |s| {
                s.runs_report.as_ref().is_some_and(|b| &b.wanted == wanted)
            });
            snapshot.runs_report.unwrap()
        };
        let stats = |built: &Built<WantedRuns>| {
            let view = built.report.as_ref().unwrap().view.clone().unwrap();
            let crucible_view::Node::Stats { items } = view.body else {
                panic!("the view is its stats");
            };
            items
                .into_iter()
                .map(|item| (item.label, item.value))
                .collect::<Vec<_>>()
        };
        let pair = |label: &str, value: &str| (label.to_string(), value.to_string());

        client.runs_report(wanted(None), PLUGIN.into());
        let first = built(&wanted(None));
        assert_eq!(
            stats(&first),
            [
                pair("PB-3 unfinished demo v4 -", ""),
                pair(
                    "PB-2 running demo v4 -",
                    r#"apply:["PB-2"],triage[go]:["PB-2"]"#
                ),
                pair(
                    "PB-1 done demo v4 0.5",
                    r#"apply:["PB-1"],triage[go]:["PB-1"]"#
                ),
                pair("total", "9 all"),
            ]
        );
        assert_eq!(first.report.as_ref().unwrap().asked, wanted(None).asked);
        let fetched = |from: usize| {
            let lines = asked.lock().unwrap();
            lines[from..]
                .iter()
                .filter(|line| line.contains("/api/playbook-runs/") || line.contains("/api/runs/"))
                .cloned()
                .collect::<Vec<_>>()
        };
        let once = fetched(0);
        assert_eq!(
            once.iter().filter(|line| line.contains("/files/")).count(),
            4,
            "the two named files of each launch with a run: {once:?}"
        );
        assert!(
            once.iter().all(|line| !line.contains("notes.md")),
            "{once:?}"
        );
        assert!(
            once.iter().all(|line| !line.contains("huge")),
            "a file over the bytes a view reads is not fetched: {once:?}"
        );
        let so_far = asked.lock().unwrap().len();

        client.runs_report(wanted(Some("go")), PLUGIN.into());
        let again = built(&wanted(Some("go")));
        assert_eq!(stats(&again)[3], pair("total", "9 go"));
        let more = fetched(so_far);
        assert!(!more.is_empty());
        assert!(
            more.iter().all(|line| line.contains("PB-2")),
            "only the running launch is read again: {more:?}"
        );

        let event = |turn: u64, wanted: WantedRuns| WantedRuns {
            asked: crate::report::Asked {
                turn,
                state: Some("s".into()),
                event: Some(crucible_view::Event::Tick),
                ..wanted.asked.clone()
            },
            ..wanted
        };
        let so_far = asked.lock().unwrap().len();
        let ticked = event(1, wanted(Some("go")));
        client.runs_report(ticked.clone(), PLUGIN.into());
        assert_eq!(stats(&built(&ticked)), stats(&again));
        assert!(
            fetched(so_far).is_empty(),
            "an event reads no launch again, running or not"
        );
        let fewer = WantedRuns {
            launches: wanted(None).launches[1..].to_vec(),
            ..event(2, wanted(Some("go")))
        };
        client.runs_report(fewer.clone(), PLUGIN.into());
        assert_eq!(stats(&built(&fewer)), stats(&again)[1..]);
        let more = fetched(so_far);
        assert!(!more.is_empty());
        assert!(
            more.iter().all(|line| line.contains("PB-2")),
            "an event for other launches reads the running one: {more:?}"
        );

        let other = WantedRuns {
            plugin: "ui/other.js".into(),
            ..wanted(None)
        };
        let so_far = asked.lock().unwrap().len();
        client.runs_report(
            other.clone(),
            "export const inputs = []; export function view() {}".into(),
        );
        assert_eq!(built(&other).report, None, "the plugin draws no runs view");
        assert!(fetched(so_far).is_empty(), "nothing is read for it");

        let broken = WantedRuns {
            plugin: "ui/broken.js".into(),
            ..wanted(None)
        };
        client.runs_report(broken.clone(), "export const inputs = [;".into());
        let error = built(&broken)
            .report
            .as_ref()
            .unwrap()
            .view
            .clone()
            .unwrap_err();
        assert!(error.starts_with("the script does not compile"), "{error}");

        let cut = WantedRuns {
            launches: vec![
                launched("PB-2", crucible_view::Status::Running),
                launched("PB-0", crucible_view::Status::Done),
                launched("PB-1", crucible_view::Status::Done),
            ],
            ..wanted(None)
        };
        let so_far = asked.lock().unwrap().len();
        client.runs_report(cut.clone(), PLUGIN.into());
        assert_eq!(
            stats(&built(&cut)),
            [
                pair(
                    "PB-2 running demo v4 -",
                    r#"apply:["PB-2"],triage[go]:["PB-2"]"#
                ),
                pair("total", "9 all"),
            ],
            "a launch listed as holding more than a view reads ends the launches read"
        );
        let more = fetched(so_far);
        assert!(
            more.iter()
                .all(|line| !(line.contains("PB-0") && line.contains("/files/"))),
            "none of its files is fetched: {more:?}"
        );
    }

    #[test]
    fn a_launch_that_cannot_be_read_is_left_out_of_the_runs_view_with_why() {
        use base64::Engine;
        let pack = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/report");
        let wasm = std::fs::read(pack.join("ui/report.wasm"))
            .expect("build it with `just example-report-view`");
        let plugin = base64::engine::general_purpose::STANDARD.encode(&wasm);
        let verdicts = std::fs::read_to_string(pack.join("inbox/go.json")).unwrap();
        let size = verdicts.len();
        let down = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let asked = Arc::new(Mutex::new(Vec::<String>::new()));
        let (failing, seen) = (Arc::clone(&down), Arc::clone(&asked));
        let (url, _) = crate::testserver::serve_replies(move |request| {
            let line = &request.line;
            seen.lock().unwrap().push(line.clone());
            let lost = line.contains("PB-8")
                || (line.contains("PB-9") && failing.load(std::sync::atomic::Ordering::SeqCst));
            if lost {
                return crate::testserver::Reply {
                    status: 500,
                    headers: Vec::new(),
                    body: r#"{"error":"the launch is gone"}"#.into(),
                };
            }
            let launch = ["PB-1", "PB-2", "PB-9"]
                .into_iter()
                .find(|key| line.contains(key))
                .unwrap_or("none");
            crate::testserver::Reply::ok(if line.contains("/files/") {
                verdicts.clone()
            } else if line.contains("/files") {
                serde_json::json!({ "files": [
                    { "task": "triage-go", "path": "verdicts.json", "size_bytes": size,
                      "key": "triage-go/verdicts.json" },
                ] })
                .to_string()
            } else {
                serde_json::json!({
                    "launch": { "params": {}, "draft_version": 1 },
                    "dispatch": { "state": "dispatched" },
                    "runs": [{ "run_id": format!("run-{launch}") }],
                })
                .to_string()
            })
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
        let launched = |key: &str| crate::report::Launched {
            key: key.into(),
            status: crucible_view::Status::Done,
            runs: 1,
            started: 1_790_000_000,
            cost: Some(0.5),
            origin: "manual".into(),
        };
        let wanted = |turn: u64, keys: &[&str]| WantedRuns {
            playbook: "triage".into(),
            plugin: "ui/report.wasm.b64".into(),
            launches: keys.iter().map(|key| launched(key)).collect(),
            total: keys.len(),
            asked: crate::report::Asked {
                turn,
                event: (turn > 0).then_some(crucible_view::Event::Tick),
                ..Default::default()
            },
        };
        let built = |wanted: &WantedRuns| {
            let snapshot = snapshot_when(&client, Duration::from_secs(60), |s| {
                s.runs_report.as_ref().is_some_and(|b| &b.wanted == wanted)
            });
            snapshot.runs_report.unwrap().report.clone().unwrap()
        };
        let read = |report: &crate::report::Report| {
            let view = report.view.as_ref().unwrap();
            let crucible_view::Node::Stack { children } = &view.body else {
                panic!("the runs view is a stack");
            };
            let crucible_view::Node::Stats { items } = &children[0] else {
                panic!("the runs view starts with its stats");
            };
            items[0].value.clone()
        };
        let details = |key: &str| {
            let lines = asked.lock().unwrap();
            lines
                .iter()
                .filter(|line| line.contains(&format!("/api/playbook-runs/{key} ")))
                .count()
        };

        let holed = wanted(0, &["PB-2", "PB-9", "PB-1"]);
        client.runs_report(holed.clone(), plugin.clone());
        let first = built(&holed);
        assert_eq!(read(&first), "2 of 3");
        let [unread] = first.unread.as_slice() else {
            panic!("one launch is unread: {:?}", first.unread);
        };
        assert_eq!(unread.launch, "PB-9");
        assert!(unread.why.contains("the launch"), "{}", unread.why);

        let ticked = wanted(1, &["PB-2", "PB-9", "PB-1"]);
        client.runs_report(ticked.clone(), plugin.clone());
        let again = built(&ticked);
        assert_eq!(read(&again), "2 of 3");
        assert_eq!(again.unread, first.unread, "an event keeps what was unread");
        assert_eq!(details("PB-9"), 1, "and reads no launch again");

        down.store(false, std::sync::atomic::Ordering::SeqCst);
        let grown = wanted(0, &["PB-2", "PB-9", "PB-1", "PB-8"]);
        client.runs_report(grown.clone(), plugin.clone());
        let healed = built(&grown);
        assert_eq!(read(&healed), "3 of 4");
        assert_eq!(
            healed
                .unread
                .iter()
                .map(|unread| unread.launch.as_str())
                .collect::<Vec<_>>(),
            ["PB-8"],
            "a launch that was unread is read again"
        );
        assert_eq!(details("PB-9"), 2);
        assert_eq!(details("PB-2"), 1, "a stopped launch that was read is not");

        let lost = wanted(0, &["PB-8"]);
        client.runs_report(lost.clone(), plugin.clone());
        let none = built(&lost);
        assert!(none.unread.is_empty());
        assert!(
            none.view
                .as_ref()
                .is_err_and(|why| why.contains("the launch")),
            "with no launch read the view is why: {:?}",
            none.view
        );
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
