//! A playbook tracker for Crucible. Two backends, one at a time: the controller's registered
//! playbooks (through `crux`, with the same config a terminal uses) and the local `examples/` packs
//! (through each pack's `state/session.jsonl`).

mod activity;
mod admin;
mod agent;
mod apikeys;
mod camera;
mod captured;
mod commands;
mod cron;
mod diff;
mod draft;
mod editor;
mod filter;
mod finder;
mod fleet;
mod form;
mod gfx;
mod handoff;
mod history;
#[cfg(not(target_arch = "wasm32"))]
mod httpcache;
#[cfg(target_arch = "wasm32")]
#[path = "httpcache_web.rs"]
mod httpcache;
mod ignition;
mod inspect;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[cfg(not(target_arch = "wasm32"))]
mod instance;
mod issues;
mod keymap;
mod keys;
mod link;
mod live;
mod logline;
mod lsp;
mod model;
mod net;
mod notify;
mod opened;
mod overrides;
mod pack;
mod palette;
mod plugin;
mod preview;
mod principals;
mod profile;
mod provider;
mod remote;
mod report;
mod revisions;
mod runs;
mod runs_on;
mod search;
mod session;
mod settings;
#[cfg(not(target_arch = "wasm32"))]
mod signin;
mod studio;
mod syntax;
mod teams;
#[cfg(test)]
mod testserver;
mod timeline;
mod transcript;
mod tray;
mod tune;
mod ui;
mod unfurl;
mod update;
mod version;
mod webhook;
mod workspace;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::OnceLock;

use rune::{AppSpec, TextEdit, Tx, Waker};

use crate::editor::Editor;
use crate::filter::{LandedFilter, LaunchFilter, OriginFilter, OwnerFilter};
use crate::link::Link;
use crate::live::{LiveDetail, LivePlaybook, LiveView, Target};
use crate::logline::{LogLine, LogView};
use crate::model::{Client, PackView, Snapshot, Verdict};
use crate::pack::{Disk, PackFile, PackFiles};
use crate::session::TaskView;
use crate::settings::{LogFont, Settings, Theme};
use crate::workspace::{Divider, Geometry, PaneDrag, PaneKind, Workspace};

const MAX_TIME: &str = "10m";
/// The id the desktop matches the window to its `.desktop` entry and icon by.
const APP_ID: &str = "dev.crucible.playbooks";

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// What the run views draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphSource {
    Run,
    Preview,
}

impl GraphSource {
    /// Whether the run views draw the compiled pack's tasks in place of the run's: when
    /// `picked` says so, and with nothing picked, when only the compiled pack has any.
    pub fn previews(picked: Option<GraphSource>, ran: bool, compiles: bool) -> bool {
        match picked {
            Some(GraphSource::Preview) => true,
            Some(GraphSource::Run) => false,
            None => !ran && compiles,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Controller,
    Local,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Backend::Controller => "Controller",
            Backend::Local => "Local",
        }
    }
}

/// What the log pane reads: the agents' session transcript, or the engine's console.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    Transcript,
    Console,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    /// A live playbook. With no `launch` it follows the newest launch the filters keep.
    Live {
        playbook: String,
        launch: Option<String>,
    },
    Local(String),
}

impl Selection {
    pub fn playbook(id: &str) -> Self {
        Selection::Live {
            playbook: id.to_string(),
            launch: None,
        }
    }

    pub fn launch(launch: &LivePlaybook) -> Self {
        Selection::Live {
            playbook: launch.id.clone(),
            launch: Some(launch.key.clone()),
        }
    }
}

/// What the runs page shows of a playbook's launches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunsTab {
    #[default]
    Runs,
    /// The pack's view of the launches.
    Report,
}

/// A plugin file that stands in for a live pack's.
struct StandIn {
    /// The file's name.
    file: String,
    /// The path and text a controller's pack would carry it under.
    path: String,
    text: String,
}

impl StandIn {
    /// The plugin file `named`, when it can be read as one.
    fn read(named: &str) -> Option<StandIn> {
        let file = std::path::Path::new(named)
            .file_name()?
            .to_str()?
            .to_string();
        let (path, text) = report::stand_in(named, std::fs::read(named).ok()?)?;
        Some(StandIn { file, path, text })
    }
}

/// What the Report pane asks a worker to build.
#[derive(Clone, Debug, PartialEq)]
enum ReportAsk {
    /// A local pack's report, with what it is asked with.
    Local(String, report::Asked),
    Live(report::Wanted),
}

impl ReportAsk {
    /// Whether the worker has yet to answer: `local` is what the local worker last built,
    /// `shown` whether the pane has a report to draw.
    fn awaited(&self, local: Option<&model::PackReport>, shown: bool) -> bool {
        match self {
            ReportAsk::Local(pack, _) => local.is_none_or(|built| built.pack != *pack),
            ReportAsk::Live(_) => !shown,
        }
    }
}

/// Where a launch form stands.
#[derive(Clone, Debug, PartialEq)]
pub enum Stage {
    /// Reading the params schema and the caps.
    Reading,
    Ready,
    /// The schema could not be read.
    Failed(String),
    /// The launch is on its way.
    Sending,
}

/// A launch form for one playbook or draft: its fields from the params schema, the answers so
/// far, and the ceilings, prefilled from the playbook's newest launch.
pub struct Launcher {
    pub target: Target,
    pub stage: Stage,
    pub fields: Vec<form::Field>,
    pub answers: Vec<TextEdit>,
    /// Fields that take a typed value in place of their offered choices.
    pub typing: std::collections::BTreeSet<usize>,
    pub cost: TextEdit,
    pub time: TextEdit,
    /// Where it runs: the dispatch target, and the agent's provider and model.
    pub runs_on: runs_on::RunsOnForm,
    /// The admin caps: dollars and a duration.
    pub caps: (f64, String),
    pub problems: form::Problems,
    /// Why the controller refused the launch, or a ceiling over its cap.
    pub refusal: Option<String>,
    /// A draft's schema digest, sent so a save that lands meanwhile refuses the launch.
    digest: Option<String>,
    /// Set when this form reruns a finished run instead of starting afresh.
    pub rerun: Option<studio::Rerun>,
    /// Set when this form saves a schedule rather than launching now.
    pub schedule: Option<admin::ScheduleForm>,
    /// Only replies newer than this one answer this form.
    after: u64,
    /// The newest schema reply this form took, so a repeated snapshot does not refill it.
    filled: u64,
}

/// What the detail area shows: the selected run, or one of the controller's management pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Page {
    Runs,
    Triggers,
    Secrets,
    Decisions,
    Approvals,
    Teams,
    Providers,
    Blueprints,
    Activity,
    Issues,
    Admin,
    Keys,
    Settings,
}

impl Page {
    pub fn id(self) -> &'static str {
        match self {
            Page::Runs => "runs",
            Page::Triggers => "triggers",
            Page::Secrets => "secrets",
            Page::Decisions => "decisions",
            Page::Approvals => "approvals",
            Page::Teams => "teams",
            Page::Providers => "providers",
            Page::Blueprints => "blueprints",
            Page::Activity => "activity",
            Page::Issues => "issues",
            Page::Admin => "admin",
            Page::Keys => "keys",
            Page::Settings => "settings",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Page::Runs => "Runs",
            Page::Triggers => "Triggers",
            Page::Secrets => "Secrets",
            Page::Decisions => "Decisions",
            Page::Approvals => "Approvals",
            Page::Teams => "Teams",
            Page::Providers => "Providers",
            Page::Blueprints => "Blueprints",
            Page::Activity => "Activity",
            Page::Issues => "Issues",
            Page::Admin => "Admin",
            Page::Keys => "API keys",
            Page::Settings => "Settings",
        }
    }
}

/// Pages shown as tabs of one another, behind one entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Group {
    Inbox,
    Activity,
    Settings,
}

impl Group {
    pub fn id(self) -> &'static str {
        match self {
            Group::Inbox => "inbox",
            Group::Activity => "activity",
            Group::Settings => "settings",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Group::Inbox => "Inbox",
            Group::Activity => "Activity",
            Group::Settings => "Settings",
        }
    }

    /// Its pages, in tab order.
    pub fn pages(self) -> &'static [Page] {
        match self {
            Group::Inbox => &[Page::Decisions, Page::Approvals],
            Group::Activity => &[Page::Activity, Page::Issues],
            Group::Settings => &[
                Page::Settings,
                Page::Secrets,
                Page::Teams,
                Page::Providers,
                Page::Keys,
                Page::Admin,
            ],
        }
    }
}

impl Page {
    /// The group whose tabs this page is one of.
    pub fn group(self) -> Option<Group> {
        [Group::Inbox, Group::Activity, Group::Settings]
            .into_iter()
            .find(|group| group.pages().contains(&self))
    }

    /// What the page's tab reads.
    pub fn tab(self) -> &'static str {
        match self {
            Page::Settings => "General",
            Page::Activity => "Events",
            page => page.title(),
        }
    }
}

/// Which pack the Pack tab shows when there is a working copy beside the run's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackSource {
    Run,
    Working,
}

/// `$VISUAL` or `$EDITOR` as the user's shell sets them, read once when first needed.
enum LoginEditor {
    Unasked,
    Asking(std::sync::mpsc::Receiver<Option<String>>),
    Known(Option<String>),
}

/// Something to open in the user's editor.
enum Open {
    Path {
        path: PathBuf,
        line: usize,
    },
    /// A file of the pack on screen.
    PackFile {
        file: String,
        line: usize,
    },
    /// The captured file the Files tab last fetched.
    Captured,
}

