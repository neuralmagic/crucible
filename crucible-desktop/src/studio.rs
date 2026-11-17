//! Fixing a pack and running it again from the tracker: a draft's studio beside the run that
//! showed the problem, and a rerun of that run that reuses what already passed.

use std::collections::BTreeMap;

use crux::dto::RerunPreview;
use rune::{TextEdit, Tx};
use serde_json::{Map, Value};

use crate::live::{self, StudioAsk, StudioDone, StudioReply, Target};
use crate::model::Verdict;
use crate::{App, PackSource};

/// How much of a failed run's console a re-draft's intent carries.
const FAILURE_TAIL_LINES: usize = 30;

/// A draft open for editing: its buffers as the app holds them, the version they came from, and
/// the drafting turn's last answer. The buffers are the master while the studio is open; every
/// sync rewrites them — and, on the desktop, the working copy the watcher reads — from the
/// controller's reply.
pub struct Studio {
    pub draft: String,
    /// The registered playbook the draft was created from, when it was.
    pub template: Option<String>,
    /// The version the buffers came from; the base the next save carries.
    pub base: Option<i64>,
    pub files: BTreeMap<String, TextEdit>,
    /// Each file as it was last synced, so an edit tells without asking the controller.
    pub saved: BTreeMap<String, String>,
    /// The file the editor shows.
    pub active: Option<String>,
    /// The compile of the version the buffers came from.
    pub compile: Option<crux::dto::DraftCompile>,
    /// The same compile folded the way a run's first event leaves it, for the graph.
    pub compiled: Option<crate::preview::Preview>,
    /// What the last sync said, shown over the buffers.
    pub note: Option<Result<String, String>>,
    /// A save another writer landed first: the refusal, and the version to reload onto.
    pub stale: Option<crux::dto::StaleBase>,
    /// The intent the drafting turn runs on.
    pub intent: TextEdit,
    /// The drafting turn's last answer, for the report line.
    pub report: Option<crux::dto::GeneratedDraft>,
    /// A generate is on its way or a turn is running; the progress poll runs while it is.
    pub generating: bool,
    /// The clock reading the generate started at, for the elapsed label.
    pub started: Option<f64>,
    /// When the progress poll next fires, on the app clock.
    pub next_poll: Option<f64>,
    /// The drafting turn's progress as last read.
    pub progress: crux::dto::DraftProgress,
    /// The file list's new-file field.
    pub adding: TextEdit,
}

impl Studio {
    fn open(draft: String, template: Option<String>) -> Studio {
        Studio {
            draft,
            template,
            base: None,
            files: BTreeMap::new(),
            saved: BTreeMap::new(),
            active: None,
            compile: None,
            compiled: None,
            note: None,
            stale: None,
            intent: TextEdit::new(""),
            report: None,
            generating: false,
            started: None,
            next_poll: None,
            progress: Default::default(),
            adding: TextEdit::new(""),
        }
    }

    /// Whether the buffers hold an edit the controller has not seen.
    pub fn edited(&self) -> bool {
        self.files.len() != self.saved.len()
            || self
                .files
                .iter()
                .any(|(path, edit)| self.saved.get(path) != Some(&edit.text))
    }

    /// The buffers as a save carries them.
    pub fn buffers(&self) -> BTreeMap<String, String> {
        self.files
            .iter()
            .map(|(path, edit)| (path.clone(), edit.text.clone()))
            .collect()
    }

    /// Load `files` as the buffers at `version`, keeping the editor on a file that stayed.
    fn loaded(
        &mut self,
        version: i64,
        files: BTreeMap<String, String>,
        compile: crux::dto::DraftCompile,
    ) {
        self.base = Some(version);
        self.files = files
            .iter()
            .map(|(path, text)| (path.clone(), TextEdit::new(text)))
            .collect();
        self.saved = files;
        if !self
            .files
            .contains_key(self.active.as_deref().unwrap_or_default())
        {
            self.active = self.files.keys().next().cloned();
        }
        self.compiled = Some(crate::preview::from_compile(&compile));
        self.compile = Some(compile);
        self.stale = None;
    }
}

