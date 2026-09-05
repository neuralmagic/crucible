//! The deterministic plan executor. Readiness is visibility: a task with an unmet dependency
//! does not exist to the dispatcher. Retry ≠ check: transport failures get bounded retries,
//! measured failures never rerun. Truncation is fail-closed and happens before any
//! dispatch; advisory failures block dependents but not validity; a required failure
//! short-circuits; budget fails closed.
//!
//! Dispatch is serial in topo order, with one exception: simultaneously-ready
//! isolation-marked tasks go to the runner as a single [`TaskRunner::run_many`] batch
//! (the wide fan-out), and their results are recorded in declaration order so the event
//! stream stays deterministic regardless of completion order.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::crucible::Direction;
use crate::diagram::IllegalTransition;
use crate::plan::ir::{
    ITEM_INPUT, Join, OUTCOME_INPUT, Stage, Task, TaskKind, TaskName, ValidPlan,
};
use crate::plan::machine::{
    BlockedReason, PlanEvent, PlanMachine, TaskEvent, TaskMachine, TaskState,
};
use crucible_contract::TransportCause;

/// What the substrate can measure. Missing caps truncate the plan fail-closed.
#[derive(Clone, Debug, Default)]
pub struct Substrate {
    pub caps: BTreeSet<String>,
}

impl Substrate {
    fn supports(&self, needs: &str) -> bool {
        needs == "any" || self.caps.contains(needs)
    }
}

/// One attempt's result, as reported by the runner.
#[derive(Debug)]
pub enum AttemptOutcome {
    /// Measured success with the task's structured output (the edge payload).
    Pass(Value),
    /// Measured failure. Never retried: a task that failed, failed. `output` is the object this
    /// attempt itself produced (a graded evaluation record, a nonzero exit's JSON line), and
    /// `None` when it produced nothing readable.
    Fail { note: String, output: Option<Value> },
    /// The task ran and found its check inapplicable: no evidence either way, and nobody accused.
    /// Declared by the task itself via `"status": "skipped"` in its output — distinct from the
    /// walker's own skip (a task filtered out by substrate caps, which never ran at all).
    Skipped(Value, String),
    /// Transport failure (infra, not the work). Retried, bounded, every attempt visible.
    Transport(TransportFailure),
}

/// What an attempt died on before the task could be judged, and the engine's account of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportFailure {
    pub cause: TransportCause,
    pub note: String,
}

impl TransportFailure {
    pub fn new(cause: TransportCause, note: impl Into<String>) -> Self {
        Self {
            cause,
            note: note.into(),
        }
    }
}

impl std::fmt::Display for TransportFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.note)
    }
}

impl AttemptOutcome {
    /// A measured failure that produced no readable output.
    pub fn fail(note: impl Into<String>) -> Self {
        AttemptOutcome::Fail {
            note: note.into(),
            output: None,
        }
    }

    /// The terminal outcome a task's own reserved `status` field declares, or this outcome
    /// unchanged. A runner that acts on an attempt before returning it — capturing declared
    /// files, discarding a workspace — settles the declared status first, or it acts on a
    /// veto as if it were a pass.
    pub fn settle_declared(self) -> Self {
        let value = match self {
            AttemptOutcome::Pass(value) => value,
            other => return other,
        };
        match DeclaredStatus::of(&value) {
            Some(DeclaredStatus::Skipped) => {
                let note = declared_note(&value, DeclaredStatus::Skipped);
                AttemptOutcome::Skipped(value, note)
            }
            Some(DeclaredStatus::Fail) => {
                let note = declared_note(&value, DeclaredStatus::Fail);
                AttemptOutcome::Fail {
                    note,
                    output: Some(value),
                }
            }
            Some(DeclaredStatus::Pass) | None => AttemptOutcome::Pass(value),
        }
    }
}

pub struct Attempt {
    pub outcome: AttemptOutcome,
    pub cost_usd: f64,
}

impl Attempt {
    pub fn failed(cost_usd: f64, note: String) -> Self {
        Self {
            outcome: AttemptOutcome::fail(note),
            cost_usd,
        }
    }

    pub fn transport(cause: TransportCause, note: impl Into<String>) -> Self {
        Self {
            outcome: AttemptOutcome::Transport(TransportFailure::new(cause, note)),
            cost_usd: 0.0,
        }
    }
}

/// One task of a concurrent dispatch batch (see [`TaskRunner::run_many`]).
pub struct BatchItem<'a> {
    pub task: &'a Task,
    pub attempt: u32,
    pub inputs: BTreeMap<TaskName, Value>,
}

/// Runs one attempt of an `Agent` or `Command` task. The engine implements this against the
/// harness and broker; tests program it directly. `TopK` never reaches the runner: reducers
/// are engine-owned.
pub trait TaskRunner {
    fn run(&mut self, task: &Task, attempt: u32, inputs: &BTreeMap<TaskName, Value>) -> Attempt;

    /// Run several isolation-marked tasks, possibly concurrently. The executor only
    /// batches tasks that are simultaneously ready, so items never depend on each other.
    /// The default runs them serially through [`TaskRunner::run`]; a runner that can
    /// parallelize (per-task worktrees) overrides this.
    /// Stage every declared file produced by the tasks named, into the workspace this task is
    /// about to run in. An error refuses the dispatch rather than running a task whose inputs
    /// are not there.
    fn stage(&mut self, _task: &Task, _producers: &[&Task]) -> Result<(), String> {
        Ok(())
    }

    /// Called once per task after it settles, with whether it passed. A runner that owns
    /// durable state records what a passing task did and drops what a failing one did; the
    /// default keeps nothing, which is what a stateless runner wants.
    fn settled(&mut self, _task: &Task, _passed: bool) {}

    /// Whether a complete declared file set captured in this run is there to stage for `task`.
    /// A runner that keeps no durable state has captured nothing, so the default is `false`.
    fn has_captured_files(&self, _task: &Task) -> bool {
        false
    }

    /// Discard any file set published under `task`'s name. Called when a task settles without
    /// producing evidence, so a set from an earlier run cannot outlive its producer's silence.
    fn drop_captured(&mut self, _task: &Task) {}

    fn run_many(&mut self, batch: &[BatchItem<'_>]) -> Vec<Attempt> {
        batch
            .iter()
            .map(|b| self.run(b.task, b.attempt, &b.inputs))
            .collect()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ExecCfg {
    /// Bounded auto-retry for transport failures (measured failures never retry).
    pub transport_retries: u32,
    /// How long the whole run may take. `None` means unbounded, which the scored loop
    /// tolerates because an operator is watching it; a playbook must supply one.
    pub wall_clock: Option<Duration>,
}

impl Default for ExecCfg {
    fn default() -> Self {
        ExecCfg {
            transport_retries: 2,
            wall_clock: None,
        }
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("task status must be pass|fail|transport|skipped|blocked|truncated, got {got:?}")]
pub struct UnknownTaskStatus {
    pub got: String,
}

/// Terminal task states.
///
/// A `task_result` line carries one of these, and a consumer reading a session log decodes it back
/// through this type rather than matching the token itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pass,
    Fail,
    Transport,
    Skipped,
    Blocked,
    Truncated,
}

impl TaskStatus {
    /// The stable wire token (`SessionEvent::TaskResult.status`).
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Pass => "pass",
            TaskStatus::Fail => "fail",
            TaskStatus::Transport => "transport",
            TaskStatus::Skipped => "skipped",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Truncated => "truncated",
        }
    }

    /// Whether this status is the task having done what it was asked. Everything else is a
    /// failure of some kind, and none of them may read as a pass.
    pub fn passed(self) -> bool {
        self == TaskStatus::Pass
    }
}

impl std::str::FromStr for TaskStatus {
    type Err = UnknownTaskStatus;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pass" => Ok(TaskStatus::Pass),
            "fail" => Ok(TaskStatus::Fail),
            "transport" => Ok(TaskStatus::Transport),
            "skipped" => Ok(TaskStatus::Skipped),
            "blocked" => Ok(TaskStatus::Blocked),
            "truncated" => Ok(TaskStatus::Truncated),
            other => Err(UnknownTaskStatus {
                got: other.to_owned(),
            }),
        }
    }
}