/// What the window shows of the selected run; picking another run starts it afresh.
#[derive(Default)]
pub struct RunView {
    /// The task card shown open, and the task the Data pane shows.
    pub expanded: Option<String>,
    pub inspect: inspect::Inspect,
    /// Fan-outs the graph draws without their instances.
    pub folded: HashSet<String>,
    /// How far the graph is zoomed and moved.
    pub camera: camera::Camera,
    /// Whether the graph is drawn compact; `None` takes the setting.
    pub compact_graph: Option<bool>,
    /// The launch picked to compare against; `None` takes the one before.
    pub compare_to: Option<String>,
    /// The saves the History pane compares, once picked by hand; `None` opens on the save the
    /// run ran.
    pub history_range: Option<revisions::Range>,
    /// Only lines from this task or fan-out instance.
    pub log_task: Option<String>,
    /// Log blocks opened or folded by hand, by task or instance name.
    pub log_folds: HashMap<String, bool>,
    /// The pack file the Pack tab shows; the manifest when none is picked.
    pub pack_file: Option<String>,
    /// The pack the user picked to show; `None` shows a draft run's working copy.
    pub pack_source: Option<PackSource>,
    /// What the last pull or save said.
    pub draft_note: Option<Result<String, String>>,
    /// Where the last brief for an agent was written, or why it was not.
    pub handoff_note: Option<Result<PathBuf, String>>,
}

pub struct App {
    pub repo: String,
    net: net::Net,
    /// The profile in use and everything read from it.
    pub remote: remote::Remote,
    pub run: RunView,
    local: Client,
    local_seen: u64,
    lsp: lsp::Lsp,
    lsp_seen: u64,
    /// What `crucible-lsp` last said about the packs on screen.
    pub checks: lsp::View,
    /// The file last handed to the language server, and a hash of its text.
    lsp_sent: Option<(PathBuf, u64)>,
    /// Why the last hand-off to the editor failed.
    pub editor_error: Option<String>,
    pub backend: Backend,
    pub packs: Vec<PackView>,
    pub playbook_search: TextEdit,
    pub owner_filter: OwnerFilter,
    pub origin_filter: OriginFilter,
    pub landed_filter: LandedFilter,
    /// Whether the sidebar shows the owner, origin and landed filters.
    pub filters_open: bool,
    pub selection: Option<Selection>,
    /// The log pane's search field; its text filters the log lines shown.
    pub log_search: TextEdit,
    pub log_view: LogView,
    pub log_source: LogSource,
    /// Show library `tracing` lines below WARN.
    pub log_chatter: bool,
    /// The log's layout when picked; `None` takes the one the log suits.
    pub log_mode: Option<unfurl::LogMode>,
    /// Which keys run which commands, and the bindings in the config file it could not use.
    pub keymap: keymap::Keymap,
    pub keys_refused: Vec<keymap::Refused>,
    /// The command palette, while open.
    pub palette: Option<palette::Palette>,
    /// The pane keyboard navigation starts from.
    pub pane_focus: Option<PaneKind>,
    /// Posts notifications once the app's loop is up.
    notifier: Option<notify::Notifier>,
    /// Links of clicked notifications, to open.
    clicks: Option<std::sync::mpsc::Receiver<String>>,
    /// Each command's key recorder in Settings, once touched.
    pub recorders: HashMap<keymap::Command, rune_kit::ShortcutRecorderState>,
    /// Where the provider picker's state goes once the launch sheet has closed.
    pub provider_scratch: rune_kit::ComboboxState,
    /// The captured file the Files tab shows, by key; the first listed when none is picked.
    pub run_file: Option<String>,
    pub files_md_source: bool,
    /// The Report pane's report, controls, picked tabs and its table's sort, kept for the
    /// selection.
    pub report_shown: report::shown::Scoped<Selection>,
    /// The report last asked of a worker for the Report pane.
    report_sent: Option<ReportAsk>,
    /// The report the local worker last built for the Report pane.
    local_report: Option<model::PackReport>,
    /// The runs page's view of the launches, its controls, tabs and table, kept for the
    /// playbook.
    pub runs_shown: report::shown::Scoped<String>,
    pub runs_tab: RunsTab,
    /// The plugin file that stands in for a live pack's: what `CRUCIBLE_VIEW_PLUGIN` named at
    /// start.
    view_plugin: Option<StandIn>,
    /// The workspace the user arranged; `None` lays each run out by its graph's depth.
    pub layout: Option<Workspace>,
    /// Panes picked from a tab bar while the layout follows the run, oldest first.
    pub shown: Vec<PaneKind>,
    pub pane_drag: Option<PaneDrag>,
    /// The divider being dragged and its neighbours' shares when the drag began.
    pub resizing: Option<(Divider, (f32, f32))>,
    pub rail_drag: Option<workspace::RailDrag>,
    pub geometry: Geometry,
    /// The flare on the run a launch just started.
    pub ignition: ignition::Ignition,
    /// Clicks on the molten logo, which slosh it.
    pub logo_clicks: u32,
    /// The logo's top-left on screen, in points; a window move sloshes it.
    pub logo_screen: [f32; 2],
    /// Every profile started so far, with what it keeps running.
    targets: HashMap<String, (profile::Target, Option<profile::Keeper>)>,
    /// The profile being added or edited in Settings.
    pub profile_form: Option<profile::Form>,
    /// Why the last move of keys between the key files and the keychain failed.
    pub keys_error: Option<String>,
    /// The account row's menu of profiles and local packs, while open.
    pub source_menu: bool,
    pub panes_menu: bool,
    /// What the Dock and the menu bar were last told to show.
    tray: Option<tray::Shown>,
    /// The profiles not in use, as the background worker last read them, and which it reads.
    pub fleet: Vec<fleet::Status>,
    fleet_client: fleet::Client,
    fleet_seen: u64,
    fleet_sent: Vec<(String, profile::Profile)>,
    /// The approvals each profile was last seen waiting on, to say when they grow.
    approvals_known: HashMap<String, usize>,
    /// Ids of the menu bar lines picked, to act on.
    picks: Option<std::sync::mpsc::Receiver<String>>,
    /// Folders the system opened with the app, to launch, and the loop's waker once it runs.
    opened: Option<std::sync::mpsc::Receiver<Vec<PathBuf>>>,
    open_wake: std::sync::Arc<std::sync::Mutex<Option<Waker>>>,
    /// The places the window showed, to go back and forward through.
    history: history::History,
    waker: Option<Waker>,
    release: update::Check,
    /// Where a text field's edits land once its form has closed.
    pub scratch: TextEdit,
    pub settings: Settings,
    /// The Pack tab shows a Markdown file's source rather than its rendering.
    pub pack_md_source: bool,
    login_editor: LoginEditor,
    /// What waits to open until the login shell has named the editor.
    opening: Option<Open>,
    /// The editor command field in Settings.
    pub editor_command: TextEdit,
    /// The drop ceilings as typed in Settings, and why the last pair did not take.
    pub drop_cost: TextEdit,
    pub drop_time: TextEdit,
    pub drop_error: Option<String>,
    /// The drafts folder as typed in Settings, and why it did not take.
    pub drafts_dir: TextEdit,
    pub drafts_error: Option<String>,
    /// Why the last pack folder dropped on the app did not launch.
    pub drop_failed: Option<String>,
    /// Effect and layout values tuned live; their defaults unless built with `knobs`.
    pub knobs: rune_knobs::Knobs,
    pub page: Page,
    /// The matched id of what the shown run was opened from, which its view flies out of.
    pub flight: Option<String>,
    /// Why the local packs could not be read or launched.
    pub local_error: Option<String>,
    /// Why the controller refused the team last acted as, until the next pick.
    pub act_as_refused: Option<String>,
}

impl App {
    pub fn selected_pack(&self) -> Option<&PackView> {
        match &self.selection {
            Some(Selection::Local(name)) if self.backend == Backend::Local => {
                self.visible_packs().find(|pack| &pack.name == name)
            }
            _ => None,
        }
    }