/// A launch form's request to rerun a finished run from some of its tasks.
#[derive(Clone, Debug, PartialEq)]
pub struct Rerun {
    pub run: String,
    /// The tasks to dispatch again; empty reruns what did not pass.
    pub from: Vec<String>,
    /// The parent launch's params, sent unchanged.
    pub params: Value,
    pub preview: Option<Result<RerunPreview, String>>,
    /// Only a preview newer than this answers the form.
    asked: u64,
}

impl Rerun {
    /// Why the rerun cannot be sent yet.
    pub fn blocked(&self) -> Option<String> {
        match &self.preview {
            None => Some("Still reading what the rerun would reuse".to_string()),
            Some(Err(error)) => Some(error.clone()),
            Some(Ok(preview)) if !preview.changed.is_empty() => Some(format!(
                "Changed since the run: {}",
                preview.changed.join(", ")
            )),
            Some(Ok(_)) => None,
        }
    }
}

/// The body of a launch: a draft's params go as strings, and a rerun names its parent.
pub fn launch_body(
    target: &Target,
    params: Value,
    max_cost: f64,
    max_time: &str,
    digest: Option<&str>,
    rerun: Option<&Rerun>,
) -> Value {
    let params = match (target, params) {
        (Target::Draft(_), Value::Object(object)) => Value::Object(
            object
                .into_iter()
                .map(|(name, value)| {
                    let text = match value {
                        Value::String(text) => text,
                        other => other.to_string(),
                    };
                    (name, Value::String(text))
                })
                .collect::<Map<_, _>>(),
        ),
        (_, params) => params,
    };
    let mut body = serde_json::json!({
        "params": params,
        "max_cost": max_cost,
        "max_time": max_time,
    });
    if let (Target::Draft(_), Some(digest)) = (target, digest) {
        body["schema_digest"] = Value::String(digest.to_string());
    }
    if let Some(rerun) = rerun {
        body["rerun"] = serde_json::json!({ "of": rerun.run, "from": rerun.from });
    }
    body
}

/// The draft id a user's edits of registered playbook `playbook` go to.
pub fn draft_id(playbook: &str, user: &str) -> String {
    let user: String = user
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let user = user.trim_matches('-');
    if user.is_empty() {
        format!("{playbook}-draft")
    } else {
        format!("{playbook}-{user}")
    }
}

/// The intent a re-draft from a failed test-fire runs on: the error the run printed, capped to
/// its tail so a long console cannot swallow the instruction around it.
fn redraft_intent(failure: &str) -> String {
    let lines: Vec<&str> = failure.trim_end().lines().collect();
    let tail = lines[lines.len().saturating_sub(FAILURE_TAIL_LINES)..]
        .join("\n")
        .trim()
        .to_string();
    let body = if tail.is_empty() {
        "The run failed with no error output held on this machine."
    } else {
        &tail
    };
    format!(
        "The last test-fire of this pack failed. This is what the run printed, oldest line first:\n\n{body}\n\nWrite the next version of the pack so this failure cannot happen again and the experiment completes."
    )
}

impl App {
    fn user(&self) -> Option<&str> {
        match &*self.remote.live {
            live::LiveView::Ready { identity, .. } if !identity.user.is_empty() => {
                Some(&identity.user)
            }
            _ => None,
        }
    }

    /// Whether the shown launch ran a draft's save.
    fn draft_run(&self) -> bool {
        self.selected_detail()
            .and_then(|detail| detail.launch.as_ref())
            .is_some_and(|record| record.launch.draft_version.is_some())
    }

    /// The draft the shown run's pack is edited in, and the registered playbook a new one
    /// starts from: a draft picked from the rail, the draft itself for a draft run, else the
    /// user's own draft of the playbook.
    pub fn draft_target(&self) -> Option<(String, Option<String>)> {
        if let Some(draft) = self.rail_draft() {
            return Some((draft.to_string(), None));
        }
        let launch = self.selected_live()?;
        if self.draft_run() {
            return Some((launch.id.clone(), None));
        }
        if !self.remote.registered.iter().any(|id| id == &launch.id) {
            return None;
        }
        Some((draft_id(&launch.id, self.user()?), Some(launch.id.clone())))
    }

