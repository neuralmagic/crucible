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

use serde::Serialize;
use serde_json::Value;

use crate::plan::ir::{Direction, Join, Task, TaskKind, TaskName, ValidPlan};

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
pub enum AttemptOutcome {
    /// Measured success with the task's structured output (the edge payload).
    Pass(Value),
    /// Measured failure. Never retried: a task that failed, failed.
    Fail(String),
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
}

impl Default for ExecCfg {
    fn default() -> Self {
        ExecCfg {
            transport_retries: 2,
        }
    }
}

/// Terminal task states.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
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
}

#[derive(Debug, Serialize)]
pub struct TaskResult {
    pub status: TaskStatus,
    pub attempts: u32,
    pub cost_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl TaskResult {
    fn undispatched(status: TaskStatus, note: impl Into<String>) -> Self {
        TaskResult {
            status,
            attempts: 0,
            cost_usd: 0.0,
            output: None,
            note: Some(note.into()),
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
pub(crate) fn runnable_set<'a>(
    plan: &'a ValidPlan,
    substrate: &Substrate,
) -> BTreeSet<&'a TaskName> {
    let mut runnable: BTreeSet<&TaskName> = BTreeSet::new();
    for t in plan.tasks_topo() {
        if substrate.supports(&t.needs) && t.depends_on.iter().all(|d| runnable.contains(d)) {
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

    let mut results: BTreeMap<TaskName, TaskResult> = BTreeMap::new();
    let mut spent = 0.0f64;
    let budget = plan.plan().budget.usd;
    let mut halted: Option<PlanExit> = None;

    // Readiness scan: repeated topo passes. Each pass settles everything decidable
    // without dispatch (halt-blocked, skipped, dep-failed, over-budget), then dispatches
    // the first ready task and restarts, or, when the first ready task is
    // isolation-marked, the whole simultaneously-ready isolated set as one batch. For a
    // plan with no isolated tasks this reproduces the serial topo walk exactly.
    let mut record = |t: &Task,
                      r: TaskResult,
                      results: &mut BTreeMap<TaskName, TaskResult>,
                      halted: &mut Option<PlanExit>| {
        let failed = r.status != TaskStatus::Pass;
        on_result(t, &r);
        results.insert(t.name.clone(), r);
        if failed && t.required && halted.is_none() {
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
            if let Some(exit) = &halted {
                let why = match exit {
                    PlanExit::ShortCircuit { task } => format!("required task {task} failed"),
                    PlanExit::BudgetExceeded => "budget ceiling reached".to_string(),
                    _ => "halted".to_string(),
                };
                let r = TaskResult::undispatched(TaskStatus::Blocked, why);
                record(t, r, &mut results, &mut halted);
                continue;
            }
            if !runnable.contains(&t.name) {
                let r =
                    TaskResult::undispatched(TaskStatus::Skipped, "unrunnable on this substrate");
                record(t, r, &mut results, &mut halted);
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
                // Lossy join: dispatch over whatever passed (a Fail dep just contributes
                // no input; the inputs collection below filters on output presence).
                Join::Passed => true,
            };
            if !deps_ok {
                // Nothing runs on top of a failure (or a skip): advisory failures gate
                // their dependents even though they never gate validity.
                let r = TaskResult::undispatched(TaskStatus::Blocked, "dependency did not pass");
                record(t, r, &mut results, &mut halted);
                continue;
            }
            if spent >= budget {
                halted = Some(PlanExit::BudgetExceeded);
                let r = TaskResult::undispatched(TaskStatus::Blocked, "budget ceiling reached");
                record(t, r, &mut results, &mut halted);
                continue;
            }
            if dispatch.is_empty() {
                dispatch.push(t);
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

        if dispatch.len() == 1 {
            let t = first;
            let inputs = inputs_for(t, &results);
            let (result, budget_exceeded) = match &t.task {
                TaskKind::TopK { k, direction } => (reduce_top_k(&inputs, *k, *direction), false),
                TaskKind::Agent { .. } | TaskKind::Command { .. } | TaskKind::Engine { .. } => {
                    run_with_retries(t, &inputs, cfg, runner, &mut spent, budget)
                }
            };
            if budget_exceeded {
                halted = Some(PlanExit::BudgetExceeded);
            }
            record(t, result, &mut results, &mut halted);
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
                halted = Some(PlanExit::BudgetExceeded);
            }
            for (t, result) in batch_results {
                record(t, result, &mut results, &mut halted);
            }
        }
    }

    let exit = halted.unwrap_or(PlanExit::Completed);
    let valid = exit == PlanExit::Completed
        && plan
            .tasks_topo()
            .filter(|t| t.required)
            .all(|t| results.get(&t.name).map(|r| r.status) == Some(TaskStatus::Pass));
    PlanOutcome {
        valid,
        exit,
        spent_usd: spent,
        results,
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
        match a.outcome {
            AttemptOutcome::Pass(output) => {
                return (
                    TaskResult {
                        status: TaskStatus::Pass,
                        attempts,
                        cost_usd: cost,
                        output: Some(output),
                        note: None,
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
            attempted.push((idx, item, a.outcome));
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::ir::{Join, Plan, PlanBudget};

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
            isolation: None,
            join: Join::default(),
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
            isolation: None,
            join: Join::default(),
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
            isolation: None,
            join: Join::Passed,
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
}