impl TaskState {
    /// The status a settled state reports; `None` while the task is still open.
    pub fn status(self) -> Option<TaskStatus> {
        Some(match self {
            TaskState::Pass => TaskStatus::Pass,
            TaskState::Fail => TaskStatus::Fail,
            TaskState::Skipped => TaskStatus::Skipped,
            TaskState::Transport => TaskStatus::Transport,
            TaskState::Blocked => TaskStatus::Blocked,
            TaskState::Truncated => TaskStatus::Truncated,
            TaskState::Pending | TaskState::Running | TaskState::Fanout => return None,
        })
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct FanoutSummary {
    pub instances: usize,
    pub passed: usize,
    pub failed: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct TaskResult {
    pub status: TaskStatus,
    pub attempts: u32,
    pub cost_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Present only on a mapped node. A node's single status cannot say "two of three survived",
    /// which is exactly what a `join = "passed"` dependent needs to know, so the counts travel
    /// with the result rather than being inferred from its output's shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fanout: Option<FanoutSummary>,
    /// Present exactly when `status` is [`TaskStatus::Blocked`]; `note` is its `Display`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<BlockedReason>,
    /// Present exactly when `status` is [`TaskStatus::Transport`]: what the last attempt died on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<TransportCause>,
}

impl TaskResult {
    /// Whether anything downstream can be built on this result. A plain task contributes when
    /// it passed; a mapped node contributes when any instance passed, which is what makes
    /// `join = "passed"` mean the same thing over a fan-out as over a set of siblings.
    fn contributed(&self) -> bool {
        self.status == TaskStatus::Pass || self.fanout.as_ref().is_some_and(|f| f.passed > 0)
    }
}

impl TaskResult {
    fn blocked(reason: &BlockedReason) -> Self {
        TaskResult {
            blocked: Some(reason.clone()),
            ..Self::undispatched(TaskStatus::Blocked, reason.to_string())
        }
    }

    fn undispatched(status: TaskStatus, note: impl Into<String>) -> Self {
        TaskResult {
            status,
            attempts: 0,
            cost_usd: 0.0,
            output: None,
            note: Some(note.into()),
            fanout: None,
            blocked: None,
            transport: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PlanExit {
    Completed,
    /// A required task can never run on this substrate; nothing was dispatched.
    Truncated {
        task: TaskName,
    },
    /// A required task failed; undispatched tasks were blocked.
    ShortCircuit {
        task: TaskName,
    },
    /// The budget ceiling was hit; undispatched tasks were blocked.
    BudgetExceeded,
    /// The wall-clock ceiling was reached; undispatched tasks were blocked.
    TimeExceeded,
}

/// Why the plan stopped dispatching, fixed on the first halt. Everything still pending settles
/// as blocked on this reason.
enum Halt {
    ShortCircuit(TaskName),
    Budget,
    Time,
}

impl Halt {
    fn exit(&self) -> PlanExit {
        match self {
            Halt::ShortCircuit(task) => PlanExit::ShortCircuit { task: task.clone() },
            Halt::Budget => PlanExit::BudgetExceeded,
            Halt::Time => PlanExit::TimeExceeded,
        }
    }

    fn event(&self) -> PlanEvent {
        match self {
            Halt::ShortCircuit(_) => PlanEvent::RequiredTaskFailed,
            Halt::Budget => PlanEvent::BudgetCeiling,
            Halt::Time => PlanEvent::WallClockCeiling,
        }
    }

    fn blocked(&self) -> BlockedReason {
        match self {
            Halt::ShortCircuit(task) => BlockedReason::RequiredTaskFailed(task.clone()),
            Halt::Budget => BlockedReason::BudgetCeiling,
            Halt::Time => BlockedReason::WallClockCeiling,
        }
    }
}

impl PlanExit {
    /// The shutdown-outcome token this exit reports, in the vocabulary the shutdown event
    /// carries. A run whose graph completed without passing every required task is still an
    /// error, which the caller decides from the verdict rather than from the exit.
    pub fn shutdown_token(&self) -> &'static str {
        match self {
            PlanExit::Completed => "finished",
            PlanExit::BudgetExceeded | PlanExit::TimeExceeded => "budget",
            PlanExit::Truncated { .. } | PlanExit::ShortCircuit { .. } => "error",
        }
    }
}

pub struct PlanOutcome {
    pub valid: bool,
    pub exit: PlanExit,
    pub spent_usd: f64,
    pub results: BTreeMap<TaskName, TaskResult>,
}

/// The tasks that can run on this substrate. Runnability is transitive: `needs` satisfied and
/// every dependency runnable. Computed for the whole plan before anything dispatches, so
/// truncation costs zero spend; the CLI preview folds the same set so it can't drift.
/// Every task reachable backwards from `task`, in topological order.
///
/// Declared files stage from every ancestor, not only direct dependencies: a pipeline whose
/// third stage needs the first stage's artifact is the ordinary case, and requiring each hop to
/// re-emit what it received would reproduce by hand the state files this replaces.
fn ancestors<'a>(plan: &'a ValidPlan, task: &Task) -> Vec<&'a Task> {
    let mut wanted: BTreeSet<&TaskName> = task.depends_on.iter().collect();
    let mut found = Vec::new();
    for candidate in plan.tasks_topo().collect::<Vec<_>>().into_iter().rev() {
        if wanted.contains(&candidate.name) {
            wanted.extend(candidate.depends_on.iter());
            found.push(candidate);
        }
    }
    found.reverse();
    found
}

pub fn runnable_set<'a>(plan: &'a ValidPlan, substrate: &Substrate) -> BTreeSet<&'a TaskName> {
    let mut runnable: BTreeSet<&TaskName> = BTreeSet::new();
    for t in plan.tasks_topo() {
        let deps_runnable = match t.join {
            Join::All => t.depends_on.iter().all(|d| runnable.contains(d)),
            // A lossy join remains runnable if any dependency can run.
            Join::Passed => t.depends_on.iter().any(|d| runnable.contains(d)),
            // A settled join imposes no runnability condition: a reporting tip stays reachable
            // on a substrate that cannot run the half of the graph it reports on.
            Join::Settled => true,
        };
        if substrate.supports(&t.needs) && deps_runnable {
            runnable.insert(&t.name);
        }
    }
    runnable
}

/// Execute a plan; report each terminal result in dispatch order.
pub fn execute(
    plan: &ValidPlan,
    substrate: &Substrate,
    cfg: ExecCfg,
    runner: &mut dyn TaskRunner,
    mut on_result: impl FnMut(&Task, &TaskResult),
) -> Result<PlanOutcome, IllegalTransition> {
    let runnable = runnable_set(plan, substrate);
    let mut plan_machine = PlanMachine::new();
    let mut machines: BTreeMap<TaskName, TaskMachine> = BTreeMap::new();
    if let Some(t) = plan
        .tasks_topo()
        .find(|t| t.required && t.stage == Stage::Iteration && !runnable.contains(&t.name))
    {
        // A truncated DAG can never produce an honest pass: fail fast, dispatch nothing.
        plan_machine.advance(PlanEvent::RequiredTaskUnrunnable)?;
        let mut results = BTreeMap::new();
        for task in plan.tasks_topo() {
            machines
                .entry(task.name.clone())
                .or_default()
                .advance(TaskEvent::PlanTruncated)?;
            let r = TaskResult::undispatched(
                TaskStatus::Truncated,
                format!("required task {} unrunnable on this substrate", t.name),
            );
            runner.drop_captured(task);
            on_result(task, &r);
            results.insert(task.name.clone(), r);
        }
        return Ok(PlanOutcome {
            valid: false,
            exit: PlanExit::Truncated {
                task: t.name.clone(),
            },
            spent_usd: 0.0,
            results,
        });
    }
    plan_machine.advance(PlanEvent::Started)?;

    let started = Instant::now();
    let mut results: BTreeMap<TaskName, TaskResult> = BTreeMap::new();
    let mut spent = 0.0f64;
    let budget = plan.plan().budget.usd;
    let mut halted: Option<Halt> = None;

    // Readiness scan: repeated topo passes. Each pass settles everything decidable
    // without dispatch (halt-blocked, skipped, dep-failed, over-budget), then dispatches
    // the first ready task and restarts, or, when the first ready task is
    // isolation-marked, the whole simultaneously-ready isolated set as one batch. For a
    // plan with no isolated tasks this reproduces the serial topo walk exactly.
    // `gates` is false for one mapped instance: an instance is not a node of the graph, so its
    // failure is folded into the node's result and it is the node that short-circuits or does
    // not. Reporting still happens, because a reader wants the row per item.
    let mut record = |runner: &mut dyn TaskRunner,
                      t: &Task,
                      r: TaskResult,
                      event: TaskEvent,
                      results: &mut BTreeMap<TaskName, TaskResult>,
                      machines: &mut BTreeMap<TaskName, TaskMachine>,
                      halted: &mut Option<Halt>,
                      plan_machine: &mut PlanMachine,
                      gates: bool|
     -> Result<(), IllegalTransition> {
        settle(machines.entry(t.name.clone()).or_default(), &r, event)?;
        let failed = r.status != TaskStatus::Pass;
        if matches!(
            r.status,
            TaskStatus::Skipped | TaskStatus::Transport | TaskStatus::Blocked
        ) {
            runner.drop_captured(t);
        }
        on_result(t, &r);
        results.insert(t.name.clone(), r);
        if gates && failed && t.required && t.stage == Stage::Iteration {
            halt(halted, plan_machine, Halt::ShortCircuit(t.name.clone()))?;
        }
        Ok(())
    };
    loop {
        let mut dispatch: Vec<&Task> = Vec::new();
        for t in plan.tasks_topo() {
            if results.contains_key(&t.name) {
                continue;
            }
            // An epilogue observes the settled main graph. Declaration order is not an
            // ordering primitive, so an independent epilogue task must not race ahead of it.
            if t.stage == Stage::Epilogue
                && plan
                    .tasks_topo()
                    .any(|main| main.stage == Stage::Iteration && !results.contains_key(&main.name))
            {
                continue;
            }
            // The epilogue is what reports a required failure, so a short-circuit is the one
            // halt it outlives. A ceiling still blocks it.
            let reports_the_short_circuit =
                t.stage == Stage::Epilogue && matches!(halted, Some(Halt::ShortCircuit(_)));
            if let Some(halt) = &halted
                && !reports_the_short_circuit
            {
                let reason = halt.blocked();
                let r = TaskResult::blocked(&reason);
                record(
                    &mut *runner,
                    t,
                    r,
                    reason.event(),
                    &mut results,
                    &mut machines,
                    &mut halted,
                    &mut plan_machine,
                    true,
                )?;
                continue;
            }
            if !runnable.contains(&t.name) {
                let r =
                    TaskResult::undispatched(TaskStatus::Skipped, "unrunnable on this substrate");
                record(
                    &mut *runner,
                    t,
                    r,
                    TaskEvent::Unrunnable,
                    &mut results,
                    &mut machines,
                    &mut halted,
                    &mut plan_machine,
                    true,
                )?;
                continue;
            }
            if t.depends_on.iter().any(|d| !results.contains_key(d)) {
                continue;
            }
            let deps_ok = match t.join {
                Join::All => t
                    .depends_on
                    .iter()
                    .all(|d| results.get(d).map(|r| r.status) == Some(TaskStatus::Pass)),
                // Only passing outputs feed a lossy join. A mapped node contributes when any
                // of its instances passed, so a fan-out reads the same as a set of siblings.
                Join::Passed => t
                    .depends_on
                    .iter()
                    .any(|d| results.get(d).is_some_and(TaskResult::contributed)),
                // Every dependency already holds a result by the guard above, so terminality is
                // established and no status blocks the dispatch.
                Join::Settled => true,
            };
            if !deps_ok {
                // Nothing runs on top of a failure (or a skip): advisory failures gate
                // their dependents even though they never gate validity.
                let reason = BlockedReason::DependencyDidNotPass;
                let r = TaskResult::blocked(&reason);
                record(
                    &mut *runner,
                    t,
                    r,
                    reason.event(),
                    &mut results,
                    &mut machines,
                    &mut halted,
                    &mut plan_machine,
                    true,
                )?;
                continue;
            }
            if spent >= budget {
                halt(&mut halted, &mut plan_machine, Halt::Budget)?;
                let reason = BlockedReason::BudgetCeiling;
                let r = TaskResult::blocked(&reason);
                record(
                    &mut *runner,
                    t,
                    r,
                    reason.event(),
                    &mut results,
                    &mut machines,
                    &mut halted,
                    &mut plan_machine,
                    true,
                )?;
                continue;
            }
            // Elapsed time is known continuously, unlike a cost total, so the ceiling is
            // checked before every dispatch rather than after an attempt settles.
            if cfg
                .wall_clock
                .is_some_and(|limit| started.elapsed() >= limit)
            {
                halt(&mut halted, &mut plan_machine, Halt::Time)?;
                let reason = BlockedReason::WallClockCeiling;
                let r = TaskResult::blocked(&reason);
                record(
                    &mut *runner,
                    t,
                    r,
                    reason.event(),
                    &mut results,
                    &mut machines,
                    &mut halted,
                    &mut plan_machine,
                    true,
                )?;
                continue;
            }
            if dispatch.is_empty() {
                dispatch.push(t);
                if t.over.is_some() {
                    // A mapped task is dispatched alone: its own instances are the batch.
                    break;
                }
                if t.isolation.is_none() {
                    // Serial task: dispatch it alone, in strict topo position.
                    break;
                }
                // Isolated: keep scanning for other ready isolated tasks to batch.
            } else if t.isolation.is_some() {
                dispatch.push(t);
            } else {
                // A serial task is a barrier after the ready isolated prefix.
                break;
            }
        }
        let Some(first) = dispatch.first() else {
            // Nothing dispatchable and nothing left to settle: done.
            break;
        };

        // Every dispatched task is staged, in its own right and with its own ancestors: batched
        // isolated tasks do not share an ancestor set, and a task with no producers still has to
        // say so, or the previous dispatch's inputs are still lying there when it runs.
        //
        // A settled entry's `files` key is read off this list, so the list is built first.
        let mut refused = None;
        let mut producers_for: BTreeMap<TaskName, Vec<Task>> = BTreeMap::new();
        let mut inputs_for_dispatch: BTreeMap<TaskName, BTreeMap<TaskName, Value>> =
            BTreeMap::new();
        for t in &dispatch {
            let producers = file_producers(plan, t, &results, &*runner);
            let mut inputs = inputs_for(plan, t, &results, &producers);
            if t.stage == Stage::Epilogue {
                inputs.insert(
                    TaskName(OUTCOME_INPUT.to_string()),
                    main_graph_outcome(plan, &results, halted.as_ref()),
                );
            }
            inputs_for_dispatch.insert(t.name.clone(), inputs);
            let staged: Vec<&Task> = producers.iter().collect();
            if let Err(why) = runner.stage(t, &staged) {
                refused = Some((*t, why));
                break;
            }
            producers_for.insert(t.name.clone(), producers);
        }
        if let Some((t, why)) = refused {
            let reason = BlockedReason::StagingRefused(why);
            let r = TaskResult::blocked(&reason);
            record(
                &mut *runner,
                t,
                r,
                reason.event(),
                &mut results,
                &mut machines,
                &mut halted,
                &mut plan_machine,
                true,
            )?;
            continue;
        }

        if let Some(node) = dispatch.first().filter(|t| t.over.is_some()) {
            let node = *node;
            let base = inputs_for_dispatch.remove(&node.name).unwrap_or_default();
            match fanout_items(node, &results) {
                Err(why) => {
                    let r = TaskResult {
                        status: TaskStatus::Fail,
                        attempts: 1,
                        cost_usd: 0.0,
                        output: None,
                        note: Some(why),
                        fanout: None,
                        blocked: None,
                        transport: None,
                    };
                    record(
                        &mut *runner,
                        node,
                        r,
                        TaskEvent::FanoutItemsInvalid,
                        &mut results,
                        &mut machines,
                        &mut halted,
                        &mut plan_machine,
                        true,
                    )?;
                }
                Ok(keys) => {
                    let instances: Vec<Task> = keys
                        .iter()
                        .map(|key| Task {
                            name: instance_name(&node.name, key),
                            emits_files: node.emits_files.clone(),
                            over: None,
                            max_fanout: None,
                            ..node.clone()
                        })
                        .collect();
                    // An instance is staged under its own name: it runs in its own root, and
                    // the node itself never runs.
                    let producers: Vec<&Task> = producers_for
                        .get(&node.name)
                        .map(|producers| producers.iter().collect())
                        .unwrap_or_default();
                    let refused = instances
                        .iter()
                        .find_map(|instance| runner.stage(instance, &producers).err());
                    if let Some(why) = refused {
                        let reason = BlockedReason::StagingRefused(why);
                        let r = TaskResult::blocked(&reason);
                        record(
                            &mut *runner,
                            node,
                            r,
                            reason.event(),
                            &mut results,
                            &mut machines,
                            &mut halted,
                            &mut plan_machine,
                            true,
                        )?;
                        continue;
                    }
                    machines
                        .entry(node.name.clone())
                        .or_default()
                        .advance(TaskEvent::FannedOut)?;
                    let item_inputs = |key: &String| {
                        let mut inputs = base.clone();
                        inputs.insert(TaskName(ITEM_INPUT.to_string()), Value::String(key.clone()));
                        inputs
                    };
                    // Each instance settles in its own right, so a reader sees one row per item
                    // rather than one row standing for all of them.
                    let mut settled: Vec<(String, TaskResult)> = Vec::new();
                    if node.isolation.is_some() {
                        let batch: Vec<BatchItem<'_>> = instances
                            .iter()
                            .zip(&keys)
                            .map(|(task, key)| BatchItem {
                                task,
                                attempt: 1,
                                inputs: item_inputs(key),
                            })
                            .collect();
                        for instance in &instances {
                            machines
                                .entry(instance.name.clone())
                                .or_default()
                                .advance(TaskEvent::Dispatched)?;
                        }
                        let (batch_results, budget_exceeded) =
                            run_batch_with_retries(batch, cfg, runner, &mut spent, budget);
                        if budget_exceeded {
                            halt(&mut halted, &mut plan_machine, Halt::Budget)?;
                        }
                        for ((task, result, event), key) in batch_results.into_iter().zip(&keys) {
                            runner.settled(task, result.status == TaskStatus::Pass);
                            settled.push((key.clone(), result.clone()));
                            record(
                                &mut *runner,
                                task,
                                result,
                                event,
                                &mut results,
                                &mut machines,
                                &mut halted,
                                &mut plan_machine,
                                false,
                            )?;
                        }
                    } else {
                        // Instances of a shared-workspace node are one serial task each: they
                        // write the same tree and the same result file, so the next one is not
                        // dispatched until this one has settled.
                        for (instance, key) in instances.iter().zip(&keys) {
                            if spent >= budget {
                                halt(&mut halted, &mut plan_machine, Halt::Budget)?;
                            } else if cfg
                                .wall_clock
                                .is_some_and(|limit| started.elapsed() >= limit)
                            {
                                halt(&mut halted, &mut plan_machine, Halt::Time)?;
                            }
                            if let Some(halt) = &halted {
                                let reason = halt.blocked();
                                let r = TaskResult::blocked(&reason);
                                settled.push((key.clone(), r.clone()));
                                record(
                                    &mut *runner,
                                    instance,
                                    r,
                                    reason.event(),
                                    &mut results,
                                    &mut machines,
                                    &mut halted,
                                    &mut plan_machine,
                                    false,
                                )?;
                                continue;
                            }
                            machines
                                .entry(instance.name.clone())
                                .or_default()
                                .advance(TaskEvent::Dispatched)?;
                            let (result, event, budget_exceeded) = run_with_retries(
                                instance,
                                &item_inputs(key),
                                cfg,
                                runner,
                                &mut spent,
                                budget,
                            );
                            if budget_exceeded {
                                halt(&mut halted, &mut plan_machine, Halt::Budget)?;
                            }
                            runner.settled(instance, result.status == TaskStatus::Pass);
                            settled.push((key.clone(), result.clone()));
                            record(
                                &mut *runner,
                                instance,
                                result,
                                event,
                                &mut results,
                                &mut machines,
                                &mut halted,
                                &mut plan_machine,
                                false,
                            )?;
                        }
                    }
                    let folded = fold_instances(settled);
                    let event = if folded.status == TaskStatus::Pass {
                        TaskEvent::InstancesPassed
                    } else {
                        TaskEvent::InstancesFailed
                    };
                    record(
                        &mut *runner,
                        node,
                        folded,
                        event,
                        &mut results,
                        &mut machines,
                        &mut halted,
                        &mut plan_machine,
                        true,
                    )?;
                }
            }
        } else if dispatch.len() == 1 {
            let t = first;
            let inputs = inputs_for_dispatch.remove(&t.name).unwrap_or_default();
            machines
                .entry(t.name.clone())
                .or_default()
                .advance(TaskEvent::Dispatched)?;
            let (result, event, budget_exceeded) = match &t.task {
                TaskKind::TopK { k, direction } => {
                    let reduced = reduce_top_k(&inputs, *k, *direction);
                    let event = if reduced.status == TaskStatus::Pass {
                        TaskEvent::Passed
                    } else {
                        TaskEvent::Failed
                    };
                    (reduced, event, false)
                }
                TaskKind::Agent { .. }
                | TaskKind::Command { .. }
                | TaskKind::Evaluate { .. }
                | TaskKind::Report { .. }
                | TaskKind::Engine { .. } => {
                    run_with_retries(t, &inputs, cfg, runner, &mut spent, budget)
                }
            };
            if budget_exceeded {
                halt(&mut halted, &mut plan_machine, Halt::Budget)?;
            }
            runner.settled(t, result.status == TaskStatus::Pass);
            record(
                &mut *runner,
                t,
                result,
                event,
                &mut results,
                &mut machines,
                &mut halted,
                &mut plan_machine,
                true,
            )?;
        } else {
            // A concurrent batch of independent isolated tasks; results are recorded in
            // declaration order regardless of completion order, so the event stream
            // stays deterministic.
            let batch: Vec<BatchItem<'_>> = dispatch
                .iter()
                .map(|t| BatchItem {
                    task: t,
                    attempt: 1,
                    inputs: inputs_for_dispatch.remove(&t.name).unwrap_or_default(),
                })
                .collect();
            for t in &dispatch {
                machines
                    .entry(t.name.clone())
                    .or_default()
                    .advance(TaskEvent::Dispatched)?;
            }
            let (batch_results, budget_exceeded) =
                run_batch_with_retries(batch, cfg, runner, &mut spent, budget);
            if budget_exceeded {
                halt(&mut halted, &mut plan_machine, Halt::Budget)?;
            }
            for (t, result, event) in batch_results {
                runner.settled(t, result.status == TaskStatus::Pass);
                record(
                    &mut *runner,
                    t,
                    result,
                    event,
                    &mut results,
                    &mut machines,
                    &mut halted,
                    &mut plan_machine,
                    true,
                )?;
            }
        }
    }

    plan_machine.advance(PlanEvent::Settled)?;
    let exit = halted
        .as_ref()
        .map(Halt::exit)
        .unwrap_or(PlanExit::Completed);
    let valid = exit == PlanExit::Completed
        && plan
            .tasks_topo()
            .filter(|t| t.required && t.stage == Stage::Iteration)
            .all(|t| results.get(&t.name).map(|r| r.status) == Some(TaskStatus::Pass));
    Ok(PlanOutcome {
        valid,
        exit,
        spent_usd: spent,
        results,
    })
}

/// Fix the plan's exit on its first halt; later ceilings do not change how it ended.
fn halt(
    halted: &mut Option<Halt>,
    plan_machine: &mut PlanMachine,
    halt: Halt,
) -> Result<(), IllegalTransition> {
    if halted.is_some() {
        return Ok(());
    }
    plan_machine.advance(halt.event())?;
    *halted = Some(halt);
    Ok(())
}

/// Settle one task's machine on its result: the retries its attempts imply, then the event
/// that ends it, which must land on the status the result carries.
fn settle(
    machine: &mut TaskMachine,
    r: &TaskResult,
    event: TaskEvent,
) -> Result<(), IllegalTransition> {
    for _ in 1..r.attempts {
        machine.advance(TaskEvent::TransportRetried)?;
    }
    let state = machine.advance(event)?;
    if state.status() != Some(r.status) {
        return Err(IllegalTransition {
            machine: "task",
            from: format!("{state:?}"),
            event: format!("{event:?} settling as {:?}", r.status),
        });
    }
    Ok(())
}

/// One instance's name, `node[key]`. The key is the item, never its position: a list that comes
/// back reordered or shorter still names the same work the same way, which is what makes a
/// folded result on resume safe to match. Airflow's mapped tasks key on the index, and clearing
/// one after its input shifted reprocesses the wrong item.
fn instance_name(node: &TaskName, key: &str) -> TaskName {
    TaskName(format!("{}[{}]", node.0, key))
}

/// The item key `name` carries when it is an instance of the mapped node `node`. Declared names
/// may not contain a bracket ([`crate::plan::ir::PlanError::BracketInTaskName`]), so the match is
/// exact.
fn instance_key<'a>(node: &TaskName, name: &'a TaskName) -> Option<&'a str> {
    name.0
        .strip_prefix(&node.0)
        .and_then(|rest| rest.strip_prefix('['))
        .and_then(|rest| rest.strip_suffix(']'))
}

/// Whether `name` is an instance of the mapped node `node`. A declared task name may not
/// contain a bracket, so an instance name can never be mistaken for a task of its own.
pub fn is_instance_of(node: &TaskName, name: &TaskName) -> bool {
    instance_key(node, name).is_some()
}