    /// The selected draft as the rail lists it: one the controller holds with no launch of its
    /// own to show.
    pub(crate) fn rail_draft(&self) -> Option<&str> {
        let Some(crate::Selection::Live { playbook, .. }) = &self.selection else {
            return None;
        };
        let known = self.remote.drafts.iter().any(|draft| &draft.id == playbook);
        let shown = match &*self.remote.live {
            live::LiveView::Ready { .. } => self.remote.live.launches_of(playbook).next().is_some(),
            live::LiveView::Loading | live::LiveView::Unavailable(_) => true,
        };
        (known && !shown).then_some(playbook.as_str())
    }

    /// Whether the Pack tab shows the studio's buffers rather than the run's own pack.
    pub fn showing_working(&self) -> bool {
        let open = self.remote.studio.is_some() || self.remote.working_pack.is_some();
        open && match self.run.pack_source {
            Some(source) => source == PackSource::Working,
            None => self.draft_run() || self.selected_live().is_none(),
        }
    }

    /// Keep the working copy and the studio in step with the shown run, and with what the
    /// watcher last read.
    pub fn refresh_working(&mut self) -> bool {
        let target = self.draft_target();
        let draft = target.as_ref().map(|(draft, _)| draft.clone());
        let mut changed = false;
        if self.remote.working_sent != draft {
            self.remote.working_watch.watch(draft.as_deref());
            self.remote.working_sent = draft;
            self.remote.working = None;
            self.remote.working_pack = None;
            self.remote.working_preview = None;
            self.remote.working_edited = false;
            self.remote.studio = None;
            changed = true;
        }
        // A draft picked from the rail has no run to show, and a run of the draft itself opens
        // its studio on its own.
        let solo = self.selected_live().is_none() || self.draft_run();
        if let Some((draft, template)) = target.as_ref()
            && solo
            && self
                .remote
                .studio
                .as_ref()
                .is_none_or(|studio| &studio.draft != draft)
        {
            self.open_studio(draft.clone(), template.clone());
            changed = true;
        }
        let Some((version, copy)) = self.remote.working_watch.take(self.remote.working_seen) else {
            return changed;
        };
        self.remote.working_seen = version;
        match copy {
            Some(copy) if Some(&copy.working.draft) == self.remote.working_sent.as_ref() => {
                self.remote.working = Some(copy.working.clone());
                self.remote.working_pack = Some(copy.pack.clone());
                self.remote.working_preview = Some(copy.preview.clone());
                self.remote.working_edited = copy.edited;
            }
            Some(_) => return changed,
            None => {
                self.remote.working = None;
                self.remote.working_pack = None;
                self.remote.working_preview = None;
                self.remote.working_edited = false;
            }
        }
        true
    }

    /// Keep working copies in the folder typed in Settings from now on.
    pub fn set_drafts_dir(&mut self) {
        let typed = self.drafts_dir.text.clone();
        self.drafts_error = self.settings.set_drafts_dir(&typed).err();
        if self.drafts_error.is_none() {
            self.remote
                .set_drafts(&self.net, self.settings.drafts_root(), self.waker.as_ref());
        }
    }

    /// Open `draft`'s studio, creating the draft from `template` when it is new. Any buffers the
    /// studio held are dropped: the pull that follows reloads them from the newest save.
    pub fn open_studio(&mut self, draft: String, template: Option<String>) {
        self.remote.studio = Some(Studio::open(draft.clone(), template));
        self.remote.draft_busy = true;
        self.remote.draft_after = self.remote.replies;
        let template = self.remote.studio.as_ref().and_then(|s| s.template.clone());
        self.remote.live_client.pull(&draft, template.as_deref());
    }

    /// Pull the shown run's draft into its studio, creating the draft when it is new.
    pub fn pull_draft(&mut self) {
        let Some((draft, template)) = self.draft_target() else {
            return;
        };
        self.open_studio(draft, template);
    }

    /// Save the studio's buffers as its draft's next version.
    pub fn save_studio(&mut self) {
        let Some(studio) = self.remote.studio.as_ref() else {
            return;
        };
        let Some(base) = studio.base else {
            return;
        };
        let (draft, files) = (studio.draft.clone(), studio.buffers());
        self.remote.draft_busy = true;
        self.remote.draft_after = self.remote.replies;
        self.remote.live_client.save_studio(&draft, &files, base);
    }

