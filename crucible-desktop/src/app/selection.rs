//! What is selected and shown: the pack or launch, its tasks, log and files, and the links that
//! lead to them.

use crate::filter::LaunchFilter;
use crate::link::Link;
use crate::live::{LiveDetail, LivePlaybook};
use crate::logline::LogLine;
use crate::model::PackView;
use crate::pack::{PackFile, PackFiles};
use crate::session::TaskView;
use crate::settings::LogFont;
use crate::workspace::PaneKind;
use crate::{App, Backend, GraphSource, Page, RunView, Selection, inspect, syntax};

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
    pub(crate) fn sync_checks(&mut self) {
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

    pub(crate) fn forget_run_view(&mut self) {
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
}
