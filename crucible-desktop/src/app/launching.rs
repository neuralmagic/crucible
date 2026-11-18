//! Launching: the launch form, sending it, and announcing the launches that land.

use crate::live::{LivePlaybook, LiveView, Target};
use crate::model::Verdict;
use crate::{
    App, Backend, Launcher, MAX_TIME, Selection, Stage, admin, form, live, notify, runs_on, studio,
};
use rune::TextEdit;

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
            launcher.schedule = Some(admin::forms::ScheduleForm::editing(schedule));
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
