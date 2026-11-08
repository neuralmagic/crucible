//! A playbook tracker for Crucible. Two backends, one at a time: the controller's registered
//! playbooks (through `crux`, with the same config a terminal uses) and the local `examples/` packs
//! (through each pack's `state/session.jsonl`).

mod commands;
mod diff;
mod draft;
mod editor;
mod filter;
mod form;
mod gfx;
mod handoff;
mod httpcache;
mod inspect;
mod keymap;
mod link;
mod live;
mod logline;
mod lsp;
mod model;
mod notify;
mod pack;
mod palette;
mod preview;
mod runs;
mod session;
mod settings;
mod signin;
mod studio;
mod syntax;
mod timeline;
mod transcript;
mod ui;
mod unfurl;
mod workspace;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::OnceLock;

use rune::{AppSpec, TextEdit, Tx, Waker};

use crate::draft::Working;
use crate::editor::Editor;
use crate::filter::{LandedFilter, LaunchFilter, OriginFilter, OwnerFilter};
use crate::link::Link;
use crate::live::{LiveDetail, LivePlaybook, LiveView, Target};
use crate::logline::LogView;
use crate::model::{Client, PackView, Snapshot, Verdict};
use crate::pack::{Disk, PackFile, PackFiles};
use crate::session::TaskView;
use crate::settings::{LogFont, Settings, Theme};
use crate::workspace::{Divider, Geometry, PaneDrag, PaneKind, Workspace};

const MAX_TIME: &str = "10m";

static SETTINGS: OnceLock<Settings> = OnceLock::new();

#[derive(Clone, Copy, PartialEq, Eq)]
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

#[derive(Clone, PartialEq, Eq)]
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
    pub cost: TextEdit,
    pub time: TextEdit,
    /// The admin caps: dollars and a duration.
    pub caps: (f64, String),
    pub problems: form::Problems,
    /// Why the controller refused the launch, or a ceiling over its cap.
    pub refusal: Option<String>,
    /// A draft's schema digest, sent so a save that lands meanwhile refuses the launch.
    digest: Option<String>,
    /// Set when this form reruns a finished run instead of starting afresh.
    pub rerun: Option<studio::Rerun>,
    /// Only replies newer than this one answer this form.
    after: u64,
    /// The newest schema reply this form took, so a repeated snapshot does not refill it.
    filled: u64,
}

/// Which pack the Pack tab shows when there is a working copy beside the run's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackSource {
    Run,
    Working,
}