    /// The task a report names, or the fan-out and instance key for `task[key]`.
    fn report_task<'a>(&self, named: &'a str) -> Option<(&'a str, Option<&'a str>)> {
        let tasks = self.run_tasks();
        report::task_named(|name| tasks.iter().any(|task| task.name == name), named)
    }

    /// The key of the captured file a report names as `<task>/<path>`.
    fn report_file(&self, named: &str) -> Option<String> {
        let files = self.selected_detail()?.files.as_ref()?.as_ref().ok()?;
        report::file_named(files, named).map(|file| file.key.clone())
    }

    /// Whether an address a report holds leads to the web, to something this run has, or to a
    /// launch the controller listed.
    pub fn report_leads(&self, href: &str) -> bool {
        match crucible_view::Link::parse(href) {
            Some(crucible_view::Link::Web(_)) => true,
            Some(crucible_view::Link::Task(task)) => self.report_task(task).is_some(),
            Some(crucible_view::Link::File(file)) => self.report_file(file).is_some(),
            Some(crucible_view::Link::Launch { launch, .. }) => {
                self.listed_launch(launch).is_some()
            }
            None => false,
        }
    }

    /// Opens a web address a report holds, shows the task or captured file it names, or opens
    /// the launch it names.
    pub fn follow_report_link(&mut self, href: &str, tx: &mut Tx) {
        match crucible_view::Link::parse(href) {
            Some(crucible_view::Link::Web(url)) => {
                let _ = tx.open_url(url);
            }
            Some(crucible_view::Link::Launch { launch, task }) => self.open_launch(launch, task),
            Some(crucible_view::Link::Task(named)) => match self.report_task(named) {
                Some((task, Some(key))) => self.pick_instance(task, key),
                Some((task, None)) => {
                    self.pick_task(task, None);
                    self.show_pane(PaneKind::Tasks);
                }
                None => {}
            },
            Some(crucible_view::Link::File(named)) => {
                if let Some(key) = self.report_file(named) {
                    self.pick_file(&key);
                    self.show_pane(PaneKind::Files);
                }
            }
            None => {}
        }
    }

    /// Whether an address the runs view holds leads to the web or to a launch the controller
    /// listed. One into a run leads nowhere: the view is of no one run.
    pub fn runs_leads(&self, href: &str) -> bool {
        crucible_view::Link::parse(href).is_some_and(|link| !link.in_run())
            && self.report_leads(href)
    }

    /// Opens a web address or a launch the runs view holds.
    pub fn follow_runs_link(&mut self, href: &str, tx: &mut Tx) {
        if self.runs_leads(href) {
            self.follow_report_link(href, tx);
        }
    }

    /// The launch `key` names, when the controller's list has it.
    fn listed_launch(&self, key: &str) -> Option<&LivePlaybook> {
        (self.backend == Backend::Controller)
            .then(|| self.remote.live.launch(key))
            .flatten()
    }

    /// Opens the launch `key` names, with `task` open when one is named.
    fn open_launch(&mut self, key: &str, task: Option<&str>) {
        let Some(launch) = self.listed_launch(key) else {
            return;
        };
        let link = Link::Launch {
            playbook: launch.id.clone(),
            key: launch.key.clone(),
            task: task.map(ToString::to_string),
        };
        self.follow(&link);
    }

    /// The local packs matching the sidebar's search field.
    pub fn visible_packs(&self) -> impl Iterator<Item = &PackView> {
        let query = self.query();
        self.packs
            .iter()
            .filter(move |pack| query.is_empty() || pack.matches(&query))
    }

    fn query(&self) -> String {
        self.playbook_search.text.trim().to_ascii_lowercase()
    }

    pub fn launch_filter(&self) -> LaunchFilter {
        LaunchFilter {
            query: self.query(),
            owner: self.owner_filter,
            origin: self.origin_filter,
            landed: self.landed_filter,
        }
    }

    /// The sidebar's live rows: each playbook's newest launch the filters keep.
    pub fn visible_playbooks(&self) -> Vec<&LivePlaybook> {
        self.remote.live.playbooks(&self.launch_filter())
    }

    /// Whether the sidebar is showing a subset of the active backend's list.
    pub fn filtering(&self) -> bool {
        match self.backend {
            Backend::Controller => self.launch_filter().narrows(),
            Backend::Local => !self.query().is_empty(),
        }
    }

    pub fn selected_playbook(&self) -> Option<&str> {
        match &self.selection {
            Some(Selection::Live { playbook, .. }) if self.backend == Backend::Controller => {
                Some(playbook)
            }
            _ => None,
        }
    }

    /// The launch the detail pane shows.
    pub fn selected_live(&self) -> Option<&LivePlaybook> {
        if self.backend != Backend::Controller {
            return None;
        }
        let Some(Selection::Live { playbook, launch }) = &self.selection else {
            return None;
        };
        self.remote
            .live
            .resolve(playbook, launch.as_deref(), &self.launch_filter())
    }

    /// The live graph, only when it is the one the selection shows.
    /// The selected run's tasks, from whichever backend it came from.
    pub fn run_tasks(&self) -> &[TaskView] {
        match &self.selection {
            Some(Selection::Local(_)) => self.selected_pack().map_or(&[], |pack| &pack.tasks),
            Some(Selection::Live { .. }) => self
                .selected_detail()
                .map_or(&[], |detail| &detail.fold.tasks),
            None => &[],
        }
    }

    /// The tasks the run views draw: the selected run's, or its pack's as it compiles now.
    pub fn shown_tasks(&self) -> &[TaskView] {
        let run = self.run_tasks();
        let preview = match &self.selection {
            Some(Selection::Local(_)) => self.selected_pack().map(|pack| &pack.preview),
            Some(Selection::Live { .. }) => self.remote.working_preview.as_ref(),
            None => None,
        };
        match preview {
            Some(preview)
                if GraphSource::previews(
                    self.remote.graph_source,
                    !run.is_empty(),
                    !preview.fold.tasks.is_empty(),
                ) =>
            {
                &preview.fold.tasks
            }
            _ => run,
        }
    }

    /// The workspace to draw: the user's own, or one fitted to the graph on screen.
    pub fn workspace(&self) -> Workspace {
        self.layout.clone().unwrap_or_else(|| {
            let tasks = self.shown_tasks();
            let mut fitted = Workspace::for_depth(workspace::depth(
                tasks
                    .iter()
                    .map(|task| (task.name.as_str(), task.depends_on.as_slice())),
            ));
            for kind in &self.shown {
                fitted.activate(*kind);
            }
            fitted
        })
    }

    /// Brings `kind` to the front of its group without making the layout the user's own.
    pub fn show_pane(&mut self, kind: PaneKind) {
        match &mut self.layout {
            Some(layout) => {
                layout.activate(kind);
                self.save_layout();
            }
            None => {
                self.shown.retain(|shown| *shown != kind);
                self.shown.push(kind);
            }
        }
    }

    /// Changes the workspace, which from then on is the user's own.
    pub fn arrange(&mut self, f: impl FnOnce(&mut Workspace)) {
        let mut layout = self.workspace();
        f(&mut layout);
        self.layout = Some(layout);
    }

    pub fn save_layout(&mut self) {
        self.settings.set_layout(self.layout.clone());
    }

    /// Back to the layout fitted to each run's graph.
    pub fn reset_layout(&mut self) {
        self.layout = None;
        self.save_layout();
    }

    pub fn selected_detail(&self) -> Option<&LiveDetail> {
        let launch = self.selected_live()?;
        self.remote
            .live_detail
            .as_deref()
            .filter(|detail| detail.key == launch.key)
    }

    /// The selected run's transcript and console.
    pub(crate) fn log_lines(&self) -> (&[LogLine], &[LogLine]) {
        match &self.selection {
            Some(Selection::Local(_)) => self
                .selected_pack()
                .map(|pack| (pack.transcript.as_slice(), pack.log.as_slice())),
            Some(Selection::Live { .. }) => self
                .selected_detail()
                .map(|detail| (detail.fold.transcript.as_slice(), detail.log.as_slice())),
            None => None,
        }
        .unwrap_or_default()
    }

    /// The pack the Pack tab shows, local or from the controller.
    pub fn shown_pack(&self) -> Option<&PackFiles> {
        match &self.selection {
            Some(Selection::Local(_)) => self.selected_pack().map(|pack| &pack.pack),
            Some(Selection::Live { .. }) if self.showing_working() => {
                self.remote.working_pack.as_ref()
            }
            Some(Selection::Live { .. }) => self.selected_detail()?.pack.as_ref().ok(),
            None => None,
        }
    }

    /// The file the Pack tab shows: the one picked, else the first.
    pub fn shown_file(&self) -> Option<(&PackFiles, &PackFile)> {
        let pack = self.shown_pack()?;
        let file = self
            .run
            .pack_file
            .as_deref()
            .and_then(|path| pack.file(path))
            .or_else(|| pack.files.first())?;
        Some((pack, file))
    }

    /// Hand the language server the shown Starlark file, or its pack's workflow when the file
    /// is not Starlark, so the pack's diagnostics stay current as it changes on disk.
    /// Have crucible-lsp check the shown pack file, while checks are on and the Pack tab is in
    /// front.
    fn sync_checks(&mut self) {
        if !self.settings.checks || !self.workspace().showing(PaneKind::Pack) {
            return;
        }
        let Some((pack, shown)) = self.shown_file() else {
            return;
        };
        let file = match syntax::language(&shown.path) {
            Some("starlark") => Some(shown),
            _ => pack.file("workflow.star"),
        };
        let Some((path, text)) =
            file.and_then(|file| Some((pack.path_of(&file.path)?, file.text.as_deref()?)))
        else {
            return;
        };
        let mut hasher = std::hash::DefaultHasher::new();
        std::hash::Hash::hash(text, &mut hasher);
        let sent = (path, std::hash::Hasher::finish(&hasher));
        if self.lsp_sent.as_ref() != Some(&sent) {
            self.lsp.show(&sent.0, text);
            self.lsp_sent = Some(sent);
        }
    }

    /// The command the user set, or the editor app to open files in. `None` while the login
    /// shell is still being asked which editor it names.
    fn editor(&mut self) -> Option<Result<Result<Editor, String>, String>> {
        let command = self.settings.editor_command.trim();
        if self.settings.editor_custom && !command.is_empty() {
            return Some(Ok(Err(command.to_string())));
        }
        if self.settings.editor_custom {
            return Some(Err(format!(
                "Set the editor command in Settings, like `{}`",
                editor::EXAMPLE
            )));
        }
        match self.settings.editor {
            Some(app) => Some(Ok(Ok(app))),
            None => {
                let named = self.login_editor()?;
                Some(Editor::detect(named.as_deref()))
            }
        }
    }

    /// The editor the user's shell names, asked of a login shell on a thread of its own the
    /// first time. `None` until that answers.
    fn login_editor(&mut self) -> Option<Option<String>> {
        match &self.login_editor {
            LoginEditor::Known(named) => return Some(named.clone()),
            LoginEditor::Asking(_) => return None,
            LoginEditor::Unasked => {}
        }
        if let Some(own) = editor::env_editor() {
            self.login_editor = LoginEditor::Known(Some(own.clone()));
            return Some(Some(own));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let waker = self.waker.clone();
        let asked = std::thread::Builder::new()
            .name("crucible-playbooks-login-editor".into())
            .spawn(move || {
                let _ = tx.send(editor::shell_editor());
                if let Some(waker) = waker {
                    waker();
                }
            });
        match asked {
            Ok(_) => {
                self.login_editor = LoginEditor::Asking(rx);
                None
            }
            Err(_) => {
                self.login_editor = LoginEditor::Known(None);
                Some(None)
            }
        }
    }

    /// Take the login shell's answer, and open what was waiting on it.
    fn finish_login_editor(&mut self) -> bool {
        let LoginEditor::Asking(rx) = &self.login_editor else {
            return false;
        };
        let named = match rx.try_recv() {
            Ok(named) => named,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
        };
        self.login_editor = LoginEditor::Known(named);
        if let Some(what) = self.opening.take() {
            self.open(what);
        }
        true
    }

    /// Open `what` in the user's editor, once it is known which that is.
    fn open(&mut self, what: Open) {
        let Some(editor) = self.editor() else {
            self.opening = Some(what);
            return;
        };
        self.editor_error = match editor {
            Ok(editor) => self.open_with(editor, &what).err(),
            Err(why) => Some(why),
        };
    }

    fn open_with(&self, editor: Result<Editor, String>, what: &Open) -> Result<(), String> {
        let root = std::path::Path::new("/");
        match what {
            Open::Path { path, line } => match editor {
                Ok(app) => editor::open_file(app, path, *line),
                Err(command) => {
                    editor::run_command(&command, path, *line, path.parent().unwrap_or(root))
                }
            },
            Open::PackFile { file, line } => {
                let Some(pack) = self.shown_pack() else {
                    return Ok(());
                };
                match (&pack.disk, pack.file(file), editor) {
                    (Some(Disk::Checkout(dir)), _, Ok(app)) => {
                        editor::open_file(app, &dir.join(file), *line)
                    }
                    (Some(Disk::Checkout(dir)), _, Err(command)) => {
                        editor::run_command(&command, &dir.join(file), *line, dir)
                    }
                    (
                        _,
                        Some(PackFile {
                            text: Some(text), ..
                        }),
                        Ok(app),
                    ) => editor::open_text(app, text),
                    (
                        _,
                        Some(PackFile {
                            text: Some(text), ..
                        }),
                        Err(command),
                    ) => editor::scratch(file, text).and_then(|path| {
                        editor::run_command(&command, &path, *line, path.parent().unwrap_or(root))
                    }),
                    _ => Err(format!("{file} has no text to open")),
                }
            }
            Open::Captured => {
                let path = self
                    .remote
                    .opened_file
                    .as_ref()
                    .and_then(|file| file.disk.clone())
                    .ok_or("The file has no copy on disk to open")?;
                match editor {
                    Ok(app) => editor::open_file(app, &path, 1),
                    Err(command) => {
                        editor::run_command(&command, &path, 1, path.parent().unwrap_or(root))
                    }
                }
            }
        }
    }

    /// Open line `line` (1-based) of the file at `path` in the user's editor.
    pub fn open_path_in_editor(&mut self, path: &std::path::Path, line: usize) {
        self.open(Open::Path {
            path: path.to_path_buf(),
            line,
        });
    }

    /// Open line `line` (1-based) of pack file `file` in the user's editor: the file itself from
    /// a checkout, or its text as a scratch buffer from a controller's copy.
    pub fn open_in_editor(&mut self, file: &str, line: usize) {
        self.open(Open::PackFile {
            file: file.to_string(),
            line,
        });
    }

    fn apply_local(&mut self, snapshot: Snapshot) {
        self.packs = snapshot.packs;
        self.local_error = snapshot.error;
        self.local_report = snapshot.report;
        self.reconcile();
    }

    /// Announces launches by the signed-in user that finished since the last snapshot.
    fn announce(&mut self, finished: &[live::Finished]) {
        let me = match &*self.remote.live {
            LiveView::Ready { identity, .. } => identity.user.clone(),
            _ => String::new(),
        };
        let seen = self.remote.notified;
        for done in finished.iter().filter(|done| done.seq > seen) {
            self.remote.notified = done.seq;
            let looking = notify::app_focused()
                && self
                    .selected_live()
                    .is_some_and(|live| live.key == done.key);
            let pinned = self.remote.pinned.as_deref() == Some(done.key.as_str());
            if self.settings.notify
                && ((!me.is_empty() && done.by == me) || pinned)
                && !looking
                && let Some(notifier) = &self.notifier
            {
                notifier.post(&notify::note(done));
            }
        }
    }

    /// Opens a launch form for `target` and asks for its schema.
    pub fn open_launcher(&mut self, target: Target) {
        self.remote.launcher = Some(Launcher {
            target: target.clone(),
            stage: Stage::Reading,
            fields: Vec::new(),
            answers: Vec::new(),
            typing: Default::default(),
            cost: TextEdit::new(""),
            time: TextEdit::new(""),
            runs_on: runs_on::RunsOnForm::default(),
            caps: (0.0, String::new()),
            problems: Vec::new(),
            refusal: None,
            digest: None,
            rerun: None,
            schedule: None,
            after: self.remote.replies,
            filled: 0,
        });
        self.remote.live_client.prepare(target);
    }

    /// Opens the launch sheet on `schedule`'s playbook, to edit when and how it fires.
    pub fn edit_schedule(&mut self, schedule: &crux::dto::Schedule) {
        let target = if schedule.target_kind == "draft_head" {
            Target::Draft(schedule.playbook.clone())
        } else {
            Target::Playbook(schedule.playbook.clone())
        };
        self.open_launcher(target);
        if let Some(launcher) = &mut self.remote.launcher {
            launcher.schedule = Some(admin::ScheduleForm::editing(schedule));
        }
    }

    /// Sends the form, or marks what has to change first.
    pub fn submit_launch(&mut self) {
        let Some(launcher) = &mut self.remote.launcher else {
            return;
        };
        let answers: Vec<String> = launcher.answers.iter().map(|a| a.text.clone()).collect();
        let params = match &launcher.rerun {
            Some(rerun) => Ok(rerun.params.clone()),
            None => form::params(&launcher.fields, &answers),
        };
        let ceilings = form::ceilings(
            &launcher.cost.text,
            &launcher.time.text,
            launcher.caps.0,
            &launcher.caps.1,
        );
        launcher.problems = params.as_ref().err().cloned().unwrap_or_default();
        launcher.refusal = ceilings.as_ref().err().cloned();
        let (Ok(params), Ok((max_cost, max_time))) = (params, ceilings) else {
            return;
        };
        if let Some(why) = launcher.rerun.as_ref().and_then(studio::Rerun::blocked) {
            launcher.refusal = Some(why);
            return;
        }
        let runs_on = launcher.runs_on.runs_on();
        if let Some(schedule) = &launcher.schedule {
            match schedule.body(
                &launcher.target,
                &params,
                max_cost,
                &max_time,
                launcher.digest.as_deref(),
            ) {
                Ok(mut body) => {
                    runs_on.pin(&mut body);
                    let id = schedule.stored.as_ref().map(|s| s.id.clone());
                    launcher.stage = Stage::Sending;
                    self.act(admin::Action::SaveSchedule { id, body });
                }
                Err(why) => launcher.refusal = Some(why),
            }
            return;
        }
        let mut body = studio::launch_body(
            &launcher.target,
            params,
            max_cost,
            &max_time,
            launcher.digest.as_deref(),
            launcher.rerun.as_ref(),
        );
        runs_on.pin(&mut body);
        launcher.stage = Stage::Sending;
        launcher.after = self.remote.replies;
        self.remote
            .live_client
            .launch(launcher.target.clone(), body);
    }

    /// The newest launch of `playbook` the list shows, to prefill a form from.
    pub(crate) fn newest_launch(&self, playbook: &str) -> Option<&LivePlaybook> {
        self.remote
            .live
            .launches_of(playbook)
            .max_by_key(|launch| launch.landed)
    }

    /// Fills an open form from the schema the worker read, or lands the launch it sent.
    fn answer_launcher(
        &mut self,
        prepared: Option<live::Prepared>,
        launched: Option<live::Launched>,
    ) {
        let Some(after) = self.remote.launcher.as_ref().map(|l| l.after) else {
            return;
        };
        if let Some(launched) = launched.filter(|l| l.seq > after) {
            self.remote.replies = self.remote.replies.max(launched.seq);
            match launched.result {
                Ok(key) => {
                    self.remote.launcher = None;
                    self.backend = Backend::Controller;
                    self.ignition.launched(launched.playbook.clone());
                    self.select(Selection::Live {
                        playbook: launched.playbook,
                        launch: Some(key),
                    });
                }
                Err(error) => {
                    if let Some(launcher) = &mut self.remote.launcher {
                        launcher.stage = Stage::Ready;
                        launcher.refusal = Some(form::Refusal::from(error.as_str()).0);
                    }
                }
            }
            return;
        }
        let filled = self.remote.launcher.as_ref().map_or(0, |l| l.filled);
        let wanted = self.remote.launcher.as_ref().map(|l| l.target.clone());
        let Some(prepared) = prepared
            .filter(|p| p.seq > after && p.seq > filled && Some(&p.target) == wanted.as_ref())
        else {
            return;
        };
        self.remote.replies = self.remote.replies.max(prepared.seq);
        let newest = self.newest_launch(prepared.target.id()).cloned();
        let stored = self
            .remote
            .launcher
            .as_ref()
            .and_then(|l| l.schedule.as_ref())
            .and_then(|s| s.stored.as_deref())
            .cloned();
        let shown = self
            .selected_live()
            .filter(|live| live.id == prepared.target.id())
            .and(self.selected_detail())
            .and_then(|detail| detail.launch.as_ref());
        let previous = match &stored {
            Some(stored) => Some(serde_json::Value::Object(stored.params.clone())),
            None => shown.map(|record| record.launch.params.clone()),
        };
        let before = match &stored {
            Some(stored) => runs_on::RunsOn::from(stored),
            None => shown
                .and_then(|record| runs_on::RunsOn::try_from(record).ok())
                .unwrap_or_default(),
        };
        let Some(launcher) = &mut self.remote.launcher else {
            return;
        };
        launcher.filled = prepared.seq;
        match prepared.result {
            Err(error) => launcher.stage = Stage::Failed(error),
            Ok(live::Form {
                schema,
                digest,
                max_cost,
                max_time,
                targets,
                providers,
            }) => {
                launcher.runs_on = runs_on::RunsOnForm::new(&targets, &providers, before);
                launcher.digest = digest;
                launcher.fields = form::fields(&schema);
                launcher.answers = launcher
                    .fields
                    .iter()
                    .map(|field| {
                        let before = previous
                            .as_ref()
                            .and_then(|params| params.get(&field.name))
                            .map(|value| match value {
                                serde_json::Value::String(text) => text.clone(),
                                other => other.to_string(),
                            });
                        TextEdit::new(before.unwrap_or_else(|| field.default.clone()))
                    })
                    .collect();
                let cost = stored
                    .as_ref()
                    .map(|s| s.max_cost)
                    .or(newest.as_ref().map(|n| n.cap))
                    .unwrap_or(5.0)
                    .min(max_cost);
                let time = stored
                    .as_ref()
                    .map(|s| s.max_time.clone())
                    .or(newest.as_ref().map(|n| n.max_time.clone()))
                    .filter(|t| form::minutes(t).is_some())
                    .unwrap_or_else(|| "30m".to_string());
                launcher.cost = TextEdit::new(format!("{cost:.2}"));
                launcher.time = TextEdit::new(&time);
                launcher.caps = (max_cost, max_time);
                launcher.stage = Stage::Ready;
            }
        }
    }

    fn apply_live(&mut self, snapshot: live::Snapshot) {
        self.remote.live = snapshot.view;
        self.remote.live_detail = snapshot.detail;
        self.remote.live_base = snapshot.base;
        self.remote.registered = snapshot.registered;
        self.answer_rerun(snapshot.reran);
        self.answer_launcher(snapshot.prepared, snapshot.launched);
        self.answer_draft(snapshot.synced);
        self.answer_drop(snapshot.dropped);
        self.answer_file(snapshot.file);
        self.answer_report(snapshot.report);
        if let Some(built) = snapshot.runs_report {
            self.remote.runs_ask.answer(built);
        }
        self.answer_revisions(snapshot.revisions);
        self.announce(&snapshot.finished);
        self.reconcile();
    }

    /// Keep the selection inside the active backend, and drop a pin on a launch the list lost.
    fn reconcile(&mut self) {
        let valid = match (self.backend, &self.selection) {
            (Backend::Local, Some(Selection::Local(name))) => {
                self.visible_packs().any(|pack| &pack.name == name)
            }
            (Backend::Controller, Some(Selection::Live { playbook, .. })) => match &*self
                .remote
                .live
            {
                LiveView::Ready { .. } => self.remote.live.launches_of(playbook).next().is_some(),
                LiveView::Loading | LiveView::Unavailable(_) => true,
            },
            _ => false,
        };
        if !valid {
            self.selection = self.initial_selection();
        } else if let (LiveView::Ready { .. }, Some(Selection::Live { launch, .. })) =
            (&*self.remote.live, &mut self.selection)
            && launch
                .as_deref()
                .is_some_and(|key| self.remote.live.launch(key).is_none())
        {
            *launch = None;
        }
    }

    fn initial_selection(&self) -> Option<Selection> {
        match self.backend {
            Backend::Controller => self
                .visible_playbooks()
                .first()
                .map(|playbook| Selection::playbook(&playbook.id)),
            Backend::Local => self
                .visible_packs()
                .find(|pack| pack.verdict == Verdict::Running)
                .or_else(|| {
                    self.visible_packs().find(|pack| {
                        pack.verdict == Verdict::Valid || pack.verdict == Verdict::Invalid
                    })
                })
                .or_else(|| self.visible_packs().find(|pack| pack.name == "playbook"))
                .or_else(|| self.visible_packs().next())
                .map(|pack| Selection::Local(pack.name.clone())),
        }
    }

    /// Point the live worker at the launch the selection resolves to. The selection, the list
    /// and the filters all move it, so this runs once per frame.
    fn sync_live(&mut self) {
        let key = self.selected_live().map(|launch| launch.key.clone());
        if key != self.remote.live_sent {
            self.remote.live_client.select(key.as_deref());
            self.remote.live_sent = key;
        }
        let base = self
            .workspace()
            .showing(PaneKind::Compare)
            .then(|| self.compare_base())
            .flatten()
            .map(|launch| launch.key.clone());
        if base != self.remote.compare_sent {
            self.remote.live_client.compare(base.as_deref());
            self.remote.compare_sent = base;
        }
        let file = self.wanted_file();
        if file != self.remote.file_sent {
            if let Some((run, key, size)) = &file {
                self.remote
                    .live_client
                    .file(run, key, *size, self.selected_settled());
            }
            self.remote.file_sent = file;
        }
        let history = self.wanted_revisions();
        if history != self.remote.revisions_sent {
            if let Some(wanted) = &history {
                self.remote.live_client.revisions(wanted.clone());
            }
            self.remote.revisions_sent = history;
        }
    }

    fn wanted_revisions(&self) -> Option<revisions::Wanted> {
        if !self.workspace().showing(PaneKind::History) {
            return None;
        }
        let (draft, _) = self.draft_target()?;
        let ran = self
            .selected_detail()
            .and_then(|detail| detail.launch.as_ref())
            .and_then(|record| record.launch.draft_version);
        Some(revisions::Wanted {
            draft,
            range: self.run.history_range,
            ran,
        })
    }

    fn answer_revisions(&mut self, read: Option<std::sync::Arc<revisions::Revisions>>) {
        let Some(read) = read else {
            return;
        };
        let held = self.remote.revisions.as_ref();
        if held.is_some_and(|held| std::sync::Arc::ptr_eq(held, &read))
            || Some(&read.wanted) != self.remote.revisions_sent.as_ref()
        {
            return;
        }
        if let Ok(history) = &read.result {
            self.remote
                .revisions_diff
                .set_files(rune_kit::diff::parse(&revisions::patch(&history.diff)));
        }
        self.remote.revisions = Some(read);
    }

    pub fn pick_range(&mut self, range: revisions::Range) {
        self.run.history_range = Some(range);
    }

    pub fn reread_history(&mut self) {
        self.remote.revisions_sent = None;
    }

    /// Whether the selected live launch has stopped, so its captured files' copies on disk can
    /// stand in for them.
    fn selected_settled(&self) -> bool {
        self.selected_live()
            .is_some_and(|launch| !launch.status.is_running())
    }

    /// Asks the worker behind the selection for the report the Report pane shows, and the live
    /// worker for the runs view the runs page shows.
    fn sync_report(&mut self) {
        self.sync_runs_report();
        self.follow_report();
        let wanted = self.wanted_report();
        if wanted == self.report_sent {
            return;
        }
        match &wanted {
            Some(ReportAsk::Local(pack, asked)) => {
                self.local.report(Some((pack.clone(), asked.clone())));
            }
            Some(ReportAsk::Live(wanted)) => {
                if let Some((_, (_, text), listed)) = self.live_report_source() {
                    self.remote.live_client.report(
                        wanted.clone(),
                        text.to_string(),
                        listed.to_vec(),
                    );
                }
            }
            None => {}
        }
        let was_local = matches!(self.report_sent, Some(ReportAsk::Local(..)));
        if was_local && !matches!(wanted, Some(ReportAsk::Local(..))) {
            self.local.report(None);
        }
        self.report_sent = wanted;
    }

    /// The report the Report pane shows while it is open: the selected local pack's, or the
    /// selected live run's when its pack carries a plugin and its captured files are listed.
    fn wanted_report(&self) -> Option<ReportAsk> {
        if !self.workspace().showing(PaneKind::Report) || self.runs_page().is_some() {
            return None;
        }
        let asked = self.report_shown.shown.asked();
        if let Some(pack) = self.selected_pack() {
            return Some(ReportAsk::Local(pack.name.clone(), asked));
        }
        let (run, (plugin, _), files) = self.live_report_source()?;
        Some(ReportAsk::Live(report::Wanted {
            run: run.to_string(),
            plugin: plugin.to_string(),
            listed: files.iter().map(|f| (f.key.clone(), f.size)).collect(),
            settled: self.selected_settled(),
            asked,
        }))
    }

    /// The selected live run, its plugin as path and text, and the files it captured.
    fn live_report_source(&self) -> Option<(&str, (&str, &str), &[captured::Captured])> {
        let detail = self.selected_detail()?;
        let plugin = self.live_plugin()?;
        let files = detail.files.as_ref()?.as_ref().ok()?;
        Some((detail.run_id.as_deref()?, plugin, files))
    }

    /// The plugin a live run and its playbook's runs page draw with, as its path and text: the
    /// file `CRUCIBLE_VIEW_PLUGIN` named, else the one the selected launch's pack carries.
    fn live_plugin(&self) -> Option<(&str, &str)> {
        if let Some(plugin) = &self.view_plugin {
            return Some((&plugin.path, &plugin.text));
        }
        report::text_plugin(self.selected_detail()?.pack.as_ref().ok()?)
    }

    /// The name of the file that stands in for a live pack's plugin.
    pub fn stand_in(&self) -> Option<&str> {
        self.view_plugin.as_ref().map(|plugin| plugin.file.as_str())
    }

    fn answer_report(&mut self, built: Option<std::sync::Arc<report::Built<report::Wanted>>>) {
        if let Some(built) = built
            && matches!(&self.report_sent, Some(ReportAsk::Live(sent)) if *sent == built.wanted)
        {
            self.remote.report = Some(built);
        }
    }

    /// The report the Report tab shows: the selected local pack's, or the one built for the
    /// selected live run.
    pub fn shown_report(&self) -> Option<&std::sync::Arc<report::Report>> {
        if let Some(pack) = self.selected_pack() {
            return self
                .local_report
                .as_ref()
                .filter(|built| built.pack == pack.name)
                .and_then(|built| built.report.as_ref());
        }
        let run = self.selected_detail()?.run_id.as_deref()?;
        self.remote
            .report
            .as_deref()
            .filter(|built| built.wanted.run == run)
            .and_then(|built| built.report.as_ref())
    }

    /// Drops what the Report pane kept for a selection or a report it no longer shows.
    fn follow_report(&mut self) {
        let report = self.shown_report().cloned();
        self.report_shown
            .follow(self.selection.as_ref(), report.as_ref());
    }

    /// The selected playbook and the launches the filters keep, newest first, when there is
    /// more than one.
    pub fn playbook_launches(&self) -> Option<(&str, Vec<&LivePlaybook>)> {
        let playbook = self.selected_playbook()?;
        let launches: Vec<&LivePlaybook> = self
            .remote
            .live
            .kept(&self.launch_filter())
            .into_iter()
            .filter(|launch| launch.id == playbook)
            .collect();
        (launches.len() > 1).then_some((playbook, launches))
    }

    /// The playbook and launches the runs page shows, while it is on screen in place of one
    /// run.
    pub fn runs_page(&self) -> Option<(&str, Vec<&LivePlaybook>)> {
        let following = matches!(self.selection, Some(Selection::Live { launch: None, .. }));
        let listed = matches!(&*self.remote.live, LiveView::Ready { .. });
        (self.page == Page::Runs && following && listed)
            .then(|| self.playbook_launches())
            .flatten()
    }

    /// Asks the live worker for the runs view of the playbook the runs page shows.
    fn sync_runs_report(&mut self) {
        let playbook = self.selected_playbook().map(str::to_string);
        let view = self.runs_view().cloned();
        self.runs_shown.follow(playbook.as_ref(), view.as_ref());
        let wanted = self.wanted_runs_report();
        if let Some(wanted) = self.remote.runs_ask.ask(wanted).cloned()
            && let Some((_, text)) = self.live_plugin()
        {
            self.remote
                .live_client
                .runs_report(wanted, text.to_string());
        }
    }

    /// The runs view of the playbook the runs page shows, while its newest launch's pack carries
    /// a plugin: the newest [`report::RUNS`] launches the filters keep.
    fn wanted_runs_report(&self) -> Option<report::WantedRuns> {
        let (playbook, launches) = self.runs_page()?;
        let (plugin, _) = self.live_plugin()?;
        Some(report::WantedRuns::of(
            playbook,
            plugin,
            &launches,
            self.runs_shown.shown.asked(),
        ))
    }

    /// The runs view built for the playbook the runs page shows, when its plugin draws one.
    pub fn runs_view(&self) -> Option<&std::sync::Arc<report::Report>> {
        let (playbook, _) = self.runs_page()?;
        self.remote.runs_ask.view(playbook)
    }

    /// Fires the tick of each live view on screen that is due at `clock`, and says how many
    /// seconds until the next is due.
    fn tick_reports(&mut self, clock: f64) -> Option<f64> {
        let report = self.workspace().showing(PaneKind::Report) && self.runs_page().is_none();
        let runs = self.runs_tab == RunsTab::Report && self.runs_view().is_some();
        let waits = [
            report.then(|| self.report_shown.shown.play.tick(clock)),
            runs.then(|| self.runs_shown.shown.play.tick(clock)),
        ];
        waits.into_iter().flatten().flatten().reduce(f64::min)
    }

    /// Whether a worker has been asked for a report it has not built yet.
    pub fn awaits_report(&self) -> bool {
        self.report_sent.as_ref().is_some_and(|ask| {
            ask.awaited(self.local_report.as_ref(), self.shown_report().is_some())
        })
    }

    /// The run, key and listed size of the captured file the Files tab shows, while it is open
    /// and the file is small enough to draw. A size that moves fetches the file again.
    fn wanted_file(&self) -> Option<(String, String, u64)> {
        if !self.workspace().showing(PaneKind::Files) {
            return None;
        }
        let detail = self.selected_detail()?;
        let run = detail.run_id.clone()?;
        let files = detail.files.as_ref()?.as_ref().ok()?;
        let file = captured::pick(files, self.run_file.as_deref())?;
        file.shown().then(|| (run, file.key.clone(), file.size))
    }

    /// Show the captured file under `key`, fetching it again if it is the one already asked for.
    pub fn pick_file(&mut self, key: &str) {
        self.run_file = Some(key.to_string());
        self.remote.file_sent = None;
    }

    fn answer_file(&mut self, file: Option<captured::Opened>) {
        if file == self.remote.opened_file {
            return;
        }
        if let Some(captured::Opened {
            key,
            body: Ok(captured::Body::Text(text)),
            ..
        }) = &file
            && captured::Kind::of(key) == captured::Kind::Patch
        {
            self.remote.file_diff.set_files(rune_kit::diff::parse(text));
        }
        self.remote.opened_file = file;
    }

    /// Open the fetched captured file's copy on disk in the user's editor.
    pub fn open_captured(&mut self) {
        self.open(Open::Captured);
    }

    /// Earlier launches of the selected one's playbook that ran, newest first.
    pub fn compare_candidates(&self) -> Vec<&live::LivePlaybook> {
        let Some(selected) = self.selected_live() else {
            return Vec::new();
        };
        let mut earlier: Vec<&live::LivePlaybook> = self
            .remote
            .live
            .launches_of(&selected.id)
            .filter(|launch| launch.key != selected.key && launch.runs > 0)
            .collect();
        earlier.sort_by_key(|launch| std::cmp::Reverse(launch.landed));
        earlier
    }

    /// The launch the selected one is compared against: the picked one, else the newest that
    /// landed before it.
    pub fn compare_base(&self) -> Option<&live::LivePlaybook> {
        let selected = self.selected_live()?;
        let candidates = self.compare_candidates();
        candidates
            .iter()
            .find(|launch| Some(&launch.key) == self.run.compare_to.as_ref())
            .or_else(|| {
                candidates
                    .iter()
                    .find(|launch| launch.landed < selected.landed)
            })
            .copied()
    }

    /// Sign in through the controller's own page, which sets the session cookie and comes back
    /// to this one.
    #[cfg(target_arch = "wasm32")]
    pub fn sign_in(&mut self) {
        let here = web_sys::window()
            .and_then(|window| window.location().pathname().ok())
            .unwrap_or_else(|| "/".into());
        if let Err(why) = rune::web::open_url_same_tab(&format!("/auth/login?rd={here}")) {
            self.remote.sign_in_error = Some(why);
        }
    }

    /// Sign in to the profile in use through the browser, on a thread of its own since it waits
    /// for the person.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn sign_in(&mut self) {
        if self.remote.signing_in {
            return;
        }
        let target = self.remote.target.clone();
        let waker = self.waker.clone();
        let runtime = self.net.handle();
        let (tx, rx) = std::sync::mpsc::channel();
        let (cancel, stop) = signin::Cancel::new();
        self.remote.signing_in = true;
        self.remote.sign_in_error = None;
        self.remote.sign_in_done = Some(rx);
        self.remote.sign_in_cancel = Some(cancel);
        let spawned = std::thread::Builder::new()
            .name("crucible-playbooks-sign-in".into())
            .spawn(move || {
                let _ = tx.send(signin::sign_in(&target, runtime, &stop));
                if let Some(waker) = waker {
                    waker();
                }
            });
        if let Err(e) = spawned {
            self.cancel_sign_in();
            self.remote.sign_in_error = Some(format!("starting the sign-in: {e}"));
        }
    }

    /// Stop waiting on the browser.
    pub fn cancel_sign_in(&mut self) {
        self.remote.signing_in = false;
        self.remote.sign_in_done = None;
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.remote.sign_in_cancel = None;
        }
    }

    /// Take a finished sign-in's answer; a new key means connecting again.
    fn finish_sign_in(&mut self) -> bool {
        let Some(answer) = self
            .remote
            .sign_in_done
            .as_ref()
            .and_then(|rx| rx.try_recv().ok())
        else {
            return false;
        };
        self.cancel_sign_in();
        match answer {
            Ok(()) => self.reconnect(),
            Err(why) => self.remote.sign_in_error = Some(why),
        }
        true
    }

    /// Keep the pasted key as the profile's, and connect with it.
    pub fn save_pasted_key(&mut self) {
        let key = self.remote.key_paste.text.trim().to_string();
        if key.is_empty() {
            return;
        }
        match self.remote.target.save_key(key) {
            Ok(()) => {
                self.remote.key_paste = TextEdit::new("");
                self.remote.sign_in_error = None;
                self.reconnect();
            }
            Err(why) => self.remote.sign_in_error = Some(why),
        }
    }

    fn reconnect(&mut self) {
        self.remote.live_client.reconnect();
        self.remote.admin_client.reconnect();
    }

    /// Open what `link` names: a playbook, one of its launches with a task open, or a local pack.
    pub fn follow(&mut self, link: &Link) {
        self.page = Page::Runs;
        match link {
            Link::Web { place, route } => self.follow_web(place, route),
            Link::Decision { id } => {
                self.backend = Backend::Controller;
                self.page = Page::Decisions;
                self.remote.desk.pick_decision(id);
            }
            Link::Playbook { id } => {
                self.backend = Backend::Controller;
                self.select(Selection::playbook(id));
            }
            Link::Launch {
                playbook,
                key,
                task,
            } => {
                self.backend = Backend::Controller;
                self.select(Selection::Live {
                    playbook: playbook.clone(),
                    launch: Some(key.clone()),
                });
                if let Some(task) = task {
                    self.run.expanded = Some(task.clone());
                    self.show_pane(PaneKind::Tasks);
                }
            }
            Link::Local { pack } => {
                self.backend = Backend::Local;
                self.select(Selection::Local(pack.clone()));
            }
        }
    }

    /// The link to the run on screen, with `task` open when one is named.
    pub fn link_here(&self, task: Option<&str>) -> Option<Link> {
        match &self.selection {
            Some(Selection::Live { .. }) => {
                let launch = self.selected_live()?;
                Some(Link::Launch {
                    playbook: launch.id.clone(),
                    key: launch.key.clone(),
                    task: task.map(ToString::to_string),
                })
            }
            Some(Selection::Local(pack)) => Some(Link::Local { pack: pack.clone() }),
            None => None,
        }
    }

    pub fn select(&mut self, selection: Selection) {
        if self.selection.as_ref() != Some(&selection) {
            self.forget_run_view();
        }
        self.selection = Some(selection);
        self.page = Page::Runs;
        self.sync_live();
        self.sync_report();
    }

    fn forget_run_view(&mut self) {
        self.run = RunView::default();
    }

    /// Selects instance `key` of fan-out `task` from the graph and shows its output in Data.
    pub fn pick_instance(&mut self, task: &str, key: &str) {
        self.run.expanded = Some(task.to_string());
        self.run.inspect.from = None;
        self.run.inspect.pick = Some(inspect::Pick::Instance(key.to_string()));
        self.show_pane(PaneKind::Data);
    }

    pub fn toggle_folded(&mut self, task: &str) {
        if !self.run.folded.remove(task) {
            self.run.folded.insert(task.to_string());
        }
    }

    /// Selects `task` from the graph, with `from` when its edge from that dependency was picked:
    /// the Data pane shows it when in front, else the edge opens Data and a node opens Tasks.
    pub fn pick_task(&mut self, task: &str, from: Option<&str>) {
        self.run.expanded = Some(task.to_string());
        self.run.inspect.from = from.map(ToString::to_string);
        self.run.inspect.pick = None;
        if !self.workspace().showing(PaneKind::Data) {
            self.show_pane(if from.is_some() {
                PaneKind::Data
            } else {
                PaneKind::Tasks
            });
        }
    }

    pub fn toggle_task(&mut self, name: &str) {
        self.run.expanded = match self.run.expanded.take() {
            Some(open) if open == name => None,
            _ => Some(name.to_string()),
        };
    }

    /// Show only `task`'s lines, its block opened, or every line again.
    pub fn filter_log(&mut self, task: Option<&str>) {
        if let Some(task) = task {
            self.run.log_folds.insert(task.to_string(), true);
            if let Some((parent, _)) = task.split_once('[') {
                self.run.log_folds.insert(parent.to_string(), true);
            }
        }
        self.run.log_task = task.map(ToString::to_string);
    }

    pub fn set_log_font(&mut self, font: LogFont) {
        self.settings.set_log_font(font);
    }

    pub fn set_checks(&mut self, on: bool) {
        self.settings.set_checks(on);
        if !on {
            self.lsp.stop();
            self.lsp_sent = None;
        }
    }

    /// What the signed-in user may own things as, themself first.
    /// Whether the signed-in caller administers the controller.
    pub fn is_admin(&self) -> bool {
        matches!(&*self.remote.live, LiveView::Ready { identity, .. } if identity.role == "admin")
    }

    /// Whether the signed-in caller may use the controller's own issue lane.
    pub fn has_autoresearch(&self) -> bool {
        matches!(&*self.remote.live, LiveView::Ready { identity, .. } if identity.autoresearch)
    }

    pub fn owners(&self) -> Vec<String> {
        match &*self.remote.live {
            LiveView::Ready { identity, .. } if !identity.owners.is_empty() => {
                identity.owners.clone()
            }
            _ => vec!["user:self".to_string()],
        }
    }

    /// Lay every window out at `zoom` and keep it for next time.
    pub fn set_zoom(&mut self, zoom: f32) {
        rune::zoom::set(zoom);
        self.settings.set_zoom(zoom);
    }

    /// Show `page`, or the runs when it already shows.
    pub fn toggle_page(&mut self, page: Page) {
        self.page = if self.page == page { Page::Runs } else { page };
    }

    /// Whether `page` is offered to this caller on this backend.
    pub fn offers(&self, page: Page) -> bool {
        let controller = self.backend == Backend::Controller;
        match page {
            Page::Runs | Page::Settings => true,
            Page::Issues => controller && self.has_autoresearch(),
            Page::Admin => controller && self.is_admin(),
            Page::Keys => controller && SESSION_AUTH,
            _ => controller,
        }
    }

    /// The pages of `group` offered to this caller, in tab order.
    pub fn tabs(&self, group: Group) -> Vec<Page> {
        group
            .pages()
            .iter()
            .copied()
            .filter(|page| self.offers(*page))
            .collect()
    }

    /// Open `group` on its first page, or close it when one of its pages is shown. The inbox
    /// opens on approvals when only those wait.
    pub fn toggle_group(&mut self, group: Group) {
        if self.page.group() == Some(group) {
            self.page = Page::Runs;
            return;
        }
        let approvals_only =
            group == Group::Inbox && self.waiting() == 0 && self.approvals_waiting() > 0;
        self.page = if approvals_only {
            Page::Approvals
        } else {
            self.tabs(group).first().copied().unwrap_or(Page::Runs)
        };
    }

    pub fn switch(&mut self, backend: Backend) {
        if backend == self.backend || (backend == Backend::Local && !LOCAL_PACKS) {
            return;
        }
        self.backend = backend;
        self.selection = None;
        self.forget_run_view();
        self.reconcile();
        self.sync_live();
        self.sync_report();
    }

    pub fn launch(&mut self) {
        let Some(pack) = self.selected_pack() else {
            return;
        };
        if pack.verdict != Verdict::Running {
            let name = pack.name.clone();
            self.local.launch(&name, MAX_TIME);
        }
    }
}

