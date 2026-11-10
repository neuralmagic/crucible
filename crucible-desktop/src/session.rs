//! The engine's session log folded into what the run pane draws. Both backends feed it the same
//! NDJSON lines, decoded with the engine's own wire types.

use crucible_contract::emits::{DeclaredFile, EmitWire};
use crucible_contract::event::AgentEvent;
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
    /// The same events whole, for reading one turn at a time.
    pub agent_events: Vec<AgentStep>,
}

/// One event from an agent task's attempt, as the session recorded it.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentStep {
    /// The task, `task[key]` for a fan-out instance, or `task[round-N]` for a revise round.
    pub task: String,
    pub attempt: u32,
    pub event: AgentEvent,
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
    /// What a command or evaluate task runs; empty for other kinds and for logs that predate it.
    pub command: String,
    /// The fields its output promises, typed when the pack says.
    pub emits: Vec<EmitWire>,
    /// The files its output includes, with their schemas when the pack gives them.
    pub files: Vec<DeclaredFile>,
    /// `producer.field` references each instance gets narrowed to its own entry.
    pub keyed: Vec<String>,
    pub attempt: Option<Attempt>,
    pub result: Option<Outcome>,
    pub instances: Vec<Instance>,
    /// The tasks it sends back when it settles failing.
    pub revise: Vec<String>,
    /// The most rounds `revise` may run, the first included.
    pub max_rounds: u32,
    /// The revise loop it runs in, as the reviser or one of its targets.
    pub revised: Option<Revised>,
    pub rounds: Vec<Round>,
    /// When its first attempt started, from logs that record it.
    pub started_at: Option<jiff::Timestamp>,
}

/// A revise loop as one of its tasks sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct Revised {
    /// The task whose failing result starts another round.
    pub by: String,
    pub max_rounds: u32,
}

/// One round of a revise loop, `task[round-N]` in the log.
#[derive(Clone, Debug, PartialEq)]
pub struct Round {
    pub n: u32,
    pub attempt: Option<Attempt>,
    pub result: Option<Outcome>,
    pub started_at: Option<jiff::Timestamp>,
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
    /// When its first attempt started, from logs that record it.
    pub started_at: Option<jiff::Timestamp>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    pub status: TaskStatus,
    pub attempts: u32,
    pub cost_usd: f64,
    pub secs: f64,
    /// When it settled, from logs that record it.
    pub at: Option<jiff::Timestamp>,
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
    /// The command's first line that says something, and how many more lines follow it.
    pub fn command_brief(&self) -> Option<(&str, usize)> {
        let mut lines = self
            .command
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty());
        let first = lines.next()?;
        Some((first, lines.count()))
    }

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
            command: task.command,
            emits: task.emits,
            files: task.emits_files,
            keyed: task.keyed,
            attempt: None,
            result: None,
            started_at: None,
            instances: Vec::new(),
            revise: task.revise,
            max_rounds: task.max_rounds,
            revised: None,
            rounds: Vec::new(),
        }
    }

    pub fn state(&self) -> TaskState {
        if self.result.is_none()
            && let Some(round) = self.rounds.last()
        {
            return round.state();
        }
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

    /// Its own result, then each instance's and each round's.
    fn results(&self) -> impl Iterator<Item = &Outcome> {
        self.result
            .iter()
            .chain(self.instances.iter().filter_map(|i| i.result.as_ref()))
            .chain(self.rounds.iter().filter_map(|r| r.result.as_ref()))
    }

    /// The most attempts any instance or round, or the task itself, took.
    pub fn retries(&self) -> u32 {
        self.results()
            .map(|r| r.attempts)
            .max()
            .unwrap_or(0)
            .saturating_sub(1)
    }

    /// The agent the task ran on, from its own result or any instance's or round's.
    pub fn agent(&self) -> Option<&TaskAgent> {
        self.results().find_map(|r| r.agent.as_ref())
    }

    /// Every link the task and its instances and rounds reported.
    pub fn links(&self) -> Vec<&LinkView> {
        self.results().flat_map(|r| r.links.iter()).collect()
    }

    /// Every repair the task and its instances and rounds made.
    pub fn repairs(&self) -> Vec<&TaskRepair> {
        self.results().flat_map(|r| r.repairs.iter()).collect()
    }

    pub fn round(&self, n: u32) -> Option<&Round> {
        self.rounds.iter().find(|round| round.n == n)
    }

    /// Where the instance or round `key` stands.
    pub fn part_state(&self, key: &str) -> Option<TaskState> {
        match round_key(key).filter(|_| self.revised.is_some()) {
            Some(n) => self.round(n).map(Round::state),
            None => self
                .instances
                .iter()
                .find(|i| i.key == key)
                .map(Instance::state),
        }
    }

    /// The round an event naming only the task belongs to: the one in flight, else the next.
    /// `None` outside a loop or once the loop settled.
    fn round_in_flight(&self) -> Option<u32> {
        if self.revised.is_none() || self.result.is_some() {
            return None;
        }
        Some(match self.rounds.last() {
            Some(round) if round.result.is_none() => round.n,
            Some(round) => round.n + 1,
            None => 1,
        })
    }
}