/// Which producers' declared files are staged into `t` for this dispatch.
///
/// A passing ancestor contributes, as it always has. A direct dependency that settled failing
/// contributes to a consumer joining `settled`, and to no one else: staging a failed
/// grandparent's evidence into a consumer that has no envelope entry for it would read as the
/// grandparent having passed.
///
/// A mapped ancestor is expanded into one stand-in per contributing instance, so a consumer is
/// handed `inputs/node[key]/<declared>` and the runner never has to know what `over` is.
fn file_producers(
    plan: &ValidPlan,
    t: &Task,
    results: &BTreeMap<TaskName, TaskResult>,
    runner: &dyn TaskRunner,
) -> Vec<Task> {
    let contributes = |node: &Task, candidate: &Task, r: &TaskResult| match r.status {
        TaskStatus::Pass => true,
        TaskStatus::Fail => {
            t.join == Join::Settled
                && t.depends_on.contains(&node.name)
                && runner.has_captured_files(candidate)
        }
        _ => false,
    };
    let mut producers: Vec<Task> = Vec::new();
    for p in ancestors(plan, t) {
        if p.emits_files.is_empty() {
            continue;
        }
        if p.over.is_some() {
            for (name, r) in results
                .iter()
                .filter(|(name, _)| is_instance_of(&p.name, name))
            {
                let instance = Task {
                    name: name.clone(),
                    over: None,
                    max_fanout: None,
                    ..p.clone()
                };
                if contributes(p, &instance, r) {
                    producers.push(instance);
                }
            }
        } else if let Some(r) = results.get(&p.name)
            && contributes(p, p, r)
        {
            producers.push(p.clone());
        }
    }
    producers
}

/// What a dispatched task reads from its dependencies, as JSON.
///
/// Under `all` and `passed` a dependency contributes its output directly, under its own name. A
/// failure keeps its output, so the selection is by status; a mapped node is exempt, because a
/// fold that settled `Fail` is what `join = "passed"` reduces over.
///
/// Under `settled` every declared dependency contributes one entry carrying its status, note,
/// output and staged-file flag, whatever it settled as. `staged` is the producer list this
/// dispatch is about to hand the runner, which is what makes the `files` flag mean "staged for
/// this consumer, in this run".
fn inputs_for(
    plan: &ValidPlan,
    t: &Task,
    results: &BTreeMap<TaskName, TaskResult>,
    staged: &[Task],
) -> BTreeMap<TaskName, Value> {
    if t.join != Join::Settled {
        return t
            .depends_on
            .iter()
            .filter_map(|d| {
                results
                    .get(d)
                    .filter(|r| {
                        r.fanout.is_some()
                            || matches!(r.status, TaskStatus::Pass | TaskStatus::Skipped)
                    })
                    .and_then(|r| r.output.clone())
                    .map(|v| (d.clone(), v))
            })
            .collect();
    }
    let was_staged = |name: &TaskName| staged.iter().any(|p| &p.name == name);
    t.depends_on
        .iter()
        .filter_map(|d| {
            let r = results.get(d)?;
            // A mapped node is staged under its instances' names, so the node's own flag is true
            // when any of them was.
            let any_staged = was_staged(d) || staged.iter().any(|p| is_instance_of(d, &p.name));
            let mut entry = settled_entry(r, any_staged);
            if plan.get(d).is_some_and(|dep| dep.over.is_some()) {
                let per_instance: serde_json::Map<String, Value> = results
                    .iter()
                    .filter_map(|(name, r)| {
                        let key = instance_key(d, name)?;
                        Some((
                            key.to_owned(),
                            Value::Object(settled_entry(r, was_staged(name))),
                        ))
                    })
                    .collect();
                entry.insert("per_instance".to_string(), Value::Object(per_instance));
            }
            Some((d.clone(), Value::Object(entry)))
        })
        .collect()
}

/// What one settled task reports about itself: the settled entry with the output and the
/// staged-file flag omitted, which is what an epilogue task receives per main-graph task.
fn outcome_entry(r: &TaskResult) -> serde_json::Map<String, Value> {
    let mut entry = serde_json::Map::new();
    entry.insert("status".to_string(), Value::from(r.status.as_str()));
    entry.insert(
        "note".to_string(),
        r.note.clone().map_or(Value::Null, Value::String),
    );
    entry
}

/// One dependency's entry in a settled join's inputs. The entry carries the output rather than
/// being it, so a consumer cannot read a failed dependency's reading without stepping past its
/// status.
fn settled_entry(r: &TaskResult, files: bool) -> serde_json::Map<String, Value> {
    let mut entry = outcome_entry(r);
    entry.insert(
        "output".to_string(),
        r.output.clone().unwrap_or(Value::Null),
    );
    entry.insert("files".to_string(), Value::Bool(files));
    entry
}

/// What an epilogue task is told about the run it reports on: how dispatch ended, and one entry
/// per settled main-graph task. An epilogue task has no dependencies to read, so this is the
/// only channel by which it learns what happened.
fn main_graph_outcome(
    plan: &ValidPlan,
    results: &BTreeMap<TaskName, TaskResult>,
    halted: Option<&Halt>,
) -> Value {
    let tasks: serde_json::Map<String, Value> = plan
        .tasks_topo()
        .filter(|t| t.stage == Stage::Iteration)
        .filter_map(|t| {
            let r = results.get(&t.name)?;
            Some((t.name.0.clone(), Value::Object(outcome_entry(r))))
        })
        .collect();
    serde_json::json!({
        "exit": halted.map(Halt::exit).unwrap_or(PlanExit::Completed).shutdown_token(),
        "tasks": Value::Object(tasks),
    })
}

/// Read the items a mapped task fans out over, or say why it cannot.
fn fanout_items(
    node: &Task,
    results: &BTreeMap<TaskName, TaskResult>,
) -> Result<Vec<String>, String> {
    let Some(reference) = node.over.as_ref() else {
        return Err("not a mapped task".to_string());
    };
    let bound = node.max_fanout.unwrap_or(0) as usize;
    // A failure keeps its output, and a list read off a failed discovery is not a list.
    let produced = results
        .get(&reference.task)
        .filter(|r| r.status == TaskStatus::Pass)
        .and_then(|r| r.output.as_ref())
        .ok_or_else(|| format!("{} produced no output to map over", reference.task))?;
    let list = produced
        .get(&reference.field.0)
        .ok_or_else(|| format!("{reference} is absent from what {} emitted", reference.task))?
        .as_array()
        .ok_or_else(|| format!("{reference} is not a list"))?;
    if list.len() > bound {
        return Err(format!(
            "{reference} has {} items; max_fanout is {bound}. A discovery that returns more than \
             expected is a fact about the discovery, not a licence to run wider",
            list.len()
        ));
    }
    let mut keys: Vec<String> = Vec::with_capacity(list.len());
    for (position, item) in list.iter().enumerate() {
        let key = item.as_str().ok_or_else(|| {
            format!("{reference}[{position}] is not a string; a mapped item is its own key")
        })?;
        if key.is_empty() || key.contains(['[', ']']) {
            return Err(format!(
                "{reference}[{position}] = {key:?} cannot name an instance"
            ));
        }
        if keys.iter().any(|seen| seen == key) {
            return Err(format!(
                "{reference} repeats {key:?}; two instances cannot share a key"
            ));
        }
        keys.push(key.to_owned());
    }
    Ok(keys)
}

/// Fold what the instances did into the one result the graph sees for the node.
///
/// The node is required or advisory as a whole, so every instance has to pass for it to pass. A
/// dependent that wants whatever survived says so with `join = "passed"`, which is the vocabulary
/// that already exists; a fan-out needs no join of its own.
fn fold_instances(settled: Vec<(String, TaskResult)>) -> TaskResult {
    let passed = settled
        .iter()
        .filter(|(_, r)| r.status == TaskStatus::Pass)
        .count();
    let failed = settled.len() - passed;
    let cost: f64 = settled.iter().map(|(_, r)| r.cost_usd).sum();
    // Only passing instances feed a downstream join. A failed instance contributed nothing, and
    // a null under its key reads as "it ran and found nothing", which is a different claim.
    let outputs: serde_json::Map<String, Value> = settled
        .iter()
        .filter(|(_, r)| r.status == TaskStatus::Pass)
        .map(|(key, r)| (key.clone(), r.output.clone().unwrap_or(Value::Null)))
        .collect();
    let note = (failed > 0).then(|| {
        let names: Vec<&str> = settled
            .iter()
            .filter(|(_, r)| r.status != TaskStatus::Pass)
            .map(|(key, _)| key.as_str())
            .collect();
        format!(
            "{failed} of {} instances failed: {}",
            settled.len(),
            names.join(", ")
        )
    });
    TaskResult {
        blocked: None,
        transport: None,
        status: if failed == 0 {
            TaskStatus::Pass
        } else {
            TaskStatus::Fail
        },
        attempts: 1,
        cost_usd: cost,
        output: Some(serde_json::json!({
            "instances": settled.len(),
            "passed": passed,
            "failed": failed,
            "outputs": Value::Object(outputs),
        })),
        note,
        fanout: Some(FanoutSummary {
            instances: settled.len(),
            passed,
            failed,
        }),
    }
}

/// Declared-output check: a passing attempt whose JSON lacks a promised field is a
/// measured failure at the producing task, not a mystery downstream.
/// A task's self-declared `status`, which wins over the boolean `pass` when present.
///
/// Without this a task that RAN could only report pass or fail, so a rung whose check turned out to
/// be inapplicable had to pick between claiming success and accusing the candidate. The GLM A/B rung
/// picked success, and six hours of GPU reported a green attribution rung with no attribution behind
/// it. `skipped` is the honest third answer: ran, measured nothing, blames nobody. `fail` is how a
/// task with no exit code and no `pass` grading vetoes itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclaredStatus {
    Pass,
    Fail,
    Skipped,
}

impl DeclaredStatus {
    /// Every value the engine acts on. Any other value of the field is ignored.
    pub const ALL: [DeclaredStatus; 3] = [
        DeclaredStatus::Pass,
        DeclaredStatus::Fail,
        DeclaredStatus::Skipped,
    ];

    /// The token a task writes into its output's reserved `status` field.
    pub fn as_str(self) -> &'static str {
        match self {
            DeclaredStatus::Pass => "pass",
            DeclaredStatus::Fail => "fail",
            DeclaredStatus::Skipped => "skipped",
        }
    }

    fn of(value: &Value) -> Option<Self> {
        match value.get("status").and_then(Value::as_str)? {
            "pass" => Some(DeclaredStatus::Pass),
            "fail" => Some(DeclaredStatus::Fail),
            "skipped" => Some(DeclaredStatus::Skipped),
            _ => None,
        }
    }
}

/// The note a self-declaring task gave, or a stand-in naming what it declared.
fn declared_note(value: &Value, declared: DeclaredStatus) -> String {
    value
        .get("note")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("task declared status={}", declared.as_str()))
}

fn enforce_emits(task: &Task, outcome: AttemptOutcome) -> AttemptOutcome {
    // Declared fields are owed by a passing attempt only, so a task that settled itself skipped or
    // failed is read before they are checked: checking them would turn an honest skip into a
    // spurious failure and would replace a task's own verdict with a drift complaint.
    let value = match outcome.settle_declared() {
        AttemptOutcome::Pass(value) => value,
        other => return other,
    };
    match task
        .emits
        .iter()
        .find(|field| value.get(&field.0).is_none())
    {
        None => AttemptOutcome::Pass(value),
        Some(missing) => AttemptOutcome::Fail {
            note: format!("output missing declared field {:?}", missing.0),
            output: Some(value),
        },
    }
}

/// A finished attempt as the task's result, or the transport note when it should be retried.
fn settle_attempt(
    outcome: AttemptOutcome,
    attempts: u32,
    cost_usd: f64,
) -> Result<(TaskResult, TaskEvent), TransportFailure> {
    let (status, output, note, event) = match outcome {
        AttemptOutcome::Pass(output) => (TaskStatus::Pass, Some(output), None, TaskEvent::Passed),
        AttemptOutcome::Skipped(output, note) => (
            TaskStatus::Skipped,
            Some(output),
            Some(note),
            TaskEvent::SkippedByTask,
        ),
        AttemptOutcome::Fail { note, output } => {
            (TaskStatus::Fail, output, Some(note), TaskEvent::Failed)
        }
        AttemptOutcome::Transport(failure) => return Err(failure),
    };
    Ok((
        TaskResult {
            status,
            attempts,
            cost_usd,
            output,
            note,
            fanout: None,
            blocked: None,
            transport: None,
        },
        event,
    ))
}

/// A task that died on transport and will not be retried: the retries ran out, or the budget
/// did with retries still allowed.
fn transport_result(
    attempts: u32,
    max_attempts: u32,
    cost_usd: f64,
    failure: &TransportFailure,
    cut_by_budget: bool,
) -> (TaskResult, TaskEvent) {
    let (note, event) = if cut_by_budget {
        (
            format!("budget ceiling reached after transport attempt: {failure}"),
            TaskEvent::TransportCutByBudget,
        )
    } else {
        (
            format!("transport retries exhausted ({max_attempts} attempts): {failure}"),
            TaskEvent::TransportExhausted,
        )
    };
    (
        TaskResult {
            status: TaskStatus::Transport,
            attempts,
            cost_usd,
            output: None,
            note: Some(note),
            fanout: None,
            blocked: None,
            transport: Some(failure.cause),
        },
        event,
    )
}

/// Run one task, retrying transport-class failures up to `cfg.transport_retries` times while
/// the budget allows. The flag says whether this task's spend crossed the budget.
fn run_with_retries(
    t: &Task,
    inputs: &BTreeMap<TaskName, Value>,
    cfg: ExecCfg,
    runner: &mut dyn TaskRunner,
    spent: &mut f64,
    budget: f64,
) -> (TaskResult, TaskEvent, bool) {
    let max_attempts = 1 + cfg.transport_retries;
    let mut attempts = 0;
    let mut cost = 0.0;
    let mut last_transport = TransportFailure::new(TransportCause::Other, String::new());
    while attempts < max_attempts {
        attempts += 1;
        let a = runner.run(t, attempts, inputs);
        cost += a.cost_usd;
        *spent += a.cost_usd;
        match settle_attempt(enforce_emits(t, a.outcome), attempts, cost) {
            Ok((result, event)) => return (result, event, *spent > budget),
            Err(failure) => {
                if *spent > budget || (*spent >= budget && attempts < max_attempts) {
                    let (result, event) =
                        transport_result(attempts, max_attempts, cost, &failure, true);
                    return (result, event, true);
                }
                last_transport = failure;
            }
        }
    }
    let (result, event) = transport_result(attempts, max_attempts, cost, &last_transport, false);
    (result, event, *spent > budget)
}