fn repo_from_args() -> Option<PathBuf> {
    let args = rune::extra_args();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--repo" {
            return args.next().map(PathBuf::from);
        }
        if let Some(path) = arg.strip_prefix("--repo=") {
            return Some(PathBuf::from(path));
        }
    }
    None
}

/// Whether requests are signed by a signed-in session, which is what managing API keys takes.
/// The desktop build presents a key, which the controller refuses there.
pub const SESSION_AUTH: bool = cfg!(target_arch = "wasm32");

/// Whether this build reads packs from this machine. The browser build does not.
pub const LOCAL_PACKS: bool = cfg!(not(target_arch = "wasm32"));

fn init() -> App {
    let repo = repo_from_args().unwrap_or_else(model::default_repo);
    let settings = SETTINGS.get().cloned().unwrap_or_else(Settings::load);
    let (keymap, keys_refused) = keymap::Keymap::new(&settings.keys);
    let editor_command = TextEdit::new(&settings.editor_command);
    let drop_cost = TextEdit::new(format!("{}", settings.drop_max_cost));
    let drop_time = TextEdit::new(&settings.drop_max_time);
    let drafts_dir = TextEdit::new(
        settings
            .drafts_dir
            .as_deref()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default(),
    );
    let (opened_tx, opened) = std::sync::mpsc::channel();
    let open_wake: std::sync::Arc<std::sync::Mutex<Option<Waker>>> = Default::default();
    let woken = open_wake.clone();
    rune_mac::files::on_open(move |paths| {
        let _ = opened_tx.send(paths);
        if let Some(wake) = woken.lock().ok().and_then(|w| w.clone()) {
            wake();
        }
    });
    let (name, chosen) = profile::pick(&settings);
    let target =
        profile::Target::new(&name, chosen, &settings.keys()).acting_as(settings.act_as(&name));
    let keeper = target.start(&repo, profile::supervisor);
    let net = net::Net::start();
    let remote = remote::Remote::new(target.clone(), &net, settings.drafts_root(), None);
    App {
        repo: repo.display().to_string(),
        remote,
        run: RunView::default(),
        local: Client::spawn(repo),
        local_seen: 0,
        lsp: lsp::Lsp::spawn(),
        lsp_seen: 0,
        checks: lsp::View::default(),
        lsp_sent: None,
        editor_error: None,
        backend: Backend::Controller,
        packs: Vec::new(),
        playbook_search: TextEdit::new(""),
        owner_filter: OwnerFilter::All,
        origin_filter: OriginFilter::All,
        landed_filter: LandedFilter::Any,
        filters_open: false,
        selection: None,
        log_search: TextEdit::new(""),
        log_view: LogView::All,
        log_source: LogSource::Transcript,
        log_chatter: false,
        log_mode: None,
        keymap,
        keys_refused,
        palette: None,
        pane_focus: None,
        notifier: None,
        clicks: None,
        recorders: HashMap::new(),
        provider_scratch: rune_kit::ComboboxState::default(),
        layout: settings.layout.clone(),
        shown: Vec::new(),
        pane_drag: None,
        resizing: None,
        rail_drag: None,
        geometry: Geometry::default(),
        ignition: ignition::Ignition::default(),
        logo_clicks: 0,
        logo_screen: [0.0; 2],
        targets: HashMap::from([(name, (target, keeper))]),
        profile_form: None,
        keys_error: None,
        source_menu: false,
        panes_menu: false,
        tray: None,
        fleet: Vec::new(),
        fleet_client: fleet::Client::spawn(net.handle()),
        net,
        fleet_seen: 0,
        fleet_sent: Vec::new(),
        approvals_known: HashMap::new(),
        picks: None,
        opened: Some(opened),
        open_wake,
        history: history::History::default(),
        waker: None,
        release: update::Check::default(),
        scratch: TextEdit::new(""),
        settings,
        editor_command,
        drop_cost,
        drop_time,
        drop_error: None,
        drafts_dir,
        drafts_error: None,
        drop_failed: None,
        login_editor: LoginEditor::Unasked,
        opening: None,
        pack_md_source: false,
        run_file: None,
        files_md_source: false,
        report_shown: report::shown::Scoped::default(),
        report_sent: None,
        local_report: None,
        runs_shown: report::shown::Scoped::default(),
        runs_tab: RunsTab::default(),
        view_plugin: std::env::var("CRUCIBLE_VIEW_PLUGIN")
            .ok()
            .and_then(|named| StandIn::read(&named)),
        knobs: rune_knobs::Knobs::default(),
        page: Page::Runs,
        flight: None,
        local_error: None,
        act_as_refused: None,
    }
}