/// The reviser's failing result that sent `task` into round `n`.
pub fn sent_back<'a>(tasks: &'a [TaskView], task: &TaskView, n: u32) -> Option<&'a Outcome> {
    let by = &task.revised.as_ref()?.by;
    tasks
        .iter()
        .find(|t| t.name == *by)?
        .round(n.checked_sub(1)?)?
        .result
        .as_ref()
        .filter(|result| result.status == TaskStatus::Fail)
}

impl Round {
    pub fn state(&self) -> TaskState {
        TaskState::of(self.attempt.as_ref(), self.result.as_ref())
    }

    /// `task[round-N]`, as the log names it.
    pub fn name(&self, task: &str) -> String {
        format!("{task}[round-{}]", self.n)
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

/// `edit[layernorm]` is instance `layernorm` of `edit`, and `review[round-2]` its second round.
pub fn split_name(task: &str) -> (&str, Option<&str>) {
    match task.split_once('[') {
        Some((base, rest)) => (base, Some(rest.strip_suffix(']').unwrap_or(rest))),
        None => (task, None),
    }
}

/// `round-2` is round 2.
pub fn round_key(key: &str) -> Option<u32> {
    key.strip_prefix("round-")?.parse().ok().filter(|n| *n >= 1)
}

/// Each loop task's reviser and round cap, from the revisers' `revise` lists.
fn loops(tasks: &mut [TaskView]) {
    let revisers: Vec<(String, Vec<String>, u32)> = tasks
        .iter()
        .filter(|task| !task.revise.is_empty())
        .map(|task| (task.name.clone(), task.revise.clone(), task.max_rounds))
        .collect();
    for task in tasks.iter_mut() {
        task.revised = revisers
            .iter()
            .find(|(by, targets, _)| *by == task.name || targets.contains(&task.name))
            .map(|(by, _, max_rounds)| Revised {
                by: by.clone(),
                max_rounds: *max_rounds,
            });
    }
}

impl RunFold {
    /// Fold one NDJSON line. A line this build cannot decode is skipped, so a newer engine's events
    /// never stop the rest of the log from reading.
    pub fn apply_line(&mut self, line: &str) -> bool {
        match crucible_contract::session::decode(line) {
            Some(event) => self.apply(event),
            None => false,
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
                            task.rounds = old.rounds;
                        }
                        task
                    })
                    .collect();
                loops(&mut self.tasks);
                self.plan_version = plan_version;
                self.budget_usd = budget_usd;
                true
            }
            SessionEvent::TaskStarted {
                task,
                attempt,
                estimate_usd,
                at,
            } => {
                let attempt = Attempt {
                    number: attempt,
                    estimate_usd,
                };
                let at = at.and_then(|at| at.parse().ok());
                let task = self.placed(task);
                let (newest, first) = match self.slot(&task) {
                    Some(Slot::Task(task)) => (&mut task.attempt, &mut task.started_at),
                    Some(Slot::Instance(instance)) => {
                        (&mut instance.attempt, &mut instance.started_at)
                    }
                    Some(Slot::Round(round)) => (&mut round.attempt, &mut round.started_at),
                    None => return false,
                };
                *newest = Some(attempt);
                if first.is_none() {
                    *first = at;
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
                at,
                reused_from,
                ..
            } => {
                let outcome = Outcome {
                    status,
                    attempts,
                    cost_usd: cost_usd.max(0.0),
                    secs,
                    at: at.and_then(|at| at.parse().ok()),
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
                    Some(Slot::Round(round)) => round.result = Some(outcome),
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
            SessionEvent::TaskAgent {
                task,
                attempt,
                event,
            } => {
                let task = self.placed(task);
                let line = LogLine::from_agent(&task, &event);
                let kept = line.is_some();
                if let Some(line) = line {
                    self.transcript.push(line);
                    self.agent_events.push(AgentStep {
                        task,
                        attempt,
                        event,
                    });
                }
                kept
            }
            SessionEvent::TaskOutput {
                task,
                stream,
                lines,
                ..
            } => {
                let stderr = stream == crucible_contract::event::RawStream::Stderr;
                let task = self.placed(task);
                self.transcript
                    .extend(LogLine::from_output(&task, stderr, &lines));
                true
            }
            SessionEvent::TaskOutputDropped { task, lines, .. } => {
                let task = self.placed(task);
                self.transcript.push(LogLine::dropped_output(&task, lines));
                true
            }
            SessionEvent::Shutdown { outcome, reason } => {
                self.shutdown = Some(Shutdown { outcome, reason });
                true
            }
            _ => false,
        }
    }

    /// The engine names a loop task's attempts, transcript and output by the task alone and only
    /// its results by round; rounds run one after another, so each lands in the round in flight.
    fn placed(&self, name: String) -> String {
        let round = self
            .tasks
            .iter()
            .find(|task| task.name == name)
            .and_then(TaskView::round_in_flight);
        match round {
            Some(n) => format!("{name}[round-{n}]"),
            None => name,
        }
    }

    fn slot(&mut self, name: &str) -> Option<Slot<'_>> {
        let (base, key) = split_name(name);
        let task = self.tasks.iter_mut().find(|task| task.name == base)?;
        let Some(key) = key else {
            return Some(Slot::Task(task));
        };
        if let Some(n) = round_key(key).filter(|_| task.revised.is_some()) {
            let at = match task.rounds.binary_search_by_key(&n, |round| round.n) {
                Ok(at) => at,
                Err(at) => {
                    task.rounds.insert(
                        at,
                        Round {
                            n,
                            attempt: None,
                            result: None,
                            started_at: None,
                        },
                    );
                    at
                }
            };
            return task.rounds.get_mut(at).map(Slot::Round);
        }
        let at = match task.instances.iter().position(|i| i.key == key) {
            Some(at) => at,
            None => {
                task.instances.push(Instance {
                    key: key.to_string(),
                    attempt: None,
                    result: None,
                    started_at: None,
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
    Round(&'a mut Round),
}

#[cfg(test)]
mod tests {
    use crate::session::{Revised, RunFold, TaskState, sent_back};
    use crucible_contract::emits::{DeclaredFile, EmitWire};
    use crucible_contract::session::{BlockedReasonKind, TaskStatus};

    const FANOUT: &str = r##"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":10.0,"tasks":[{"name":"seed","kind":"command","depends_on":[],"required":true,"emits":[{"field":"kernels"}],"emits_files":["traces/seed.log"],"timeout":"90m","command":"\nset -eu\n\n  python seed.py --out traces/seed.log\n"},{"name":"edit","kind":"agent","depends_on":["seed"],"required":true,"over":"seed.kernels"},{"name":"audit","kind":"agent","depends_on":["edit"],"required":false}]}
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
        assert_eq!(
            seed.emits,
            [EmitWire {
                field: "kernels".into(),
                ty: None
            }]
        );
        assert_eq!(seed.files, [DeclaredFile::from("traces/seed.log")]);
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
    fn a_command_task_keeps_what_it_runs_and_briefs_it_by_its_first_line() {
        let fold = fold(FANOUT);
        let seed = &fold.tasks[0];
        assert_eq!(
            seed.command,
            "\nset -eu\n\n  python seed.py --out traces/seed.log\n"
        );
        assert_eq!(seed.command_brief(), Some(("set -eu", 1)));
        assert_eq!(fold.tasks[1].command, "");
        assert_eq!(fold.tasks[1].command_brief(), None);
    }

    #[test]
    fn command_output_folds_into_the_tasks_log_by_stream_with_the_cut_said() {
        let text = format!(
            "{FANOUT}{}\n{}\n{}\n",
            r#"{"v":1,"kind":"task_output","task":"edit[a]","attempt":1,"stream":"stdout","lines":["fetching RHAI-21041","2 tickets"]}"#,
            r#"{"v":1,"kind":"task_output","task":"edit[a]","attempt":1,"stream":"stderr","lines":["warning: slow"]}"#,
            r#"{"v":1,"kind":"task_output_dropped","task":"edit[a]","attempt":1,"lines":41}"#,
        );
        let fold = fold(&text);
        use crate::logline::{Body, Group};
        let bodies: Vec<&Body> = fold.transcript.iter().map(|l| &l.body).collect();
        assert_eq!(
            bodies[..3],
            [
                &Body::Output {
                    stderr: false,
                    text: "fetching RHAI-21041".into()
                },
                &Body::Output {
                    stderr: false,
                    text: "2 tickets".into()
                },
                &Body::Output {
                    stderr: true,
                    text: "warning: slow".into()
                },
            ]
        );
        assert!(fold.transcript.iter().all(|l| l.is_for("edit")));
        assert_eq!(fold.transcript[0].group(), Group::Task);
        let dropped = &fold.transcript[3];
        assert!(dropped.is_problem());
        assert_eq!(
            dropped.summary().as_deref(),
            Some("41 more output lines past the cap were not recorded")
        );
        assert!(fold.agent_events.is_empty(), "output is not an agent turn");
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
        assert_eq!(fold.agent_events.len(), 4);
        assert_eq!(
            (
                fold.agent_events[2].task.as_str(),
                fold.agent_events[2].attempt
            ),
            ("edit[b]", 1)
        );
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

    const REVISE: &str = include_str!("../fixtures/revise-session.jsonl");

    /// The first `n` lines of the revise run, as a live follower would have them.
    fn revise_until(n: usize) -> RunFold {
        fold(&REVISE.lines().take(n).collect::<Vec<_>>().join("\n"))
    }

    #[test]
    fn a_revise_loop_folds_its_rounds_apart_from_fanout_instances() {
        let fold = fold(REVISE);
        let [author, review, report] = &fold.tasks[..] else {
            panic!("three tasks, got {:?}", fold.tasks);
        };
        for task in [author, review] {
            assert!(task.instances.is_empty(), "{} has no instances", task.name);
            assert!(!task.is_fanout(), "{} is not a fan-out", task.name);
            assert_eq!(
                task.revised,
                Some(Revised {
                    by: "review".into(),
                    max_rounds: 3
                })
            );
            assert_eq!(
                task.rounds.iter().map(|r| r.n).collect::<Vec<_>>(),
                [1, 2],
                "{}",
                task.name
            );
            assert!(task.rounds.iter().all(|r| r.started_at.is_some()));
            assert_eq!(task.state(), TaskState::Settled(TaskStatus::Pass));
        }
        assert_eq!(review.revise, ["author"]);
        assert!(author.revise.is_empty());
        assert_eq!(report.revised, None);
        assert!(report.rounds.is_empty());

        let states: Vec<TaskState> = review.rounds.iter().map(|r| r.state()).collect();
        assert_eq!(
            states,
            [
                TaskState::Settled(TaskStatus::Fail),
                TaskState::Settled(TaskStatus::Pass)
            ]
        );
        assert_eq!(
            author
                .round(2)
                .and_then(|r| r.result.as_ref()?.output.clone()),
            Some(serde_json::json!({"draft": 2}))
        );
        assert_eq!(
            author.part_state("round-1"),
            Some(TaskState::Settled(TaskStatus::Pass))
        );
        assert_eq!(author.part_state("round-3"), None);
        assert!(fold.required_passed());
    }

    #[test]
    fn a_round_names_the_reviser_failure_that_sent_it_back() {
        let fold = fold(REVISE);
        let (author, review) = (&fold.tasks[0], &fold.tasks[1]);
        let why = sent_back(&fold.tasks, author, 2).expect("round 2 was sent back");
        assert_eq!(why.status, TaskStatus::Fail);
        assert_eq!(why.note, "exit 1: rejected: no verdict in hand");
        assert_eq!(
            why.output,
            Some(serde_json::json!({"accepted": false, "why": "the probe hit an unrelated 400"}))
        );
        assert_eq!(sent_back(&fold.tasks, review, 2), Some(why));
        assert_eq!(
            sent_back(&fold.tasks, author, 1),
            None,
            "the first round is not a revision"
        );
        assert_eq!(
            sent_back(&fold.tasks, author, 3),
            None,
            "round 2 passed its review"
        );
        assert_eq!(sent_back(&fold.tasks, &fold.tasks[2], 2), None);
    }

    #[test]
    fn a_loop_in_flight_places_each_start_in_its_round() {
        let fold = revise_until(6);
        let (author, review) = (&fold.tasks[0], &fold.tasks[1]);
        assert_eq!(author.rounds.len(), 2);
        assert_eq!(
            author.round(2).map(|r| r.state()),
            Some(TaskState::Running(1))
        );
        assert_eq!(author.state(), TaskState::Running(1));
        assert!(author.attempt.is_none(), "the start belongs to the round");
        assert_eq!(review.rounds.len(), 1);
        assert_eq!(review.state(), TaskState::Settled(TaskStatus::Fail));
        assert!(review.result.is_none(), "the loop has not settled");

        let next = revise_until(8);
        assert_eq!(next.tasks[1].rounds.len(), 2);
        assert_eq!(next.tasks[1].state(), TaskState::Running(1));
    }

    #[test]
    fn a_rounds_transcript_and_output_land_in_that_round() {
        let mut lines: Vec<&str> = REVISE.lines().collect();
        let said = r#"{"v":1,"kind":"task_agent","task":"author","attempt":1,"event":{"kind":"text","delta":"Adding the verdict."}}"#;
        let printed = r#"{"v":1,"kind":"task_output","task":"author","attempt":1,"stream":"stdout","lines":["revising against the review"]}"#;
        let first = r#"{"v":1,"kind":"task_agent","task":"author","attempt":1,"event":{"kind":"text","delta":"First draft."}}"#;
        lines.insert(6, printed);
        lines.insert(6, said);
        lines.insert(2, first);
        let fold = fold(&lines.join("\n"));
        let named: Vec<Option<&str>> = fold.transcript.iter().map(|l| l.task.as_deref()).collect();
        assert_eq!(
            named,
            [
                Some("author[round-1]"),
                Some("author[round-2]"),
                Some("author[round-2]")
            ]
        );
        assert_eq!(
            fold.agent_events
                .iter()
                .map(|e| e.task.as_str())
                .collect::<Vec<_>>(),
            ["author[round-1]", "author[round-2]"]
        );
        assert!(fold.transcript.iter().all(|l| l.is_for("author")));
        assert!(fold.transcript[1].is_for("author[round-2]"));
        assert!(!fold.transcript[0].is_for("author[round-2]"));
    }

    #[test]
    fn a_fanout_key_that_reads_like_a_round_stays_an_instance() {
        let text = format!(
            "{FANOUT}{}\n",
            r#"{"v":1,"kind":"task_result","task":"edit[round-1]","status":"pass","attempts":1}"#,
        );
        let fold = fold(&text);
        let edit = &fold.tasks[1];
        assert!(edit.rounds.is_empty());
        assert!(edit.instances.iter().any(|i| i.key == "round-1"));
    }

    #[test]
    fn a_revised_plan_keeps_the_rounds_its_loop_ran() {
        let plan = REVISE.lines().next().expect("a plan");
        let mut fold = revise_until(9);
        assert!(fold.apply_line(&plan.replace("\"plan_version\":1", "\"plan_version\":2")));
        assert_eq!(fold.plan_version, 2);
        assert_eq!(fold.tasks[0].rounds.len(), 2);
        assert_eq!(fold.tasks[1].rounds.len(), 2);
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
