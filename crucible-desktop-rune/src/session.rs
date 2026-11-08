//! The engine's session log folded into what the run pane draws. Both backends feed it the same
//! NDJSON lines, decoded with the engine's own wire types.

use crucible_contract::session::{
    PlanTaskWire, SessionEvent, TaskAgent, TaskBlocked, TaskRepair, TaskStatus, TransportCause,
};
use serde_json::Value;

use crate::logline::LogLine;

const NOTES: usize = 20;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunFold {
    pub plan_version: u32,
    pub budget_usd: f64,
    pub tasks: Vec<TaskView>,
    pub notes: Vec<String>,
    pub shutdown: Option<Shutdown>,
    /// Every agent task's transcript, in the order the engine recorded it.
    pub transcript: Vec<LogLine>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Shutdown {
    pub outcome: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskView {
    pub name: String,
    pub kind: String,
    pub depends_on: Vec<String>,
    pub required: bool,
    pub session: String,
    /// The output a fan-out spreads over, `task.field`.
    pub over: String,
    pub when: String,
    pub timeout: String,
    pub emits: Vec<String>,
    pub files: Vec<String>,
    pub attempt: Option<Attempt>,
    pub result: Option<Outcome>,
    pub instances: Vec<Instance>,
}

/// The newest attempt the engine started.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Attempt {
    pub number: u32,
    pub estimate_usd: f64,
}

/// One fan-out instance, `task[key]` in the log.
#[derive(Clone, Debug, PartialEq)]
pub struct Instance {
    pub key: String,
    pub attempt: Option<Attempt>,
    pub result: Option<Outcome>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    pub status: TaskStatus,
    pub attempts: u32,
    pub cost_usd: f64,
    pub secs: f64,
    pub output: Option<Value>,
    pub note: String,
    pub links: Vec<LinkView>,
    pub agent: Option<TaskAgent>,
    pub repairs: Vec<TaskRepair>,
    pub blocked: Option<TaskBlocked>,
    pub transport: Option<TransportCause>,
    pub reused_from: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LinkView {
    pub provider: &'static str,
    pub kind: &'static str,
    pub label: String,
    pub url: String,
}

/// Where a task or instance stands.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TaskState {
    Pending,
    Running(u32),
    Settled(TaskStatus),
}

impl TaskState {
    fn of(attempt: Option<&Attempt>, result: Option<&Outcome>) -> Self {
        match (result, attempt) {
            (Some(result), _) => TaskState::Settled(result.status),
            (None, Some(attempt)) => TaskState::Running(attempt.number),
            (None, None) => TaskState::Pending,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TaskState::Pending => "pending",
            TaskState::Running(_) => "running",
            TaskState::Settled(status) => status.as_str(),
        }
    }
}

impl TaskView {
    fn from_wire(task: PlanTaskWire) -> Self {
        TaskView {
            name: task.name,
            kind: task.kind,
            depends_on: task.depends_on,
            required: task.required,
            session: task.session,
            over: task.over,
            when: task.when,
            timeout: task.timeout,
            emits: task.emits.into_iter().map(|emit| emit.field).collect(),
            files: task.emits_files.into_iter().map(|file| file.path).collect(),
            attempt: None,
            result: None,
            instances: Vec::new(),
        }
    }

    pub fn state(&self) -> TaskState {
        if self.result.is_none() && self.instances.iter().any(|i| i.state().is_running()) {
            let newest = self
                .instances
                .iter()
                .filter(|i| i.state().is_running())
                .filter_map(|i| i.attempt.map(|a| a.number))
                .max()
                .unwrap_or(1);
            return TaskState::Running(newest);
        }
        TaskState::of(self.attempt.as_ref(), self.result.as_ref())
    }

    pub fn is_fanout(&self) -> bool {
        !self.over.is_empty() || !self.instances.is_empty()
    }

    /// How many instances settled with each kind of outcome.
    pub fn instance_counts(&self) -> InstanceCounts {
        let mut counts = InstanceCounts::default();
        for instance in &self.instances {
            match instance.state() {
                TaskState::Pending => counts.pending += 1,
                TaskState::Running(_) => counts.running += 1,
                TaskState::Settled(TaskStatus::Pass) => counts.passed += 1,
                TaskState::Settled(TaskStatus::Skipped | TaskStatus::NotTaken) => {
                    counts.skipped += 1
                }
                TaskState::Settled(_) => counts.failed += 1,
            }
        }
        counts
    }

    /// The most attempts any instance, or the task itself, took.
    pub fn retries(&self) -> u32 {
        let own = self.result.as_ref().map_or(0, |r| r.attempts);
        self.instances
            .iter()
            .filter_map(|i| i.result.as_ref().map(|r| r.attempts))
            .chain([own])
            .max()
            .unwrap_or(0)
            .saturating_sub(1)
    }

    /// The agent the task ran on, from its own result or any instance's.
    pub fn agent(&self) -> Option<&TaskAgent> {
        self.result
            .as_ref()
            .and_then(|r| r.agent.as_ref())
            .or_else(|| {
                self.instances
                    .iter()
                    .find_map(|i| i.result.as_ref().and_then(|r| r.agent.as_ref()))
            })
    }

    /// Every link the task and its instances reported.
    pub fn links(&self) -> Vec<&LinkView> {
        self.result
            .iter()
            .chain(self.instances.iter().filter_map(|i| i.result.as_ref()))
            .flat_map(|r| r.links.iter())
            .collect()
    }
}

impl TaskState {
    pub fn is_running(self) -> bool {
        matches!(self, TaskState::Running(_))
    }
}

impl Instance {
    pub fn state(&self) -> TaskState {
        TaskState::of(self.attempt.as_ref(), self.result.as_ref())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InstanceCounts {
    pub pending: usize,
    pub running: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
}

impl InstanceCounts {
    pub fn total(self) -> usize {
        self.pending + self.running + self.passed + self.failed + self.skipped
    }
}

/// `edit[layernorm]` is instance `layernorm` of `edit`.
fn split_instance(task: &str) -> (&str, Option<&str>) {
    match task.split_once('[') {
        Some((base, rest)) => (base, Some(rest.strip_suffix(']').unwrap_or(rest))),
        None => (task, None),
    }
}

impl RunFold {
    /// Fold one NDJSON line. A line this build cannot decode is skipped, so a newer engine's events
    /// never stop the rest of the log from reading.
    pub fn apply_line(&mut self, line: &str) -> bool {
        let line = line.trim();
        if line.is_empty() {
            return false;
        }
        match serde_json::from_str::<SessionEvent>(line) {
            Ok(event) => self.apply(event),
            Err(_) => false,
        }
    }

    pub fn apply(&mut self, event: SessionEvent) -> bool {
        match event {
            SessionEvent::PlanAdmitted {
                plan_version,
                budget_usd,
                tasks,
                ..
            } => {
                let mut previous = std::mem::take(&mut self.tasks);
                self.tasks = tasks
                    .into_iter()
                    .map(|wire| {
                        let mut task = TaskView::from_wire(wire);
                        if let Some(at) = previous.iter().position(|old| old.name == task.name) {
                            let old = previous.swap_remove(at);
                            task.attempt = old.attempt;
                            task.result = old.result;
                            task.instances = old.instances;
                        }
                        task
                    })
                    .collect();
                self.plan_version = plan_version;
                self.budget_usd = budget_usd;
                true
            }
            SessionEvent::TaskStarted {
                task,
                attempt,
                estimate_usd,
            } => {
                let attempt = Attempt {
                    number: attempt,
                    estimate_usd,
                };
                match self.slot(&task) {
                    Some(Slot::Task(task)) => task.attempt = Some(attempt),
                    Some(Slot::Instance(instance)) => instance.attempt = Some(attempt),
                    None => return false,
                }
                true
            }
            SessionEvent::TaskResult {
                task,
                status,
                attempts,
                cost_usd,
                output,
                links,
                agent,
                repairs,
                note,
                blocked,
                transport,
                secs,
                reused_from,
                ..
            } => {
                let outcome = Outcome {
                    status,
                    attempts,
                    cost_usd: cost_usd.max(0.0),
                    secs,
                    output,
                    note,
                    links: links
                        .into_iter()
                        .map(|link| LinkView {
                            provider: link.provider.as_str(),
                            kind: link.kind.as_str(),
                            label: link.label,
                            url: link.url.as_str().to_string(),
                        })
                        .collect(),
                    agent: agent.map(|agent| *agent),
                    repairs,
                    blocked,
                    transport,
                    reused_from,
                };
                match self.slot(&task) {
                    Some(Slot::Task(task)) => task.result = Some(outcome),
                    Some(Slot::Instance(instance)) => instance.result = Some(outcome),
                    None => return false,
                }
                true
            }
            SessionEvent::Note { msg } => {
                self.notes.push(msg);
                if self.notes.len() > NOTES {
                    self.notes.remove(0);
                }
                true
            }
            SessionEvent::TaskAgent { task, event, .. } => {
                match LogLine::from_agent(&task, &event) {
                    Some(line) => {
                        self.transcript.push(line);
                        true
                    }
                    None => false,
                }
            }
            SessionEvent::Shutdown { outcome, reason } => {
                self.shutdown = Some(Shutdown { outcome, reason });
                true
            }
            _ => false,
        }
    }

    fn slot(&mut self, name: &str) -> Option<Slot<'_>> {
        let (base, key) = split_instance(name);
        let task = self.tasks.iter_mut().find(|task| task.name == base)?;
        let Some(key) = key else {
            return Some(Slot::Task(task));
        };
        let at = match task.instances.iter().position(|i| i.key == key) {
            Some(at) => at,
            None => {
                task.instances.push(Instance {
                    key: key.to_string(),
                    attempt: None,
                    result: None,
                });
                task.instances.len() - 1
            }
        };
        task.instances.get_mut(at).map(Slot::Instance)
    }

    /// Spend across the plan's own tasks; a fan-out's result already carries its instances.
    pub fn spent(&self) -> f64 {
        self.tasks
            .iter()
            .filter_map(|task| task.result.as_ref())
            .map(|result| result.cost_usd)
            .sum::<f64>()
            .max(0.0)
    }

    pub fn passed(&self) -> usize {
        self.settled()
            .filter(|status| *status == TaskStatus::Pass)
            .count()
    }

    pub fn failed(&self) -> usize {
        self.settled()
            .filter(|status| {
                !matches!(
                    status,
                    TaskStatus::Pass | TaskStatus::Skipped | TaskStatus::NotTaken
                )
            })
            .count()
    }

    fn settled(&self) -> impl Iterator<Item = TaskStatus> + '_ {
        self.tasks
            .iter()
            .filter_map(|task| task.result.as_ref().map(|result| result.status))
    }

    pub fn required_settled(&self) -> bool {
        self.tasks
            .iter()
            .filter(|task| task.required)
            .all(|task| task.result.is_some())
    }

    pub fn required_passed(&self) -> bool {
        self.tasks.iter().filter(|task| task.required).all(|task| {
            task.result
                .as_ref()
                .is_some_and(|result| result.status == TaskStatus::Pass)
        })
    }
}

enum Slot<'a> {
    Task(&'a mut TaskView),
    Instance(&'a mut Instance),
}

#[cfg(test)]
mod tests {
    use crate::session::{RunFold, TaskState};
    use crucible_contract::session::{BlockedReasonKind, TaskStatus};

    const FANOUT: &str = r##"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":10.0,"tasks":[{"name":"seed","kind":"command","depends_on":[],"required":true,"emits":[{"field":"kernels"}],"emits_files":["traces/seed.log"],"timeout":"90m"},{"name":"edit","kind":"agent","depends_on":["seed"],"required":true,"over":"seed.kernels"},{"name":"audit","kind":"agent","depends_on":["edit"],"required":false}]}
{"v":1,"kind":"task_started","task":"seed","attempt":1,"estimate_usd":0.0}
{"v":1,"kind":"task_result","task":"seed","status":"pass","task_kind":"command","attempts":1,"cost_usd":0.0,"output":{"kernels":["a","b"]},"note":"","secs":3.5}
{"v":1,"kind":"task_started","task":"edit[a]","attempt":1,"estimate_usd":1.25}
{"v":1,"kind":"task_started","task":"edit[b]","attempt":1,"estimate_usd":1.25}
{"v":1,"kind":"task_started","task":"edit[a]","attempt":2,"estimate_usd":1.25}
{"v":1,"kind":"task_result","task":"edit[a]","status":"pass","attempts":2,"cost_usd":0.75,"agent":{"harness":"claude","model":"glm-5-3","effort":"low"},"links":[{"url":"https://github.com/o/r/pull/7","provider":"github","kind":"pull_request","label":"#7"}],"repairs":[{"label":"edit[a] repair 1/2","round":1,"of":2,"cost_usd":0.1,"notes":["missing field"]}]}
not json at all
{"v":1,"kind":"some_future_event","x":1}
{"v":1,"kind":"note","msg":"budget 40% spent"}
"##;

    fn fold(text: &str) -> RunFold {
        let mut fold = RunFold::default();
        for line in text.lines() {
            fold.apply_line(line);
        }
        fold
    }

    #[test]
    fn a_fanout_in_flight_tracks_each_instance_and_its_retries() {
        let fold = fold(FANOUT);
        assert_eq!(fold.plan_version, 1);
        assert_eq!(fold.tasks.len(), 3);

        let seed = &fold.tasks[0];
        assert_eq!(seed.state(), TaskState::Settled(TaskStatus::Pass));
        assert_eq!(seed.emits, ["kernels"]);
        assert_eq!(seed.files, ["traces/seed.log"]);
        assert_eq!(seed.timeout, "90m");
        assert!((seed.result.as_ref().unwrap().secs - 3.5).abs() < 1e-9);

        let edit = &fold.tasks[1];
        assert!(edit.is_fanout());
        assert_eq!(edit.instances.len(), 2);
        assert_eq!(edit.state(), TaskState::Running(1));
        let counts = edit.instance_counts();
        assert_eq!((counts.passed, counts.running, counts.total()), (1, 1, 2));
        assert_eq!(edit.retries(), 1);
        assert_eq!(edit.agent().map(|a| a.model.as_str()), Some("glm-5-3"));
        let links = edit.links();
        assert_eq!(links.len(), 1);
        assert_eq!(
            (links[0].provider, links[0].kind, links[0].label.as_str()),
            ("github", "pull_request", "#7")
        );
        assert_eq!(edit.instances[0].result.as_ref().unwrap().repairs.len(), 1);

        assert_eq!(fold.tasks[2].state(), TaskState::Pending);
        assert_eq!(fold.notes, ["budget 40% spent"]);
        assert!(fold.shutdown.is_none());
    }

    #[test]
    fn a_settled_run_counts_its_plan_tasks_not_their_instances() {
        let text = format!(
            "{FANOUT}{}\n{}\n{}\n{}\n",
            r#"{"v":1,"kind":"task_result","task":"edit[b]","status":"fail","attempts":1,"cost_usd":0.5,"note":"build broke"}"#,
            r#"{"v":1,"kind":"task_result","task":"edit","status":"fail","attempts":1,"cost_usd":1.25,"output":{"instances":2,"passed":1,"failed":1}}"#,
            r#"{"v":1,"kind":"task_result","task":"audit","status":"blocked","attempts":0,"blocked":{"reason":"dependency_did_not_pass"},"note":"edit did not pass"}"#,
            r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":"required task edit failed"}"#,
        );
        let fold = fold(&text);
        assert_eq!(fold.passed(), 1);
        assert_eq!(fold.failed(), 2);
        assert!((fold.spent() - 1.25).abs() < 1e-9);
        assert!(fold.required_settled());
        assert!(!fold.required_passed());
        let audit = &fold.tasks[2];
        assert_eq!(
            audit
                .result
                .as_ref()
                .unwrap()
                .blocked
                .as_ref()
                .map(|b| b.reason),
            Some(BlockedReasonKind::DependencyDidNotPass)
        );
        assert_eq!(
            fold.shutdown.as_ref().map(|s| s.reason.as_str()),
            Some("required task edit failed")
        );
        assert_eq!(fold.tasks[1].instance_counts().failed, 1);
    }

    #[test]
    fn a_revised_plan_keeps_what_its_surviving_tasks_already_did() {
        let text = format!(
            "{FANOUT}{}\n",
            r#"{"v":1,"kind":"plan_admitted","plan_version":2,"budget_usd":12.0,"tasks":[{"name":"seed","kind":"command","required":true},{"name":"report","kind":"command","required":true}]}"#,
        );
        let fold = fold(&text);
        assert_eq!(fold.plan_version, 2);
        assert_eq!(
            fold.tasks
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["seed", "report"]
        );
        assert!(fold.tasks[0].result.is_some());
        assert_eq!(fold.tasks[1].state(), TaskState::Pending);
    }

    #[test]
    fn task_agent_events_fold_into_a_whole_text_transcript_per_instance() {
        let text = format!(
            "{FANOUT}{}\n{}\n{}\n{}\n{}\n",
            r#"{"v":1,"kind":"task_agent","task":"edit[a]","attempt":2,"event":{"kind":"thinking","delta":"Which kernel?"}}"#,
            r#"{"v":1,"kind":"task_agent","task":"edit[a]","attempt":2,"event":{"kind":"text","delta":"First line.\nSecond line, yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy"}}"#,
            r#"{"v":1,"kind":"task_agent","task":"edit[b]","attempt":1,"event":{"kind":"tool","name":"Bash","summary":"$ make","failed":true}}"#,
            r#"{"v":1,"kind":"task_agent","task":"edit[b]","attempt":1,"event":{"kind":"raw","text":"noise","stream":"stderr"}}"#,
            r#"{"v":1,"kind":"task_agent","task":"edit[b]","attempt":1,"event":{"kind":"result","subtype":"success","turns":3,"cost_usd":0.2}}"#,
        );
        let fold = fold(&text);
        assert_eq!(fold.transcript.len(), 4, "raw output has no line");
        let said = &fold.transcript[1];
        assert_eq!(said.task.as_deref(), Some("edit[a]"));
        assert!(said.is_for("edit"));
        let crate::logline::Body::Say(body) = &said.body else {
            panic!("expected assistant text, got {:?}", said.body);
        };
        assert!(body.ends_with("yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy"), "kept whole");
        assert!(body.contains('\n'), "kept its lines");

        let activity = crate::logline::activity(&fold.transcript, "edit");
        assert_eq!((activity.tools, activity.failed_tools), (1, 1));
        assert_eq!(
            activity.last.as_deref(),
            Some("result success turns=3 cost=$0.2000")
        );
    }

    #[test]
    fn an_event_for_a_task_the_plan_never_named_changes_nothing() {
        let mut fold = fold(FANOUT);
        let before = fold.clone();
        assert!(!fold.apply_line(
            r#"{"v":1,"kind":"task_result","task":"ghost","status":"pass","attempts":1}"#
        ));
        assert_eq!(fold, before);
    }
}