fn setup(app: &mut App, waker: Waker) {
    rune::zoom::set(app.settings.zoom);
    app.waker = Some(waker.clone());
    let (clicked, clicks) = std::sync::mpsc::channel();
    #[cfg(target_os = "linux")]
    {
        instance::serve(clicked.clone(), waker.clone());
        if let Some(link) = instance::launch_link(rune::extra_args()) {
            let _ = clicked.send(link);
        }
    }
    app.notifier = Some(notify::Notifier::start(clicked, waker.clone()));
    app.clicks = Some(clicks);
    let (picked, picks) = std::sync::mpsc::channel();
    let pick_waker = waker.clone();
    rune_mac::status::on_pick(move |id| {
        let _ = picked.send(id);
        pick_waker();
    });
    app.picks = Some(picks);
    if let Ok(mut wake) = app.open_wake.lock() {
        *wake = Some(waker.clone());
    }
    app.local.set_waker(waker.clone());
    app.remote.set_waker(&waker);
    app.fleet_client.set_waker(waker.clone());
    app.lsp.set_waker(waker);
    app.check_release();
}

/// Nothing here touches a file, a process or the network: the workers own all of that and wake the
/// loop once per finished snapshot.
fn poll(app: &mut App, clock: f64, tx: &mut Tx) -> bool {
    let mut changed = app.ignition.tick(clock);
    if let Some((version, snapshot)) = app.local.take(app.local_seen) {
        app.local_seen = version;
        app.apply_local(snapshot);
        changed = true;
    }
    if app.heed_act_as_refusal() {
        changed = true;
    }
    if let Some((version, snapshot)) = app.remote.live_client.take(app.remote.live_seen) {
        app.remote.live_seen = version;
        app.apply_live(snapshot);
        changed = true;
    }
    if let Some((version, found)) = app.remote.finder_client.take(app.remote.finder_seen) {
        app.remote.finder_seen = version;
        app.apply_found(found);
        changed = true;
    }
    if let Some((version, view)) = app.remote.admin_client.take(app.remote.admin_seen) {
        app.remote.admin_seen = version;
        app.apply_admin(view);
        changed = true;
    }
    while let Some(link) = app
        .clicks
        .as_ref()
        .and_then(|clicks| clicks.try_recv().ok())
        .and_then(|url| Link::parse(&url))
    {
        app.follow(&link);
        notify::bring_to_front();
        changed = true;
    }
    while let Some(paths) = app
        .opened
        .as_ref()
        .and_then(|opened| opened.try_recv().ok())
    {
        app.open_paths(paths);
        changed = true;
    }
    while let Some(id) = app.picks.as_ref().and_then(|picks| picks.try_recv().ok()) {
        app.picked(&id);
        changed = true;
    }
    if let Some((version, statuses)) = app.fleet_client.take(app.fleet_seen) {
        app.fleet_seen = version;
        app.fleet = statuses;
        changed = true;
    }
    if let Some(wait) = app.tick_reports(clock) {
        tx.wake_after(std::time::Duration::try_from_secs_f64(wait).unwrap_or_default());
    }
    app.sync_live();
    app.sync_report();
    app.sync_admin();
    app.sync_fleet();
    app.sync_tray();
    let place = app.place();
    app.history.saw(place);
    changed |= app.refresh_working();
    changed |= app.refresh_agent();
    changed |= app.finish_sign_in();
    changed |= app.finish_login_editor();
    changed |= app.finish_release_check();
    if let Some((version, view)) = app.lsp.take(app.lsp_seen) {
        app.lsp_seen = version;
        app.checks = view;
        changed = true;
    }
    app.sync_checks();
    if let Some(due) = app.ignition.due(clock) {
        tx.wake_at(due);
    }
    changed
}

