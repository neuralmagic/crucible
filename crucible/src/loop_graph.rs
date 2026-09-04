//! The loop's work-graph templates and the runners that adapt the executor's task
//! contract onto the loop's `World`/`Judge`/`Reporter`. Cross-round state, keep/discard,
//! and every between-round drain stay in [`crate::loop_driver`].
//!
//! The templates carry no budget of their own (`f64::MAX`): the driver owns the run budget
//! and checks it between rounds, so a turn that blows the cap is still measured and decided.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::loop_driver::{self, Decided, IterStep, Measured, TurnVerdict};
use crate::manifest::{WorkflowCaps, WorkflowCfg, WorkflowType};
use crate::plan::exec::{
    Attempt, AttemptOutcome, BatchItem, ExecCfg, Substrate, TaskResult, TaskRunner, TaskStatus,
    execute, execute_from,
};
use crate::plan::ir::{
    EngineOp, Join, Plan, PlanBudget, Stage, Task, TaskKind, TaskName, ValidPlan,
};
use crate::reporter::{Reporter, Row, TurnBudget};
use crate::session::{EvidenceDisposition, EvidenceEntry};
use crate::{Args, Paths, agent, control};
use crucible::crucible::{Judge, MeasureCtx, Reading, World};

#[derive(Debug, thiserror::Error)]
#[error("graph iteration ended with neither a decision nor a control signal (exit: {exit})")]
struct NoDecisionOrSignal {
    exit: String,
}

/// Terminal status + note per settled task, shared between the executor's `on_result`
/// callback and the grade runner (both live inside one serial `execute` call). Grade
/// needs it because its `inputs` hold only the passing dependencies: the declared
/// evidence tasks that failed or never ran are invisible there.
type TaskStates = Rc<RefCell<BTreeMap<TaskName, (TaskStatus, Option<String>)>>>;

/// Everything one graph iteration reads from the driver. Scalars are copies of the
/// segment state; the driver folds the returned [`IterStep`] + cost back itself.
pub(crate) struct IterCtx<'a> {
    pub args: &'a Args,
    pub p: &'a Paths,
    pub world: &'a dyn World,
    pub judge: &'a dyn Judge,
    pub control: Option<&'a control::ControlState>,
    pub it: u32,
    pub prompt: &'a str,
    /// Delta-only prompt for a resumed proposer session.
    pub resume_prompt: &'a str,
    pub rows: &'a [Row],
    pub baseline_score: f64,
    pub baseline_total: u64,
    pub best_score: f64,
    /// The kept best's secondary tiebreak scalar, alongside `best_score`.
    pub best_tiebreak: Option<f64>,
    /// Run spend before this iteration, so the runner reports the same cumulative budget
    /// the typestate path would after the turn.
    pub spent_before: f64,
    pub started: Instant,
    pub heartbeat: Option<std::sync::Arc<crate::heartbeat::Heartbeat>>,
    pub workflow: Option<&'a WorkflowCfg>,
    /// Results a previous process settled in this same iteration, from the session log.
    pub prior: Option<PriorPlan>,
}

/// The plan tasks a dead process settled in the iteration it died in, as the contract fold
/// restored them. A resumed iteration whose freshly built plan declares the same tasks under
/// the same version starts from these instead of re-dispatching them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PriorPlan {
    pub iter: u32,
    pub plan_version: u32,
    pub declared: Vec<String>,
    pub results: BTreeMap<String, crucible_contract::TaskResultWire>,
}

impl PriorPlan {
    /// The results to seed `plan` with: only when the plan is the one the log admitted (same
    /// version, same declared tasks), and only the tasks that passed. Engine operations are
    /// never seeded: the world a passing `propose` or `apply` produced did not survive the
    /// process, so they run again.
    pub(crate) fn seed(&self, plan: &ValidPlan) -> BTreeMap<TaskName, TaskResult> {
        let declared: Vec<String> = plan.plan().tasks.iter().map(|t| t.name.0.clone()).collect();
        if plan.plan().version != self.plan_version || declared != self.declared {
            return BTreeMap::new();
        }
        plan.plan()
            .tasks
            .iter()
            .filter(|t| !matches!(t.task, TaskKind::Engine { .. }))
            .filter_map(|t| {
                let prior = self.results.get(&t.name.0)?;
                let status = TaskStatus::parse(&prior.status)?;
                (status == TaskStatus::Pass).then(|| {
                    (
                        t.name.clone(),
                        TaskResult {
                            status,
                            attempts: prior.attempts,
                            cost_usd: prior.cost_usd,
                            output: prior.output.clone(),
                            note: (!prior.note.is_empty()).then(|| prior.note.clone()),
                            fanout: None,
                        },
                    )
                })
            })
            .collect()
    }
}

