//! The Report pane and the runs page's view: what the workers are asked for, what they
//! answered, where a view's links lead, and the captured files and comparisons beside them.

use crate::link::Link;
use crate::live::{LivePlaybook, LiveView};
use crate::workspace::PaneKind;
use crate::{App, Backend, Open, Page, ReportAsk, RunsTab, Selection, captured, live, report};
use rune::Tx;

impl App {
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

    /// Whether the selected live launch has stopped, so its captured files' copies on disk can
    /// stand in for them.
    pub(crate) fn selected_settled(&self) -> bool {
        self.selected_live()
            .is_some_and(|launch| !launch.status.is_running())
    }

    /// Asks the worker behind the selection for the report the Report pane shows, and the live
    /// worker for the runs view the runs page shows.
    pub(crate) fn sync_report(&mut self) {
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

    pub(crate) fn answer_report(
        &mut self,
        built: Option<std::sync::Arc<report::Built<report::Wanted>>>,
    ) {
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
    pub(crate) fn tick_reports(&mut self, clock: f64) -> Option<f64> {
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
    pub(crate) fn wanted_file(&self) -> Option<(String, String, u64)> {
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

    pub(crate) fn answer_file(&mut self, file: Option<captured::Opened>) {
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
}