    /// Hand the intent to the draft's drafting turn.
    pub fn generate_studio(&mut self) {
        let Some(studio) = self.remote.studio.as_mut() else {
            return;
        };
        if studio.edited() || studio.generating || studio.intent.text.trim().is_empty() {
            return;
        }
        studio.generating = true;
        studio.started = None;
        studio.next_poll = None;
        let (draft, intent) = (studio.draft.clone(), studio.intent.text.trim().to_string());
        self.remote.live_client.generate(&draft, &intent);
    }

    /// Feed the shown failed run of the studio's draft back into a drafting turn: the error
    /// becomes the intent, and the turn runs on it at once.
    pub fn redraft_studio(&mut self) {
        let Some(failure) = self.draft_failure() else {
            return;
        };
        let intent = redraft_intent(&failure);
        if let Some(studio) = self.remote.studio.as_mut() {
            studio.intent = TextEdit::new(&intent);
        }
        self.generate_studio();
    }

    /// Drop `path` from the studio's buffers; the editor moves to what is left.
    pub fn remove_studio_file(&mut self, path: &str) {
        let Some(studio) = self.remote.studio.as_mut() else {
            return;
        };
        if studio.files.remove(path).is_some() && studio.active.as_deref() == Some(path) {
            studio.active = studio.files.keys().next().cloned();
        }
    }

    /// Add the file the list's field names, and open it. A name that is blank or already held
    /// only opens what it names.
    pub fn add_studio_file(&mut self) {
        let Some(studio) = self.remote.studio.as_mut() else {
            return;
        };
        let asked = studio.adding.text.trim().to_string();
        studio.adding = TextEdit::new("");
        if asked.is_empty() {
            return;
        }
        if !studio.files.contains_key(&asked) {
            studio.files.insert(asked.clone(), TextEdit::new(""));
        }
        studio.active = Some(asked);
    }

    /// The failure a re-draft of the studio's draft would run on: the shown run's dispatch
    /// refusal or console, when that run is the draft's own and did not pass.
    pub(crate) fn draft_failure(&self) -> Option<String> {
        let studio = self.remote.studio.as_ref()?;
        let launch = self
            .selected_live()
            .filter(|launch| launch.id == studio.draft)?;
        if launch.status.is_running() {
            return None;
        }
        let detail = self.selected_detail()?;
        if detail.verdict == Verdict::Valid {
            return None;
        }
        if let Some(failure) = detail
            .launch
            .as_ref()
            .and_then(|record| record.dispatch.failure.clone())
        {
            return Some(failure);
        }
        let mut tail: Vec<String> = detail
            .log
            .iter()
            .rev()
            .take(FAILURE_TAIL_LINES)
            .map(|line| line.raw.clone())
            .collect();
        tail.reverse();
        (!tail.is_empty()).then(|| tail.join("\n"))
    }