/// Run one admitted autoresearch iteration and return its step and cost.
pub(crate) fn run_iteration<R: Reporter>(cx: IterCtx<'_>, r: &mut R) -> Result<(IterStep, f64)> {
    let mut caps = WorkflowCaps::for_lane(
        cx.workflow
            .map(|workflow| workflow.workflow_type)
            .unwrap_or_default(),
    );
    if agent::supports_persistent_sessions(cx.args) {
        caps = caps.with_persistent_sessions();
    }
    let plan = iteration_template(cx.workflow, &caps)?;
    let prior = cx
        .prior
        .as_ref()
        .map(|prior| prior.seed(&plan))
        .unwrap_or_default();
    if !prior.is_empty() {
        r.note(&format!(
            "resume: {} task(s) settled before the run died carry over; the rest run",
            prior.len()
        ));
    }
    let result_task = cx
        .workflow
        .filter(|workflow| !workflow.is_legacy_splice())
        .and_then(|workflow| workflow.result.clone())
        .unwrap_or_else(|| "decide".into());
    r.plan_event(&crate::plan::cli::plan_admitted_event(&plan));

    let task_states: TaskStates = TaskStates::default();
    let mut runner = LoopTaskRunner {
        args: cx.args,
        p: cx.p,
        world: cx.world,
        judge: cx.judge,
        control: cx.control,
        r,
        it: cx.it,
        prompt: cx.prompt,
        resume_prompt: cx.resume_prompt,
        rows: cx.rows,
        ctx: MeasureCtx {
            baseline_score: Some(cx.baseline_score),
            baseline_total: Some(cx.baseline_total),
            best_score: Some(cx.best_score),
        },
        best_score: cx.best_score,
        best_tiebreak: cx.best_tiebreak,
        spent_before: cx.spent_before,
        started: cx.started,
        heartbeat: cx.heartbeat,
        signal: None,
        measured: BTreeMap::new(),
        decided: BTreeMap::new(),
        fatal: None,
        task_states: Rc::clone(&task_states),
        workflow_runner: crate::plan::harness::HarnessRunner {
            args: cx.args.clone(),
            paths: cx.p.clone(),
            commit_per_task: false,
            captured_bytes: std::sync::atomic::AtomicU64::new(0),
            staged: Default::default(),
            gate: crate::plan::gate::GateCtx::default(),
        },
    };
    // The runner and the on_result hook both need the reporter; collect the wire lines
    // here and append them after the executor returns (they're additive either way).
    let mut task_events = Vec::new();
    let outcome = execute_from(
        &plan,
        &Substrate::default(),
        ExecCfg::default(),
        &mut runner,
        prior,
        |task, result| {
            task_states
                .borrow_mut()
                .insert(task.name.clone(), (result.status, result.note.clone()));
            task_events.push(crate::plan::cli::task_result_event(
                plan.plan().version,
                cx.it,
                task,
                result,
            ));
        },
    );
    for ev in &task_events {
        runner.r.plan_event(ev);
    }

    if let Some(e) = runner.fatal.take() {
        return Err(e);
    }
    let step = match runner.signal.take() {
        Some(Signal::Discard(reason)) => IterStep::Discarded { reason },
        Some(Signal::Escalate) => IterStep::Escalated,
        Some(Signal::Park(pp)) => IterStep::Parked(pp),
        Some(Signal::Stop) => IterStep::Stopped,
        None => match runner.decided.remove(&result_task) {
            Some(d) => IterStep::Decided(Box::new(d)),
            // A pre-gate rejection discards the candidate.
            None => match &outcome.exit {
                crate::plan::exec::PlanExit::ShortCircuit { task } => {
                    let result = outcome.results.get(task);
                    let why = result.and_then(|r| r.note.clone()).unwrap_or_default();
                    // A propose task that exhausted its transport retries never started a
                    // turn: no candidate exists to reject, so hand the driver NeverStarted
                    // (it re-runs the iteration instead of consuming it, bounded there).
                    let propose_dead = result.is_some_and(|r| r.status == TaskStatus::Transport)
                        && plan.plan().tasks.iter().any(|t| {
                            t.name == *task
                                && matches!(
                                    t.task,
                                    TaskKind::Engine {
                                        op: EngineOp::Propose,
                                        ..
                                    }
                                )
                        });
                    if propose_dead {
                        runner.r.note(&format!(
                            "workflow task {task} never started (iter {} not consumed): {why}",
                            cx.it
                        ));
                        IterStep::NeverStarted {
                            reason: format!("{task} died on transport: {why}"),
                        }
                    } else {
                        runner.r.note(&format!(
                            "workflow task {task} rejected the candidate (discarding iter {}): {why}",
                            cx.it
                        ));
                        IterStep::Discarded {
                            reason: format!("{task} rejected the candidate: {why}"),
                        }
                    }
                }
                exit => {
                    return Err(NoDecisionOrSignal {
                        exit: format!("{exit:?}"),
                    }
                    .into());
                }
            },
        },
    };
    Ok((step, outcome.spent_usd))
}

/// Build and admit the default or authored iteration graph.
pub(crate) fn iteration_template(
    workflow: Option<&WorkflowCfg>,
    caps: &WorkflowCaps,
) -> Result<ValidPlan> {
    if let Some(workflow) = workflow.filter(|workflow| !workflow.is_legacy_splice()) {
        workflow
            .admit(caps)
            .context("admitting authored workflow into the autoresearch loop")?;
        return Plan {
            version: 1,
            reason: None,
            budget: PlanBudget { usd: f64::MAX },
            tasks: workflow.iteration_tasks(),
        }
        .validate()
        .context("building authored iteration workflow");
    }

    let engine =
        |name: &str, op: EngineOp, source: Option<TaskName>, deps: Vec<TaskName>| -> Task {
            Task {
                name: name.into(),
                task: TaskKind::Engine {
                    op,
                    source,
                    tiebreak: None,
                },
                depends_on: deps,
                session: None,
                needs: "any".to_string(),
                required: true,
                isolation: None,
                join: Join::default(),
                stage: Stage::Iteration,
                emits: Vec::new(),
                emits_files: Vec::new(),
                over: None,
                max_fanout: None,
            }
        };
    let mut tasks = vec![engine("propose", EngineOp::Propose, None, vec![])];

    // Legacy splice tasks run between `propose` and `apply`; `apply` waits on every sink.
    // Epilogue tasks never splice: they run once post-loop, not per iteration.
    let mut apply_deps = vec![TaskName("propose".to_string())];
    if let Some(w) = workflow.filter(|w| !w.tasks.is_empty()) {
        for mut t in w.iteration_tasks() {
            if t.depends_on.is_empty() {
                t.depends_on = vec![TaskName("propose".to_string())];
            }
            tasks.push(t);
        }
        let sinks = w.sinks();
        if !sinks.is_empty() {
            apply_deps = sinks;
        }
    }

    tasks.push(engine("apply", EngineOp::Apply, None, apply_deps));
    tasks.push(engine(
        "measure",
        EngineOp::Measure,
        None,
        vec![TaskName("apply".to_string())],
    ));
    tasks.push(engine(
        "decide",
        EngineOp::Decide,
        Some(TaskName("measure".to_string())),
        vec![TaskName("measure".to_string())],
    ));
    let workflow = WorkflowCfg {
        workflow_type: WorkflowType::Autoresearch,
        result: Some("decide".into()),
        tasks,
        file: None,
        resolved_from: None,
    };
    workflow
        .admit(caps)
        .context("admitting the default autoresearch workflow")?;
    Plan {
        version: 1,
        reason: None,
        budget: PlanBudget { usd: f64::MAX },
        tasks: workflow.tasks,
    }
    .validate()
    .context("building the iteration template")
}

/// Build the workflow's run-scoped epilogue subgraph; `None` when it declares none.
pub(crate) fn epilogue_template(workflow: &WorkflowCfg) -> Result<Option<ValidPlan>> {
    let tasks = workflow.epilogue_tasks();
    if tasks.is_empty() {
        return Ok(None);
    }
    Plan {
        version: 1,
        reason: None,
        budget: PlanBudget { usd: f64::MAX },
        tasks,
    }
    .validate()
    .context("building the epilogue template")
    .map(Some)
}

/// The final kept candidate, as epilogue tasks see it: injected into every task's inputs
/// under the reserved [`crate::manifest::KEPT_INPUT`] key, so a command/evaluate task
/// reads it from `CRUCIBLE_INPUTS` exactly like any upstream result.
pub(crate) struct KeptContext {
    pub iter: u32,
    pub score: Option<f64>,
    pub tiebreak: Option<f64>,
    pub sha: Option<String>,
    pub snapshot: Option<String>,
    pub note: String,
}

impl KeptContext {
    fn to_value(&self) -> Value {
        serde_json::json!({
            "iter": self.iter,
            "score": self.score,
            "tiebreak": self.tiebreak,
            "sha": self.sha,
            "snapshot": self.snapshot,
            "note": self.note,
        })
    }
}