/// Shows the pane `CRUCIBLE_TAB` names in the `CRUCIBLE_THEME` theme, for headless scenes,
/// without saving either.
fn scene_tab(app: &mut App) {
    let theme = std::env::var("CRUCIBLE_THEME").unwrap_or_default();
    if let Some(theme) = Theme::ALL
        .into_iter()
        .find(|t| t.label().eq_ignore_ascii_case(&theme))
    {
        app.settings.theme = theme;
    }
    let size = std::env::var("CRUCIBLE_TEXT").unwrap_or_default();
    if let Some(size) = settings::TextSize::ALL
        .into_iter()
        .find(|s| s.label().eq_ignore_ascii_case(&size))
    {
        app.settings.text_size = size;
    }
    app.run_file = std::env::var("CRUCIBLE_FILE").ok();
    let tab = std::env::var("CRUCIBLE_TAB").unwrap_or_default();
    if let Some(kind) = PaneKind::ALL
        .into_iter()
        .find(|kind| kind.label().eq_ignore_ascii_case(&tab))
    {
        match &mut app.layout {
            Some(layout) => layout.activate(kind),
            None => app.shown.push(kind),
        }
    }
}

/// A link the system opened with the app. It comes from outside, so anything that does not
/// parse is dropped.
#[cfg(not(feature = "hot"))]
fn open_link(app: &mut App, url: &str, _tx: &mut Tx) {
    if let Some(link) = Link::parse(url) {
        app.follow(&link);
    }
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    run();
}