    /// Take a studio reply: load pulled and generated buffers, note a save, or hold a refusal.
    pub(crate) fn answer_studio(&mut self, done: Option<StudioDone>) {
        let Some(done) = done else {
            return;
        };
        self.remote.replies = self.remote.replies.max(done.seq);
        let busy = self.remote.draft_busy && done.seq > self.remote.draft_after;
        let generating = self.remote.studio.as_ref().is_some_and(|s| s.generating);
        let answers = match done.ask {
            StudioAsk::Pull | StudioAsk::Save => busy,
            StudioAsk::Generate => generating,
        };
        if !answers {
            return;
        }
        if done.ask != StudioAsk::Generate {
            self.remote.draft_busy = false;
        }
        let draft = done.draft.clone();
        let Some(studio) = self.remote.studio.as_mut().filter(|s| s.draft == draft) else {
            return;
        };
        let mut reread = false;
        let mut poll = None;
        match done.result {
            Ok(StudioReply::Pulled {
                version,
                files,
                compile,
                note,
            }) => {
                if let Some(compile) = *compile {
                    studio.loaded(version, files, compile);
                }
                studio.note = note.map(Ok);
                studio.report = None;
                reread = true;
                poll = Some(studio.draft.clone());
                self.run.pack_source = Some(PackSource::Working);
                self.run.pack_file = None;
            }
            Ok(StudioReply::Saved(compile)) => {
                studio.saved = studio.buffers();
                studio.base = Some(compile.version);
                studio.compile = Some((*compile).clone());
                studio.compiled = Some(crate::preview::from_compile(&compile));
                studio.stale = None;
                let errors: Vec<&str> = compile
                    .diagnostics
                    .iter()
                    .map(|found| found.message.as_str())
                    .collect();
                studio.note = Some(if compile.schema_digest.is_none() || !errors.is_empty() {
                    Err(format!(
                        "Saved version {}, which does not compile: {}",
                        compile.version,
                        errors.join("; ")
                    ))
                } else {
                    Ok(format!("Saved version {}", compile.version))
                });
                reread = true;
                self.run.pack_source = Some(PackSource::Working);
            }
            Ok(StudioReply::Generated { files, result }) => {
                studio.loaded(result.compile.version, files, result.compile.clone());
                studio.report = Some((*result).clone());
                studio.generating = false;
                studio.started = None;
                studio.note = Some(Ok(format!(
                    "v{} · {} rounds · ${:.2} · check {}",
                    result.compile.version,
                    result.rounds,
                    result.cost_usd,
                    if result.ok { "passed" } else { "failed" }
                )));
                reread = true;
                poll = Some(studio.draft.clone());
                self.run.pack_source = Some(PackSource::Working);
                self.run.pack_file = None;
            }
            Ok(StudioReply::Stale(stale)) => {
                studio.stale = Some(stale.clone());
                studio.note = Some(Err(stale.error));
                studio.generating = false;
            }
            Err(error) => {
                studio.note = Some(Err(error));
                studio.generating = false;
            }
        }
        if reread {
            self.remote.working_watch.reread();
        }
        if let Some(draft) = poll {
            self.remote.live_client.progress(&draft);
        }
    }

    /// Take the drafting turn's progress as last read: follow a turn that is running, including
    /// one this window did not start.
    pub(crate) fn apply_progress(&mut self, found: Option<live::Progressed>) {
        let Some(found) = found else {
            return;
        };
        self.remote.replies = self.remote.replies.max(found.seq);
        let Ok(progress) = found.result else {
            return;
        };
        let Some(studio) = self
            .remote
            .studio
            .as_mut()
            .filter(|s| s.draft == found.draft)
        else {
            return;
        };
        studio.progress = progress;
        if studio.progress.running && !studio.generating {
            studio.generating = true;
            studio.started = None;
            if let Some(intent) = studio.progress.intent.clone().filter(|i| !i.is_empty()) {
                studio.intent = TextEdit::new(&intent);
            }
        }
    }

    /// Fire the studio's progress poll on its cadence, and keep the frame loop awake while a
    /// drafting turn runs so the elapsed label ticks. Returns whether the studio changed.
    pub(crate) fn tick_studio(&mut self, clock: f64, tx: &mut Tx) -> bool {
        let Some(studio) = self.remote.studio.as_mut().filter(|s| s.generating) else {
            return false;
        };
        if studio.started.is_none() {
            studio.started = Some(clock);
        }
        tx.wake_at(clock + 1.0);
        if studio.next_poll.is_none_or(|at| clock >= at) {
            studio.next_poll = Some(clock + 2.0);
            let draft = studio.draft.clone();
            self.remote.live_client.progress(&draft);
        }
        true
    }

    pub fn show_pack(&mut self, source: PackSource) {
        self.run.pack_source = Some(source);
        self.run.pack_file = None;
    }

    /// Launch the studio's draft.
    pub fn launch_working(&mut self) {
        if let Some(studio) = &self.remote.studio {
            let target = Target::Draft(studio.draft.clone());
            self.open_launcher(target);
        }
    }

    /// What a launch form for `playbook` starts: the registered playbook, or the draft the shown
    /// run came from.
    pub fn launch_target(&self, playbook: &str) -> Option<Target> {
        if self.remote.registered.iter().any(|id| id == playbook) {
            return Some(Target::Playbook(playbook.to_string()));
        }
        let shown = self.selected_live()?;
        (shown.id == playbook && self.draft_run()).then(|| Target::Draft(playbook.to_string()))
    }

