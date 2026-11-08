//! Fixing a pack and running it again from the tracker: a draft's working copy beside the run
//! that showed the problem, and a rerun of that run that reuses what already passed.

use std::time::{Duration, Instant};

use crux::dto::RerunPreview;
use serde_json::{Map, Value};

use crate::draft::Working;
use crate::live::{self, Target};
use crate::pack::PackFiles;
use crate::{App, PackSource};

/// How often the working copy is re-read for edits made in the editor.
const REREAD: Duration = Duration::from_secs(1);

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

impl App {
    fn user(&self) -> Option<&str> {
        match &self.live {
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
    /// starts from: the draft itself for a draft run, else the user's own draft of the playbook.
    pub fn draft_target(&self) -> Option<(String, Option<String>)> {
        let launch = self.selected_live()?;
        if self.draft_run() {
            return Some((launch.id.clone(), None));
        }
        if !self.registered.iter().any(|id| id == &launch.id) {
            return None;
        }
        Some((draft_id(&launch.id, self.user()?), Some(launch.id.clone())))
    }

    /// Whether the Pack tab shows the working copy rather than the run's own pack.
    pub fn showing_working(&self) -> bool {
        self.working_pack.is_some()
            && match self.pack_source {
                Some(source) => source == PackSource::Working,
                None => self.draft_run(),
            }
    }

    /// Keep the working copy in step with the shown run, and re-read it for editor saves.
    pub fn refresh_working(&mut self) -> bool {
        let draft = self.draft_target().map(|(draft, _)| draft);
        if self.working.as_ref().map(|w| &w.draft) != draft.as_ref() {
            self.working = draft
                .as_deref()
                .and_then(|draft| Working::open(&crate::draft::root()?, draft));
            self.working_pack = None;
            self.working_read = None;
            if self.working.is_none() {
                return true;
            }
        }
        let Some(working) = &self.working else {
            return false;
        };
        if self.working_read.is_some_and(|at| at.elapsed() < REREAD) {
            return false;
        }
        self.working_read = Some(Instant::now());
        let edited = working.edited();
        let pack = PackFiles {
            origin: format!(
                "working copy of draft {} at version {}{}",
                working.draft,
                working.base,
                if edited { ", edited" } else { "" }
            ),
            ..PackFiles::read_dir(&working.dir)
        };
        let changed = self.working_pack.as_ref() != Some(&pack) || self.working_edited != edited;
        self.working_pack = Some(pack);
        self.working_edited = edited;
        changed
    }

    /// Pull the shown run's draft into its working copy, creating the draft when it is new.
    pub fn pull_draft(&mut self) {
        let Some((draft, template)) = self.draft_target() else {
            return;
        };
        self.draft_busy = true;
        self.draft_note = None;
        self.draft_after = self.replies;
        self.live_client.pull(&draft, template.as_deref());
    }

    /// Save the working copy as its draft's next version.
    pub fn save_draft(&mut self) {
        let Some(working) = self.working.clone() else {
            return;
        };
        self.draft_busy = true;
        self.draft_note = None;
        self.draft_after = self.replies;
        self.live_client.save(working);
    }

    pub fn show_pack(&mut self, source: PackSource) {
        self.pack_source = Some(source);
        self.pack_file = None;
    }

    /// Launch the working copy's draft.
    pub fn launch_working(&mut self) {
        if let Some(working) = &self.working {
            let target = Target::Draft(working.draft.clone());
            self.open_launcher(target);
        }
    }

    pub(crate) fn answer_draft(&mut self, synced: Option<live::Synced>) {
        let Some(synced) = synced.filter(|s| self.draft_busy && s.seq > self.draft_after) else {
            return;
        };
        self.replies = self.replies.max(synced.seq);
        self.draft_busy = false;
        match synced.result {
            Ok((working, note)) => {
                self.working = Some(working);
                self.working_pack = None;
                self.working_read = None;
                self.pack_source = Some(PackSource::Working);
                self.pack_file = None;
                self.draft_note = note.map(Ok);
                self.refresh_working();
            }
            Err(error) => self.draft_note = Some(Err(error)),
        }
    }

    /// What a launch form for `playbook` starts: the registered playbook, or the draft the shown
    /// run came from.
    pub fn launch_target(&self, playbook: &str) -> Option<Target> {
        if self.registered.iter().any(|id| id == playbook) {
            return Some(Target::Playbook(playbook.to_string()));
        }
        let shown = self.selected_live()?;
        (shown.id == playbook && self.draft_run()).then(|| Target::Draft(playbook.to_string()))
    }

    /// The shown run, when it has finished and can be rerun: what a rerun starts, its run id,
    /// and the params it was launched with.
    pub fn rerunnable(&self) -> Option<(Target, String, Value)> {
        let launch = self.selected_live()?;
        if launch.status == "running" {
            return None;
        }
        let record = self.selected_detail()?.launch.as_ref()?;
        let run = record.runs.first()?.run_id.clone();
        let target = match record.launch.draft_version {
            Some(_) => Target::Draft(launch.id.clone()),
            None if self.registered.iter().any(|id| id == &launch.id) => {
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
        let asked = self.replies;
        if let Some(launcher) = &mut self.launcher {
            launcher.rerun = Some(Rerun {
                run: run.clone(),
                from: from.clone(),
                params,
                preview: None,
                asked,
            });
        }
        self.live_client.preview_rerun(&run, from);
    }

    /// Add the tasks a preview found changed to the rerun, and ask again.
    pub fn widen_rerun(&mut self) {
        let asked = self.replies;
        let Some(rerun) = self.launcher.as_mut().and_then(|l| l.rerun.as_mut()) else {
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
        self.live_client.preview_rerun(&run, from);
    }

    pub(crate) fn answer_rerun(&mut self, reran: Option<live::Reran>) {
        let Some(rerun) = self.launcher.as_mut().and_then(|l| l.rerun.as_mut()) else {
            return;
        };
        let Some(reran) =
            reran.filter(|r| r.seq > rerun.asked && r.run == rerun.run && r.from == rerun.from)
        else {
            return;
        };
        rerun.asked = reran.seq;
        rerun.preview = Some(reran.result);
        self.replies = self.replies.max(reran.seq);
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
        let root = crate::draft::root()?;
        let mine = self.working.as_ref().filter(|w| w.draft == draft);
        let pulled = mine
            .is_some_and(|w| !self.working_edited && Some(w.base) == record.launch.draft_version);
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
            self.handoff_note = Some(Err("This run has no pack to fix".to_string()));
            return None;
        };
        let text = crate::handoff::brief(&handoff);
        self.handoff_note = Some(
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
    use crate::studio::{Rerun, draft_id, launch_body};

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
}