pub struct App {
    pub repo: String,
    local: Client,
    local_seen: u64,
    live_client: live::Client,
    live_seen: u64,
    lsp: lsp::Lsp,
    lsp_seen: u64,
    /// What `crucible-lsp` last said about the packs on screen.
    pub checks: lsp::View,
    /// The file last handed to the language server, and a hash of its text.
    lsp_sent: Option<(PathBuf, u64)>,
    /// Why the last hand-off to the editor failed.
    pub editor_error: Option<String>,
    /// The launch key the live worker was last told to read.
    live_sent: Option<String>,
    /// The launch the Compare pane reads against, as last asked of the worker.
    compare_sent: Option<String>,
    /// The launch picked to compare against; `None` takes the one before.
    pub compare_to: Option<String>,
    /// That launch's run, once read.
    pub live_base: Option<live::Base>,
    pub backend: Backend,
    pub packs: Vec<PackView>,
    pub playbook_search: TextEdit,
    pub owner_filter: OwnerFilter,
    pub origin_filter: OriginFilter,
    pub landed_filter: LandedFilter,
    pub live: LiveView,
    pub live_detail: Option<LiveDetail>,
    pub selection: Option<Selection>,
    /// The log pane's search field; its text filters the log lines shown.
    pub log_search: TextEdit,
    pub log_view: LogView,
    pub log_source: LogSource,
    /// Show library `tracing` lines below WARN.
    pub log_chatter: bool,
    /// Only lines from this task or fan-out instance.
    pub log_task: Option<String>,
    /// The log's layout when picked; `None` takes the one the log suits.
    pub log_mode: Option<unfurl::LogMode>,
    /// Log blocks opened or folded by hand, by task or instance name.
    pub log_folds: HashMap<String, bool>,
    /// The task card shown open, and the task the Data pane shows.
    pub expanded: Option<String>,
    pub inspect: inspect::Inspect,
    /// Fan-outs the graph draws with their instances.
    pub unfolded: HashSet<String>,
    /// Which keys run which commands, and the bindings in the config file it could not use.
    pub keymap: keymap::Keymap,
    pub keys_refused: Vec<keymap::Refused>,
    /// The command palette, while open.
    pub palette: Option<palette::Palette>,
    /// The pane keyboard navigation starts from.
    pub pane_focus: Option<PaneKind>,
    /// Each command's key recorder in Settings, once touched.
    pub recorders: HashMap<keymap::Command, rune_kit::ShortcutRecorderState>,
    /// The pack file the Pack tab shows; the manifest when none is picked.
    pub pack_file: Option<String>,
    /// The workspace the user arranged; `None` lays each run out by its graph's depth.
    pub layout: Option<Workspace>,
    /// Panes picked from a tab bar while the layout follows the run, oldest first.
    pub shown: Vec<PaneKind>,
    pub pane_drag: Option<PaneDrag>,
    /// The divider being dragged and its neighbours' shares when the drag began.
    pub resizing: Option<(Divider, (f32, f32))>,
    pub geometry: Geometry,
    /// Clicks on the molten logo, which slosh it.
    pub logo_clicks: u32,
    /// The logo's top-left on screen, in points; a window move sloshes it.
    pub logo_screen: [f32; 2],
    /// The newest finished launch already announced.
    notified: u64,
    /// The working copy of the draft the shown pack edits, and its files as last read.
    pub working: Option<Working>,
    pub working_pack: Option<PackFiles>,
    pub working_edited: bool,
    working_read: Option<std::time::Instant>,
    /// The pack the user picked to show; `None` shows a draft run's working copy.
    pub pack_source: Option<PackSource>,
    /// What the last pull or save said, and whether one is on its way.
    pub draft_note: Option<Result<String, String>>,
    pub draft_busy: bool,
    draft_after: u64,
    /// Where the last brief for an agent was written, or why it was not.
    pub handoff_note: Option<Result<PathBuf, String>>,
    /// The controller a browser sign-in goes to, and how the last one went.
    pub sign_in_url: TextEdit,
    pub signing_in: bool,
    pub sign_in_error: Option<String>,
    sign_in_done: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    waker: Option<Waker>,
    /// Playbooks the controller has registered: the ones a launch form can start.
    pub registered: Vec<String>,
    /// The open launch form, when there is one.
    pub launcher: Option<Launcher>,
    /// The newest worker reply a launch form has taken.
    replies: u64,
    /// Where a text field's edits land once its form has closed.
    pub scratch: TextEdit,
    pub settings: Settings,
    pub settings_open: bool,
    pub error: Option<String>,
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
        self.live.playbooks(&self.launch_filter())
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
        self.live
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