    /// The shown run, when it has finished and can be rerun: what a rerun starts, its run id,
    /// and the params it was launched with.
    pub fn rerunnable(&self) -> Option<(Target, String, Value)> {
        let launch = self.selected_live()?;
        if launch.status.is_running() {
            return None;
        }
        let record = self.selected_detail()?.launch.as_ref()?;
        let run = record.runs.first()?.run_id.clone();
        let target = match record.launch.draft_version {
            Some(_) => Target::Draft(launch.id.clone()),
            None if self.remote.registered.iter().any(|id| id == &launch.id) => {
                Target::Playbook(launch.id.clone())
            }
            None => return None,
        };
        Some((target, run, record.launch.params.clone()))
    }

    /// Open a launch form that reruns the shown run from `from`.
    pub fn open_rerun(&mut self, from: Vec<String>) {
        let Some((target, run, params)) = self.rerunnable() else {
            return;
        };
        self.open_launcher(target);
        let asked = self.remote.replies;
        if let Some(launcher) = &mut self.remote.launcher {
            launcher.rerun = Some(Rerun {
                run: run.clone(),
                from: from.clone(),
                params,
                preview: None,
                asked,
            });
        }
        self.remote.live_client.preview_rerun(&run, from);
    }

    /// Add the tasks a preview found changed to the rerun, and ask again.
    pub fn widen_rerun(&mut self) {
        let asked = self.remote.replies;
        let Some(rerun) = self.remote.launcher.as_mut().and_then(|l| l.rerun.as_mut()) else {
            return;
        };
        let Some(Ok(preview)) = &rerun.preview else {
            return;
        };
        for task in &preview.changed {
            if !rerun.from.contains(task) {
                rerun.from.push(task.clone());
            }
        }
        rerun.preview = None;
        rerun.asked = asked;
        let (run, from) = (rerun.run.clone(), rerun.from.clone());
        self.remote.live_client.preview_rerun(&run, from);
    }

    pub(crate) fn answer_rerun(&mut self, reran: Option<live::Reran>) {
        let Some(rerun) = self.remote.launcher.as_mut().and_then(|l| l.rerun.as_mut()) else {
            return;
        };
        let Some(reran) =
            reran.filter(|r| r.seq > rerun.asked && r.run == rerun.run && r.from == rerun.from)
        else {
            return;
        };
        rerun.asked = reran.seq;
        rerun.preview = Some(reran.result);
        self.remote.replies = self.remote.replies.max(reran.seq);
    }