/// Run the epilogue subgraph once, against the kept candidate live in the workspace.
/// Advisory by contract: the returned rows and the notes make a failure loud, but nothing
/// here can un-keep the candidate. Returns the advisory rows and the subgraph's cost.
pub(crate) fn run_epilogue<R: Reporter>(
    args: &Args,
    p: &Paths,
    workflow: &WorkflowCfg,
    kept: &KeptContext,
    r: &mut R,
) -> Result<(Vec<Row>, f64)> {
    let Some(plan) = epilogue_template(workflow)? else {
        return Ok((Vec::new(), 0.0));
    };
    r.note(&format!(
        "epilogue: running {} run-scoped task(s) against the kept candidate (iter {})",
        plan.plan().tasks.len(),
        kept.iter
    ));
    r.plan_event(&crate::plan::cli::plan_admitted_event(&plan));

    let mut runner = EpilogueRunner {
        inner: crate::plan::harness::HarnessRunner {
            args: args.clone(),
            paths: p.clone(),
            commit_per_task: false,
            captured_bytes: std::sync::atomic::AtomicU64::new(0),
            staged: Default::default(),
            gate: crate::plan::gate::GateCtx::default(),
        },
        kept: kept.to_value(),
    };
    let mut task_events = Vec::new();
    let outcome = execute(
        &plan,
        &Substrate::default(),
        ExecCfg::default(),
        &mut runner,
        |task, result| {
            task_events.push(crate::plan::cli::task_result_event(
                plan.plan().version,
                kept.iter,
                task,
                result,
            ));
        },
    );
    for ev in &task_events {
        r.plan_event(ev);
    }

    let mut rows = Vec::new();
    for task in plan.tasks_topo() {
        let Some(result) = outcome.results.get(&task.name) else {
            continue;
        };
        let (decision, failed) = match result.status {
            TaskStatus::Pass => ("epilogue", false),
            TaskStatus::Skipped | TaskStatus::Blocked => ("epilogue-skip", false),
            TaskStatus::Fail | TaskStatus::Transport | TaskStatus::Truncated => {
                ("epilogue-fail", true)
            }
        };
        let why = result.note.clone().unwrap_or_else(|| match result.status {
            TaskStatus::Pass => "ok".to_string(),
            other => other.as_str().to_string(),
        });
        if failed {
            r.note(&format!(
                "epilogue task {} FAILED (advisory — the kept candidate stands): {why}",
                task.name
            ));
        }
        rows.push(Row {
            iter: kept.iter,
            decision: decision.to_string(),
            note: format!("{}: {why}", task.name),
            detail: result
                .output
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            score: result
                .output
                .as_ref()
                .and_then(|o| o.get("score"))
                .and_then(Value::as_f64),
            phase: Some("epilogue".to_string()),
            ..Default::default()
        });
    }
    Ok((rows, outcome.spent_usd))
}

/// [`crate::plan::harness::HarnessRunner`] plus the kept-candidate input: the executor
/// derives inputs from dependencies, and the epilogue's roots have none, so the kept
/// context rides in here on every task.
struct EpilogueRunner {
    inner: crate::plan::harness::HarnessRunner,
    kept: Value,
}

impl EpilogueRunner {
    fn with_kept(&self, inputs: &BTreeMap<TaskName, Value>) -> BTreeMap<TaskName, Value> {
        let mut inputs = inputs.clone();
        inputs.insert(crate::manifest::KEPT_INPUT.into(), self.kept.clone());
        inputs
    }
}

impl TaskRunner for EpilogueRunner {
    fn run(&mut self, task: &Task, attempt: u32, inputs: &BTreeMap<TaskName, Value>) -> Attempt {
        let inputs = self.with_kept(inputs);
        self.inner.run(task, attempt, &inputs)
    }

    fn run_many(&mut self, batch: &[BatchItem<'_>]) -> Vec<Attempt> {
        let batch: Vec<BatchItem<'_>> = batch
            .iter()
            .map(|b| BatchItem {
                task: b.task,
                attempt: b.attempt,
                inputs: self.with_kept(&b.inputs),
            })
            .collect();
        self.inner.run_many(&batch)
    }
}

/// What the propose task's post-turn drains decided, parked here for the driver: the
/// executor only sees pass/fail, the loop control travels out of band.
enum Signal {
    Discard(String),
    Escalate,
    Park(crate::provisioning::PendingProvisioning),
    Stop,
}

/// [`TaskRunner`] over the driver's trait objects. One instance per iteration: the
/// `measured`/`decided` slots flow between the engine tasks in dispatch order (serial
/// executor), and `signal`/`fatal` carry loop control back to [`run_iteration`].
struct LoopTaskRunner<'a, R: Reporter> {
    args: &'a Args,
    p: &'a Paths,
    world: &'a dyn World,
    judge: &'a dyn Judge,
    control: Option<&'a control::ControlState>,
    r: &'a mut R,
    it: u32,
    prompt: &'a str,
    resume_prompt: &'a str,
    rows: &'a [Row],
    ctx: MeasureCtx,
    best_score: f64,
    best_tiebreak: Option<f64>,
    spent_before: f64,
    started: Instant,
    heartbeat: Option<std::sync::Arc<crate::heartbeat::Heartbeat>>,
    signal: Option<Signal>,
    measured: BTreeMap<TaskName, Measured>,
    decided: BTreeMap<TaskName, Decided>,
    fatal: Option<anyhow::Error>,
    /// Settled-task dispositions, fed by the executor's `on_result` callback; read by
    /// `grade` to record which declared evidence tasks actually ran.
    task_states: TaskStates,
    workflow_runner: crate::plan::harness::HarnessRunner,
}