    /// The workspace to draw: the user's own, or one fitted to the selected run's graph.
    pub fn workspace(&self) -> Workspace {
        self.layout.clone().unwrap_or_else(|| {
            let tasks = self.run_tasks();
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
        self.live_detail
            .as_ref()
            .filter(|detail| detail.key == launch.key)
    }

    /// The pack the Pack tab shows, local or from the controller.
    pub fn shown_pack(&self) -> Option<&PackFiles> {
        match &self.selection {
            Some(Selection::Local(_)) => self.selected_pack().map(|pack| &pack.pack),
            Some(Selection::Live { .. }) if self.showing_working() => self.working_pack.as_ref(),
            Some(Selection::Live { .. }) => self.selected_detail()?.pack.as_ref().ok(),
            None => None,
        }
    }

    /// The file the Pack tab shows: the one picked, else the first.
    pub fn shown_file(&self) -> Option<(&PackFiles, &PackFile)> {
        let pack = self.shown_pack()?;
        let file = self
            .pack_file
            .as_deref()
            .and_then(|path| pack.file(path))
            .or_else(|| pack.files.first())?;
        Some((pack, file))
    }

    /// Hand the language server the shown Starlark file, or its pack's workflow when the file
    /// is not Starlark, so the pack's diagnostics stay current as it changes on disk.
    fn sync_checks(&mut self) {
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

    /// Open line `line` (1-based) of pack file `file` in the user's editor: the file itself from
    /// a checkout, or its text as a scratch buffer from a controller's copy.
    pub fn open_in_editor(&mut self, file: &str, line: usize) {
        let Some(editor) = self.settings.editor.or_else(Editor::detect) else {
            self.editor_error = Some("No editor found. Pick one in Settings.".to_string());
            return;
        };
        let Some(pack) = self.shown_pack() else {
            return;
        };
        let result = match (&pack.disk, pack.file(file)) {
            (Some(Disk::Checkout(dir)), _) => editor::open_file(editor, &dir.join(file), line),
            (
                _,
                Some(PackFile {
                    text: Some(text), ..
                }),
            ) => editor::open_text(editor, text),
            _ => Err(format!("{file} has no text to open")),
        };
        self.editor_error = result.err();
    }

    fn apply_local(&mut self, snapshot: Snapshot) {
        self.packs = snapshot.packs;
        self.error = snapshot.error;
        self.reconcile();
    }

    /// Announces launches by the signed-in user that finished since the last snapshot.
    fn announce(&mut self, finished: &[live::Finished]) {
        let me = match &self.live {
            LiveView::Ready { identity, .. } => identity.user.clone(),
            _ => String::new(),
        };
        let seen = self.notified;
        for done in finished.iter().filter(|done| done.seq > seen) {
            self.notified = done.seq;
            if self.settings.notify && !me.is_empty() && done.by == me {
                let (title, body) = notify::message(done);
                notify::post(&title, &body);
            }
        }
    }

    /// Opens a launch form for `target` and asks for its schema.
    pub fn open_launcher(&mut self, target: Target) {
        self.launcher = Some(Launcher {
            target: target.clone(),
            stage: Stage::Reading,
            fields: Vec::new(),
            answers: Vec::new(),
            cost: TextEdit::new(""),
            time: TextEdit::new(""),
            caps: (0.0, String::new()),
            problems: Vec::new(),
            refusal: None,
            digest: None,
            rerun: None,
            after: self.replies,
            filled: 0,
        });
        self.live_client.prepare(target);
    }

    /// Sends the form, or marks what has to change first.
    pub fn submit_launch(&mut self) {
        let Some(launcher) = &mut self.launcher else {
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
        let body = studio::launch_body(
            &launcher.target,
            params,
            max_cost,
            &max_time,
            launcher.digest.as_deref(),
            launcher.rerun.as_ref(),
        );
        launcher.stage = Stage::Sending;
        launcher.after = self.replies;
        self.live_client.launch(launcher.target.clone(), body);
    }

    /// The newest launch of `playbook` the list shows, to prefill a form from.
    fn newest_launch(&self, playbook: &str) -> Option<&LivePlaybook> {
        self.live
            .launches_of(playbook)
            .max_by_key(|launch| launch.landed)
    }

    /// Fills an open form from the schema the worker read, or lands the launch it sent.
    fn answer_launcher(
        &mut self,
        prepared: Option<live::Prepared>,
        launched: Option<live::Launched>,
    ) {
        let Some(after) = self.launcher.as_ref().map(|l| l.after) else {
            return;
        };
        if let Some(launched) = launched.filter(|l| l.seq > after) {
            self.replies = self.replies.max(launched.seq);
            match launched.result {
                Ok(key) => {
                    self.launcher = None;
                    self.backend = Backend::Controller;
                    self.select(Selection::Live {
                        playbook: launched.playbook,
                        launch: Some(key),
                    });
                }
                Err(error) => {
                    if let Some(launcher) = &mut self.launcher {
                        launcher.stage = Stage::Ready;
                        launcher.refusal = Some(error);
                    }
                }
            }
            return;
        }
        let filled = self.launcher.as_ref().map_or(0, |l| l.filled);
        let Some(prepared) = prepared.filter(|p| p.seq > after && p.seq > filled) else {
            return;
        };
        self.replies = self.replies.max(prepared.seq);
        let newest = self.newest_launch(prepared.target.id()).cloned();
        let previous = self
            .selected_live()
            .filter(|live| live.id == prepared.target.id())
            .and(self.selected_detail())
            .and_then(|detail| detail.launch.as_ref())
            .map(|record| record.launch.params.clone());
        let Some(launcher) = &mut self.launcher else {
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
            }) => {
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
                let cost = newest.as_ref().map_or(5.0, |n| n.cap).min(max_cost);
                let time = newest
                    .as_ref()
                    .map(|n| n.max_time.clone())
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
        self.live = snapshot.view;
        self.live_detail = snapshot.detail;
        self.live_base = snapshot.base;
        self.registered = snapshot.registered;
        self.answer_rerun(snapshot.reran);
        self.answer_launcher(snapshot.prepared, snapshot.launched);
        self.answer_draft(snapshot.synced);
        self.announce(&snapshot.finished);
        self.reconcile();
    }

    /// Keep the selection inside the active backend, and drop a pin on a launch the list lost.
    fn reconcile(&mut self) {
        let valid = match (self.backend, &self.selection) {
            (Backend::Local, Some(Selection::Local(name))) => {
                self.visible_packs().any(|pack| &pack.name == name)
            }
            (Backend::Controller, Some(Selection::Live { playbook, .. })) => match &self.live {
                LiveView::Ready { .. } => self.live.launches_of(playbook).next().is_some(),
                LiveView::Loading | LiveView::Unavailable(_) => true,
            },
            _ => false,
        };
        if !valid {
            self.selection = self.initial_selection();
        } else if let (LiveView::Ready { .. }, Some(Selection::Live { launch, .. })) =
            (&self.live, &mut self.selection)
            && launch
                .as_deref()
                .is_some_and(|key| self.live.launch(key).is_none())
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
        if key != self.live_sent {
            self.live_client.select(key.as_deref());
            self.live_sent = key;
        }
        let base = self
            .workspace()
            .showing(PaneKind::Compare)
            .then(|| self.compare_base())
            .flatten()
            .map(|launch| launch.key.clone());
        if base != self.compare_sent {
            self.live_client.compare(base.as_deref());
            self.compare_sent = base;
        }
    }

    /// Earlier launches of the selected one's playbook that ran, newest first.
    pub fn compare_candidates(&self) -> Vec<&live::LivePlaybook> {
        let Some(selected) = self.selected_live() else {
            return Vec::new();
        };
        let mut earlier: Vec<&live::LivePlaybook> = self
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
            .find(|launch| Some(&launch.key) == self.compare_to.as_ref())
            .or_else(|| {
                candidates
                    .iter()
                    .find(|launch| launch.landed < selected.landed)
            })
            .copied()
    }

    /// Sign in to the controller at the sign-in field's URL through the browser, on a thread of
    /// its own since it waits for the person.
    pub fn sign_in(&mut self) {
        if self.signing_in {
            return;
        }
        let url = self.sign_in_url.text.trim().to_string();
        let waker = self.waker.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.signing_in = true;
        self.sign_in_error = None;
        self.sign_in_done = Some(rx);
        let spawned = std::thread::Builder::new()
            .name("crucible-playbooks-sign-in".into())
            .spawn(move || {
                let _ = tx.send(signin::sign_in(&url));
                if let Some(waker) = waker {
                    waker();
                }
            });
        if let Err(e) = spawned {
            self.signing_in = false;
            self.sign_in_done = None;
            self.sign_in_error = Some(format!("starting the sign-in: {e}"));
        }
    }

    /// Take a finished sign-in's answer; a key now in crux's config means connecting again.
    fn finish_sign_in(&mut self) -> bool {
        let Some(answer) = self.sign_in_done.as_ref().and_then(|rx| rx.try_recv().ok()) else {
            return false;
        };
        self.sign_in_done = None;
        self.signing_in = false;
        match answer {
            Ok(()) => self.live_client.reconnect(),
            Err(why) => self.sign_in_error = Some(why),
        }
        true
    }

    /// Open what `link` names: a playbook, one of its launches with a task open, or a local pack.
    pub fn follow(&mut self, link: &Link) {
        self.settings_open = false;
        match link {
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
                    self.expanded = Some(task.clone());
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
        self.sync_live();
    }

    fn forget_run_view(&mut self) {
        self.expanded = None;
        self.inspect = inspect::Inspect::default();
        self.unfolded.clear();
        self.compare_to = None;
        self.log_task = None;
        self.log_folds.clear();
        self.pack_file = None;
        self.pack_source = None;
        self.draft_note = None;
        self.handoff_note = None;
    }

    /// Selects instance `key` of fan-out `task` from the graph and shows its output in Data.
    pub fn pick_instance(&mut self, task: &str, key: &str) {
        self.expanded = Some(task.to_string());
        self.inspect.from = None;
        self.inspect.pick = Some(inspect::Pick::Instance(key.to_string()));
        self.show_pane(PaneKind::Data);
    }

    pub fn toggle_unfolded(&mut self, task: &str) {
        if !self.unfolded.remove(task) {
            self.unfolded.insert(task.to_string());
        }
    }

    /// Selects `task` from the graph, with `from` when its edge from that dependency was picked:
    /// the Data pane shows it when in front, else the edge opens Data and a node opens Tasks.
    pub fn pick_task(&mut self, task: &str, from: Option<&str>) {
        self.expanded = Some(task.to_string());
        self.inspect.from = from.map(ToString::to_string);
        self.inspect.pick = None;
        if !self.workspace().showing(PaneKind::Data) {
            self.show_pane(if from.is_some() {
                PaneKind::Data
            } else {
                PaneKind::Tasks
            });
        }
    }

    pub fn toggle_task(&mut self, name: &str) {
        self.expanded = match self.expanded.take() {
            Some(open) if open == name => None,
            _ => Some(name.to_string()),
        };
    }

    /// Show only `task`'s lines, its block opened, or every line again.
    pub fn filter_log(&mut self, task: Option<&str>) {
        if let Some(task) = task {
            self.log_folds.insert(task.to_string(), true);
            if let Some((parent, _)) = task.split_once('[') {
                self.log_folds.insert(parent.to_string(), true);
            }
        }
        self.log_task = task.map(ToString::to_string);
    }

    pub fn set_log_font(&mut self, font: LogFont) {
        self.settings.set_log_font(font);
    }

    pub fn toggle_settings(&mut self) {
        self.settings_open = !self.settings_open;
    }

    pub fn switch(&mut self, backend: Backend) {
        if backend == self.backend {
            return;
        }
        self.backend = backend;
        self.selection = None;
        self.forget_run_view();
        self.reconcile();
        self.sync_live();
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

fn init() -> App {
    let repo = repo_from_args().unwrap_or_else(model::default_repo);
    let settings = SETTINGS.get().cloned().unwrap_or_else(Settings::load);
    let (keymap, keys_refused) = keymap::Keymap::new(&settings.keys);
    App {
        repo: repo.display().to_string(),
        local: Client::spawn(repo),
        local_seen: 0,
        live_client: live::Client::spawn(),
        live_seen: 0,
        lsp: lsp::Lsp::spawn(),
        lsp_seen: 0,
        checks: lsp::View::default(),
        lsp_sent: None,
        editor_error: None,
        live_sent: None,
        compare_sent: None,
        compare_to: None,
        live_base: None,
        backend: Backend::Controller,
        packs: Vec::new(),
        playbook_search: TextEdit::new(""),
        owner_filter: OwnerFilter::All,
        origin_filter: OriginFilter::All,
        landed_filter: LandedFilter::Any,
        live: LiveView::Loading,
        live_detail: None,
        selection: None,
        log_search: TextEdit::new(""),
        log_view: LogView::All,
        log_source: LogSource::Transcript,
        log_chatter: false,
        log_task: None,
        log_mode: None,
        log_folds: HashMap::new(),
        expanded: None,
        inspect: inspect::Inspect::default(),
        unfolded: HashSet::new(),
        keymap,
        keys_refused,
        palette: None,
        pane_focus: None,
        recorders: HashMap::new(),
        pack_file: None,
        layout: settings.layout.clone(),
        shown: Vec::new(),
        pane_drag: None,
        resizing: None,
        geometry: Geometry::default(),
        logo_clicks: 0,
        logo_screen: [0.0; 2],
        notified: 0,
        handoff_note: None,
        sign_in_url: TextEdit::new(
            crux::config::ConnectArgs::from_env()
                .resolve()
                .map(|config| config.url)
                .unwrap_or_default(),
        ),
        signing_in: false,
        sign_in_error: None,
        sign_in_done: None,
        waker: None,
        working: None,
        working_pack: None,
        working_edited: false,
        working_read: None,
        pack_source: None,
        draft_note: None,
        draft_busy: false,
        draft_after: 0,
        registered: Vec::new(),
        launcher: None,
        replies: 0,
        scratch: TextEdit::new(""),
        settings,
        settings_open: false,
        error: None,
    }
}

fn setup(app: &mut App, waker: Waker) {
    app.waker = Some(waker.clone());
    app.local.set_waker(waker.clone());
    app.live_client.set_waker(waker.clone());
    app.lsp.set_waker(waker);
}

/// Nothing here touches a file, a process or the network: the workers own all of that and wake the
/// loop once per finished snapshot.
fn poll(app: &mut App, _clock: f64, _tx: &mut Tx) -> bool {
    let mut changed = false;
    if let Some((version, snapshot)) = app.local.take(app.local_seen) {
        app.local_seen = version;
        app.apply_local(snapshot);
        changed = true;
    }
    if let Some((version, snapshot)) = app.live_client.take(app.live_seen) {
        app.live_seen = version;
        app.apply_live(snapshot);
        changed = true;
    }
    app.sync_live();
    changed |= app.refresh_working();
    changed |= app.finish_sign_in();
    if let Some((version, view)) = app.lsp.take(app.lsp_seen) {
        app.lsp_seen = version;
        app.checks = view;
        changed = true;
    }
    app.sync_checks();
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

fn main() {
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
            // `CRUCIBLE_TAB` shows that pane (tasks, timeline, graph, pack, launch or log).
            ("local", |app: &mut App| {
                app.backend = Backend::Local;
                app.select(Selection::Local("playbook".into()));
                scene_tab(app);
            }),
            ("settings", |app: &mut App| {
                scene_tab(app);
                app.settings_open = true;
            }),
            // `CRUCIBLE_LIVE_SCENE=<playbook id>` opens that live playbook headlessly, and
            // `CRUCIBLE_LINK=<crucible-playbooks:// link>` follows a link as the system would.
            ("live", |app: &mut App| {
                app.backend = Backend::Controller;
                scene_tab(app);
                if let Ok(id) = std::env::var("CRUCIBLE_LIVE_SCENE") {
                    app.select(Selection::playbook(&id));
                }
                if let Some(link) = std::env::var("CRUCIBLE_LINK")
                    .ok()
                    .and_then(|url| Link::parse(&url))
                {
                    app.follow(&link);
                }
            }),
        ],
        min_size: Some((900.0, 560.0)),
        ..AppSpec::new("Crucible Playbooks", (1120.0, 720.0), init, ui::view)
    };
    #[cfg(feature = "hot")]
    rune::run(spec);
    #[cfg(not(feature = "hot"))]
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
    use crate::model::default_repo;

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