    /// The shown run written up for a coding agent, about `task` when one is named.
    pub fn handoff(&self, task: Option<&str>) -> Option<crate::handoff::Handoff> {
        use crate::handoff::{Fix, Handoff, LOG_LINES};
        use crate::session::TaskState;
        use crucible_contract::session::TaskStatus;

        let launch = self.selected_live()?;
        let detail = self.selected_detail()?;
        let record = detail.launch.as_ref()?;
        let (draft, template) = self.draft_target()?;
        let root = self.settings.drafts_root()?;
        let mine = self.remote.working.as_ref().filter(|w| w.draft == draft);
        let pulled = mine.is_some_and(|w| {
            !self.remote.working_edited && Some(w.base) == record.launch.draft_version
        });
        let dir = match mine {
            Some(working) if pulled => working.dir.clone(),
            Some(_) => crate::draft::dir(&root, &format!("{draft}-agent")),
            None => crate::draft::dir(&root, &draft),
        };
        let fix = match template {
            Some(playbook) => Fix::Registered { playbook, draft },
            None => Fix::Draft { draft, pulled },
        };
        let failed = detail
            .fold
            .tasks
            .iter()
            .flat_map(|t| {
                let own = (t.name.clone(), t.state(), t.result.as_ref().map(|r| r.note.clone()));
                let instances = t.instances.iter().map(|i| {
                    (
                        format!("{}[{}]", t.name, i.key),
                        i.state(),
                        i.result.as_ref().map(|r| r.note.clone()),
                    )
                });
                std::iter::once(own).chain(instances)
            })
            .filter(|(_, state, _)| {
                matches!(state, TaskState::Settled(status) if *status != TaskStatus::Pass)
            })
            .map(|(name, _, note)| (name, note.unwrap_or_default()))
            .collect();
        let mut log: Vec<String> = detail
            .log
            .iter()
            .filter(|line| task.is_none_or(|task| line.is_for(task)))
            .map(|line| line.raw.clone())
            .rev()
            .take(LOG_LINES)
            .collect();
        log.reverse();
        let pack_dir = self
            .shown_pack()
            .and_then(|pack| pack.disk.as_ref())
            .map(|d| d.dir().to_path_buf());
        let mut problems: Vec<String> = self
            .checks
            .problems
            .iter()
            .flat_map(|(path, found)| {
                let file = pack_dir
                    .as_deref()
                    .and_then(|dir| path.strip_prefix(dir).ok())
                    .unwrap_or(path)
                    .display()
                    .to_string();
                found.iter().map(move |p| {
                    format!(
                        "{file}:{}:{} {}: {}",
                        p.start.line + 1,
                        p.start.character + 1,
                        format!("{:?}", p.severity).to_lowercase(),
                        p.message
                    )
                })
            })
            .collect();
        problems.sort();
        Some(Handoff {
            playbook: launch.id.clone(),
            key: launch.key.clone(),
            run: record.runs.first().map(|run| run.run_id.clone()),
            tracker: self.link_here(task)?.url(),
            task: task.map(ToString::to_string),
            why: launch.why.clone(),
            failed,
            log,
            problems,
            fix,
            dir,
            max_cost: record.launch.max_cost,
            max_time: Some(record.launch.max_time.clone())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "30m".to_string()),
        })
    }

    /// Write the brief for the shown run, about `task` when one is named, and return its text
    /// for the clipboard.
    pub fn hand_off(&mut self, task: Option<&str>) -> Option<String> {
        let Some(handoff) = self.handoff(task) else {
            self.run.handoff_note = Some(Err("This run has no pack to fix".to_string()));
            return None;
        };
        let text = crate::handoff::brief(&handoff);
        self.run.handoff_note = Some(
            crate::handoff::root()
                .ok_or_else(|| "no home directory for briefs".to_string())
                .and_then(|root| {
                    crate::handoff::save(&root, &handoff.key, task, &text)
                        .map_err(|e| format!("writing the brief: {e}"))
                }),
        );
        Some(text)
    }

    /// Open `file` at `line` on the Git host the shown pack is pinned to.
    pub fn open_on_git(&mut self, file: &str, line: usize) {
        let Some(url) = self.shown_pack().and_then(|pack| pack.web_url(file, line)) else {
            return;
        };
        self.editor_error = crate::editor::open_url(&url).err();
    }
}

#[cfg(test)]
mod tests {
    use crux::dto::RerunPreview;
    use serde_json::json;

    use crate::live::Target;
    use crate::studio::{Rerun, Studio, draft_id, launch_body, redraft_intent};

    fn rerun(preview: Option<Result<RerunPreview, String>>) -> Rerun {
        Rerun {
            run: "run-7".into(),
            from: vec!["check".into()],
            params: json!({"repo": "a/b", "limit": 2}),
            preview,
            asked: 0,
        }
    }