pub fn run() {
    #[cfg(not(target_arch = "wasm32"))]
    if std::env::args().nth(1).as_deref() == Some(profile::KEEP_FLAG) {
        std::process::exit(profile::keep());
    }
    #[cfg(target_os = "linux")]
    if let Some(link) = instance::launch_link(rune::extra_args())
        && instance::forward(&link)
    {
        return;
    }
    rune::set_app_id(APP_ID);
    rune_mac::files::listen();
    let settings = Settings::load();
    let fonts = settings::fonts(&settings);
    let _ = SETTINGS.set(settings);
    let spec = AppSpec {
        fonts,
        families: settings::families(),
        setup: Some(setup),
        poll: Some(poll),
        on_key: Some(commands::on_key),
        scenes: &[
            // `CRUCIBLE_TAB` shows that pane (tasks, timeline, graph, pack, files, report, launch
            // or log), `CRUCIBLE_FILE` the captured file the Files pane shows, by key, and
            // `CRUCIBLE_PICK` opens that task's card.
            ("local", |app: &mut App| {
                app.backend = Backend::Local;
                app.select(Selection::Local("playbook".into()));
                app.run.expanded = std::env::var("CRUCIBLE_PICK").ok();
                scene_tab(app);
            }),
            ("settings", |app: &mut App| {
                scene_tab(app);
                app.page = Page::Settings;
            }),
            // `CRUCIBLE_PAGE` opens that page (triggers, secrets, decisions, approvals, teams, providers, blueprints, activity, issues, admin or keys) against the
            // controller crux's config names.
            ("page", |app: &mut App| {
                app.backend = Backend::Controller;
                let page = std::env::var("CRUCIBLE_PAGE").unwrap_or_default();
                if let Some(page) = [
                    Page::Triggers,
                    Page::Secrets,
                    Page::Decisions,
                    Page::Approvals,
                    Page::Teams,
                    Page::Providers,
                    Page::Blueprints,
                    Page::Activity,
                    Page::Issues,
                    Page::Admin,
                    Page::Keys,
                ]
                .into_iter()
                .find(|p| p.id() == page)
                {
                    app.page = page;
                }
                if let Ok(id) = std::env::var("CRUCIBLE_PICK") {
                    match app.page {
                        Page::Secrets => app.remote.desk.secret = Some(id),
                        Page::Decisions => app.remote.desk.pick_decision(&id),
                        _ => {}
                    }
                }
            }),
            // An open decision request from `fixtures/decision.json`, with no controller.
            ("decision", |app: &mut App| {
                app.backend = Backend::Local;
                app.page = Page::Decisions;
                admin::show_fixture(app);
            }),
            // `CRUCIBLE_LIVE_SCENE=<playbook id>` opens that live playbook headlessly, and
            // `CRUCIBLE_LINK=<crucible-playbooks:// link>` follows a link as the system would.
            // `CRUCIBLE_RANGE=<from>..<to>` picks the saves the History pane compares, and
            // `CRUCIBLE_DIFF=split` draws them side by side.
            // `CRUCIBLE_RUNS_TAB=report` shows the runs page's Report tab, and
            // `CRUCIBLE_VIEW_PLUGIN=<file>`, in any scene, draws live runs and runs pages with
            // that plugin file in place of the pack's.
            ("live", |app: &mut App| {
                app.backend = Backend::Controller;
                scene_tab(app);
                if std::env::var("CRUCIBLE_RUNS_TAB").is_ok_and(|tab| tab == "report") {
                    app.runs_tab = RunsTab::Report;
                }
                if let Ok(id) = std::env::var("CRUCIBLE_LIVE_SCENE") {
                    app.select(Selection::playbook(&id));
                }
                if let Some(link) = std::env::var("CRUCIBLE_LINK")
                    .ok()
                    .and_then(|url| Link::parse(&url))
                {
                    app.follow(&link);
                }
                app.run.history_range = std::env::var("CRUCIBLE_RANGE")
                    .ok()
                    .and_then(|range| revisions::Range::parse(&range));
                if std::env::var("CRUCIBLE_DIFF").is_ok_and(|mode| mode == "split") {
                    app.remote
                        .revisions_diff
                        .set_mode(rune_kit::diff::DiffMode::Split);
                }
            }),
        ],
        min_size: Some((900.0, 560.0)),
        ..AppSpec::new("Crucible Playbooks", (1120.0, 720.0), init, ui::view)
    };
    #[cfg(feature = "hot")]
    rune::run(spec);
    #[cfg(target_arch = "wasm32")]
    rune::App::new(spec).on_open_url(open_link).run();
    #[cfg(not(any(feature = "hot", target_arch = "wasm32")))]
    rune::App::new(spec)
        .on_open_url(open_link)
        .chrome(rune::Chrome {
            traffic_lights: Some(ui::TRAFFIC_LIGHTS),
            ..rune::Chrome::DEFAULT
        })
        .compositor(gfx::Gfx::default())
        .run();
}