/// Run a batch of isolated tasks through the runner's parallel path, retrying the transport
/// failures as a smaller wave until every task has a result or retries run out.
fn run_batch_with_retries<'a>(
    batch: Vec<BatchItem<'a>>,
    cfg: ExecCfg,
    runner: &mut dyn TaskRunner,
    spent: &mut f64,
    budget: f64,
) -> (Vec<(&'a Task, TaskResult, TaskEvent)>, bool) {
    let max_attempts = 1 + cfg.transport_retries;
    let mut done: BTreeMap<usize, (TaskResult, TaskEvent)> = BTreeMap::new();
    let mut cost_so_far: Vec<f64> = vec![0.0; batch.len()];
    let order: Vec<&'a Task> = batch.iter().map(|b| b.task).collect();
    let mut wave: Vec<(usize, BatchItem<'a>)> = batch.into_iter().enumerate().collect();
    let mut budget_exceeded = false;

    while !wave.is_empty() {
        let items: Vec<BatchItem<'_>> = wave
            .iter()
            .map(|(_, b)| BatchItem {
                task: b.task,
                attempt: b.attempt,
                inputs: b.inputs.clone(),
            })
            .collect();
        let attempts = runner.run_many(&items);
        let mut attempted = Vec::new();
        for ((idx, item), a) in wave.into_iter().zip(attempts) {
            *spent += a.cost_usd;
            cost_so_far[idx] += a.cost_usd;
            let outcome = enforce_emits(item.task, a.outcome);
            attempted.push((idx, item, outcome));
        }
        let retry_budget_blocked = *spent >= budget;
        budget_exceeded |= *spent > budget;
        let mut next: Vec<(usize, BatchItem<'a>)> = Vec::new();
        for (idx, item, outcome) in attempted {
            match settle_attempt(outcome, item.attempt, cost_so_far[idx]) {
                Ok(settled) => {
                    done.insert(idx, settled);
                }
                Err(failure) => {
                    if item.attempt < max_attempts && !retry_budget_blocked {
                        next.push((
                            idx,
                            BatchItem {
                                task: item.task,
                                attempt: item.attempt + 1,
                                inputs: item.inputs,
                            },
                        ));
                    } else {
                        let cut_by_budget = item.attempt < max_attempts;
                        budget_exceeded |= cut_by_budget;
                        done.insert(
                            idx,
                            transport_result(
                                item.attempt,
                                max_attempts,
                                cost_so_far[idx],
                                &failure,
                                cut_by_budget,
                            ),
                        );
                    }
                }
            }
        }
        wave = next;
    }
    (
        done.into_iter()
            .map(|(idx, (r, event))| (order[idx], r, event))
            .collect(),
        budget_exceeded,
    )
}

/// Engine-built-in fold: keep the k best inputs by their `score` field.
/// Output: `{"kept": [{"task": ..., "score": ...}, ...]}`, best first.
fn reduce_top_k(inputs: &BTreeMap<TaskName, Value>, k: u32, direction: Direction) -> TaskResult {
    let mut scored: Vec<(&TaskName, f64)> = Vec::with_capacity(inputs.len());
    for (name, v) in inputs {
        match v.get("score").and_then(Value::as_f64) {
            Some(s) if s.is_finite() => scored.push((name, s)),
            _ => {
                return TaskResult {
                    status: TaskStatus::Fail,
                    attempts: 1,
                    cost_usd: 0.0,
                    output: None,
                    note: Some(format!("input {name} has no finite numeric `score` field")),
                    fanout: None,
                    blocked: None,
                    transport: None,
                };
            }
        }
    }
    scored.sort_by(|a, b| match direction {
        Direction::Lower => a.1.total_cmp(&b.1),
        Direction::Higher => b.1.total_cmp(&a.1),
    });
    scored.truncate(k as usize);
    let kept: Vec<Value> = scored
        .iter()
        .map(|(n, s)| serde_json::json!({"task": n.0, "score": s}))
        .collect();
    TaskResult {
        status: TaskStatus::Pass,
        attempts: 1,
        cost_usd: 0.0,
        output: Some(serde_json::json!({ "kept": kept })),
        note: None,
        fanout: None,
        blocked: None,
        transport: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_task_status_is_a_settled_state_the_table_reaches() {
        use crate::plan::machine::TASK_TRANSITIONS;
        for status in [
            TaskStatus::Pass,
            TaskStatus::Fail,
            TaskStatus::Skipped,
            TaskStatus::Transport,
            TaskStatus::Blocked,
            TaskStatus::Truncated,
        ] {
            assert!(
                TASK_TRANSITIONS
                    .iter()
                    .any(|(_, _, to)| to.status() == Some(status)),
                "{status:?} is never reached"
            );
        }
        for state in TASK_TRANSITIONS.iter().flat_map(|(a, _, b)| [*a, *b]) {
            assert_eq!(state.settled(), state.status().is_some(), "{state:?}");
        }
    }

    /// The executor's own transitions are in its table; a test that trips one fails here.
    fn execute(
        plan: &ValidPlan,
        substrate: &Substrate,
        cfg: ExecCfg,
        runner: &mut dyn TaskRunner,
        on_result: impl FnMut(&Task, &TaskResult),
    ) -> PlanOutcome {
        crate::plan::exec::execute(plan, substrate, cfg, runner, on_result)
            .expect("an executor transition its table does not list")
    }
    use crate::plan::ir::{Join, Plan, PlanBudget, Stage};

    type Script = BTreeMap<(String, u32), (fn() -> AttemptOutcome, f64)>;

    /// Programmable runner: outcomes per (task, attempt), and a dispatch ledger so tests can
    /// assert what was and wasn't run.
    struct ScriptRunner {
        script: Script,
        default_cost: f64,
        dispatched: Vec<(String, u32)>,
        seen_inputs: BTreeMap<String, Vec<String>>,
        seen_values: BTreeMap<String, BTreeMap<TaskName, Value>>,
        staged: BTreeMap<String, Vec<String>>,
        /// Producers this runner claims to hold a complete captured set for.
        captured: BTreeSet<String>,
        dropped: Vec<String>,
    }

    impl ScriptRunner {
        fn new() -> Self {
            ScriptRunner {
                script: BTreeMap::new(),
                default_cost: 0.1,
                dispatched: Vec::new(),
                seen_inputs: BTreeMap::new(),
                seen_values: BTreeMap::new(),
                staged: BTreeMap::new(),
                captured: BTreeSet::new(),
                dropped: Vec::new(),
            }
        }
        fn on(&mut self, task: &str, attempt: u32, f: fn() -> AttemptOutcome, cost: f64) {
            self.script.insert((task.to_string(), attempt), (f, cost));
        }
        /// The entry a settled consumer received for one dependency.
        fn entry(&self, consumer: &str, dependency: &str) -> &Value {
            &self.seen_values[consumer][&TaskName(dependency.to_string())]
        }
    }

    impl TaskRunner for ScriptRunner {
        fn stage(&mut self, task: &Task, producers: &[&Task]) -> Result<(), String> {
            self.staged.insert(
                task.name.0.clone(),
                producers.iter().map(|p| p.name.0.clone()).collect(),
            );
            Ok(())
        }

        fn has_captured_files(&self, task: &Task) -> bool {
            self.captured.contains(&task.name.0)
        }

        fn drop_captured(&mut self, task: &Task) {
            self.dropped.push(task.name.0.clone());
        }

        fn run(
            &mut self,
            task: &Task,
            attempt: u32,
            inputs: &BTreeMap<TaskName, Value>,
        ) -> Attempt {
            self.dispatched.push((task.name.0.clone(), attempt));
            self.seen_inputs.insert(
                task.name.0.clone(),
                inputs.keys().map(|k| k.0.clone()).collect(),
            );
            self.seen_values.insert(task.name.0.clone(), inputs.clone());
            match self.script.get(&(task.name.0.clone(), attempt)) {
                Some((f, cost)) => Attempt {
                    outcome: f(),
                    cost_usd: *cost,
                },
                None => Attempt {
                    outcome: AttemptOutcome::Pass(serde_json::json!({"score": 1.0})),
                    cost_usd: self.default_cost,
                },
            }
        }
    }

    fn task(name: &str, deps: &[&str], needs: &str, required: bool) -> Task {
        Task {
            name: name.into(),
            task: TaskKind::Command {
                command: "true".into(),
            },
            depends_on: deps.iter().map(|d| (*d).into()).collect(),
            session: None,
            needs: needs.into(),
            required,
            isolation: None,
            join: Join::default(),
            stage: Stage::Iteration,
            emits: Vec::new(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
        }
    }

    fn valid(tasks: Vec<Task>, usd: f64) -> ValidPlan {
        Plan {
            version: 1,
            reason: None,
            budget: PlanBudget { usd },
            tasks,
        }
        .validate()
        .unwrap()
    }

    fn any_substrate() -> Substrate {
        Substrate::default()
    }

    #[test]
    fn chain_passes_and_outputs_flow_downstream() {
        let plan = valid(
            vec![task("a", &[], "any", true), task("b", &["a"], "any", true)],
            10.0,
        );
        let mut r = ScriptRunner::new();
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        assert!(out.valid);
        assert_eq!(out.exit, PlanExit::Completed);
        assert_eq!(out.results[&"b".into()].status, TaskStatus::Pass);
        assert_eq!(r.seen_inputs["b"], vec!["a".to_string()]);
    }

    /// The wall-clock ceiling blocks every task not yet dispatched and invalidates the run.
    /// Unlike the cost ceiling it is checked before dispatch, because elapsed time is known
    /// continuously while a cost total is only known once an attempt finishes.
    #[test]
    fn an_exhausted_wall_clock_ceiling_blocks_dispatch_and_invalidates() {
        let plan = valid(
            vec![
                task("a", &[], "any", true),
                task("b", &["a"], "any", true),
                task("c", &["b"], "any", true),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        let cfg = ExecCfg {
            wall_clock: Some(Duration::ZERO),
            ..ExecCfg::default()
        };
        let out = execute(&plan, &any_substrate(), cfg, &mut r, |_, _| {});
        assert_eq!(out.exit, PlanExit::TimeExceeded);
        assert!(!out.valid, "an exhausted ceiling invalidates whatever ran");
        for name in ["a", "b", "c"] {
            assert_eq!(
                out.results[&name.into()].status,
                TaskStatus::Blocked,
                "{name} should never have been dispatched"
            );
        }
        assert_eq!(out.spent_usd, 0.0);
    }

    /// A ceiling that has not been reached changes nothing, and no ceiling at all is the
    /// scored loop's existing behavior.
    #[test]
    fn a_wall_clock_ceiling_with_time_left_is_invisible() {
        let plan = valid(vec![task("a", &[], "any", true)], 10.0);
        for wall_clock in [None, Some(Duration::from_secs(3600))] {
            let mut r = ScriptRunner::new();
            let cfg = ExecCfg {
                wall_clock,
                ..ExecCfg::default()
            };
            let out = execute(&plan, &any_substrate(), cfg, &mut r, |_, _| {});
            assert!(out.valid, "{wall_clock:?}");
            assert_eq!(out.exit, PlanExit::Completed, "{wall_clock:?}");
        }
    }

    #[test]
    fn required_unrunnable_truncates_with_zero_dispatch() {
        let plan = valid(
            vec![
                task("cpu", &[], "any", true),
                task("gpu", &["cpu"], "fp8-tc", true),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        assert!(!out.valid);
        assert_eq!(out.exit, PlanExit::Truncated { task: "gpu".into() });
        assert_eq!(out.spent_usd, 0.0);
        assert!(
            r.dispatched.is_empty(),
            "truncation must not dispatch anything"
        );
        assert!(
            out.results
                .values()
                .all(|t| t.status == TaskStatus::Truncated)
        );
    }

    #[test]
    fn advisory_unrunnable_is_skipped_and_dependent_of_it_too() {
        let plan = valid(
            vec![
                task("base", &[], "any", true),
                task("trace", &["base"], "ncu", false),
                task("trace-report", &["trace"], "any", false),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        assert!(out.valid, "advisory truncation must not gate validity");
        assert_eq!(out.results[&"trace".into()].status, TaskStatus::Skipped);
        // Runnability is transitive, so the dependent is skipped, not blocked.
        assert_eq!(
            out.results[&"trace-report".into()].status,
            TaskStatus::Skipped
        );
        assert_eq!(r.dispatched.len(), 1);
    }

    #[test]
    fn passed_join_can_ignore_an_unrunnable_advisory_branch() {
        let mut tasks = vec![
            task("score", &[], "any", true),
            task("racecheck", &[], "gpu", false),
        ];
        let mut grade = task("grade", &["score", "racecheck"], "any", true);
        grade.join = Join::Passed;
        tasks.push(grade);
        let plan = valid(tasks, 10.0);
        let mut runner = ScriptRunner::new();
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        );
        assert!(out.valid, "lossy grade remains runnable: {:?}", out.exit);
        assert_eq!(out.results[&"racecheck".into()].status, TaskStatus::Skipped);
        assert_eq!(out.results[&"grade".into()].status, TaskStatus::Pass);
        assert_eq!(runner.seen_inputs["grade"], vec!["score".to_string()]);
    }

    #[test]
    fn passed_join_truncates_when_no_dependency_can_run() {
        let mut tasks = vec![
            task("trace", &[], "ncu", false),
            task("racecheck", &[], "gpu", false),
        ];
        let mut grade = task("grade", &["trace", "racecheck"], "any", true);
        grade.join = Join::Passed;
        tasks.push(grade);
        let plan = valid(tasks, 10.0);
        let mut runner = ScriptRunner::new();
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        );

        assert!(!out.valid);
        assert_eq!(
            out.exit,
            PlanExit::Truncated {
                task: "grade".into()
            }
        );
        assert!(runner.dispatched.is_empty(), "preflight must fail closed");
    }

    #[test]
    fn passed_join_blocks_when_no_dependency_passes() {
        let mut tasks = vec![
            task("trace", &[], "any", false),
            task("racecheck", &[], "any", false),
        ];
        let mut grade = task("grade", &["trace", "racecheck"], "any", true);
        grade.join = Join::Passed;
        tasks.push(grade);
        let plan = valid(tasks, 10.0);
        let mut runner = ScriptRunner::new();
        runner.on("trace", 1, || AttemptOutcome::fail("no trace"), 0.0);
        runner.on("racecheck", 1, || AttemptOutcome::fail("race"), 0.0);
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        );

        assert!(!out.valid);
        assert_eq!(out.results[&"grade".into()].status, TaskStatus::Blocked);
        assert_eq!(
            out.results[&"grade".into()].blocked,
            Some(BlockedReason::DependencyDidNotPass)
        );
        assert_eq!(
            runner.dispatched,
            vec![("trace".to_string(), 1), ("racecheck".to_string(), 1)]
        );
    }

    #[test]
    fn advisory_failure_blocks_dependents_but_not_validity() {
        let plan = valid(
            vec![
                task("base", &[], "any", true),
                task("adv", &["base"], "any", false),
                task("adv-child", &["adv"], "any", false),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on("adv", 1, || AttemptOutcome::fail("nope"), 0.1);
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        assert!(out.valid);
        assert_eq!(out.exit, PlanExit::Completed);
        assert_eq!(out.results[&"adv".into()].status, TaskStatus::Fail);
        assert_eq!(out.results[&"adv-child".into()].status, TaskStatus::Blocked);
    }

    #[test]
    fn required_failure_short_circuits_and_blocks_the_rest() {
        let plan = valid(
            vec![
                task("a", &[], "any", true),
                task("b", &["a"], "any", true),
                task("c", &["b"], "any", true),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on("b", 1, || AttemptOutcome::fail("measured failure"), 0.1);
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        assert!(!out.valid);
        assert_eq!(out.exit, PlanExit::ShortCircuit { task: "b".into() });
        assert_eq!(out.results[&"c".into()].status, TaskStatus::Blocked);
        assert!(!r.dispatched.iter().any(|(n, _)| n == "c"));
    }

    #[test]
    fn transport_retries_bounded_and_measured_failure_never_retries() {
        let plan = valid(
            vec![task("t", &[], "any", false), task("f", &[], "any", false)],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on(
            "t",
            1,
            || AttemptOutcome::Transport(TransportFailure::new(TransportCause::Other, "blip")),
            0.1,
        );
        r.on(
            "t",
            2,
            || AttemptOutcome::Transport(TransportFailure::new(TransportCause::Other, "blip")),
            0.1,
        );
        r.on(
            "t",
            3,
            || AttemptOutcome::Transport(TransportFailure::new(TransportCause::Other, "blip")),
            0.1,
        );
        r.on("f", 1, || AttemptOutcome::fail("wrong answer"), 0.1);
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        let t = &out.results[&"t".into()];
        assert_eq!(t.status, TaskStatus::Transport);
        assert_eq!(t.attempts, 3, "1 attempt + 2 bounded retries");
        assert!(
            (t.cost_usd - 0.3).abs() < 1e-9,
            "every attempt's cost is booked"
        );
        assert_eq!(
            out.results[&"f".into()].attempts,
            1,
            "measured failure never retries"
        );
    }

    #[test]
    fn transport_exhaustion_on_required_task_short_circuits() {
        let plan = valid(
            vec![task("t", &[], "any", true), task("z", &["t"], "any", true)],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on(
            "t",
            1,
            || AttemptOutcome::Transport(TransportFailure::new(TransportCause::Other, "x")),
            0.1,
        );
        r.on(
            "t",
            2,
            || AttemptOutcome::Transport(TransportFailure::new(TransportCause::Other, "x")),
            0.1,
        );
        r.on(
            "t",
            3,
            || AttemptOutcome::Transport(TransportFailure::new(TransportCause::Other, "x")),
            0.1,
        );
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        assert!(!out.valid);
        assert_eq!(out.exit, PlanExit::ShortCircuit { task: "t".into() });
    }

    #[test]
    fn the_last_attempts_transport_cause_lands_on_the_result() {
        let plan = valid(vec![task("t", &[], "any", false)], 10.0);
        let mut r = ScriptRunner::new();
        r.on(
            "t",
            1,
            || AttemptOutcome::Transport(TransportFailure::new(TransportCause::Sandbox, "pull")),
            0.1,
        );
        r.on(
            "t",
            2,
            || AttemptOutcome::Transport(TransportFailure::new(TransportCause::Sandbox, "pull")),
            0.1,
        );
        r.on(
            "t",
            3,
            || {
                AttemptOutcome::Transport(TransportFailure::new(
                    TransportCause::Gateway,
                    "gateway did not become healthy",
                ))
            },
            0.1,
        );
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        let t = &out.results[&"t".into()];
        assert_eq!(t.status, TaskStatus::Transport);
        assert_eq!(t.transport, Some(TransportCause::Gateway));
        assert_eq!(
            t.note.as_deref(),
            Some("transport retries exhausted (3 attempts): gateway did not become healthy")
        );
        let passed = &out.results;
        assert!(
            passed
                .values()
                .all(|r| r.status != TaskStatus::Pass || r.transport.is_none())
        );
    }

    #[test]
    fn budget_fails_closed_mid_plan() {
        let plan = valid(
            vec![task("a", &[], "any", false), task("b", &[], "any", false)],
            0.5,
        );
        let mut r = ScriptRunner::new();
        r.on("a", 1, || AttemptOutcome::Pass(serde_json::json!({})), 0.6);
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        assert!(!out.valid);
        assert_eq!(out.exit, PlanExit::BudgetExceeded);
        assert_eq!(out.results[&"b".into()].status, TaskStatus::Blocked);
        assert!(!r.dispatched.iter().any(|(n, _)| n == "b"));
    }

    #[test]
    fn final_task_overspend_fails_closed() {
        let plan = valid(vec![task("a", &[], "any", true)], 0.5);
        let mut r = ScriptRunner::new();
        r.on("a", 1, || AttemptOutcome::Pass(serde_json::json!({})), 0.6);
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        assert!(!out.valid);
        assert_eq!(out.exit, PlanExit::BudgetExceeded);
        assert_eq!(out.results[&"a".into()].status, TaskStatus::Pass);
    }

    #[test]
    fn exact_budget_on_the_final_task_is_valid() {
        let plan = valid(vec![task("a", &[], "any", true)], 0.5);
        let mut r = ScriptRunner::new();
        r.on("a", 1, || AttemptOutcome::Pass(serde_json::json!({})), 0.5);
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        assert!(out.valid);
        assert_eq!(out.exit, PlanExit::Completed);
    }

    #[test]
    fn transport_retry_stops_when_the_budget_is_consumed() {
        let plan = valid(vec![task("a", &[], "any", true)], 0.5);
        let mut r = ScriptRunner::new();
        r.on(
            "a",
            1,
            || AttemptOutcome::Transport(TransportFailure::new(TransportCause::Other, "blip")),
            0.5,
        );
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        assert_eq!(out.exit, PlanExit::BudgetExceeded);
        assert_eq!(out.results[&"a".into()].attempts, 1);
        assert_eq!(r.dispatched, [("a".to_string(), 1)]);
    }

    #[test]
    fn top_k_reduces_upstream_scores() {
        let mut tasks = vec![
            task("m-a", &[], "any", true),
            task("m-b", &[], "any", true),
            task("m-c", &[], "any", true),
        ];
        tasks.push(Task {
            name: "pick".into(),
            task: TaskKind::TopK {
                k: 2,
                direction: Direction::Lower,
            },
            depends_on: vec!["m-a".into(), "m-b".into(), "m-c".into()],
            session: None,
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::default(),
            stage: Stage::Iteration,
            emits: Vec::new(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
        });
        let plan = valid(tasks, 10.0);
        let mut r = ScriptRunner::new();
        r.on(
            "m-a",
            1,
            || AttemptOutcome::Pass(serde_json::json!({"score": 300.0})),
            0.1,
        );
        r.on(
            "m-b",
            1,
            || AttemptOutcome::Pass(serde_json::json!({"score": 100.0})),
            0.1,
        );
        r.on(
            "m-c",
            1,
            || AttemptOutcome::Pass(serde_json::json!({"score": 200.0})),
            0.1,
        );
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        assert!(out.valid);
        let kept = out.results[&"pick".into()].output.as_ref().unwrap()["kept"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0]["task"], "m-b");
        assert_eq!(kept[1]["task"], "m-c");
    }

    #[test]
    fn top_k_fails_on_scoreless_input() {
        let mut tasks = vec![task("m", &[], "any", false)];
        tasks.push(Task {
            name: "pick".into(),
            task: TaskKind::TopK {
                k: 1,
                direction: Direction::Higher,
            },
            depends_on: vec!["m".into()],
            session: None,
            needs: "any".into(),
            required: false,
            isolation: None,
            join: Join::default(),
            stage: Stage::Iteration,
            emits: Vec::new(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
        });
        let plan = valid(tasks, 10.0);
        let mut r = ScriptRunner::new();
        r.on(
            "m",
            1,
            || AttemptOutcome::Pass(serde_json::json!({"notes": "no score"})),
            0.1,
        );
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        let pick = &out.results[&"pick".into()];
        assert_eq!(pick.status, TaskStatus::Fail);
        assert!(pick.note.as_ref().unwrap().contains("score"));
    }

    #[test]
    fn on_result_fires_per_task_in_dispatch_order() {
        let plan = valid(
            vec![
                task("a", &[], "any", true),
                task("b", &["a"], "any", true),
                task("c", &["b"], "any", true),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on("b", 1, || AttemptOutcome::fail("nope"), 0.1);
        let mut seen: Vec<(String, TaskStatus)> = Vec::new();
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |t, res| seen.push((t.name.0.clone(), res.status)),
        );
        assert_eq!(
            seen,
            vec![
                ("a".to_string(), TaskStatus::Pass),
                ("b".to_string(), TaskStatus::Fail),
                ("c".to_string(), TaskStatus::Blocked),
            ],
            "one event per task, live, in dispatch order"
        );
        assert_eq!(seen.len(), out.results.len(), "callback and fold agree");
    }

    #[test]
    fn isolated_ready_tasks_dispatch_as_one_batch_in_declaration_order() {
        // Three isolated roots + a serial collector. The roots must arrive at the runner
        // as ONE run_many batch; the collector dispatches alone afterwards; on_result
        // order stays declaration-stable regardless of (simulated) completion order.
        let mut tasks: Vec<Task> = ["p-a", "p-b", "p-c"]
            .iter()
            .map(|n| {
                let mut t = task(n, &[], "any", false);
                t.isolation = Some(crate::plan::ir::Isolation::Worktree);
                t
            })
            .collect();
        tasks.push(task("collect", &["p-a", "p-b", "p-c"], "any", false));
        let plan = valid(tasks, 10.0);

        struct BatchRecorder {
            batches: Vec<Vec<String>>,
        }
        impl TaskRunner for BatchRecorder {
            fn run(&mut self, task: &Task, _: u32, _: &BTreeMap<TaskName, Value>) -> Attempt {
                self.batches.push(vec![task.name.0.clone()]);
                Attempt {
                    outcome: AttemptOutcome::Pass(serde_json::json!({})),
                    cost_usd: 0.0,
                }
            }
            fn run_many(&mut self, batch: &[BatchItem<'_>]) -> Vec<Attempt> {
                self.batches
                    .push(batch.iter().map(|b| b.task.name.0.clone()).collect());
                // The executor zips results by position; the runner must answer in order.
                batch
                    .iter()
                    .map(|_| Attempt {
                        outcome: AttemptOutcome::Pass(serde_json::json!({})),
                        cost_usd: 0.0,
                    })
                    .collect()
            }
        }
        let mut r = BatchRecorder {
            batches: Vec::new(),
        };
        let mut seen: Vec<String> = Vec::new();
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |t, _| seen.push(t.name.0.clone()),
        );
        assert!(out.valid);
        assert_eq!(
            r.batches,
            vec![
                vec!["p-a".to_string(), "p-b".into(), "p-c".into()],
                vec!["collect".to_string()],
            ],
            "isolated roots batch together; the serial collector dispatches alone"
        );
        assert_eq!(seen, ["p-a", "p-b", "p-c", "collect"]);
    }

    #[test]
    fn ready_serial_task_is_a_barrier_between_isolated_batches() {
        let mut a = task("a", &[], "any", false);
        a.isolation = Some(crate::plan::ir::Isolation::Worktree);
        let b = task("b", &[], "any", false);
        let mut c = task("c", &[], "any", false);
        c.isolation = Some(crate::plan::ir::Isolation::Worktree);
        let plan = valid(vec![a, b, c], 10.0);
        let mut r = ScriptRunner::new();
        let mut seen = Vec::new();
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |t, _| seen.push(t.name.0.clone()),
        );
        assert!(out.valid);
        assert_eq!(seen, ["a", "b", "c"]);
        assert_eq!(
            r.dispatched,
            [
                ("a".to_string(), 1),
                ("b".to_string(), 1),
                ("c".to_string(), 1)
            ]
        );
    }

    #[test]
    fn concurrent_batch_aggregate_overspend_fails_closed() {
        let mut a = task("a", &[], "any", true);
        a.isolation = Some(crate::plan::ir::Isolation::Worktree);
        let mut b = task("b", &[], "any", true);
        b.isolation = Some(crate::plan::ir::Isolation::Worktree);
        let plan = valid(vec![a, b], 0.5);
        let mut r = ScriptRunner::new();
        r.on("a", 1, || AttemptOutcome::Pass(serde_json::json!({})), 0.4);
        r.on("b", 1, || AttemptOutcome::Pass(serde_json::json!({})), 0.4);
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        assert!(!out.valid);
        assert_eq!(out.exit, PlanExit::BudgetExceeded);
        assert!((out.spent_usd - 0.8).abs() < 1e-9);
    }

    #[test]
    fn passed_join_folds_only_passing_dependencies() {
        // Wide's reducer shape: one candidate fails, the reducer still ranks the rest.
        let mut tasks = vec![
            task("m-ok", &[], "any", false),
            task("m-bad", &[], "any", false),
        ];
        let pick = Task {
            name: "pick".into(),
            task: TaskKind::TopK {
                k: 1,
                direction: Direction::Higher,
            },
            depends_on: vec!["m-ok".into(), "m-bad".into()],
            session: None,
            needs: "any".into(),
            required: false,
            isolation: None,
            join: Join::Passed,
            stage: Stage::Iteration,
            emits: Vec::new(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
        };
        tasks.push(pick);
        let plan = valid(tasks, 10.0);
        let mut r = ScriptRunner::new();
        r.on(
            "m-ok",
            1,
            || AttemptOutcome::Pass(serde_json::json!({"score": 5.0})),
            0.1,
        );
        r.on("m-bad", 1, || AttemptOutcome::fail("broke"), 0.1);
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        let pick = &out.results[&"pick".into()];
        assert_eq!(
            pick.status,
            TaskStatus::Pass,
            "lossy join must not block on the failed dep: {:?}",
            pick.note
        );
        let kept = pick.output.as_ref().unwrap()["kept"].as_array().unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0]["task"], "m-ok");
    }

    /// A retained failure output must not become an input under a lossy join: a consumer that
    /// sees a key for every dependency cannot tell a reading apart from a verdict, and reads a
    /// failed run as green.
    #[test]
    fn a_passed_join_receives_no_entry_for_a_failed_plain_dependency() {
        let mut grade = task("grade", &["ok", "bad"], "any", true);
        grade.join = Join::Passed;
        let plan = valid(
            vec![
                task("ok", &[], "any", false),
                task("bad", &[], "any", false),
                grade,
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on(
            "bad",
            1,
            || AttemptOutcome::Fail {
                note: "measured a regression".to_string(),
                output: Some(serde_json::json!({"pass": false, "score": 12.0})),
            },
            0.1,
        );
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        let bad = &out.results[&"bad".into()];
        assert_eq!(bad.status, TaskStatus::Fail);
        assert_eq!(bad.output.as_ref().unwrap()["score"], 12.0);
        assert_eq!(out.results[&"grade".into()].status, TaskStatus::Pass);
        assert_eq!(r.seen_inputs["grade"], vec!["ok".to_string()]);
    }

    /// The other side of the status filter: a self-declared skip is a reading the task stands
    /// behind, so a lossy join still receives it alongside a passing sibling.
    #[test]
    fn a_passed_join_receives_the_output_of_a_dependency_that_declared_a_skip() {
        let mut grade = task("grade", &["ok", "unmeasured"], "any", true);
        grade.join = Join::Passed;
        let plan = valid(
            vec![
                task("ok", &[], "any", false),
                task("unmeasured", &[], "any", false),
                grade,
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on(
            "unmeasured",
            1,
            || {
                AttemptOutcome::Pass(serde_json::json!({
                    "status": "skipped",
                    "note": "broker rejected toggle 'VLLM_GLM_TOGGLE'"
                }))
            },
            0.1,
        );
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        assert_eq!(
            out.results[&"unmeasured".into()].status,
            TaskStatus::Skipped
        );
        assert_eq!(out.results[&"grade".into()].status, TaskStatus::Pass);
        assert_eq!(
            r.seen_inputs["grade"],
            vec!["ok".to_string(), "unmeasured".to_string()]
        );
    }

    #[test]
    fn all_join_still_blocks_on_a_failed_dependency() {
        // The default join is unchanged by the Passed addition.
        let tasks = vec![
            task("m", &[], "any", false),
            task("child", &["m"], "any", false),
        ];
        let plan = valid(tasks, 10.0);
        let mut r = ScriptRunner::new();
        r.on("m", 1, || AttemptOutcome::fail("broke"), 0.1);
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        assert_eq!(out.results[&"child".into()].status, TaskStatus::Blocked);
    }

    #[test]
    fn batch_transport_failures_retry_bounded() {
        let mut a = task("iso-a", &[], "any", false);
        a.isolation = Some(crate::plan::ir::Isolation::Worktree);
        let mut b = task("iso-b", &[], "any", false);
        b.isolation = Some(crate::plan::ir::Isolation::Worktree);
        let plan = valid(vec![a, b], 10.0);

        // iso-a passes on attempt 1; iso-b transports forever. The retry waves must
        // re-batch only iso-b, and its attempts must be bounded at 1 + retries.
        struct FlakyBatch {
            waves: Vec<Vec<(String, u32)>>,
        }
        impl TaskRunner for FlakyBatch {
            fn run(&mut self, _: &Task, _: u32, _: &BTreeMap<TaskName, Value>) -> Attempt {
                unreachable!("batchable plan must go through run_many");
            }
            fn run_many(&mut self, batch: &[BatchItem<'_>]) -> Vec<Attempt> {
                self.waves.push(
                    batch
                        .iter()
                        .map(|x| (x.task.name.0.clone(), x.attempt))
                        .collect(),
                );
                batch
                    .iter()
                    .map(|x| Attempt {
                        outcome: if x.task.name.0 == "iso-a" {
                            AttemptOutcome::Pass(serde_json::json!({}))
                        } else {
                            AttemptOutcome::Transport(TransportFailure::new(
                                TransportCause::Other,
                                "flaky",
                            ))
                        },
                        cost_usd: 0.1,
                    })
                    .collect()
            }
        }
        let mut r = FlakyBatch { waves: Vec::new() };
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        assert_eq!(
            r.waves,
            vec![
                vec![("iso-a".to_string(), 1), ("iso-b".to_string(), 1)],
                vec![("iso-b".to_string(), 2)],
                vec![("iso-b".to_string(), 3)],
            ],
            "only the transport-failed task re-batches, attempt numbers advance"
        );
        let b = &out.results[&"iso-b".into()];
        assert_eq!(b.status, TaskStatus::Transport);
        assert_eq!(b.attempts, 3);
        assert!((b.cost_usd - 0.3).abs() < 1e-9, "every wave's cost booked");
        assert_eq!(out.results[&"iso-a".into()].status, TaskStatus::Pass);
    }

    fn emitting(name: &str, deps: &[&str], required: bool, emits: &[&str]) -> Task {
        let mut t = task(name, deps, "any", required);
        t.emits = emits
            .iter()
            .map(|f| crate::plan::ir::OutputField((*f).to_string()))
            .collect();
        t
    }

    #[test]
    fn declared_emits_present_passes_and_missing_is_a_measured_failure() {
        let plan = valid(
            vec![
                emitting("ok", &[], false, &["score"]),
                emitting("drifted", &[], false, &["score", "pass"]),
                task("child", &["drifted"], "any", false),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on(
            "ok",
            1,
            || AttemptOutcome::Pass(serde_json::json!({"score": 1.0})),
            0.1,
        );
        r.on(
            "drifted",
            1,
            || AttemptOutcome::Pass(serde_json::json!({"score": 1.0})),
            0.1,
        );
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        assert_eq!(out.results[&"ok".into()].status, TaskStatus::Pass);
        let drifted = &out.results[&"drifted".into()];
        assert_eq!(drifted.status, TaskStatus::Fail);
        assert_eq!(drifted.attempts, 1, "a measured failure never retries");
        assert!(
            drifted.note.as_ref().unwrap().contains("\"pass\""),
            "the note names the missing field: {:?}",
            drifted.note
        );
        assert_eq!(out.results[&"child".into()].status, TaskStatus::Blocked);
    }

    /// Output drift is measured, so the object it drifted from is the evidence of what drifted.
    #[test]
    fn a_task_that_missed_a_declared_field_keeps_the_object_it_did_emit() {
        let plan = valid(
            vec![emitting("drifted", &[], false, &["score", "pass"])],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on(
            "drifted",
            1,
            || AttemptOutcome::Pass(serde_json::json!({"score": 1.0})),
            0.1,
        );
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        let drifted = &out.results[&"drifted".into()];
        assert_eq!(drifted.status, TaskStatus::Fail);
        assert_eq!(drifted.output.as_ref().unwrap()["score"], 1.0);
    }

    #[test]
    fn a_declared_skip_is_not_a_pass_and_owes_no_emits() {
        // The GLM A/B rung's shape: it ran, could not measure, and declares so. Before this it had
        // to claim a pass (green attribution over nothing) or a fail (accusing the candidate for a
        // manifest bug). It also carries no `score`, so the emits check must not fire.
        let plan = valid(vec![emitting("ab-toggle", &[], false, &["score"])], 10.0);
        let mut r = ScriptRunner::new();
        r.on(
            "ab-toggle",
            1,
            || {
                AttemptOutcome::Pass(serde_json::json!({
                    "status": "skipped",
                    "note": "broker rejected toggle 'VLLM_GLM_TOGGLE'"
                }))
            },
            0.1,
        );
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        let ab = &out.results[&"ab-toggle".into()];
        assert_eq!(ab.status, TaskStatus::Skipped, "not Pass, not Fail");
        assert_eq!(ab.attempts, 1, "a declared skip never retries");
        assert!(
            ab.note.as_ref().unwrap().contains("VLLM_GLM_TOGGLE"),
            "the reason survives: {:?}",
            ab.note
        );
        assert!(
            ab.output.is_some(),
            "the record keeps what the task reported"
        );
    }

    #[test]
    fn a_required_task_that_declares_skipped_leaves_the_reading_invalid() {
        // Skipped must never satisfy `valid = every required task ran and passed` — a rung that
        // measured nothing cannot vouch for a candidate.
        let plan = valid(vec![task("terminal", &[], "any", true)], 10.0);
        let mut r = ScriptRunner::new();
        r.on(
            "terminal",
            1,
            || AttemptOutcome::Pass(serde_json::json!({"status": "skipped"})),
            0.1,
        );
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        assert_eq!(out.results[&"terminal".into()].status, TaskStatus::Skipped);
        assert!(!out.valid, "a skipped required task cannot produce a pass");
    }

    #[test]
    fn an_unknown_or_absent_status_keeps_the_boolean_contract() {
        // Back-compat: every gate that emits no `status` behaves exactly as before, and a status
        // this engine does not know is not silently honoured.
        let plan = valid(
            vec![
                emitting("legacy", &[], false, &["score"]),
                emitting("odd", &[], false, &["score"]),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on(
            "legacy",
            1,
            || AttemptOutcome::Pass(serde_json::json!({"score": 1.0})),
            0.1,
        );
        r.on(
            "odd",
            1,
            || AttemptOutcome::Pass(serde_json::json!({"score": 1.0, "status": "banana"})),
            0.1,
        );
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        assert_eq!(out.results[&"legacy".into()].status, TaskStatus::Pass);
        assert_eq!(out.results[&"odd".into()].status, TaskStatus::Pass);
    }

    #[test]
    fn required_emits_violation_short_circuits() {
        let plan = valid(
            vec![
                emitting("gate", &[], true, &["score"]),
                task("rest", &["gate"], "any", true),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on(
            "gate",
            1,
            || AttemptOutcome::Pass(serde_json::json!({"latency_ms": 3})),
            0.1,
        );
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        assert!(!out.valid);
        assert_eq!(
            out.exit,
            PlanExit::ShortCircuit {
                task: "gate".into()
            }
        );
        assert!(!r.dispatched.iter().any(|(n, _)| n == "rest"));
    }

    #[test]
    fn batch_path_enforces_emits_per_item() {
        let mut ok = emitting("iso-ok", &[], false, &["score"]);
        ok.isolation = Some(crate::plan::ir::Isolation::Worktree);
        let mut bad = emitting("iso-bad", &[], false, &["score"]);
        bad.isolation = Some(crate::plan::ir::Isolation::Worktree);
        let plan = valid(vec![ok, bad], 10.0);
        let mut r = ScriptRunner::new();
        r.on(
            "iso-ok",
            1,
            || AttemptOutcome::Pass(serde_json::json!({"score": 2.0})),
            0.1,
        );
        r.on(
            "iso-bad",
            1,
            || AttemptOutcome::Pass(serde_json::json!({})),
            0.1,
        );
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        assert_eq!(out.results[&"iso-ok".into()].status, TaskStatus::Pass);
        let bad = &out.results[&"iso-bad".into()];
        assert_eq!(bad.status, TaskStatus::Fail);
        assert_eq!(bad.attempts, 1);
    }

    #[test]
    fn substrate_caps_enable_gated_tasks() {
        let plan = valid(vec![task("gpu", &[], "fp8-tc", true)], 10.0);
        let mut r = ScriptRunner::new();
        let substrate = Substrate {
            caps: ["fp8-tc".to_string()].into(),
        };
        let out = execute(&plan, &substrate, ExecCfg::default(), &mut r, |_, _| {});
        assert!(out.valid);
        assert_eq!(out.results[&"gpu".into()].status, TaskStatus::Pass);
    }
    /// A runner that records the order it was asked to do things in, so a test can assert on
    /// dispatch shape rather than on the results dispatch happened to produce.
    struct FanoutRunner {
        items: Vec<String>,
        fail: BTreeSet<String>,
        cost: f64,
        log: Vec<String>,
        staged: BTreeMap<String, Vec<String>>,
        seen_inputs: BTreeMap<String, BTreeMap<TaskName, Value>>,
        /// Producers this runner claims to hold a complete captured set for.
        captured: BTreeSet<String>,
    }

    impl FanoutRunner {
        fn new(items: &[&str]) -> Self {
            FanoutRunner {
                items: items.iter().map(|i| (*i).to_string()).collect(),
                fail: BTreeSet::new(),
                cost: 0.0,
                log: Vec::new(),
                staged: BTreeMap::new(),
                seen_inputs: BTreeMap::new(),
                captured: BTreeSet::new(),
            }
        }
    }

    impl TaskRunner for FanoutRunner {
        fn run(
            &mut self,
            task: &Task,
            _attempt: u32,
            inputs: &BTreeMap<TaskName, Value>,
        ) -> Attempt {
            self.log.push(format!("run {}", task.name));
            self.seen_inputs.insert(task.name.0.clone(), inputs.clone());
            if task.name.0 == "discover" {
                return Attempt {
                    outcome: AttemptOutcome::Pass(serde_json::json!({"targets": self.items})),
                    cost_usd: 0.0,
                };
            }
            let item = inputs
                .get(&TaskName(ITEM_INPUT.to_string()))
                .cloned()
                .unwrap_or(Value::Null);
            let outcome = if self.fail.contains(&task.name.0) {
                AttemptOutcome::Fail {
                    note: format!("{} was programmed to fail", task.name),
                    output: Some(serde_json::json!({"item": item, "status": "fail"})),
                }
            } else {
                AttemptOutcome::Pass(serde_json::json!({"item": item, "ran": task.name.0}))
            };
            Attempt {
                outcome,
                cost_usd: self.cost,
            }
        }

        fn run_many(&mut self, batch: &[BatchItem<'_>]) -> Vec<Attempt> {
            self.log.push(format!("batch of {}", batch.len()));
            batch
                .iter()
                .map(|b| self.run(b.task, b.attempt, &b.inputs))
                .collect()
        }

        fn stage(&mut self, task: &Task, producers: &[&Task]) -> Result<(), String> {
            self.staged.insert(
                task.name.0.clone(),
                producers.iter().map(|p| p.name.0.clone()).collect(),
            );
            Ok(())
        }

        fn has_captured_files(&self, task: &Task) -> bool {
            self.captured.contains(&task.name.0)
        }

        fn settled(&mut self, task: &Task, passed: bool) {
            self.log.push(format!("settled {} {passed}", task.name));
        }
    }

    fn mapped_node(name: &str, producer: &str, field: &str, required: bool) -> Task {
        let mut t = task(name, &[producer], "any", required);
        t.over = Some(crate::plan::ir::OutputRef {
            task: producer.into(),
            field: crate::plan::ir::OutputField(field.to_string()),
        });
        t.max_fanout = Some(8);
        t
    }

    /// Instances of a node with no worktree write one workspace and one result file, so they run
    /// one at a time and each settles before the next is dispatched.
    #[test]
    fn shared_workspace_instances_run_and_settle_one_at_a_time() {
        let plan = valid(
            vec![
                task("discover", &[], "any", true),
                mapped_node("audit", "discover", "targets", true),
            ],
            5.0,
        );
        let mut runner = FanoutRunner::new(&["alpha", "beta", "gamma", "delta"]);
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        );
        assert!(out.valid, "{:?}", out.results);
        assert_eq!(
            runner.log,
            vec![
                "run discover".to_string(),
                "settled discover true".to_string(),
                "run audit[alpha]".to_string(),
                "settled audit[alpha] true".to_string(),
                "run audit[beta]".to_string(),
                "settled audit[beta] true".to_string(),
                "run audit[gamma]".to_string(),
                "settled audit[gamma] true".to_string(),
                "run audit[delta]".to_string(),
                "settled audit[delta] true".to_string(),
            ]
        );
        // Each instance received its own item, not whichever sibling ran last.
        let folded = out.results[&"audit".into()].output.clone().unwrap();
        for key in ["alpha", "beta", "gamma", "delta"] {
            assert_eq!(folded["outputs"][key]["item"], key);
            assert_eq!(folded["outputs"][key]["ran"], format!("audit[{key}]"));
        }
    }

    /// An isolated mapped node keeps its worktree-per-instance batch: concurrency is what
    /// isolation buys, and a private tree is what makes it safe.
    #[test]
    fn isolated_instances_still_go_to_the_runner_as_one_batch() {
        let mut node = mapped_node("audit", "discover", "targets", true);
        node.isolation = Some(crate::plan::ir::Isolation::Worktree);
        let plan = valid(vec![task("discover", &[], "any", true), node], 5.0);
        let mut runner = FanoutRunner::new(&["alpha", "beta"]);
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        );
        assert!(out.valid, "{:?}", out.results);
        assert!(
            runner.log.contains(&"batch of 2".to_string()),
            "{:?}",
            runner.log
        );
    }

    /// The batch path keeps a failing instance's object on its own row, and the fold still
    /// reduces over the passing set alone.
    #[test]
    fn a_failing_instance_keeps_its_output_but_stays_out_of_the_fold() {
        let mut node = mapped_node("audit", "discover", "targets", false);
        node.isolation = Some(crate::plan::ir::Isolation::Worktree);
        let plan = valid(vec![task("discover", &[], "any", true), node], 5.0);
        let mut runner = FanoutRunner::new(&["alpha", "beta"]);
        runner.fail.insert("audit[beta]".to_string());
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        );
        assert!(
            runner.log.contains(&"batch of 2".to_string()),
            "{:?}",
            runner.log
        );
        let beta = &out.results[&"audit[beta]".into()];
        assert_eq!(beta.status, TaskStatus::Fail);
        assert_eq!(
            beta.output.as_ref().expect("the failing instance's object"),
            &serde_json::json!({"item": "beta", "status": "fail"})
        );
        let fold = out.results[&"audit".into()]
            .output
            .clone()
            .expect("a mapped node folds an output");
        assert_eq!(fold["outputs"]["alpha"]["ran"], "audit[alpha]");
        assert!(
            fold["outputs"].get("beta").is_none(),
            "the failed instance reached the fold: {fold}"
        );
    }

    /// The budget is checked before every instance, exactly as it is before every serial task:
    /// the ones that never ran are Blocked rows the fold counts as failures.
    #[test]
    fn a_budget_hit_mid_fanout_blocks_the_instances_that_never_ran() {
        let plan = valid(
            vec![
                task("discover", &[], "any", true),
                mapped_node("audit", "discover", "targets", false),
            ],
            0.25,
        );
        let mut runner = FanoutRunner::new(&["alpha", "beta", "gamma", "delta"]);
        runner.cost = 0.15;
        let mut rows: Vec<(String, TaskStatus)> = Vec::new();
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut runner,
            |t, r| rows.push((t.name.0.clone(), r.status)),
        );
        assert_eq!(out.exit, PlanExit::BudgetExceeded);
        assert!(!out.valid);
        assert!(
            !runner.log.contains(&"run audit[gamma]".to_string()),
            "a blocked instance was dispatched anyway: {:?}",
            runner.log
        );
        assert_eq!(
            rows,
            vec![
                ("discover".to_string(), TaskStatus::Pass),
                ("audit[alpha]".to_string(), TaskStatus::Pass),
                ("audit[beta]".to_string(), TaskStatus::Pass),
                ("audit[gamma]".to_string(), TaskStatus::Blocked),
                ("audit[delta]".to_string(), TaskStatus::Blocked),
                ("audit".to_string(), TaskStatus::Fail),
            ]
        );
        let node = &out.results[&"audit".into()];
        let fanout = node.fanout.as_ref().expect("a mapped node folds counts");
        assert_eq!((fanout.instances, fanout.passed, fanout.failed), (4, 2, 2));
    }

    /// A mapped producer reaches a consumer as one stand-in per passing instance: the file
    /// channel says what the join says, and the runner never learns what `over` is.
    #[test]
    fn only_passing_instances_stage_their_files_downstream() {
        let mut node = mapped_node("audit", "discover", "targets", false);
        node.emits_files = vec!["OUT.md".to_string()];
        let mut consumer = task("roundup", &["audit"], "any", true);
        consumer.join = Join::Passed;
        let plan = valid(
            vec![task("discover", &[], "any", true), node, consumer],
            5.0,
        );
        let mut runner = FanoutRunner::new(&["alpha", "beta", "gamma"]);
        runner.fail.insert("audit[beta]".to_string());
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        );
        assert_eq!(out.exit, PlanExit::Completed);
        assert_eq!(out.results[&"roundup".into()].status, TaskStatus::Pass);
        assert_eq!(
            runner.staged["roundup"],
            vec!["audit[alpha]".to_string(), "audit[gamma]".to_string()],
            "the failed instance's files reached a descendant"
        );
    }

    /// The JSON mirror of the staging test above: a fold that settled `Fail` because one instance
    /// failed is still what `join = "passed"` exists to reduce over, so it must reach the reducer
    /// even though a plain task's failure output does not.
    #[test]
    fn a_passed_join_over_a_fail_folded_mapped_node_still_receives_the_fold() {
        let node = mapped_node("audit", "discover", "targets", false);
        let mut consumer = task("roundup", &["audit"], "any", true);
        consumer.join = Join::Passed;
        let plan = valid(
            vec![task("discover", &[], "any", true), node, consumer],
            5.0,
        );
        let mut runner = FanoutRunner::new(&["alpha", "beta"]);
        runner.fail.insert("audit[beta]".to_string());
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        );
        assert_eq!(out.results[&"audit".into()].status, TaskStatus::Fail);
        assert_eq!(out.results[&"roundup".into()].status, TaskStatus::Pass);
        let fold = &runner.seen_inputs["roundup"][&TaskName("audit".to_string())];
        assert_eq!(fold["instances"], 2);
        assert_eq!(fold["passed"], 1);
        assert_eq!(fold["outputs"]["alpha"]["ran"], "audit[alpha]");
    }

    /// A retained failure output is a reading, not a work list. The `over` source has to have
    /// passed, or a mapped node spends on a list read off a discovery that failed.
    #[test]
    fn a_mapped_node_whose_over_source_failed_refuses_to_fan_out() {
        let mut node = mapped_node("audit", "discover", "targets", false);
        node.depends_on = vec!["discover".into(), "sibling".into()];
        node.join = Join::Passed;
        let plan = valid(
            vec![
                task("discover", &[], "any", false),
                task("sibling", &[], "any", false),
                node,
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on(
            "discover",
            1,
            || AttemptOutcome::Fail {
                note: "discovery did not pass".to_string(),
                output: Some(serde_json::json!({"targets": ["alpha", "beta"]})),
            },
            0.1,
        );
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut r,
            |_, _| {},
        );
        let audit = &out.results[&"audit".into()];
        assert_eq!(audit.status, TaskStatus::Fail);
        assert_eq!(
            audit.note.as_deref(),
            Some("discover produced no output to map over")
        );
        assert!(
            !r.dispatched.iter().any(|(n, _)| n.starts_with("audit[")),
            "an instance ran over a failed discovery: {:?}",
            r.dispatched
        );
    }

    /// The same guard read at the source, so it holds for every join that can dispatch a mapped
    /// node: only the status separates a usable list from a retained failure reading.
    #[test]
    fn fanout_items_reads_a_list_only_from_a_source_that_passed() {
        let node = mapped_node("audit", "discover", "targets", false);
        let source = |status| TaskResult {
            status,
            attempts: 1,
            cost_usd: 0.0,
            output: Some(serde_json::json!({"targets": ["alpha"]})),
            note: None,
            fanout: None,
            blocked: None,
            transport: None,
        };
        for status in [
            TaskStatus::Fail,
            TaskStatus::Skipped,
            TaskStatus::Transport,
            TaskStatus::Blocked,
            TaskStatus::Truncated,
        ] {
            let results = BTreeMap::from([(TaskName("discover".to_string()), source(status))]);
            assert_eq!(
                fanout_items(&node, &results),
                Err("discover produced no output to map over".to_string()),
                "{status} was read as a work list"
            );
        }
        let results =
            BTreeMap::from([(TaskName("discover".to_string()), source(TaskStatus::Pass))]);
        assert_eq!(fanout_items(&node, &results), Ok(vec!["alpha".to_string()]));
    }

    fn epilogue(name: &str, deps: &[&str], required: bool) -> Task {
        let mut t = task(name, deps, "any", required);
        t.stage = Stage::Epilogue;
        t
    }

    fn run_plan(plan: &ValidPlan, runner: &mut dyn TaskRunner) -> PlanOutcome {
        execute(
            plan,
            &any_substrate(),
            ExecCfg::default(),
            runner,
            |_, _| {},
        )
    }

    #[test]
    fn an_epilogue_waits_for_every_main_task_even_when_declared_first() {
        let plan = valid(
            vec![
                epilogue("report", &[], true),
                task("a", &[], "any", true),
                task("b", &["a"], "any", true),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        let out = run_plan(&plan, &mut r);
        assert!(out.valid);
        let order: Vec<&str> = r.dispatched.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(order, vec!["a", "b", "report"]);
    }

    #[test]
    fn an_epilogue_runs_after_a_required_failure_and_the_exit_stays_a_short_circuit() {
        let plan = valid(
            vec![
                task("probe", &[], "any", true),
                task("after", &["probe"], "any", true),
                epilogue("report", &[], true),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on("probe", 1, || AttemptOutcome::fail("vetoed"), 0.1);
        let out = run_plan(&plan, &mut r);
        assert_eq!(out.results[&"report".into()].status, TaskStatus::Pass);
        assert_eq!(out.results[&"after".into()].status, TaskStatus::Blocked);
        assert_eq!(
            out.exit,
            PlanExit::ShortCircuit {
                task: "probe".into()
            }
        );
        assert!(!out.valid);
    }

    #[test]
    fn a_required_epilogue_failure_never_changes_the_verdict() {
        let plan = valid(
            vec![task("a", &[], "any", true), epilogue("report", &[], true)],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on("report", 1, || AttemptOutcome::fail("no sink"), 0.1);
        let out = run_plan(&plan, &mut r);
        assert_eq!(out.results[&"report".into()].status, TaskStatus::Fail);
        assert_eq!(out.exit, PlanExit::Completed);
        assert!(out.valid, "an epilogue is advisory by contract");
    }

    /// A failed epilogue task must not gate a second one either: the short-circuit is a
    /// main-graph device, and the epilogue has no verdict to protect.
    #[test]
    fn a_failed_epilogue_task_does_not_short_circuit_a_later_one() {
        let plan = valid(
            vec![
                task("a", &[], "any", true),
                epilogue("first", &[], true),
                epilogue("second", &[], true),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on("first", 1, || AttemptOutcome::fail("no sink"), 0.1);
        let out = run_plan(&plan, &mut r);
        assert_eq!(out.results[&"second".into()].status, TaskStatus::Pass);
        assert_eq!(out.exit, PlanExit::Completed);
        assert!(out.valid);
    }

    #[test]
    fn an_epilogue_is_blocked_once_a_budget_ceiling_ended_the_main_graph() {
        let plan = valid(
            vec![task("a", &[], "any", true), epilogue("report", &[], true)],
            0.05,
        );
        let mut r = ScriptRunner::new();
        let out = run_plan(&plan, &mut r);
        assert_eq!(out.results[&"a".into()].status, TaskStatus::Pass);
        assert_eq!(out.results[&"report".into()].status, TaskStatus::Blocked);
        assert_eq!(out.exit, PlanExit::BudgetExceeded);
        assert!(!out.valid);
        assert!(!r.dispatched.iter().any(|(n, _)| n == "report"));
    }

    #[test]
    fn an_epilogue_is_blocked_once_a_wall_clock_ceiling_ended_the_main_graph() {
        let plan = valid(
            vec![task("a", &[], "any", true), epilogue("report", &[], true)],
            10.0,
        );
        let mut r = ScriptRunner::new();
        let cfg = ExecCfg {
            wall_clock: Some(Duration::ZERO),
            ..ExecCfg::default()
        };
        let out = execute(&plan, &any_substrate(), cfg, &mut r, |_, _| {});
        assert_eq!(out.results[&"report".into()].status, TaskStatus::Blocked);
        assert_eq!(out.exit, PlanExit::TimeExceeded);
        assert!(r.dispatched.is_empty());
    }

    #[test]
    fn an_epilogue_crossing_the_budget_ceiling_does_not_relabel_a_short_circuit() {
        let plan = valid(
            vec![
                task("probe", &[], "any", true),
                epilogue("report", &[], true),
            ],
            0.15,
        );
        let mut r = ScriptRunner::new();
        r.on("probe", 1, || AttemptOutcome::fail("vetoed"), 0.1);
        let out = run_plan(&plan, &mut r);
        assert_eq!(out.results[&"report".into()].status, TaskStatus::Pass);
        assert!(out.spent_usd > 0.15);
        assert_eq!(
            out.exit,
            PlanExit::ShortCircuit {
                task: "probe".into()
            }
        );
    }

    /// The readiness scan's own ceiling check is the second halt here, and the first one wins:
    /// the epilogue is blocked naming the budget, and the run still reports the short-circuit.
    #[test]
    fn a_budget_reached_before_the_epilogue_blocks_it_without_relabelling_the_exit() {
        let plan = valid(
            vec![
                task("probe", &[], "any", true),
                epilogue("report", &[], true),
            ],
            0.1,
        );
        let mut r = ScriptRunner::new();
        r.on("probe", 1, || AttemptOutcome::fail("vetoed"), 0.1);
        let out = run_plan(&plan, &mut r);
        let report = &out.results[&"report".into()];
        assert_eq!(report.status, TaskStatus::Blocked);
        assert_eq!(report.note.as_deref(), Some("budget ceiling reached"));
        assert_eq!(
            out.exit,
            PlanExit::ShortCircuit {
                task: "probe".into()
            }
        );
        assert!(!r.dispatched.iter().any(|(n, _)| n == "report"));
    }

    /// The ceiling is spent by the failing task's own attempt, so the epilogue meets it at the
    /// readiness scan: blocked naming the clock, with the short-circuit still the run's exit.
    #[test]
    fn a_wall_clock_reached_before_the_epilogue_blocks_it_without_relabelling_the_exit() {
        let plan = valid(
            vec![
                task("probe", &[], "any", true),
                epilogue("report", &[], true),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on(
            "probe",
            1,
            || {
                std::thread::sleep(Duration::from_millis(50));
                AttemptOutcome::fail("vetoed")
            },
            0.1,
        );
        let cfg = ExecCfg {
            wall_clock: Some(Duration::from_millis(25)),
            ..ExecCfg::default()
        };
        let out = execute(&plan, &any_substrate(), cfg, &mut r, |_, _| {});
        let report = &out.results[&"report".into()];
        assert_eq!(report.status, TaskStatus::Blocked);
        assert_eq!(report.note.as_deref(), Some("wall-clock ceiling reached"));
        assert_eq!(
            out.exit,
            PlanExit::ShortCircuit {
                task: "probe".into()
            }
        );
        assert!(!r.dispatched.iter().any(|(n, _)| n == "report"));
    }

    #[test]
    fn an_epilogue_batch_crossing_the_budget_ceiling_does_not_relabel_a_short_circuit() {
        let mut first = epilogue("first", &[], true);
        first.isolation = Some(crate::plan::ir::Isolation::Worktree);
        let mut second = epilogue("second", &[], true);
        second.isolation = Some(crate::plan::ir::Isolation::Worktree);
        let plan = valid(vec![task("probe", &[], "any", true), first, second], 0.15);
        let mut r = ScriptRunner::new();
        r.on("probe", 1, || AttemptOutcome::fail("vetoed"), 0.1);
        let out = run_plan(&plan, &mut r);
        for name in ["first", "second"] {
            assert_eq!(out.results[&name.into()].status, TaskStatus::Pass);
        }
        assert!(out.spent_usd > 0.15);
        assert_eq!(
            out.exit,
            PlanExit::ShortCircuit {
                task: "probe".into()
            }
        );
    }

    /// The mapped-node batch is the second site a ceiling crossed after a required failure can
    /// relabel the exit from. A fan-out over another epilogue task crosses no stage, so the
    /// site is reachable: the instances run, spend past the ceiling, and the run still reports
    /// the short-circuit that ended the main graph.
    #[test]
    fn a_mapped_epilogue_batch_crossing_the_budget_ceiling_does_not_relabel_a_short_circuit() {
        let mut fan = mapped_node("fan", "src", "items", true);
        fan.stage = Stage::Epilogue;
        fan.isolation = Some(crate::plan::ir::Isolation::Worktree);
        let plan = valid(
            vec![
                task("probe", &[], "any", true),
                epilogue("src", &[], true),
                fan,
            ],
            0.25,
        );
        let mut r = ScriptRunner::new();
        r.on("probe", 1, || AttemptOutcome::fail("vetoed"), 0.1);
        r.on(
            "src",
            1,
            || AttemptOutcome::Pass(serde_json::json!({"items": ["x", "y"]})),
            0.05,
        );
        let out = run_plan(&plan, &mut r);
        for key in ["x", "y"] {
            let instance: TaskName = format!("fan[{key}]").as_str().into();
            assert_eq!(out.results[&instance].status, TaskStatus::Pass);
        }
        assert!(out.spent_usd > 0.25, "the batch never crossed the ceiling");
        assert_eq!(
            out.exit,
            PlanExit::ShortCircuit {
                task: "probe".into()
            }
        );
    }

    #[test]
    fn a_required_epilogue_the_substrate_cannot_run_does_not_truncate_the_plan() {
        let mut report = epilogue("report", &[], true);
        report.needs = "fp8-tc".into();
        let plan = valid(vec![task("a", &[], "any", true), report], 10.0);
        let mut r = ScriptRunner::new();
        let out = run_plan(&plan, &mut r);
        assert_eq!(out.results[&"a".into()].status, TaskStatus::Pass);
        let report = &out.results[&"report".into()];
        assert_eq!(report.status, TaskStatus::Skipped);
        assert_eq!(report.note.as_deref(), Some("unrunnable on this substrate"));
        assert_eq!(out.exit, PlanExit::Completed);
        assert!(out.valid);
    }

    #[test]
    fn a_required_main_task_the_substrate_cannot_run_truncates_the_epilogue_too() {
        let plan = valid(
            vec![
                task("gpu", &[], "fp8-tc", true),
                epilogue("report", &[], true),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        let out = run_plan(&plan, &mut r);
        assert_eq!(out.exit, PlanExit::Truncated { task: "gpu".into() });
        assert_eq!(out.results[&"report".into()].status, TaskStatus::Truncated);
        assert!(!out.valid);
        assert!(r.dispatched.is_empty());
    }

    fn settled_task(name: &str, deps: &[&str], required: bool) -> Task {
        let mut t = task(name, deps, "any", required);
        t.join = Join::Settled;
        t
    }

    fn status_of(entry: &Value) -> &str {
        entry["status"].as_str().unwrap_or("<not a status token>")
    }

    /// Every terminal status satisfies a settled join and none of them blocks it, including the
    /// two spellings of a skip and a task that was never dispatched at all.
    #[test]
    fn a_settled_join_dispatches_over_a_dependency_in_every_terminal_state() {
        let plan = valid(
            vec![
                task("boom", &[], "any", false),
                task("self_skip", &[], "any", false),
                task("walker_skip", &[], "fp8-tc", false),
                task("flaky", &[], "any", false),
                task("upstream", &[], "any", false),
                task("blocked", &["upstream"], "any", false),
                settled_task(
                    "report",
                    &[
                        "boom",
                        "self_skip",
                        "walker_skip",
                        "flaky",
                        "upstream",
                        "blocked",
                    ],
                    true,
                ),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on(
            "boom",
            1,
            || AttemptOutcome::fail("measured a negative"),
            0.0,
        );
        r.on(
            "self_skip",
            1,
            || AttemptOutcome::Pass(serde_json::json!({"status": "skipped"})),
            0.0,
        );
        r.on(
            "upstream",
            1,
            || AttemptOutcome::fail("measured a negative"),
            0.0,
        );
        for attempt in 1..=3 {
            r.on(
                "flaky",
                attempt,
                || {
                    AttemptOutcome::Transport(TransportFailure::new(
                        TransportCause::Other,
                        "the broker hung up",
                    ))
                },
                0.0,
            );
        }
        let out = run_plan(&plan, &mut r);

        assert_eq!(out.exit, PlanExit::Completed);
        assert!(out.valid, "{:?}", out.results);
        assert_eq!(out.results[&"report".into()].status, TaskStatus::Pass);
        for (dependency, status) in [
            ("boom", "fail"),
            ("self_skip", "skipped"),
            ("walker_skip", "skipped"),
            ("flaky", "transport"),
            ("upstream", "fail"),
            ("blocked", "blocked"),
        ] {
            assert_eq!(
                status_of(r.entry("report", dependency)),
                status,
                "{dependency}"
            );
        }
    }

    /// A settled join governs dependency status and nothing else: a required failure stops
    /// dispatch, and a settled tip is blocked by it like any other task.
    #[test]
    fn a_settled_join_does_not_outlive_a_required_failure() {
        let plan = valid(
            vec![
                task("gate", &[], "any", true),
                settled_task("report", &["gate"], false),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on(
            "gate",
            1,
            || AttemptOutcome::fail("measured a negative"),
            0.0,
        );
        let out = run_plan(&plan, &mut r);

        assert_eq!(
            out.exit,
            PlanExit::ShortCircuit {
                task: "gate".into()
            }
        );
        assert_eq!(out.results[&"report".into()].status, TaskStatus::Blocked);
        assert!(!r.dispatched.iter().any(|(name, _)| name == "report"));
    }

    /// A pack that puts expensive work behind a settled join must not keep spending once the
    /// cost ceiling is gone.
    #[test]
    fn a_settled_join_does_not_outlive_a_budget_halt() {
        let plan = valid(
            vec![
                task("probe", &[], "any", false),
                settled_task("report", &["probe"], false),
            ],
            0.15,
        );
        let mut r = ScriptRunner::new();
        r.on(
            "probe",
            1,
            || AttemptOutcome::Pass(serde_json::json!({"ok": true})),
            0.2,
        );
        let out = run_plan(&plan, &mut r);

        assert_eq!(out.exit, PlanExit::BudgetExceeded);
        assert_eq!(out.results[&"report".into()].status, TaskStatus::Blocked);
        assert!(!r.dispatched.iter().any(|(name, _)| name == "report"));
    }

    #[test]
    fn a_settled_join_does_not_outlive_a_wall_clock_halt() {
        let plan = valid(
            vec![
                task("probe", &[], "any", false),
                settled_task("report", &["probe"], false),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        let cfg = ExecCfg {
            wall_clock: Some(Duration::ZERO),
            ..ExecCfg::default()
        };
        let out = execute(&plan, &any_substrate(), cfg, &mut r, |_, _| {});

        assert_eq!(out.exit, PlanExit::TimeExceeded);
        assert_eq!(out.results[&"report".into()].status, TaskStatus::Blocked);
        assert!(r.dispatched.is_empty());
    }

    /// Runnability is the substrate's question, and a settled join does not inherit its
    /// dependencies' answer: a reporting tip stays reachable on a machine that cannot run the
    /// branch it reports on, so the plan is not truncated.
    #[test]
    fn a_required_settled_tip_survives_an_unrunnable_advisory_branch() {
        let plan = valid(
            vec![
                task("gpu", &[], "fp8-tc", false),
                settled_task("report", &["gpu"], true),
            ],
            10.0,
        );
        assert!(runnable_set(&plan, &any_substrate()).contains(&TaskName("report".into())));

        let mut r = ScriptRunner::new();
        let out = run_plan(&plan, &mut r);
        assert_eq!(out.exit, PlanExit::Completed);
        assert!(out.valid, "{:?}", out.results);
        assert_eq!(out.results[&"gpu".into()].status, TaskStatus::Skipped);
        assert_eq!(status_of(r.entry("report", "gpu")), "skipped");
    }

    /// The reading that says why a task failed reaches the task that reports on it, wrapped in
    /// its status. An `all` consumer of the same failure is still blocked and still receives
    /// nothing: the entry is a settled join's alone.
    #[test]
    fn a_failed_dependencys_reading_reaches_a_settled_consumer_and_no_one_else() {
        let plan = valid(
            vec![
                task("probe", &[], "any", false),
                settled_task("report", &["probe"], false),
                task("deliver", &["probe"], "any", false),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on(
            "probe",
            1,
            || AttemptOutcome::Fail {
                note: "the probe did not separate".to_string(),
                output: Some(serde_json::json!({"pass": false, "margin": 0.02})),
            },
            0.0,
        );
        let out = run_plan(&plan, &mut r);

        assert_eq!(out.results[&"deliver".into()].status, TaskStatus::Blocked);
        assert!(!r.dispatched.iter().any(|(name, _)| name == "deliver"));
        let entry = r.entry("report", "probe");
        assert_eq!(status_of(entry), "fail");
        assert_eq!(entry["note"], "the probe did not separate");
        assert_eq!(entry["output"]["margin"], 0.02);
        assert_eq!(entry["files"], false);
    }

    /// The guard a retained failure output needs: a lossy join over ordinary siblings forwards
    /// the ones that passed and nothing else. Without it a reporter reads a failed sibling's
    /// object under its own name and calls the run green.
    #[test]
    fn a_passed_join_over_one_failed_sibling_receives_only_the_passing_one() {
        let mut roundup = task("roundup", &["good", "bad"], "any", true);
        roundup.join = Join::Passed;
        let plan = valid(
            vec![
                task("good", &[], "any", false),
                task("bad", &[], "any", false),
                roundup,
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on(
            "bad",
            1,
            || AttemptOutcome::Fail {
                note: "measured a negative".to_string(),
                output: Some(serde_json::json!({"pass": false})),
            },
            0.0,
        );
        let out = run_plan(&plan, &mut r);

        assert_eq!(out.results[&"roundup".into()].status, TaskStatus::Pass);
        assert_eq!(r.seen_inputs["roundup"], vec!["good".to_string()]);
    }

    /// A settled dependent of a mapped node waits for the fold and then reads each instance,
    /// including when not one of them passed.
    #[test]
    fn a_settled_consumer_of_a_mapped_node_reads_every_failed_instance() {
        let node = mapped_node("audit", "discover", "targets", false);
        let plan = valid(
            vec![
                task("discover", &[], "any", true),
                node,
                settled_task("roundup", &["audit"], true),
            ],
            5.0,
        );
        let mut runner = FanoutRunner::new(&["alpha", "beta"]);
        runner.fail.insert("audit[alpha]".to_string());
        runner.fail.insert("audit[beta]".to_string());
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        );

        assert_eq!(out.results[&"audit".into()].status, TaskStatus::Fail);
        assert_eq!(out.results[&"roundup".into()].status, TaskStatus::Pass);
        let entry = &runner.seen_inputs["roundup"][&TaskName("audit".to_string())];
        assert_eq!(entry["status"], "fail");
        assert_eq!(entry["output"]["passed"], 0);
        assert_eq!(entry["per_instance"]["alpha"]["status"], "fail");
        assert_eq!(entry["per_instance"]["beta"]["output"]["item"], "beta");
    }

    /// An empty fan-out passes with no instances, and a node that never expanded produces no
    /// instance rows either. The entry's status, not the emptiness, is what tells them apart.
    #[test]
    fn an_empty_per_instance_mapping_is_read_by_the_nodes_own_status() {
        let empty = valid(
            vec![
                task("discover", &[], "any", true),
                mapped_node("audit", "discover", "targets", false),
                settled_task("roundup", &["audit"], true),
            ],
            5.0,
        );
        let mut runner = FanoutRunner::new(&[]);
        let out = execute(
            &empty,
            &any_substrate(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        );
        let entry = &runner.seen_inputs["roundup"][&TaskName("audit".to_string())];
        assert_eq!(out.results[&"audit".into()].status, TaskStatus::Pass);
        assert_eq!(entry["status"], "pass");
        assert_eq!(entry["output"]["instances"], 0);
        assert_eq!(entry["per_instance"], serde_json::json!({}));

        let mut node = mapped_node("audit", "discover", "targets", false);
        node.depends_on.push("gate".into());
        let never_expanded = valid(
            vec![
                task("discover", &[], "any", true),
                task("gate", &[], "any", false),
                node,
                settled_task("roundup", &["audit"], true),
            ],
            5.0,
        );
        let mut runner = FanoutRunner::new(&["alpha"]);
        runner.fail.insert("gate".to_string());
        let out = execute(
            &never_expanded,
            &any_substrate(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        );
        let entry = &runner.seen_inputs["roundup"][&TaskName("audit".to_string())];
        assert_eq!(out.results[&"audit".into()].status, TaskStatus::Blocked);
        assert_eq!(entry["status"], "blocked");
        assert_eq!(entry["output"], Value::Null);
        assert_eq!(entry["per_instance"], serde_json::json!({}));
    }

    /// A failed producer's evidence is staged along declared edges only. Staging a failed
    /// grandparent's file into a consumer with no entry for it would read as the grandparent
    /// having passed.
    #[test]
    fn a_failed_producers_files_reach_a_settled_dependent_and_not_a_settled_descendant() {
        let mut probe = task("probe", &[], "any", false);
        probe.emits_files = vec!["evidence/probe.json".to_string()];
        let plan = valid(
            vec![
                probe,
                settled_task("mid", &["probe"], false),
                settled_task("tip", &["mid"], true),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.captured.insert("probe".to_string());
        r.on(
            "probe",
            1,
            || AttemptOutcome::fail("measured a negative"),
            0.0,
        );
        let out = run_plan(&plan, &mut r);

        assert_eq!(out.results[&"tip".into()].status, TaskStatus::Pass);
        assert_eq!(r.staged["mid"], vec!["probe".to_string()]);
        assert!(
            r.staged["tip"].is_empty(),
            "a failed grandparent's evidence reached a consumer with no entry for it: {:?}",
            r.staged["tip"]
        );
        assert_eq!(r.entry("mid", "probe")["files"], true);
    }

    /// The `files` flag says a set was staged for this consumer in this run, so it is false for
    /// a producer that declares none and false for one whose set the runner does not hold.
    #[test]
    fn the_files_flag_is_false_without_a_set_staged_for_this_consumer() {
        let mut hoard = task("hoard", &[], "any", false);
        hoard.emits_files = vec!["evidence/hoard.json".to_string()];
        let plan = valid(
            vec![
                task("quiet", &[], "any", false),
                hoard,
                settled_task("report", &["quiet", "hoard"], true),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on(
            "quiet",
            1,
            || AttemptOutcome::fail("measured a negative"),
            0.0,
        );
        r.on(
            "hoard",
            1,
            || AttemptOutcome::fail("measured a negative"),
            0.0,
        );
        let out = run_plan(&plan, &mut r);

        assert_eq!(out.results[&"report".into()].status, TaskStatus::Pass);
        assert_eq!(r.entry("report", "quiet")["files"], false);
        assert_eq!(r.entry("report", "hoard")["files"], false);
        assert!(r.staged["report"].is_empty());
    }

    /// One mapped producer is staged per instance whose set exists, and each instance's own flag
    /// says which `inputs/node[key]/` directory is there to read.
    #[test]
    fn a_mapped_producers_files_flag_is_per_instance() {
        let mut node = mapped_node("audit", "discover", "targets", false);
        node.emits_files = vec!["OUT.md".to_string()];
        let plan = valid(
            vec![
                task("discover", &[], "any", true),
                node,
                settled_task("roundup", &["audit"], true),
            ],
            5.0,
        );
        let mut runner = FanoutRunner::new(&["alpha", "beta", "gamma"]);
        runner.fail.insert("audit[beta]".to_string());
        runner.fail.insert("audit[gamma]".to_string());
        runner.captured.insert("audit[beta]".to_string());
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        );

        assert_eq!(out.results[&"roundup".into()].status, TaskStatus::Pass);
        assert_eq!(
            runner.staged["roundup"],
            vec!["audit[alpha]".to_string(), "audit[beta]".to_string()],
            "an instance whose set the runner does not hold was staged anyway"
        );
        let entry = &runner.seen_inputs["roundup"][&TaskName("audit".to_string())];
        assert_eq!(entry["files"], true);
        assert_eq!(entry["per_instance"]["alpha"]["files"], true);
        assert_eq!(entry["per_instance"]["beta"]["files"], true);
        assert_eq!(entry["per_instance"]["gamma"]["files"], false);
    }

    /// An agent turn has no exit code and no `pass` to grade, so `"status": "fail"` is how it
    /// vetoes itself. It settles failing with its object intact, is not retried, and is not
    /// checked against its declared fields: those are owed by a passing attempt only.
    #[test]
    fn a_task_declaring_status_fail_settles_failing_with_its_object_kept() {
        let mut veto = task("veto", &[], "any", false);
        veto.emits = vec![crate::plan::ir::OutputField("separates".to_string())];
        let plan = valid(vec![veto, settled_task("report", &["veto"], true)], 10.0);
        let mut r = ScriptRunner::new();
        r.on(
            "veto",
            1,
            || {
                AttemptOutcome::Pass(
                    serde_json::json!({"status": "fail", "note": "the stimulus does not separate"}),
                )
            },
            0.0,
        );
        let out = run_plan(&plan, &mut r);

        let result = &out.results[&"veto".into()];
        assert_eq!(result.status, TaskStatus::Fail);
        assert_eq!(result.attempts, 1);
        assert_eq!(
            result.note.as_deref(),
            Some("the stimulus does not separate")
        );
        let entry = r.entry("report", "veto");
        assert_eq!(status_of(entry), "fail");
        assert_eq!(entry["output"]["status"], "fail");
    }

    /// A task that published nothing this run must not leave a set from an earlier one standing:
    /// disk state and run state cannot be allowed to disagree about what it produced.
    #[test]
    fn a_task_that_settles_without_evidence_drops_the_set_an_earlier_run_left() {
        let plan = valid(
            vec![
                task("gone", &[], "fp8-tc", false),
                task("flaky", &[], "any", false),
                task("upstream", &[], "any", false),
                task("blocked", &["upstream"], "any", false),
                task("kept", &[], "any", false),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        for attempt in 1..=3 {
            r.on(
                "flaky",
                attempt,
                || {
                    AttemptOutcome::Transport(TransportFailure::new(
                        TransportCause::Other,
                        "the broker hung up",
                    ))
                },
                0.0,
            );
        }
        r.on(
            "upstream",
            1,
            || AttemptOutcome::fail("measured a negative"),
            0.0,
        );
        run_plan(&plan, &mut r);

        assert_eq!(
            r.dropped,
            vec![
                "gone".to_string(),
                "flaky".to_string(),
                "blocked".to_string()
            ]
        );
    }

    /// A truncated graph dispatches nothing at all, so every task in it settles without evidence
    /// and every set an earlier run published goes with them.
    #[test]
    fn a_truncated_graph_drops_the_sets_an_earlier_run_left() {
        let plan = valid(
            vec![
                task("gpu", &[], "fp8-tc", true),
                task("report", &["gpu"], "any", false),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        let out = run_plan(&plan, &mut r);

        assert_eq!(out.exit, PlanExit::Truncated { task: "gpu".into() });
        assert_eq!(r.dropped, vec!["gpu".to_string(), "report".to_string()]);
    }

    /// A settled join is refused a halt exemption, and an epilogue task is granted one. Where a
    /// task is both, the epilogue rule wins: it dispatches over the main graph's failure.
    #[test]
    fn a_settled_epilogue_dispatches_after_a_required_failure() {
        let mut tip = epilogue("tip", &["wrap"], false);
        tip.join = Join::Settled;
        let plan = valid(
            vec![
                task("gate", &[], "any", true),
                epilogue("wrap", &[], false),
                tip,
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on(
            "gate",
            1,
            || AttemptOutcome::fail("measured a negative"),
            0.0,
        );
        r.on("wrap", 1, || AttemptOutcome::fail("teardown refused"), 0.0);
        let out = run_plan(&plan, &mut r);

        assert_eq!(
            out.exit,
            PlanExit::ShortCircuit {
                task: "gate".into()
            }
        );
        assert_eq!(out.results[&"tip".into()].status, TaskStatus::Pass);
        assert_eq!(status_of(r.entry("tip", "wrap")), "fail");
    }

    /// The edge rule holds for a mapped producer too: a failed instance's evidence is staged
    /// into a consumer that declared the node, and into nothing further down.
    #[test]
    fn a_failed_mapped_grandparents_instances_reach_no_settled_descendant() {
        let mut node = mapped_node("audit", "discover", "targets", false);
        node.emits_files = vec!["OUT.md".to_string()];
        let plan = valid(
            vec![
                task("discover", &[], "any", true),
                node,
                settled_task("mid", &["audit"], false),
                settled_task("tip", &["mid"], true),
            ],
            5.0,
        );
        let mut runner = FanoutRunner::new(&["alpha"]);
        runner.fail.insert("audit[alpha]".to_string());
        runner.captured.insert("audit[alpha]".to_string());
        let out = execute(
            &plan,
            &any_substrate(),
            ExecCfg::default(),
            &mut runner,
            |_, _| {},
        );

        assert_eq!(out.results[&"tip".into()].status, TaskStatus::Pass);
        assert_eq!(runner.staged["mid"], vec!["audit[alpha]".to_string()]);
        assert!(
            runner.staged["tip"].is_empty(),
            "a failed grandparent's instance reached a consumer with no entry for it: {:?}",
            runner.staged["tip"]
        );
    }

    /// Failure files ride settled edges only. A lossy join over the same failed dependency is
    /// staged nothing, so a reducer cannot read evidence its inputs do not mention.
    #[test]
    fn a_passed_join_is_staged_nothing_from_a_failed_direct_dependency() {
        let mut probe = task("probe", &[], "any", false);
        probe.emits_files = vec!["evidence/probe.json".to_string()];
        let mut roundup = task("roundup", &["probe", "other"], "any", true);
        roundup.join = Join::Passed;
        let plan = valid(vec![probe, task("other", &[], "any", false), roundup], 10.0);
        let mut r = ScriptRunner::new();
        r.captured.insert("probe".to_string());
        r.on(
            "probe",
            1,
            || AttemptOutcome::fail("measured a negative"),
            0.0,
        );
        let out = run_plan(&plan, &mut r);

        assert_eq!(out.results[&"roundup".into()].status, TaskStatus::Pass);
        assert!(
            r.staged["roundup"].is_empty(),
            "a lossy join was staged a failed dependency's evidence: {:?}",
            r.staged["roundup"]
        );
        assert_eq!(r.seen_inputs["roundup"], vec!["other".to_string()]);
    }

    /// "Nothing" has one spelling in the entry: a dependency the engine recorded no note for
    /// carries a null note, not an empty string, and a dependency with no output carries a null
    /// output.
    #[test]
    fn an_entry_the_engine_recorded_no_note_for_carries_null() {
        let plan = valid(
            vec![
                task("quiet", &[], "any", false),
                settled_task("report", &["quiet"], true),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        let out = run_plan(&plan, &mut r);

        assert_eq!(out.results[&"quiet".into()].note, None);
        let entry = r.entry("report", "quiet");
        assert_eq!(entry["note"], Value::Null);
        assert_eq!(status_of(entry), "pass");
        assert_eq!(entry["files"], false);
    }

    /// A mapped node whose `over` source failed reads nothing and fails naming the source, and
    /// the settled consumer that reports on it sees a node that never expanded.
    #[test]
    fn a_settled_consumer_of_a_node_that_never_expanded_reads_the_refusal() {
        let mut source = task("discover", &[], "any", false);
        source.emits = vec![crate::plan::ir::OutputField("targets".to_string())];
        let mut node = mapped_node("audit", "discover", "targets", false);
        node.join = Join::Settled;
        let plan = valid(
            vec![source, node, settled_task("roundup", &["audit"], true)],
            5.0,
        );
        let mut r = ScriptRunner::new();
        r.on(
            "discover",
            1,
            || AttemptOutcome::Fail {
                note: "discovery did not finish".to_string(),
                output: Some(serde_json::json!({"targets": ["alpha", "beta"]})),
            },
            0.0,
        );
        let out = run_plan(&plan, &mut r);

        let node = &out.results[&"audit".into()];
        assert_eq!(node.status, TaskStatus::Fail);
        assert!(
            node.note
                .as_deref()
                .unwrap_or_default()
                .contains("no output to map over"),
            "{:?}",
            node.note
        );
        assert!(
            !r.dispatched
                .iter()
                .any(|(name, _)| name.starts_with("audit[")),
            "a failed discovery's list was fanned out over: {:?}",
            r.dispatched
        );
        let entry = r.entry("roundup", "audit");
        assert_eq!(status_of(entry), "fail");
        assert_eq!(entry["per_instance"], serde_json::json!({}));
    }

    fn outcome_of(r: &ScriptRunner, task: &str) -> Value {
        r.seen_values[task][&TaskName(OUTCOME_INPUT.to_string())].clone()
    }

    /// An epilogue task has no dependencies to read, so the run's outcome is the only channel
    /// by which it learns what the main graph did. Every main-graph task settled, whatever it
    /// settled as, and no epilogue task appears in the graph it reports on.
    #[test]
    fn an_epilogue_task_reads_the_completed_runs_outcome() {
        let plan = valid(
            vec![
                task("build", &[], "any", true),
                task("check", &["build"], "any", false),
                epilogue("report", &[], true),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on(
            "check",
            1,
            || AttemptOutcome::fail("measured a negative"),
            0.1,
        );
        let out = run_plan(&plan, &mut r);

        assert_eq!(out.exit, PlanExit::Completed);
        assert!(out.valid, "an advisory failure does not change the verdict");
        let outcome = outcome_of(&r, "report");
        assert_eq!(outcome["exit"], "finished");
        assert_eq!(outcome["tasks"]["build"]["status"], "pass");
        assert_eq!(outcome["tasks"]["build"]["note"], Value::Null);
        assert_eq!(outcome["tasks"]["check"]["status"], "fail");
        assert_eq!(outcome["tasks"]["check"]["note"], "measured a negative");
        assert_eq!(
            outcome["tasks"].as_object().map(|t| t.len()),
            Some(2),
            "an epilogue task is not part of the main graph it reports on"
        );
        assert!(
            outcome["tasks"]["build"].get("output").is_none()
                && outcome["tasks"]["build"].get("files").is_none(),
            "the epilogue entry is the settled entry minus output and files"
        );
        assert!(
            !r.seen_inputs["check"].contains(&OUTCOME_INPUT.to_string()),
            "a main-graph task was given the run's outcome"
        );
    }

    /// The failure path is the one the epilogue exists for: it dispatches over a short-circuit,
    /// and the outcome names how dispatch stopped and what every task it left behind settled as.
    #[test]
    fn an_epilogue_task_reads_the_short_circuit_that_ended_the_main_graph() {
        let plan = valid(
            vec![
                task("probe", &[], "any", true),
                task("after", &["probe"], "any", true),
                epilogue("report", &[], true),
            ],
            10.0,
        );
        let mut r = ScriptRunner::new();
        r.on("probe", 1, || AttemptOutcome::fail("vetoed"), 0.1);
        let out = run_plan(&plan, &mut r);

        assert_eq!(
            out.exit,
            PlanExit::ShortCircuit {
                task: "probe".into()
            }
        );
        let outcome = outcome_of(&r, "report");
        assert_eq!(outcome["exit"], "error");
        assert_eq!(outcome["tasks"]["probe"]["status"], "fail");
        assert_eq!(outcome["tasks"]["probe"]["note"], "vetoed");
        assert_eq!(outcome["tasks"]["after"]["status"], "blocked");
        assert_eq!(
            outcome["tasks"]["after"]["note"],
            "required task probe failed"
        );
    }
}