impl<R: Reporter> LoopTaskRunner<'_, R> {
    fn propose(&mut self, task: &Task, prompt: &str) -> Attempt {
        let turn = self.r.run_agent(
            self.args,
            self.p,
            self.it,
            prompt,
            Some(self.resume_prompt),
            task.session.as_deref(),
            TurnBudget {
                spent_before: self.spent_before,
                started: self.started,
                max_cost: loop_driver::live_max_cost(self.args, self.control),
                heartbeat: self.heartbeat.clone(),
            },
        );
        let cost = turn.cost;
        if let Some(control) = self.control {
            control.set_spend(self.spent_before + cost);
        }
        self.r
            .budget(self.spent_before + cost, self.started.elapsed());
        match loop_driver::drain_turn_markers(
            self.r,
            self.p,
            self.control,
            self.it,
            &turn,
            self.rows,
        ) {
            TurnVerdict::Proceed => Attempt {
                outcome: AttemptOutcome::Pass(serde_json::json!({ "cost_usd": cost })),
                cost_usd: cost,
            },
            TurnVerdict::Discard => {
                self.signal = Some(Signal::Discard("turn failed".to_string()));
                fail(cost, "turn failed; iteration discarded".to_string())
            }
            // Transport-class turn death: hand the executor a Transport outcome so
            // `run_with_retries` re-runs the turn (the session resumes where it died).
            TurnVerdict::Retry(why) => Attempt {
                outcome: AttemptOutcome::Transport(why),
                cost_usd: cost,
            },
            TurnVerdict::Escalate => {
                self.signal = Some(Signal::Escalate);
                fail(cost, "agent escalated".to_string())
            }
            TurnVerdict::Park(pp) => {
                self.signal = Some(Signal::Park(pp));
                fail(cost, "parked on a pending approval".to_string())
            }
            TurnVerdict::Stop => {
                self.signal = Some(Signal::Stop);
                fail(cost, "stop signal".to_string())
            }
        }
    }

    fn apply(&mut self) -> Attempt {
        match self.world.apply() {
            Ok(()) => pass(serde_json::json!({})),
            Err(e) => {
                // The typestate path's exact discard note, from its apply-failure site.
                self.r.note(&format!(
                    "apply failed (discarding iter {}): {e:#}",
                    self.it
                ));
                self.signal = Some(Signal::Discard(format!("apply failed: {e:#}")));
                fail(0.0, format!("apply failed: {e:#}"))
            }
        }
    }

    fn measure(&mut self, task: &Task) -> Attempt {
        match loop_driver::measure_candidate(self.judge, &self.ctx, self.p, self.world) {
            Ok(m) => {
                let out = serde_json::json!({ "score": m.reading.score, "valid": m.reading.valid });
                self.measured.insert(task.name.clone(), m);
                pass(out)
            }
            Err(e) => {
                // Propagate after the executor unwinds.
                self.fatal = Some(e);
                fail(0.0, "measure errored; aborting the run".to_string())
            }
        }
    }

    fn grade(
        &mut self,
        task: &Task,
        source: Option<&TaskName>,
        tiebreak: Option<&TaskName>,
        inputs: &BTreeMap<TaskName, Value>,
    ) -> Attempt {
        let Some(source) = source else {
            return fail(0.0, "grade dispatched without a score source".to_string());
        };
        let Some(primary) = inputs.get(source) else {
            return fail(
                0.0,
                format!("grade score source {source} did not produce passing evidence"),
            );
        };
        let Some(score) = primary.get("score").and_then(Value::as_f64) else {
            return fail(
                0.0,
                format!("grade score source {source} has no numeric `score`"),
            );
        };
        // The declared tiebreak task's score becomes the reading's secondary scalar.
        // Best-effort by design: a tiebreak task that failed or was skipped is simply
        // absent from the passing inputs, and the decide falls back to primary-only.
        let tiebreak = tiebreak
            .and_then(|t| inputs.get(t))
            .and_then(|v| v.get("score"))
            .and_then(Value::as_f64);
        let reading = Reading {
            valid: primary
                .get("valid")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            score: Some(score),
            tiebreak,
            solved: primary
                .get("solved")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            note: primary
                .get("note")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            detail: detail_json(source, inputs, primary),
        };
        // The folded inputs hold only the passing dependencies (`join = "passed"`), so
        // record every DECLARED evidence task with its disposition: a candidate that
        // passed two of three declared rungs must not read as fully graded.
        let states = self.task_states.borrow();
        let evidence: Vec<EvidenceEntry> = task
            .depends_on
            .iter()
            .map(|dep| {
                let (status, note) = states
                    .get(dep)
                    .map(|(s, n)| (*s, n.clone().unwrap_or_default()))
                    .unwrap_or((TaskStatus::Skipped, "no terminal status recorded".into()));
                EvidenceEntry {
                    task: dep.0.clone(),
                    disposition: match status {
                        TaskStatus::Pass => EvidenceDisposition::Passed,
                        TaskStatus::Fail => EvidenceDisposition::Failed,
                        TaskStatus::Transport
                        | TaskStatus::Skipped
                        | TaskStatus::Blocked
                        | TaskStatus::Truncated => EvidenceDisposition::Skipped,
                    },
                    note,
                }
            })
            .collect();
        drop(states);
        let mut measured = loop_driver::measured_from_reading(reading, self.p, self.world);
        measured.evidence = evidence.clone();
        let out = serde_json::json!({
            "score": measured.reading.score,
            "tiebreak": measured.reading.tiebreak,
            "valid": measured.reading.valid,
            "evidence_count": inputs.len(),
            "evidence": evidence,
        });
        self.measured.insert(task.name.clone(), measured);
        pass(out)
    }

    fn decide(&mut self, task: &Task, source: Option<&TaskName>) -> Attempt {
        let Some(source) = source else {
            return fail(
                0.0,
                "decide dispatched without a measurement source".to_string(),
            );
        };
        // Admission prevents two decisions from consuming one measurement.
        let Some(m) = self.measured.remove(source) else {
            return fail(
                0.0,
                format!("decide source {source} has no measured candidate"),
            );
        };
        let d =
            loop_driver::decide_row(self.judge, self.best_score, self.best_tiebreak, self.it, m);
        let out = serde_json::json!({
            "keep": d.verdict.keep,
            "solved": d.verdict.solved,
            "score": d.reading.score,
        });
        self.decided.insert(task.name.clone(), d);
        pass(out)
    }
}

impl<R: Reporter> TaskRunner for LoopTaskRunner<'_, R> {
    fn run(&mut self, task: &Task, attempt: u32, inputs: &BTreeMap<TaskName, Value>) -> Attempt {
        match &task.task {
            TaskKind::Agent { .. }
            | TaskKind::Command { .. }
            | TaskKind::Evaluate { .. }
            | TaskKind::Report { .. } => self.workflow_runner.run(task, attempt, inputs),
            TaskKind::Engine {
                op: EngineOp::Propose,
                ..
            } => self.propose(task, self.prompt),
            TaskKind::Engine {
                op: EngineOp::Apply,
                ..
            } => self.apply(),
            TaskKind::Engine {
                op: EngineOp::Measure,
                ..
            } => self.measure(task),
            TaskKind::Engine {
                op: EngineOp::Grade,
                source,
                tiebreak,
            } => self.grade(task, source.as_ref(), tiebreak.as_ref(), inputs),
            TaskKind::Engine {
                op: EngineOp::Decide,
                source,
                ..
            } => self.decide(task, source.as_ref()),
            TaskKind::TopK { .. } | TaskKind::Approve { .. } => fail(
                0.0,
                format!(
                    "unexpected task kind in the loop template: {}",
                    task.task.label()
                ),
            ),
        }
    }

    fn run_many(&mut self, batch: &[BatchItem<'_>]) -> Vec<Attempt> {
        self.workflow_runner.run_many(batch)
    }
}

/// The graded reading's detail JSON, which the judge stringifies into the RESULTS row's
/// detail cell. The score source's broker log handles ride along so a kept row can be
/// pulled back to full logs with `fetch_log`, not just the note's stderr tail.
fn detail_json(source: &TaskName, inputs: &BTreeMap<TaskName, Value>, primary: &Value) -> Value {
    let handles: Vec<Value> = primary
        .get("logs")
        .and_then(Value::as_array)
        .map(|logs| logs.iter().filter(|v| v.is_string()).cloned().collect())
        .unwrap_or_default();
    let mut detail = serde_json::Map::new();
    detail.insert("score_source".to_string(), serde_json::json!(source.0));
    detail.insert("evidence".to_string(), serde_json::json!(inputs));
    if !handles.is_empty() {
        detail.insert("logs".to_string(), Value::Array(handles));
    }
    Value::Object(detail)
}

fn pass(v: Value) -> Attempt {
    Attempt {
        outcome: AttemptOutcome::Pass(v),
        cost_usd: 0.0,
    }
}

