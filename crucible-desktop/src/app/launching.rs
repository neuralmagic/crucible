//! Launching: the launch form, sending it, and announcing the launches that land.

use crate::link::Link;
use crate::live::{LivePlaybook, LiveView, Target};
use crate::model::Verdict;
use crate::workspace::PaneKind;
use crate::{
    App, Backend, Launcher, MAX_TIME, Selection, Stage, admin, form, live, notify, runs_on, studio,
};
use rune::TextEdit;

/// What to do about a rerun a note's button asked for.
#[derive(Debug, PartialEq, Eq)]
enum Due {
    Wait,
    Open,
    Drop,
}

/// Whether to open the rerun form asked for on a launch: it must still be the one `selected`,
/// and once it was `read` it either is `rerunnable` or never will be.
fn rerun_due(selected: bool, read: bool, rerunnable: bool) -> Due {
    match (selected, read, rerunnable) {
        (false, _, _) => Due::Drop,
        (true, _, true) => Due::Open,
        (true, true, false) => Due::Drop,
        (true, false, false) => Due::Wait,
    }
}

impl App {
    /// Announces launches by the signed-in user that finished since the last snapshot.
    pub(crate) fn announce(&mut self, finished: &[live::Finished]) {
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
                notifier.post(&notify::note(done, self.views_on()));
            }
        }
    }

    /// Opens the launch a note's button is on and does what the button says.
    pub(crate) fn press_run_button(&mut self, url: &str, action: notify::RunAction) {
        let Some(link @ Link::Launch { .. }) = Link::parse(url) else {
            return;
        };
        self.follow(&link);
        match action {
            notify::RunAction::Log => self.show_pane(PaneKind::Log),
            notify::RunAction::Report => self.show_pane(PaneKind::Report),
            notify::RunAction::Rerun => {
                if let Link::Launch { key, .. } = link {
                    self.rerun_wanted = Some(key);
                }
            }
        }
        notify::bring_to_front();
    }

    /// Opens the rerun form a note's button asked for once the launch it is on has been read,
    /// and forgets the ask when another run shows or this one cannot be rerun. True when the
    /// form opened.
    pub(crate) fn rerun_when_read(&mut self) -> bool {
        let Some(wanted) = self.rerun_wanted.as_deref() else {
            return false;
        };
        let selected = self.page == crate::Page::Runs
            && matches!(
                &self.selection,
                Some(Selection::Live { launch: Some(key), .. }) if key == wanted
            );
        let read = self
            .selected_detail()
            .is_some_and(|detail| detail.launch.is_some());
        match rerun_due(selected, read, self.rerunnable().is_some()) {
            Due::Wait => false,
            Due::Drop => {
                self.rerun_wanted = None;
                false
            }
            Due::Open => {
                self.rerun_wanted = None;
                self.open_rerun(Vec::new());
                true
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
            launcher.schedule = Some(admin::forms::ScheduleForm::editing(schedule));
        }
    }

    /// Sends the form, or marks what has to change first.
    pub fn submit_launch(&mut self) {
        let Some(launcher) = &mut self.remote.launcher else {
            return;
        };
        let (params, uploads) = match &launcher.rerun {
            Some(rerun) => (Ok(rerun.params.clone()), Ok(Vec::new())),
            None => (
                form::params(&launcher.fields, &launcher.answers),
                form::uploads(&launcher.fields, &launcher.answers),
            ),
        };
        let ceilings = form::ceilings(
            &launcher.cost.text,
            &launcher.time.text,
            launcher.caps.0,
            &launcher.caps.1,
        );
        launcher.problems = params.as_ref().err().cloned().unwrap_or_default();
        launcher
            .problems
            .extend(uploads.as_ref().err().cloned().unwrap_or_default());
        launcher.problems.sort_by_key(|(at, _)| *at);
        launcher.refusal = ceilings.as_ref().err().cloned();
        let (Ok(params), Ok(uploads), Ok((max_cost, max_time))) = (params, uploads, ceilings)
        else {
            return;
        };
        if let Some(why) = launcher.rerun.as_ref().and_then(studio::Rerun::blocked) {
            launcher.refusal = Some(why);
            return;
        }
        let runs_on = launcher.runs_on.runs_on();
        if let Some(schedule) = &launcher.schedule {
            if !uploads.is_empty() {
                launcher.refusal = Some("Files cannot be scheduled".to_string());
                return;
            }
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
            .launch(launcher.target.clone(), body, uploads);
    }

    pub fn pick_param_file(&mut self, i: usize, path: &std::path::Path) {
        let Some(launcher) = &mut self.remote.launcher else {
            return;
        };
        let Some(field) = launcher.fields.get(i) else {
            return;
        };
        let form::Kind::File(spec) = &field.kind else {
            return;
        };
        let refused = form::upload(&field.name, spec, path).err();
        if let Some(answer) = launcher.answers.get_mut(i) {
            *answer = form::Answer::typed(path.display().to_string());
        }
        launcher.problems.retain(|(at, _)| *at != i);
        if let Some(why) = refused {
            launcher.problems.push((i, why));
            launcher.problems.sort_by_key(|(at, _)| *at);
        }
    }

    /// The newest launch of `playbook` the list shows, to prefill a form from.
    pub(crate) fn newest_launch(&self, playbook: &str) -> Option<&LivePlaybook> {
        self.remote
            .live
            .launches_of(playbook)
            .max_by_key(|launch| launch.landed)
    }

    /// Fills an open form from the schema the worker read, or lands the launch it sent.
    pub(crate) fn answer_launcher(
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
                        field.answer(previous.as_ref().and_then(|params| params.get(&field.name)))
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

#[cfg(test)]
mod tests {
    use crate::app::launching::{Due, rerun_due};

    #[test]
    fn a_rerun_asked_from_a_note_opens_once_its_run_is_read_and_is_dropped_otherwise() {
        assert_eq!(rerun_due(true, false, false), Due::Wait, "still reading");
        assert_eq!(rerun_due(true, true, true), Due::Open);
        assert_eq!(rerun_due(true, false, true), Due::Open);
        assert_eq!(
            rerun_due(true, true, false),
            Due::Drop,
            "read, and it cannot be rerun"
        );
        for (read, rerunnable) in [(false, false), (true, false), (true, true)] {
            assert_eq!(
                rerun_due(false, read, rerunnable),
                Due::Drop,
                "another run shows"
            );
        }
    }
}
