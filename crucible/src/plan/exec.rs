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

use crate::plan::ir::{Direction, Join, Stage, Task, TaskKind, TaskName, ValidPlan};

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
    /// Measured failure. Never retried: a task that failed, failed.
    Fail(String),
    /// The task ran and found its check inapplicable: no evidence either way, and nobody accused.
    /// Declared by the task itself via `"status": "skipped"` in its output — distinct from the
    /// walker's own skip (a task filtered out by substrate caps, which never ran at all).
    Skipped(Value, String),
    /// Transport failure (infra, not the work). Retried, bounded, every attempt visible.
    Transport(String),
}

pub struct Attempt {
    pub outcome: AttemptOutcome,
    pub cost_usd: f64,
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

    /// Whether a complete declared file set from `task`'s latest settle is there to stage. A
    /// runner with no durable state has nothing to withhold, so the default is `true`.
    fn has_captured_files(&self, _task: &Task) -> bool {
        true
    }

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
    fn undispatched(status: TaskStatus, note: impl Into<String>) -> Self {
        TaskResult {
            status,
            attempts: 0,
            cost_usd: 0.0,
            output: None,
            note: Some(note.into()),
            fanout: None,
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
) -> PlanOutcome {
    let runnable = runnable_set(plan, substrate);
    if let Some(t) = plan
        .tasks_topo()
        .find(|t| t.required && !runnable.contains(&t.name))
    {
        // A truncated DAG can never produce an honest pass: fail fast, dispatch nothing.
        let mut results = BTreeMap::new();
        for task in plan.tasks_topo() {
            let r = TaskResult::undispatched(
                TaskStatus::Truncated,
                format!("required task {} unrunnable on this substrate", t.name),
            );
            on_result(task, &r);
            results.insert(task.name.clone(), r);
        }
        return PlanOutcome {
            valid: false,
            exit: PlanExit::Truncated {
                task: t.name.clone(),
            },
            spent_usd: 0.0,
            results,
        };
    }

    let started = Instant::now();
    let mut results: BTreeMap<TaskName, TaskResult> = BTreeMap::new();
    let mut spent = 0.0f64;
    let budget = plan.plan().budget.usd;
    let mut halted: Option<PlanExit> = None;

    // Readiness scan: repeated topo passes. Each pass settles everything decidable
    // without dispatch (halt-blocked, skipped, dep-failed, over-budget), then dispatches
    // the first ready task and restarts, or, when the first ready task is
    // isolation-marked, the whole simultaneously-ready isolated set as one batch. For a
    // plan with no isolated tasks this reproduces the serial topo walk exactly.
    // `gates` is false for one mapped instance: an instance is not a node of the graph, so its
    // failure is folded into the node's result and it is the node that short-circuits or does
    // not. Reporting still happens, because a reader wants the row per item.
    let mut record = |t: &Task,
                      r: TaskResult,
                      results: &mut BTreeMap<TaskName, TaskResult>,
                      halted: &mut Option<PlanExit>,
                      gates: bool| {
        let failed = r.status != TaskStatus::Pass;
        on_result(t, &r);
        results.insert(t.name.clone(), r);
        if gates && failed && t.required && t.stage == Stage::Iteration && halted.is_none() {
            *halted = Some(PlanExit::ShortCircuit {
                task: t.name.clone(),
            });
        }
    };
    loop {
        let mut dispatch: Vec<&Task> = Vec::new();
        for t in plan.tasks_topo() {
            if results.contains_key(&t.name) {
                continue;
            }
            // An epilogue observes the settled main graph. Declaration order is not an ordering
            // primitive, so do not let an independent epilogue task race ahead of main work.
            if t.stage == Stage::Epilogue
                && plan
                    .tasks_topo()
                    .any(|main| main.stage == Stage::Iteration && !results.contains_key(&main.name))
            {
                continue;
            }
            let report_after_failure =
                t.stage == Stage::Epilogue && matches!(halted, Some(PlanExit::ShortCircuit { .. }));
            if let Some(exit) = &halted
                && !report_after_failure
            {
                let why = match exit {
                    PlanExit::ShortCircuit { task } => format!("required task {task} failed"),
                    PlanExit::BudgetExceeded => "budget ceiling reached".to_string(),
                    PlanExit::TimeExceeded => "wall-clock ceiling reached".to_string(),
                    _ => "halted".to_string(),
                };
                let r = TaskResult::undispatched(TaskStatus::Blocked, why);
                record(t, r, &mut results, &mut halted, true);
                continue;
            }
            if !runnable.contains(&t.name) {
                let r =
                    TaskResult::undispatched(TaskStatus::Skipped, "unrunnable on this substrate");
                record(t, r, &mut results, &mut halted, true);
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
            };
            if !deps_ok {
                // Nothing runs on top of a failure (or a skip): advisory failures gate
                // their dependents even though they never gate validity.
                let r = TaskResult::undispatched(TaskStatus::Blocked, "dependency did not pass");
                record(t, r, &mut results, &mut halted, true);
                continue;
            }
            if spent >= budget {
                halted.get_or_insert(PlanExit::BudgetExceeded);
                let r = TaskResult::undispatched(TaskStatus::Blocked, "budget ceiling reached");
                record(t, r, &mut results, &mut halted, true);
                continue;
            }
            // Elapsed time is known continuously, unlike a cost total, so the ceiling is
            // checked before every dispatch rather than after an attempt settles.
            if cfg
                .wall_clock
                .is_some_and(|limit| started.elapsed() >= limit)
            {
                halted.get_or_insert(PlanExit::TimeExceeded);
                let r = TaskResult::undispatched(TaskStatus::Blocked, "wall-clock ceiling reached");
                record(t, r, &mut results, &mut halted, true);
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

        let inputs_for = |t: &Task, results: &BTreeMap<TaskName, TaskResult>| {
            t.depends_on
                .iter()
                .filter_map(|d| {
                    results
                        .get(d)
                        .and_then(|r| r.output.clone())
                        .map(|v| (d.clone(), v))
                })
                .collect::<BTreeMap<TaskName, Value>>()
        };

        // Passing ancestors contribute to an ordinary task. An epilogue additionally receives
        // every main-graph task's files, including the explicitly captured evidence of a failed
        // one; that file channel does not turn the failed task into a contributing JSON
        // dependency. A withheld set (the failure left its promised files incomplete) is the
        // runner's to report, so the producer is dropped here rather than staged empty.
        //
        // A mapped ancestor is expanded into one stand-in per passing instance, so a consumer is
        // handed `inputs/node[key]/<declared>` and the runner never has to know what `over` is.
        let file_producers = |t: &Task,
                              results: &BTreeMap<TaskName, TaskResult>,
                              runner: &dyn TaskRunner| {
            let mut producers: Vec<Task> = Vec::new();
            let mut candidates: Vec<&Task> = ancestors(plan, t);
            if t.stage == Stage::Epilogue {
                candidates.extend(plan.tasks_topo().filter(|p| p.stage == Stage::Iteration));
            }
            let contributes = |p: &Task, r: &TaskResult| {
                r.status == TaskStatus::Pass
                    || (t.stage == Stage::Epilogue
                        && r.status == TaskStatus::Fail
                        && p.capture_on_failure
                        && runner.has_captured_files(p))
            };
            for p in candidates {
                if p.emits_files.is_empty() {
                    continue;
                }
                if p.over.is_some() {
                    producers.extend(
                        results
                            .iter()
                            .filter(|(name, r)| contributes(p, r) && is_instance_of(&p.name, name))
                            .map(|(name, _)| Task {
                                name: name.clone(),
                                over: None,
                                max_fanout: None,
                                ..p.clone()
                            }),
                    );
                } else if results.get(&p.name).is_some_and(|r| contributes(p, r)) {
                    producers.push(p.clone());
                }
            }
            producers
        };

        // Every dispatched task is staged, in its own right and with its own ancestors: batched
        // isolated tasks do not share an ancestor set, and a task with no producers still has to
        // say so, or the previous dispatch's inputs are still lying there when it runs.
        let mut refused = None;
        for t in &dispatch {
            let producers = file_producers(t, &results, &*runner);
            let producers: Vec<&Task> = producers.iter().collect();
            if let Err(why) = runner.stage(t, &producers) {
                refused = Some((*t, why));
                break;
            }
        }
        if let Some((t, why)) = refused {
            let r = TaskResult::undispatched(TaskStatus::Blocked, why);
            record(t, r, &mut results, &mut halted, true);
            continue;
        }

        if let Some(node) = dispatch.first().filter(|t| t.over.is_some()) {
            let node = *node;
            let base = inputs_for(node, &results);
            match fanout_items(node, &results) {
                Err(why) => {
                    let r = TaskResult {
                        status: TaskStatus::Fail,
                        attempts: 1,
                        cost_usd: 0.0,
                        output: None,
                        note: Some(why),
                        fanout: None,
                    };
                    record(node, r, &mut results, &mut halted, true);
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
                    let producers = file_producers(node, &results, &*runner);
                    let producers: Vec<&Task> = producers.iter().collect();
                    let refused = instances
                        .iter()
                        .find_map(|instance| runner.stage(instance, &producers).err());
                    if let Some(why) = refused {
                        let r = TaskResult::undispatched(TaskStatus::Blocked, why);
                        record(node, r, &mut results, &mut halted, true);
                        continue;
                    }
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
                        let (batch_results, budget_exceeded) =
                            run_batch_with_retries(batch, cfg, runner, &mut spent, budget);
                        if budget_exceeded {
                            halted.get_or_insert(PlanExit::BudgetExceeded);
                        }
                        for ((task, result), key) in batch_results.into_iter().zip(&keys) {
                            runner.settled(task, result.status == TaskStatus::Pass);
                            settled.push((key.clone(), result.clone()));
                            record(task, result, &mut results, &mut halted, false);
                        }
                    } else {
                        // Instances of a shared-workspace node are one serial task each: they
                        // write the same tree and the same result file, so the next one is not
                        // dispatched until this one has settled.
                        for (instance, key) in instances.iter().zip(&keys) {
                            if halted.is_none() {
                                if spent >= budget {
                                    halted = Some(PlanExit::BudgetExceeded);
                                } else if cfg
                                    .wall_clock
                                    .is_some_and(|limit| started.elapsed() >= limit)
                                {
                                    halted = Some(PlanExit::TimeExceeded);
                                }
                            }
                            if let Some(exit) = &halted {
                                let why = match exit {
                                    PlanExit::BudgetExceeded => "budget ceiling reached",
                                    PlanExit::TimeExceeded => "wall-clock ceiling reached",
                                    _ => "halted",
                                };
                                let r = TaskResult::undispatched(TaskStatus::Blocked, why);
                                settled.push((key.clone(), r.clone()));
                                record(instance, r, &mut results, &mut halted, false);
                                continue;
                            }
                            let (result, budget_exceeded) = run_with_retries(
                                instance,
                                &item_inputs(key),
                                cfg,
                                runner,
                                &mut spent,
                                budget,
                            );
                            if budget_exceeded {
                                halted.get_or_insert(PlanExit::BudgetExceeded);
                            }
                            runner.settled(instance, result.status == TaskStatus::Pass);
                            settled.push((key.clone(), result.clone()));
                            record(instance, result, &mut results, &mut halted, false);
                        }
                    }
                    record(
                        node,
                        fold_instances(settled),
                        &mut results,
                        &mut halted,
                        true,
                    );
                }
            }
        } else if dispatch.len() == 1 {
            let t = first;
            let inputs = inputs_for(t, &results);
            let (result, budget_exceeded) = match &t.task {
                TaskKind::TopK { k, direction } => (reduce_top_k(&inputs, *k, *direction), false),
                TaskKind::Agent { .. }
                | TaskKind::Command { .. }
                | TaskKind::Evaluate { .. }
                | TaskKind::Report { .. }
                | TaskKind::Engine { .. } => {
                    run_with_retries(t, &inputs, cfg, runner, &mut spent, budget)
                }
            };
            if budget_exceeded {
                halted.get_or_insert(PlanExit::BudgetExceeded);
            }
            runner.settled(t, result.status == TaskStatus::Pass);
            record(t, result, &mut results, &mut halted, true);
        } else {
            // A concurrent batch of independent isolated tasks; results are recorded in
            // declaration order regardless of completion order, so the event stream
            // stays deterministic.
            let batch: Vec<BatchItem<'_>> = dispatch
                .iter()
                .map(|t| BatchItem {
                    task: t,
                    attempt: 1,
                    inputs: inputs_for(t, &results),
                })
                .collect();
            let (batch_results, budget_exceeded) =
                run_batch_with_retries(batch, cfg, runner, &mut spent, budget);
            if budget_exceeded {
                halted.get_or_insert(PlanExit::BudgetExceeded);
            }
            for (t, result) in batch_results {
                runner.settled(t, result.status == TaskStatus::Pass);
                record(t, result, &mut results, &mut halted, true);
            }
        }
    }

    let exit = halted.unwrap_or(PlanExit::Completed);
    let valid = exit == PlanExit::Completed
        && plan
            .tasks_topo()
            .filter(|t| t.required && t.stage == Stage::Iteration)
            .all(|t| results.get(&t.name).map(|r| r.status) == Some(TaskStatus::Pass));
    PlanOutcome {
        valid,
        exit,
        spent_usd: spent,
        results,
    }
}

/// The reserved input a mapped instance receives its own item under. Reserved like the
/// epilogue's kept-candidate input: a task may not declare a dependency by this name.
pub const ITEM_INPUT: &str = "item";

/// One instance's name, `node[key]`. The key is the item, never its position: a list that comes
/// back reordered or shorter still names the same work the same way, which is what makes a
/// folded result on resume safe to match. Airflow's mapped tasks key on the index, and clearing
/// one after its input shifted reprocesses the wrong item.
fn instance_name(node: &TaskName, key: &str) -> TaskName {
    TaskName(format!("{}[{}]", node.0, key))
}

/// Whether `name` is an instance of the mapped node `node`. Declared names may not contain a
/// bracket ([`crate::plan::ir::PlanError::BracketInTaskName`]), so the match is exact.
fn is_instance_of(node: &TaskName, name: &TaskName) -> bool {
    name.0
        .strip_prefix(&node.0)
        .and_then(|rest| rest.strip_prefix('['))
        .is_some_and(|rest| rest.ends_with(']'))
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
    let produced = results
        .get(&reference.task)
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
/// it. `skipped` is the honest third answer: ran, measured nothing, blames nobody.
fn declared_status(value: &Value) -> Option<&str> {
    value.get("status").and_then(Value::as_str)
}

fn enforce_emits(task: &Task, outcome: AttemptOutcome) -> AttemptOutcome {
    let AttemptOutcome::Pass(value) = &outcome else {
        return outcome;
    };
    // A skipped task produces no evidence, so its declared emits are not owed. Checking them would
    // turn an honest skip into a spurious failure.
    if declared_status(value) == Some("skipped") {
        let note = value
            .get("note")
            .and_then(Value::as_str)
            .unwrap_or("task declared status=skipped")
            .to_string();
        let AttemptOutcome::Pass(value) = outcome else {
            unreachable!("guarded by the let-else above")
        };
        return AttemptOutcome::Skipped(value, note);
    }
    match task
        .emits
        .iter()
        .find(|field| value.get(&field.0).is_none())
    {
        None => outcome,
        Some(missing) => {
            AttemptOutcome::Fail(format!("output missing declared field {:?}", missing.0))
        }
    }
}

fn run_with_retries(
    t: &Task,
    inputs: &BTreeMap<TaskName, Value>,
    cfg: ExecCfg,
    runner: &mut dyn TaskRunner,
    spent: &mut f64,
    budget: f64,
) -> (TaskResult, bool) {
    let max_attempts = 1 + cfg.transport_retries;
    let mut attempts = 0;
    let mut cost = 0.0;
    let mut last_transport_note = String::new();
    while attempts < max_attempts {
        attempts += 1;
        let a = runner.run(t, attempts, inputs);
        cost += a.cost_usd;
        *spent += a.cost_usd;
        match enforce_emits(t, a.outcome) {
            AttemptOutcome::Pass(output) => {
                return (
                    TaskResult {
                        status: TaskStatus::Pass,
                        attempts,
                        cost_usd: cost,
                        output: Some(output),
                        note: None,
                        fanout: None,
                    },
                    *spent > budget,
                );
            }
            AttemptOutcome::Skipped(output, note) => {
                return (
                    TaskResult {
                        status: TaskStatus::Skipped,
                        attempts,
                        cost_usd: cost,
                        output: Some(output),
                        note: Some(note),
                        fanout: None,
                    },
                    *spent > budget,
                );
            }
            AttemptOutcome::Fail(note) => {
                return (
                    TaskResult {
                        status: TaskStatus::Fail,
                        attempts,
                        cost_usd: cost,
                        output: None,
                        note: Some(note),
                        fanout: None,
                    },
                    *spent > budget,
                );
            }
            AttemptOutcome::Transport(note) => {
                if *spent > budget || (*spent >= budget && attempts < max_attempts) {
                    return (
                        TaskResult {
                            status: TaskStatus::Transport,
                            attempts,
                            cost_usd: cost,
                            output: None,
                            note: Some(format!(
                                "budget ceiling reached after transport attempt: {note}"
                            )),
                            fanout: None,
                        },
                        true,
                    );
                }
                last_transport_note = note;
            }
        }
    }
    (
        TaskResult {
            status: TaskStatus::Transport,
            attempts,
            cost_usd: cost,
            output: None,
            note: Some(format!(
                "transport retries exhausted ({max_attempts} attempts): {last_transport_note}"
            )),
            fanout: None,
        },
        *spent > budget,
    )
}

/// Run a concurrent batch through [`TaskRunner::run_many`], re-batching the
/// transport-failed subset until everything is terminal or retries are exhausted:
/// the same bounded-retry semantics as the serial path, one wave per attempt number.
/// Returns results paired with their tasks in the batch's (declaration) order.
fn run_batch_with_retries<'a>(
    batch: Vec<BatchItem<'a>>,
    cfg: ExecCfg,
    runner: &mut dyn TaskRunner,
    spent: &mut f64,
    budget: f64,
) -> (Vec<(&'a Task, TaskResult)>, bool) {
    let max_attempts = 1 + cfg.transport_retries;
    // (batch position, accumulated cost) so the final fold restores declaration order.
    let mut done: BTreeMap<usize, TaskResult> = BTreeMap::new();
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
            match outcome {
                AttemptOutcome::Pass(output) => {
                    done.insert(
                        idx,
                        TaskResult {
                            status: TaskStatus::Pass,
                            attempts: item.attempt,
                            cost_usd: cost_so_far[idx],
                            output: Some(output),
                            note: None,
                            fanout: None,
                        },
                    );
                }
                AttemptOutcome::Skipped(output, note) => {
                    done.insert(
                        idx,
                        TaskResult {
                            status: TaskStatus::Skipped,
                            attempts: item.attempt,
                            cost_usd: cost_so_far[idx],
                            output: Some(output),
                            note: Some(note),
                            fanout: None,
                        },
                    );
                }
                AttemptOutcome::Fail(note) => {
                    done.insert(
                        idx,
                        TaskResult {
                            status: TaskStatus::Fail,
                            attempts: item.attempt,
                            cost_usd: cost_so_far[idx],
                            output: None,
                            note: Some(note),
                            fanout: None,
                        },
                    );
                }
                AttemptOutcome::Transport(note) => {
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
                        if item.attempt < max_attempts && retry_budget_blocked {
                            budget_exceeded = true;
                        }
                        done.insert(
                            idx,
                            TaskResult {
                                status: TaskStatus::Transport,
                                attempts: item.attempt,
                                cost_usd: cost_so_far[idx],
                                output: None,
                                note: Some(if item.attempt < max_attempts {
                                    format!(
                                        "budget ceiling reached after transport attempt: {note}"
                                    )
                                } else {
                                    format!(
                                        "transport retries exhausted ({max_attempts} attempts): {note}"
                                    )
                                }),
                                fanout: None,
                            },
                        );
                    }
                }
            }
        }
        wave = next;
    }
    (
        done.into_iter().map(|(idx, r)| (order[idx], r)).collect(),
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::ir::{Join, Plan, PlanBudget, Stage};

    type Script = BTreeMap<(String, u32), (fn() -> AttemptOutcome, f64)>;

    /// Programmable runner: outcomes per (task, attempt), and a dispatch ledger so tests can
    /// assert what was and wasn't run.
    struct ScriptRunner {
        script: Script,
        default_cost: f64,
        dispatched: Vec<(String, u32)>,
        seen_inputs: BTreeMap<String, Vec<String>>,
    }

    impl ScriptRunner {
        fn new() -> Self {
            ScriptRunner {
                script: BTreeMap::new(),
                default_cost: 0.1,
                dispatched: Vec::new(),
                seen_inputs: BTreeMap::new(),
            }
        }
        fn on(&mut self, task: &str, attempt: u32, f: fn() -> AttemptOutcome, cost: f64) {
            self.script.insert((task.to_string(), attempt), (f, cost));
        }
    }

    impl TaskRunner for ScriptRunner {
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
            capture_on_failure: false,
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
        runner.on("trace", 1, || AttemptOutcome::Fail("no trace".into()), 0.0);
        runner.on("racecheck", 1, || AttemptOutcome::Fail("race".into()), 0.0);
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
        r.on("adv", 1, || AttemptOutcome::Fail("nope".into()), 0.1);
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
        r.on(
            "b",
            1,
            || AttemptOutcome::Fail("measured failure".into()),
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
        r.on("t", 1, || AttemptOutcome::Transport("blip".into()), 0.1);
        r.on("t", 2, || AttemptOutcome::Transport("blip".into()), 0.1);
        r.on("t", 3, || AttemptOutcome::Transport("blip".into()), 0.1);
        r.on("f", 1, || AttemptOutcome::Fail("wrong answer".into()), 0.1);
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
        r.on("t", 1, || AttemptOutcome::Transport("x".into()), 0.1);
        r.on("t", 2, || AttemptOutcome::Transport("x".into()), 0.1);
        r.on("t", 3, || AttemptOutcome::Transport("x".into()), 0.1);
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
        r.on("a", 1, || AttemptOutcome::Transport("blip".into()), 0.5);
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
            capture_on_failure: false,
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
            capture_on_failure: false,
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
        r.on("b", 1, || AttemptOutcome::Fail("nope".into()), 0.1);
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
            capture_on_failure: false,
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
        r.on("m-bad", 1, || AttemptOutcome::Fail("broke".into()), 0.1);
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

    #[test]
    fn all_join_still_blocks_on_a_failed_dependency() {
        // The default join is unchanged by the Passed addition.
        let tasks = vec![
            task("m", &[], "any", false),
            task("child", &["m"], "any", false),
        ];
        let plan = valid(tasks, 10.0);
        let mut r = ScriptRunner::new();
        r.on("m", 1, || AttemptOutcome::Fail("broke".into()), 0.1);
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
                            AttemptOutcome::Transport("flaky".into())
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
    }

    impl FanoutRunner {
        fn new(items: &[&str]) -> Self {
            FanoutRunner {
                items: items.iter().map(|i| (*i).to_string()).collect(),
                fail: BTreeSet::new(),
                cost: 0.0,
                log: Vec::new(),
                staged: BTreeMap::new(),
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
                AttemptOutcome::Fail(format!("{} was programmed to fail", task.name))
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

    /// The epilogue tests: a runner that remembers what it was asked to stage and answers for
    /// whether a failed producer's captured set exists, the two things the executor asks of it.
    struct StagingRunner {
        inner: ScriptRunner,
        captured: bool,
        staged: BTreeMap<String, Vec<String>>,
    }

    impl StagingRunner {
        fn new(captured: bool) -> Self {
            StagingRunner {
                inner: ScriptRunner::new(),
                captured,
                staged: BTreeMap::new(),
            }
        }
    }

    impl TaskRunner for StagingRunner {
        fn run(
            &mut self,
            task: &Task,
            attempt: u32,
            inputs: &BTreeMap<TaskName, Value>,
        ) -> Attempt {
            self.inner.run(task, attempt, inputs)
        }
        fn stage(&mut self, task: &Task, producers: &[&Task]) -> Result<(), String> {
            self.staged.insert(
                task.name.0.clone(),
                producers.iter().map(|p| p.name.0.clone()).collect(),
            );
            Ok(())
        }
        fn has_captured_files(&self, _task: &Task) -> bool {
            self.captured
        }
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
        r.on("probe", 1, || AttemptOutcome::Fail("vetoed".into()), 0.1);
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
        r.on("report", 1, || AttemptOutcome::Fail("no sink".into()), 0.1);
        let out = run_plan(&plan, &mut r);
        assert_eq!(out.results[&"report".into()].status, TaskStatus::Fail);
        assert_eq!(out.exit, PlanExit::Completed);
        assert!(out.valid, "an epilogue is advisory by contract");
    }

    #[test]
    fn an_epilogue_is_blocked_once_a_ceiling_ended_the_main_graph() {
        let plan = valid(
            vec![task("a", &[], "any", true), epilogue("report", &[], true)],
            0.05,
        );
        let mut r = ScriptRunner::new();
        let out = run_plan(&plan, &mut r);
        assert_eq!(out.results[&"a".into()].status, TaskStatus::Pass);
        assert_eq!(out.results[&"report".into()].status, TaskStatus::Blocked);
        assert_eq!(out.exit, PlanExit::BudgetExceeded);
        assert!(!r.dispatched.iter().any(|(n, _)| n == "report"));
    }

    #[test]
    fn an_epilogue_crossing_the_ceiling_does_not_relabel_a_short_circuit() {
        let plan = valid(
            vec![
                task("probe", &[], "any", true),
                epilogue("report", &[], true),
            ],
            0.15,
        );
        let mut r = ScriptRunner::new();
        r.on("probe", 1, || AttemptOutcome::Fail("vetoed".into()), 0.1);
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

    fn evidence_plan() -> ValidPlan {
        let mut probe = task("probe", &[], "any", false);
        probe.emits_files = vec!["evidence.json".into()];
        probe.capture_on_failure = true;
        probe.isolation = Some(crate::plan::ir::Isolation::Worktree);
        let mut consumer = task("consumer", &["probe"], "any", false);
        consumer.join = Join::Passed;
        let mut first = epilogue("first", &[], false);
        first.emits_files = vec!["summary.md".into()];
        let second = epilogue("second", &["first"], false);
        valid(vec![probe, consumer, first, second], 10.0)
    }

    /// The file channel of an epilogue is its own ancestors plus the main graph, and a failed
    /// producer joins it only when its captured set exists. A main-graph consumer never sees a
    /// failed producer's files.
    #[test]
    fn an_epilogue_stages_its_ancestors_and_the_captured_evidence_of_main_failures() {
        let plan = evidence_plan();
        let mut r = StagingRunner::new(true);
        r.inner
            .on("probe", 1, || AttemptOutcome::Fail("vetoed".into()), 0.1);
        let out = run_plan(&plan, &mut r);
        assert_eq!(out.results[&"probe".into()].status, TaskStatus::Fail);
        assert_eq!(out.results[&"consumer".into()].status, TaskStatus::Blocked);
        assert_eq!(out.results[&"second".into()].status, TaskStatus::Pass);
        assert_eq!(r.staged["first"], vec!["probe".to_string()]);
        assert_eq!(
            r.staged["second"],
            vec!["first".to_string(), "probe".to_string()]
        );
    }

    #[test]
    fn a_withheld_failure_set_is_not_staged_but_the_epilogue_still_runs() {
        let plan = evidence_plan();
        let mut r = StagingRunner::new(false);
        r.inner
            .on("probe", 1, || AttemptOutcome::Fail("vetoed".into()), 0.1);
        let out = run_plan(&plan, &mut r);
        assert_eq!(out.results[&"second".into()].status, TaskStatus::Pass);
        assert!(r.staged["first"].is_empty());
        assert_eq!(r.staged["second"], vec!["first".to_string()]);
    }

    #[test]
    fn a_passing_producer_reaches_the_epilogue_and_its_consumer_alike() {
        let plan = evidence_plan();
        let mut r = StagingRunner::new(false);
        let out = run_plan(&plan, &mut r);
        assert!(out.valid);
        assert_eq!(r.staged["consumer"], vec!["probe".to_string()]);
        assert_eq!(r.staged["first"], vec!["probe".to_string()]);
    }
}