fn fail(cost_usd: f64, note: String) -> Attempt {
    Attempt {
        outcome: AttemptOutcome::fail(note),
        cost_usd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loop_driver::{LoopRuntime, run_loop};
    use crate::{Prepared, manifest, reporter, run, stream};
    use clap::Parser;
    use std::path::Path;

    #[test]
    fn pack_tasks_splice_between_the_turn_and_the_gate() {
        let w: WorkflowCfg = toml::from_str(
            "[[task]]\nname = \"review\"\nkind = \"command\"\ncommand = \"true\"\n             [[task]]\nname = \"lint\"\nkind = \"command\"\ncommand = \"true\"\ndepends_on = [\"review\"]\n",
        )
        .unwrap();
        w.validate().unwrap();
        let plan = iteration_template(Some(&w), &WorkflowCaps::autoresearch_engine()).unwrap();
        let names: Vec<&str> = plan.tasks_topo().map(|t| t.name.0.as_str()).collect();
        assert_eq!(
            names,
            ["propose", "review", "lint", "apply", "measure", "decide"]
        );
        let dep = |n: &str| {
            plan.get(&n.into())
                .unwrap()
                .depends_on
                .iter()
                .map(|d| d.0.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            dep("review"),
            ["propose"],
            "an unattached task hangs off propose"
        );
        assert_eq!(
            dep("apply"),
            ["lint"],
            "apply waits on the sink, not on propose"
        );
        assert_eq!(dep("measure"), ["apply"]);
        assert_eq!(dep("decide"), ["measure"]);
    }

    /// A resumed iteration re-uses what the dead process settled only when the plan is the
    /// one it admitted, and only the passing pack tasks: engine operations run again because
    /// the world they produced did not survive the process.
    #[test]
    fn a_prior_plan_seeds_only_passing_pack_tasks_of_the_same_plan() {
        let w: WorkflowCfg = toml::from_str(
            "[[task]]\nname = \"review\"\nkind = \"command\"\ncommand = \"true\"\n             [[task]]\nname = \"lint\"\nkind = \"command\"\ncommand = \"true\"\ndepends_on = [\"review\"]\n",
        )
        .unwrap();
        w.validate().unwrap();
        let plan = iteration_template(Some(&w), &WorkflowCaps::autoresearch_engine()).unwrap();
        let declared: Vec<String> = plan.plan().tasks.iter().map(|t| t.name.0.clone()).collect();
        let wire = |status: &str| crucible_contract::TaskResultWire {
            status: status.into(),
            task_kind: "command".into(),
            iter: 2,
            attempts: 1,
            cost_usd: 0.1,
            secs: 0.2,
            note: String::new(),
            output: Some(serde_json::json!({"ok": true})),
        };
        let prior = PriorPlan {
            iter: 2,
            plan_version: 1,
            declared: declared.clone(),
            results: BTreeMap::from([
                ("propose".to_string(), wire("pass")),
                ("review".to_string(), wire("pass")),
                ("lint".to_string(), wire("fail")),
            ]),
        };
        let seeded = prior.seed(&plan);
        let names: Vec<&str> = seeded.keys().map(|n| n.0.as_str()).collect();
        assert_eq!(names, ["review"], "passing pack task only: {names:?}");
        assert_eq!(seeded[&TaskName::from("review")].status, TaskStatus::Pass);
        assert_eq!(seeded[&TaskName::from("review")].cost_usd, 0.1);

        let other_version = PriorPlan {
            plan_version: 2,
            ..prior.clone()
        };
        assert!(
            other_version.seed(&plan).is_empty(),
            "a different plan version seeds nothing"
        );
        let other_shape = PriorPlan {
            declared: vec!["review".into()],
            ..prior
        };
        assert!(
            other_shape.seed(&plan).is_empty(),
            "a different task set seeds nothing"
        );
    }

    /// Epilogue tasks stay out of the per-iteration plan entirely (legacy splice and
    /// fully-authored form) and land in their own post-loop template.
    #[test]
    fn epilogue_tasks_leave_the_iteration_plan_for_the_epilogue_template() {
        let w: WorkflowCfg = toml::from_str(
            "[[task]]\nname = \"review\"\nkind = \"command\"\ncommand = \"true\"\n\
             [[task]]\nname = \"racecheck\"\nkind = \"command\"\ncommand = \"true\"\nstage = \"epilogue\"\n",
        )
        .unwrap();
        w.validate().unwrap();
        let plan = iteration_template(Some(&w), &WorkflowCaps::autoresearch_engine()).unwrap();
        let names: Vec<&str> = plan.tasks_topo().map(|t| t.name.0.as_str()).collect();
        assert_eq!(names, ["propose", "review", "apply", "measure", "decide"]);
        assert_eq!(
            plan.get(&"apply".into()).unwrap().depends_on,
            [TaskName::from("review")],
            "apply waits on the iteration sink, never on an epilogue task"
        );

        let epilogue = epilogue_template(&w).unwrap().expect("epilogue declared");
        let names: Vec<&str> = epilogue.tasks_topo().map(|t| t.name.0.as_str()).collect();
        assert_eq!(names, ["racecheck"]);

        let authored: WorkflowCfg = toml::from_str(
            "type = \"autoresearch\"\nresult = \"choose\"\n\
             [[task]]\nname = \"invent\"\nkind = \"engine\"\nop = \"propose\"\n\
             [[task]]\nname = \"deploy\"\nkind = \"engine\"\nop = \"apply\"\ndepends_on = [\"invent\"]\n\
             [[task]]\nname = \"score\"\nkind = \"engine\"\nop = \"measure\"\ndepends_on = [\"deploy\"]\n\
             [[task]]\nname = \"choose\"\nkind = \"engine\"\nop = \"decide\"\nsource = \"score\"\ndepends_on = [\"score\"]\n\
             [[task]]\nname = \"racecheck\"\nkind = \"command\"\ncommand = \"true\"\nstage = \"epilogue\"\n",
        )
        .unwrap();
        let plan =
            iteration_template(Some(&authored), &WorkflowCaps::autoresearch_engine()).unwrap();
        assert!(
            plan.get(&"racecheck".into()).is_none(),
            "authored iteration plan must not carry the epilogue task"
        );
        let no_epilogue: WorkflowCfg =
            toml::from_str("[[task]]\nname = \"review\"\nkind = \"command\"\ncommand = \"true\"\n")
                .unwrap();
        assert!(epilogue_template(&no_epilogue).unwrap().is_none());
    }

    #[test]
    fn a_rejecting_pack_task_discards_the_iteration_before_measuring() {
        let trace = run_counter_cfg(
            true,
            2,
            BUMP,
            Some(
                "\n[[workflow.task]]\nname = \"review\"\nkind = \"command\"\ncommand = \"exit 1\"\n",
            ),
        );
        let decisions: Vec<&str> = trace.rows.iter().map(|(_, d, _)| d.as_str()).collect();
        // Discarded iterations still land on the scoreboard (decision + reason), so a run that
        // lost everything reads as what it is instead of a clean "finished" with one row.
        assert_eq!(
            decisions,
            ["baseline", "discarded", "discarded"],
            "every iteration is discarded before it can be measured: {}",
            describe(&trace)
        );
        assert!(
            trace
                .rows
                .iter()
                .all(|(_, d, score)| d != "discarded" || score.is_none()),
            "a discarded row never carries a score: {:?}",
            trace.rows
        );
        assert!(
            trace.notes.iter().any(|n| n.contains("review")
                && n.contains("rejected the candidate")
                && n.contains("exit 1")),
            "the discard names the task that rejected it: {:?}",
            trace.notes
        );
        assert!(
            trace
                .notes
                .iter()
                .all(|n| !n.contains("unexpected task kind")),
            "the command must execute rather than fail runner dispatch: {:?}",
            trace.notes
        );
        assert_eq!(trace.shutdown, "finished", "a rejection is not a run error");
    }

    #[test]
    fn template_is_the_canonical_chain() {
        let plan = iteration_template(None, &WorkflowCaps::autoresearch_engine()).unwrap();
        let names: Vec<&str> = plan.tasks_topo().map(|t| t.name.0.as_str()).collect();
        assert_eq!(names, ["propose", "apply", "measure", "decide"]);
        assert!(plan.tasks_topo().all(|t| t.required));
        let kinds: Vec<&str> = plan.tasks_topo().map(|t| t.task.label()).collect();
        assert_eq!(
            kinds,
            [
                "engine_propose",
                "engine_apply",
                "engine_measure",
                "engine_decide"
            ]
        );
    }

    #[test]
    fn authored_autoresearch_uses_semantics_instead_of_reserved_names() {
        let workflow: WorkflowCfg = toml::from_str(
            "type = \"autoresearch\"\nresult = \"keep-if-better\"\n\
             [[task]]\nname = \"invent\"\nkind = \"engine\"\nop = \"propose\"\n\
             [[task]]\nname = \"review\"\nkind = \"command\"\ncommand = \"true\"\ndepends_on = [\"invent\"]\n\
             [[task]]\nname = \"deploy-preview\"\nkind = \"engine\"\nop = \"apply\"\ndepends_on = [\"review\"]\n\
             [[task]]\nname = \"benchmark-a\"\nkind = \"engine\"\nop = \"measure\"\ndepends_on = [\"deploy-preview\"]\n\
             [[task]]\nname = \"explain-score\"\nkind = \"command\"\ncommand = \"true\"\ndepends_on = [\"benchmark-a\"]\n\
             [[task]]\nname = \"keep-if-better\"\nkind = \"engine\"\nop = \"decide\"\nsource = \"benchmark-a\"\ndepends_on = [\"benchmark-a\", \"explain-score\"]\n",
        )
        .unwrap();
        let plan =
            iteration_template(Some(&workflow), &WorkflowCaps::autoresearch_engine()).unwrap();
        let names: Vec<&str> = plan.tasks_topo().map(|task| task.name.0.as_str()).collect();
        assert_eq!(
            names,
            [
                "invent",
                "review",
                "deploy-preview",
                "benchmark-a",
                "explain-score",
                "keep-if-better"
            ]
        );
    }

    #[test]
    fn authored_measurement_subgraph_drives_real_decisions() {
        let workflow = r#"
            [workflow]
            type = "autoresearch"
            result = "choose"
            [[workflow.task]]
            name = "invent"
            kind = "engine"
            op = "propose"
            [[workflow.task]]
            name = "apply"
            kind = "engine"
            op = "apply"
            depends_on = ["invent"]
            [[workflow.task]]
            name = "correctness"
            kind = "evaluate"
            command = "v=$(cat value.txt); printf '{\"score\": %s, \"pass\": true}\n' \"$v\""
            depends_on = ["apply"]
            isolation = "worktree"
            [[workflow.task]]
            name = "score"
            kind = "evaluate"
            command = "v=$(cat value.txt); printf '{\"score\": %s, \"solved\": false}\n' \"$v\""
            depends_on = ["apply"]
            isolation = "worktree"
            [[workflow.task]]
            name = "grade"
            kind = "engine"
            op = "grade"
            source = "score"
            depends_on = ["correctness", "score"]
            join = "passed"
            [[workflow.task]]
            name = "choose"
            kind = "engine"
            op = "decide"
            source = "grade"
            depends_on = ["grade"]
        "#;
        let trace = run_counter_cfg(true, 2, BUMP, Some(workflow));
        assert_eq!(trace.best, 3.0, "{}", describe(&trace));
        assert_eq!(
            trace
                .rows
                .iter()
                .map(|row| row.1.as_str())
                .collect::<Vec<_>>(),
            ["baseline", "keep", "keep"]
        );
        assert_eq!(trace.shutdown, "finished");
        // Every declared evidence task ran and passed, so the record carries no skips.
        for (i, ev) in trace.row_evidence.iter().enumerate().skip(1) {
            let dispositions: Vec<&str> = ev
                .iter()
                .map(|e| e["disposition"].as_str().unwrap())
                .collect();
            assert_eq!(
                dispositions,
                ["passed", "passed"],
                "row {i} evidence: {ev:?}"
            );
        }
    }

    /// The run-6 shape: a declared evidence task never runs (here: unrunnable on the
    /// substrate) and another errors, `join = "passed"` folds only the survivor, and the
    /// grade output + row must still record all three declared dispositions.
    #[test]
    fn grade_records_declared_evidence_dispositions() {
        let workflow = r#"
            [workflow]
            type = "autoresearch"
            result = "choose"
            [[workflow.task]]
            name = "invent"
            kind = "engine"
            op = "propose"
            [[workflow.task]]
            name = "apply"
            kind = "engine"
            op = "apply"
            depends_on = ["invent"]
            [[workflow.task]]
            name = "score"
            kind = "evaluate"
            command = "v=$(cat value.txt); printf '{\"score\": %s, \"pass\": true}\n' \"$v\""
            depends_on = ["apply"]
            [[workflow.task]]
            name = "flaky"
            kind = "evaluate"
            command = "echo broken >&2; exit 7"
            depends_on = ["apply"]
            required = false
            [[workflow.task]]
            name = "tensor-pipe"
            kind = "evaluate"
            command = "true"
            depends_on = ["apply"]
            required = false
            needs = "ncu"
            [[workflow.task]]
            name = "grade"
            kind = "engine"
            op = "grade"
            source = "score"
            depends_on = ["score", "flaky", "tensor-pipe"]
            join = "passed"
            [[workflow.task]]
            name = "choose"
            kind = "engine"
            op = "decide"
            source = "grade"
            depends_on = ["grade"]
        "#;
        let trace = run_counter_cfg(true, 1, BUMP, Some(workflow));
        assert_eq!(
            trace
                .rows
                .iter()
                .map(|row| row.1.as_str())
                .collect::<Vec<_>>(),
            ["baseline", "keep"],
            "{}",
            describe(&trace)
        );

        // The grade task_result output carries the declared set, not just the folded one.
        let (_, status, output) = trace
            .task_results
            .iter()
            .find(|(t, _, _)| t == "grade")
            .expect("grade task_result on the wire");
        assert_eq!(status, "pass");
        let out = output.as_ref().unwrap();
        assert_eq!(out["evidence_count"], 1, "only the passing rung folded");
        let wire_evidence = out["evidence"].as_array().unwrap();

        // The kept row carries the same record, in declaration order.
        let row_evidence = &trace.row_evidence[1];
        assert_eq!(wire_evidence, row_evidence);
        let by_task: Vec<(&str, &str, &str)> = row_evidence
            .iter()
            .map(|e| {
                (
                    e["task"].as_str().unwrap(),
                    e["disposition"].as_str().unwrap(),
                    e["note"].as_str().unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(by_task[0], ("score", "passed", ""));
        assert_eq!(by_task[1].0, "flaky");
        assert_eq!(by_task[1].1, "failed");
        assert!(
            by_task[1].2.contains("exit 7"),
            "the failed rung keeps its note: {:?}",
            by_task[1].2
        );
        assert_eq!(by_task[2].0, "tensor-pipe");
        assert_eq!(by_task[2].1, "skipped");
        assert!(
            by_task[2].2.contains("unrunnable"),
            "the skipped rung keeps its note: {:?}",
            by_task[2].2
        );
    }

    /// A kept row must carry the score source's broker log handles, so the agent can
    /// pull the full log with `fetch_log` instead of squinting at the note's tail.
    #[test]
    fn grade_detail_carries_the_score_source_log_handles() {
        let workflow = r#"
            [workflow]
            type = "autoresearch"
            result = "choose"
            [[workflow.task]]
            name = "invent"
            kind = "engine"
            op = "propose"
            [[workflow.task]]
            name = "apply"
            kind = "engine"
            op = "apply"
            depends_on = ["invent"]
            [[workflow.task]]
            name = "score"
            kind = "evaluate"
            command = "v=$(cat value.txt); printf '{\"score\": %s, \"pass\": true, \"logs\": [\"job-a.log\", 7]}\n' \"$v\""
            depends_on = ["apply"]
            [[workflow.task]]
            name = "grade"
            kind = "engine"
            op = "grade"
            source = "score"
            depends_on = ["score"]
            join = "passed"
            [[workflow.task]]
            name = "choose"
            kind = "engine"
            op = "decide"
            source = "grade"
            depends_on = ["grade"]
        "#;
        let trace = run_counter_cfg(true, 1, BUMP, Some(workflow));
        assert_eq!(
            trace
                .rows
                .iter()
                .map(|row| row.1.as_str())
                .collect::<Vec<_>>(),
            ["baseline", "keep"],
            "{}",
            describe(&trace)
        );
        let detail: serde_json::Value = serde_json::from_str(&trace.row_details[1]).unwrap();
        assert_eq!(detail["score_source"], "score");
        assert_eq!(
            detail["logs"].as_array().unwrap(),
            &vec![serde_json::Value::from("job-a.log")],
            "non-string members are dropped: {}",
            trace.row_details[1]
        );
    }

    /// What one counter run left behind, for diffing the two paths.
    struct RunTrace {
        /// Session-event kind sequence, in emission order.
        kinds: Vec<String>,
        /// Decided rows as (iter, decision, score).
        rows: Vec<(u32, String, Option<f64>)>,
        /// The summary's best score.
        best: f64,
        /// Commits in the workspace (baseline + keeps).
        commits: u32,
        /// The shutdown event's outcome token.
        shutdown: String,
        /// Note messages, in order. A run that misbehaves usually says why in one.
        notes: Vec<String>,
        /// `iter` values on the task_result lines, in emission order (graph runs only).
        task_iters: Vec<u32>,
        /// Each row's `evidence` array (empty when absent), parallel to `rows`.
        row_evidence: Vec<Vec<serde_json::Value>>,
        /// Each row's `detail` string (empty when absent), parallel to `rows`.
        row_details: Vec<String>,
        /// task_result lines as (task, status, output), in emission order.
        task_results: Vec<(String, String, Option<serde_json::Value>)>,
    }

    /// The deterministic proposer: value.txt += 1 every turn.
    const BUMP: &str = "#!/bin/sh\nv=$(cat value.txt); echo $((v + 1)) > value.txt\n";

    /// A proposer that regresses on its second turn, so the run exercises
    /// discard/restore: +1, then -1, then +1... The turn counter lives OUTSIDE the
    /// workspace (`../turns`), because a discard's `git reset --hard` would roll an
    /// in-workspace counter back and replay the same turn forever.
    const ZIGZAG: &str = "#!/bin/sh\n\
         n=$(cat ../turns 2>/dev/null || echo 0); n=$((n + 1)); echo \"$n\" > ../turns\n\
         v=$(cat value.txt)\n\
         if [ \"$n\" -eq 2 ]; then echo $((v - 1)) > value.txt; else echo $((v + 1)) > value.txt; fi\n";

    /// The parity harness: the counter domain end-to-end through `run_loop` with a real
    /// git workspace, a real command-backend turn, and a real measure subprocess: once per
    /// path. sh stands in for nu so the fixture is self-contained. The measure declares
    /// solved at value >= 3, so a multi-iteration run exercises the early stop.
    fn run_counter(graph_loop: bool, iterations: u32, bump: &str) -> RunTrace {
        run_counter_cfg(graph_loop, iterations, bump, None)
    }

    fn run_counter_cfg(
        graph_loop: bool,
        iterations: u32,
        bump: &str,
        workflow: Option<&str>,
    ) -> RunTrace {
        // A counter, not a timestamp: two tests starting in the same microsecond would get
        // the same name, and the first thing this does is remove_dir_all.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "crucible-graph-parity-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("value.txt"), "1\n").unwrap();
        std::fs::write(dir.join("bump.sh"), bump).unwrap();
        std::fs::write(
            dir.join("measure.sh"),
            "#!/bin/sh\nv=$(cat value.txt)\n\
             s=false; [ \"$v\" -ge 3 ] && s=true\n\
             printf '{\"valid\": true, \"score\": %s, \"solved\": %s, \"note\": \"value=%s\"}\\n' \"$v\" \"$s\" \"$v\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for f in ["bump.sh", "measure.sh"] {
                std::fs::set_permissions(dir.join(f), std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
        }
        let manifest_path = dir.join("crucible.toml");
        let workflow_block = workflow.unwrap_or("");
        std::fs::write(
            &manifest_path,
            format!(
                r#"
            [repo]
            path = "."
            [workspace]
            dir = "workspace"
            setup_cmd = "mkdir -p workspace && cp value.txt measure.sh bump.sh workspace/ && git -C workspace init -q && git -C workspace add -A && git -C workspace -c user.email=c@l -c user.name=c -c commit.gpgsign=false commit -qm baseline"
            [agent]
            backend = "command"
            agent_cmd = "./bump.sh"
            goal = "raise the value"
            [judge]
            measure_cmd = "./measure.sh"
            direction = "higher"
            objective = "value"
            {workflow_block}"#
            ),
        )
        .unwrap();

        let m = manifest::Manifest::load_frozen(&manifest_path).unwrap();
        let workspace = dir.join("workspace");
        run::manifest_setup(&m, &dir, &workspace).unwrap();
        // The real run path does this right after setup. Without it the workspace has no
        // committer identity of its own, so every keep's snapshot fails on a machine with
        // no global git config and the run silently stops ratcheting.
        crucible_vcs::vcs::ensure_repo(&workspace).unwrap();
        let state = dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let p = crate::Paths::for_manifest(workspace.clone(), state, &dir, None);

        let mut args = crate::Cli::parse_from(["crucible"]).run;
        args.manifest = Some(manifest_path);
        args.agent_backend = manifest::AgentBackend::Command;
        args.agent_cmd = m.agent.agent_cmd.clone();
        args.iterations = iterations;
        args.graph_loop = graph_loop;
        args.workflow = m.workflow.clone();

        let prep = Prepared {
            goal: "raise the value".into(),
            template: "{{GOAL}}\nStatus: {{STATUS}}\n{{STEER}}".into(),
            run_id: "parity".into(),
            prior: String::new(),
            skip_baseline: false,
            preflight: None,
            preflight_modes: Vec::new(),
            seed_diff: None,
            identity: crate::identity::RunIdentity {
                components: Vec::new(),
                manifest_hash: "h".into(),
                inject_hash: "h".into(),
                seed_hash: String::new(),
                measure_cmd: "./measure.sh".into(),
                direction: "higher".into(),
                rig: Default::default(),
                digest: "v2:0".into(),
            },
        };
        let world = m.build_world(workspace.clone());
        let judge = m.build_judge(workspace.clone(), Vec::new()).unwrap();
        let mut r =
            stream::SessionReporter::stream(&p, reporter::RunMeta::from_args(&args)).unwrap();
        let outcome = run_loop(
            &args,
            &p,
            &prep,
            &mut r,
            world.as_ref(),
            judge.as_ref(),
            LoopRuntime::default(),
        )
        .unwrap();
        if workflow.is_none() {
            assert!(outcome.improved, "the counter bump must be kept");
        }

        let log = std::fs::read_to_string(&p.session_log).unwrap();
        let mut kinds = Vec::new();
        let mut rows = Vec::new();
        let mut best = f64::NAN;
        let mut shutdown = String::new();
        let mut task_iters = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let mut row_evidence = Vec::new();
        let mut row_details = Vec::new();
        let mut task_results = Vec::new();
        for line in log.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            let kind = v["kind"].as_str().unwrap().to_string();
            match kind.as_str() {
                "row" => {
                    rows.push((
                        v["row"]["iter"].as_u64().unwrap() as u32,
                        v["row"]["decision"].as_str().unwrap().to_string(),
                        v["row"]["score"].as_f64(),
                    ));
                    row_evidence.push(v["row"]["evidence"].as_array().cloned().unwrap_or_default());
                    row_details.push(v["row"]["detail"].as_str().unwrap_or_default().to_string());
                }
                "summary" => best = v["best_score"].as_f64().unwrap(),
                "shutdown" => shutdown = v["outcome"].as_str().unwrap().to_string(),
                "note" => notes.push(v["msg"].as_str().unwrap_or("").to_string()),
                "task_result" => {
                    task_iters.push(v["iter"].as_u64().unwrap() as u32);
                    task_results.push((
                        v["task"].as_str().unwrap().to_string(),
                        v["status"].as_str().unwrap().to_string(),
                        v.get("output").filter(|o| !o.is_null()).cloned(),
                    ));
                }
                _ => {}
            }
            kinds.push(kind);
        }
        let commits = commit_count(&workspace);
        let _ = std::fs::remove_dir_all(&dir);
        RunTrace {
            kinds,
            rows,
            best,
            commits,
            shutdown,
            task_iters,
            notes,
            row_evidence,
            row_details,
            task_results,
        }
    }

    /// Everything a failed shape assertion needs to explain itself, since these runs are
    /// reproducible on some machines and not others.
    fn describe(t: &RunTrace) -> String {
        let rows: Vec<String> = t
            .rows
            .iter()
            .map(|(i, d, s)| format!("  iter {i} {d} score={s:?}"))
            .collect();
        format!(
            "\nrows:\n{}\ncommits={} best={} shutdown={}\nnotes:\n  {}",
            rows.join("\n"),
            t.commits,
            t.best,
            t.shutdown,
            t.notes.join("\n  ")
        )
    }

    fn commit_count(workspace: &Path) -> u32 {
        let out = std::process::Command::new("git")
            .args([
                "-C",
                &workspace.to_string_lossy(),
                "rev-list",
                "--count",
                "HEAD",
            ])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
    }

    /// Diff two traces: rows, best, commits, shutdown outcome, and the event kind
    /// sequence with the graph run's additive plan lines filtered out.
    fn assert_parity(legacy: &RunTrace, graph: &RunTrace) {
        assert_eq!(legacy.rows, graph.rows, "decided rows must match");
        assert_eq!(legacy.best, graph.best, "summary best_score must match");
        assert_eq!(
            legacy.commits, graph.commits,
            "kept commit count must match"
        );
        assert_eq!(
            legacy.shutdown, graph.shutdown,
            "shutdown outcome must match"
        );
        // Both traces may carry additive plan lines; the parity claim is about everything
        // else.
        let strip = |t: &RunTrace| -> Vec<String> {
            t.kinds
                .iter()
                .filter(|k| k.as_str() != "plan_admitted" && k.as_str() != "task_result")
                .cloned()
                .collect()
        };
        assert_eq!(
            strip(legacy),
            strip(graph),
            "event kind sequence must be identical minus additive plan lines"
        );
    }

    /// One iteration, identical under both paths.
    #[test]
    fn counter_parity_between_typestate_and_graph_paths() {
        let legacy = run_counter(false, 1, BUMP);
        let graph = run_counter(true, 1, BUMP);
        assert_parity(&legacy, &graph);
        assert_eq!(legacy.shutdown, "finished");
        assert_eq!(
            graph.kinds.iter().filter(|k| *k == "plan_admitted").count(),
            1,
            "one admitted plan for the single iteration"
        );
        assert_eq!(
            graph.kinds.iter().filter(|k| *k == "task_result").count(),
            4,
            "one terminal result per template task"
        );
    }

    #[test]
    fn counter_parity_multi_iteration_with_discard_restore_and_early_stop() {
        let legacy = run_counter(false, 5, ZIGZAG);
        let graph = run_counter(true, 5, ZIGZAG);

        // The intended shape, pinned on the legacy path first so a bug in BOTH paths
        // can't slide through as "parity".
        let decisions: Vec<&str> = legacy.rows.iter().map(|(_, d, _)| d.as_str()).collect();
        assert_eq!(
            decisions,
            ["baseline", "keep", "discard", "keep"],
            "legacy path: {}",
            describe(&legacy)
        );
        let scores: Vec<Option<f64>> = legacy.rows.iter().map(|(_, _, s)| *s).collect();
        assert_eq!(
            scores,
            [Some(1.0), Some(2.0), Some(1.0), Some(3.0)],
            "the discard's restore must rewind value.txt to the kept best"
        );
        assert_eq!(legacy.shutdown, "solved", "value 3 stops the run early");

        assert_parity(&legacy, &graph);
        assert_eq!(
            graph.kinds.iter().filter(|k| *k == "plan_admitted").count(),
            3,
            "one admitted plan per executed round"
        );
        assert_eq!(
            graph.task_iters,
            [1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3],
            "task_result lines carry their loop round"
        );
    }
}
