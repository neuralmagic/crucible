//! Keeping the app in step with its workers: local packs, the live controller, revisions, and
//! the switch between backends.

use crate::live::LiveView;
use crate::model::{Snapshot, Verdict};
use crate::workspace::PaneKind;
use crate::{App, Backend, LOCAL_PACKS, Selection, live, revisions};

impl App {
    pub(crate) fn apply_local(&mut self, snapshot: Snapshot) {
        self.packs = snapshot.packs;
        self.local_error = snapshot.error;
        self.local_report = snapshot.report;
        self.reconcile();
    }

    pub(crate) fn apply_live(&mut self, snapshot: live::Snapshot) {
        self.remote.live = snapshot.view;
        self.remote.live_detail = snapshot.detail;
        self.remote.live_base = snapshot.base;
        self.remote.draft_pack = snapshot.draft;
        self.remote.registered = snapshot.registered;
        self.remote.shareable = snapshot.shareable;
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
            (Backend::Controller, Some(Selection::Live { playbook, .. })) => {
                match &*self.remote.live {
                    LiveView::Ready { .. } => {
                        self.remote.live.launches_of(playbook).next().is_some()
                            || self.remote.live.draft(playbook).is_some()
                    }
                    LiveView::Loading | LiveView::Unavailable(_) => true,
                }
            }
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
                .visible_rows()
                .first()
                .map(|row| Selection::playbook(row.id())),
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
    pub(crate) fn sync_live(&mut self) {
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
        let draft = self
            .selected_draft()
            .filter(|draft| draft.version > 0)
            .map(|draft| (draft.id.clone(), draft.version));
        if draft != self.remote.draft_sent {
            self.remote.live_client.draft(draft.clone());
            self.remote.draft_sent = draft;
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
}