#[cfg(test)]
mod tests {
    use crate::model::{self, default_repo};
    use crate::{GraphSource, Group, Page, ReportAsk, report};

    #[test]
    fn a_page_is_a_tab_of_at_most_one_group_and_the_rest_stand_alone() {
        let groups = [Group::Inbox, Group::Activity, Group::Settings];
        for group in groups {
            for page in group.pages() {
                assert_eq!(page.group(), Some(group), "{page:?}");
                let elsewhere = groups
                    .iter()
                    .filter(|other| other.pages().contains(page))
                    .count();
                assert_eq!(elsewhere, 1, "{page:?} is in one group");
            }
        }
        for alone in [Page::Runs, Page::Triggers, Page::Blueprints] {
            assert_eq!(alone.group(), None, "{alone:?}");
        }
        assert_eq!(Group::Settings.pages()[0], Page::Settings);
        assert_eq!(
            (Page::Settings.tab(), Page::Activity.tab(), Page::Keys.tab()),
            ("General", "Events", "API keys")
        );
    }

    #[test]
    fn the_compiled_pack_is_drawn_when_picked_or_when_nothing_ran() {
        let previews = GraphSource::previews;
        assert!(previews(None, false, true));
        assert!(!previews(None, true, true));
        assert!(!previews(None, false, false));
        assert!(previews(Some(GraphSource::Preview), true, false));
        assert!(!previews(Some(GraphSource::Run), false, true));
    }

    #[test]
    fn a_report_is_awaited_until_its_worker_answers_for_it() {
        let built = |pack: &str| model::PackReport {
            pack: pack.into(),
            report: None,
        };
        let local = ReportAsk::Local("demo".into(), report::Asked::default());
        assert!(local.awaited(None, false));
        assert!(local.awaited(Some(&built("other")), false), "another's");
        assert!(
            !local.awaited(Some(&built("demo")), false),
            "a pack with no plugin has been answered"
        );
        assert!(!local.awaited(Some(&built("demo")), true));

        let live = ReportAsk::Live(report::Wanted {
            run: "r1".into(),
            plugin: "ui/report.js".into(),
            listed: Vec::new(),
            settled: true,
            asked: report::Asked::default(),
        });
        assert!(live.awaited(Some(&built("demo")), false));
        assert!(!live.awaited(None, true));
    }

    #[test]
    fn the_default_repo_resolves_from_the_crate_directory() {
        let repo = default_repo();
        assert!(
            repo.join("examples/playbook/crucible.toml").is_file(),
            "{}",
            repo.display()
        );
    }
}