    fn preview(changed: &[&str]) -> RerunPreview {
        RerunPreview {
            run_id: "run-7".into(),
            playbook: "triage".into(),
            draft_version: None,
            reused: Vec::new(),
            rerun: vec!["check".into()],
            reused_usd: 0.0,
            changed: changed.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn a_draft_launch_sends_its_params_as_strings_with_the_digest() {
        let body = launch_body(
            &Target::Draft("triage-me".into()),
            json!({"repo": "a/b", "limit": 2, "deep": true}),
            3.0,
            "20m",
            Some("sha256:cc"),
            None,
        );
        assert_eq!(
            body,
            json!({
                "params": {"repo": "a/b", "limit": "2", "deep": "true"},
                "max_cost": 3.0,
                "max_time": "20m",
                "schema_digest": "sha256:cc",
            })
        );
    }

    #[test]
    fn a_registered_rerun_keeps_typed_params_and_names_its_parent() {
        let ask = rerun(None);
        let body = launch_body(
            &Target::Playbook("triage".into()),
            ask.params.clone(),
            3.0,
            "20m",
            Some("ignored for a registered playbook"),
            Some(&ask),
        );
        assert_eq!(
            body,
            json!({
                "params": {"repo": "a/b", "limit": 2},
                "max_cost": 3.0,
                "max_time": "20m",
                "rerun": {"of": "run-7", "from": ["check"]},
            })
        );
    }

    #[test]
    fn a_rerun_waits_for_a_clean_preview() {
        assert!(rerun(None).blocked().is_some());
        assert_eq!(
            rerun(Some(Err("run run-7 has not finished".into()))).blocked(),
            Some("run run-7 has not finished".into())
        );
        assert_eq!(
            rerun(Some(Ok(preview(&["plan", "fetch"])))).blocked(),
            Some("Changed since the run: plan, fetch".into())
        );
        assert_eq!(rerun(Some(Ok(preview(&[])))).blocked(), None);
    }

    #[test]
    fn a_users_draft_of_a_playbook_is_a_slug() {
        assert_eq!(draft_id("triage", "weaton"), "triage-weaton");
        assert_eq!(draft_id("triage", "Will.Eaton@x"), "triage-will-eaton-x");
        assert_eq!(draft_id("triage", "@@"), "triage-draft");
    }

    #[test]
    fn a_redraft_carries_the_tail_of_the_failure_oldest_first() {
        let console: Vec<String> = (0..40).map(|n| format!("line {n}")).collect();
        let intent = redraft_intent(&console.join("\n"));
        assert!(
            intent.contains("line 10\n"),
            "the tail starts at the 30th line from the end"
        );
        assert!(!intent.contains("line 9\n"));
        assert!(intent.starts_with(
            "The last test-fire of this pack failed. This is what the run printed, oldest line first:"
        ));
        assert!(intent.ends_with(
            "Write the next version of the pack so this failure cannot happen again and the experiment completes."
        ));
    }

    #[test]
    fn a_redraft_of_a_silent_failure_says_so() {
        let intent = redraft_intent("   \n");
        assert!(intent.contains("The run failed with no error output held on this machine."));
    }

    fn compile(version: i64) -> crux::dto::DraftCompile {
        crux::dto::DraftCompile {
            version,
            saved_by: Some("weaton".to_string()),
            saved_at: "2026-11-12T10:00:00Z".to_string(),
            schema_digest: None,
            params_schema: None,
            graph: None,
            diagnostics: Vec::new(),
            dispatch: Default::default(),
        }
    }

    fn loaded_studio(files: &[(&str, &str)]) -> Studio {
        let mut studio = Studio::open("triage-me".to_string(), None);
        studio.loaded(
            3,
            files
                .iter()
                .map(|(path, text)| ((*path).to_string(), (*text).to_string()))
                .collect(),
            compile(3),
        );
        studio
    }

    #[test]
    fn a_loaded_studio_is_clean_until_a_buffer_edits() {
        let mut studio = loaded_studio(&[("workflow.star", "x = 1\n"), ("notes.md", "hi\n")]);
        assert!(!studio.edited());
        assert_eq!(
            studio.active.as_deref(),
            Some("notes.md"),
            "the editor opens the list's first file, alphabetically"
        );
        studio
            .files
            .get_mut("workflow.star")
            .unwrap()
            .text
            .push('2');
        assert!(studio.edited());
        studio.files.remove("notes.md");
        assert!(studio.edited(), "a removed file is still an edit");
        studio.saved = studio.buffers();
        assert!(!studio.edited());
    }

    #[test]
    fn a_loaded_studio_keeps_the_open_file_and_falls_back_to_the_first() {
        let mut studio = loaded_studio(&[("workflow.star", "x = 1\n"), ("notes.md", "hi\n")]);
        studio.active = Some("notes.md".to_string());
        studio.loaded(
            4,
            [("workflow.star".to_string(), "x = 2\n".to_string())]
                .into_iter()
                .collect(),
            compile(4),
        );
        assert_eq!(studio.active.as_deref(), Some("workflow.star"));
        assert_eq!(studio.base, Some(4));
        assert_eq!(studio.files["workflow.star"].text, "x = 2\n");
    }
}
