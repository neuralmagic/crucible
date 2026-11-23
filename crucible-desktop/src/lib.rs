//! A playbook tracker for Crucible. Two backends, one at a time: the controller's registered
//! playbooks (through `crux`, with the same config a terminal uses) and the local `examples/` packs
//! (through each pack's `state/session.jsonl`).

mod activity;
mod admin;
mod afterglow;
mod agent;
mod apikeys;
mod app;
#[cfg(test)]
mod budget;
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
#[cfg(not(target_arch = "wasm32"))]
mod forge;
mod form;
mod gfx;
mod handoff;
mod history;
mod html_preview;
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

use rune::{AppSpec, TextEdit, Waker};

#[cfg(not(feature = "hot"))]
use crate::app::boot::open_link;
use crate::app::boot::{init, poll, scene_tab, setup};
use crate::link::Link;
use crate::live::{LivePlaybook, Target};
use crate::logline::LogView;
use crate::model::{Client, PackView};
use crate::settings::Settings;
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
    pub answers: Vec<form::Answer>,
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
    pub schedule: Option<admin::forms::ScheduleForm>,
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
    pub call_folds: HashSet<String>,
    pub frame_hover: Option<String>,
    pub frame_info: Option<String>,
    pub call_node: Option<crate::live::calls::CallNode>,
    /// How far the graph is zoomed and moved.
    pub camera: camera::Camera,
    /// Whether the graph is drawn compact; `None` takes the setting.
    pub compact_graph: Option<bool>,
    /// Whether the Graph pane shows its find field.
    pub graph_finding: bool,
    /// What the Graph pane's find field holds.
    pub graph_find: rune::TextEdit,
    /// The match the graph last moved to; `None` until Enter after the query changes.
    pub graph_found: Option<usize>,
    /// The launch picked to compare against; `None` takes the one before.
    pub compare_to: Option<String>,
    /// The saves the History pane compares, once picked by hand; `None` opens on the save the
    /// run ran.
    pub history_range: Option<revisions::Range>,
    /// Only lines from this task or fan-out instance.
    pub log_task: Option<String>,
    /// Log blocks opened or folded by hand, by task or instance name.
    pub log_folds: HashMap<String, bool>,
    pub node_folds: HashMap<usize, bool>,
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
    pub window_focused: bool,
    /// Posts notifications once the app's loop is up.
    notifier: Option<notify::Notifier>,
    /// The launch a notification's Rerun button was pressed on, until its rerun form can open.
    rerun_wanted: Option<String>,
    /// What clicked notifications and links handed over by a second launch ask for.
    clicks: Option<std::sync::mpsc::Receiver<notify::Clicked>>,
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
    pub files_html_source: bool,
    pub html_preview_error: Option<String>,
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
    pub afterglows: afterglow::Afterglows,
    pub runs_unfolded: HashSet<String>,
    /// Clicks on the molten logo, which slosh it.
    pub logo_clicks: u32,
    /// The logo's top-left on screen, in points; a window move sloshes it.
    pub logo_screen: [f32; 2],
    /// Every profile started so far, with what it keeps running.
    targets: HashMap<String, (profile::Target, Option<profile::supervise::Keeper>)>,
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
    #[cfg(not(target_arch = "wasm32"))]
    pub forge: std::collections::BTreeMap<forge::Account, forge::session::Session>,
    #[cfg(not(target_arch = "wasm32"))]
    pub forge_offers: forge::apps::Offers,
    #[cfg(not(target_arch = "wasm32"))]
    pub github_client_id: TextEdit,
    #[cfg(not(target_arch = "wasm32"))]
    pub gitlab_host: TextEdit,
    #[cfg(not(target_arch = "wasm32"))]
    pub gitlab_client_id: TextEdit,
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

/// Whether requests are signed by a signed-in session, which is what managing API keys takes.
/// The desktop build presents a key, which the controller refuses there.
pub const SESSION_AUTH: bool = cfg!(target_arch = "wasm32");

/// Whether this build reads packs from this machine. The browser build does not.
pub const LOCAL_PACKS: bool = cfg!(not(target_arch = "wasm32"));

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    run();
}

pub fn run() {
    #[cfg(not(target_arch = "wasm32"))]
    if std::env::args().nth(1).as_deref() == Some(profile::supervise::KEEP_FLAG) {
        std::process::exit(profile::supervise::keep());
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
                admin::app::show_fixture(app);
            }),
            // `CRUCIBLE_LIVE_SCENE=<playbook id>` opens that live playbook headlessly, and
            // `CRUCIBLE_LINK=<crucible-playbooks:// link>` follows a link as the system would.
            // `CRUCIBLE_RANGE=<from>..<to>` picks the saves the History pane compares, and
            // `CRUCIBLE_DIFF=split` draws them side by side.
            // `CRUCIBLE_FRAME_INFO=<frame>` opens that call frame's info card.
            // `CRUCIBLE_RUNS_TAB=report` shows the runs page's Report tab, and
            // `CRUCIBLE_VIEW_PLUGIN=<file>`, in any scene, draws live runs and runs pages with
            // that plugin file in place of the pack's.
            ("live", |app: &mut App| {
                app.backend = Backend::Controller;
                scene_tab(app);
                if std::env::var("CRUCIBLE_RUNS_TAB").is_ok_and(|tab| tab == "report") {
                    app.runs_tab = RunsTab::Report;
                    app.settings.view_plugins = true;
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
                app.run.frame_info = std::env::var("CRUCIBLE_FRAME_INFO").ok();
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
    html_preview::install(rune::App::new(spec))
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
