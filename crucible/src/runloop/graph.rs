//! The loop's work-graph templates and the runners that adapt the executor's task
//! contract onto the loop's `World`/`Judge`/`Reporter`. Cross-round state, keep/discard,
//! and every between-round drain stay in [`crate::runloop::driver`].
//!
//! The templates carry no budget of their own (`f64::MAX`): the driver owns the run budget
//! and checks it between rounds, so a turn that blows the cap is still measured and decided.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::args::{Args, Paths, Prepared};

use crate::agent;
use crate::control;
use crate::plan::exec::{
    Attempt, AttemptOutcome, BatchItem, ExecCfg, Substrate, TaskRunner, TaskStatus, execute,
};
use crate::plan::ir::{
    EngineOp, Isolation, Join, Plan, PlanBudget, Stage, Task, TaskKind, TaskName, ValidPlan,
};
use crate::plan::workflow::{WorkflowCaps, WorkflowCfg, WorkflowType};
use crate::process::STOP;
use crate::report::session::Row;
use crate::report::session::{EvidenceDisposition, EvidenceEntry};
use crate::report::{Reporter, TurnBudget};
use crate::runloop::step::{Decided, IterStep, Measured, TurnVerdict};
use crucible::crucible::Direction;
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
type TaskStates = Arc<Mutex<BTreeMap<TaskName, (TaskStatus, Option<String>)>>>;

/// Both holders run inside one serial `execute`, so the lock is never contended and a poisoned
/// guard carries a map that is still structurally sound. Recover it rather than panicking in a
/// runner that has a live workspace under it.
fn states_of(
    states: &TaskStates,
) -> MutexGuard<'_, BTreeMap<TaskName, (TaskStatus, Option<String>)>> {
    states.lock().unwrap_or_else(|e| e.into_inner())
}

/// Everything one graph iteration reads from the driver. Scalars are copies of the
/// segment state; the driver folds the returned [`IterStep`] + cost back itself.
pub(crate) struct IterCtx<'a> {
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
    pub workflow: Option<&'a WorkflowCfg>,
}

/// Run one admitted autoresearch iteration and return its step and cost.
pub(crate) fn run_iteration<R: Reporter>(
    cx: IterCtx<'_>,
    runner: &mut LoopTaskRunner<R>,
) -> Result<(IterStep, f64)> {
    let mut caps = WorkflowCaps::for_lane(
        cx.workflow
            .map(|workflow| workflow.workflow_type)
            .unwrap_or_default(),
    );
    if agent::supports_persistent_sessions(&runner.args) {
        caps = caps.with_persistent_sessions();
    }
    let plan = iteration_template(cx.workflow, &caps)?;
    let result_task = cx
        .workflow
        .filter(|workflow| !workflow.is_legacy_splice())
        .and_then(|workflow| workflow.result.clone())
        .unwrap_or_else(|| "decide".into());
    runner
        .r
        .plan_event(&crate::plan::events::plan_admitted_event(&plan));
    runner.arm(&cx);
    let task_states = Arc::clone(&runner.task_states);
    // The runner and the on_result hook both need the reporter; collect the wire lines
    // here and append them after the executor returns (they're additive either way).
    let mut task_events = Vec::new();
    let outcome = execute(
        &plan,
        &Substrate::default(),
        ExecCfg::default(),
        &mut *runner,
        |task, result| {
            states_of(&task_states).insert(task.name.clone(), (result.status, result.note.clone()));
            task_events.push(crate::plan::events::task_result_event(
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
/// under the reserved [`crate::plan::ir::KEPT_INPUT`] key, so a command/evaluate task
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
    r.plan_event(&crate::plan::events::plan_admitted_event(&plan));

    let mut runner = EpilogueRunner {
        inner: crate::plan::harness::HarnessRunner {
            args: args.clone(),
            paths: p.clone(),
            commit_per_task: false,
            captured_bytes: std::sync::atomic::AtomicU64::new(0),
            staged: Default::default(),
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
            task_events.push(crate::plan::events::task_result_event(
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
        inputs.insert(crate::plan::ir::KEPT_INPUT.into(), self.kept.clone());
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

    fn stage(&mut self, task: &Task, producers: &[&Task]) -> Result<(), String> {
        self.inner.stage(task, producers)
    }

    fn settled(&mut self, task: &Task, passed: bool) {
        self.inner.settled(task, passed);
    }

    fn has_captured_files(&self, task: &Task) -> bool {
        self.inner.has_captured_files(task)
    }

    fn drop_captured(&mut self, task: &Task) {
        self.inner.drop_captured(task);
    }
}

/// What the propose task's post-turn drains decided, parked here for the driver: the
/// executor only sees pass/fail, the loop control travels out of band.
enum Signal {
    Discard(String),
    Escalate,
    Park(crate::control::provisioning::PendingProvisioning),
    Stop,
}

/// [`TaskRunner`] over the driver's trait objects, armed once per iteration.
pub(crate) struct LoopTaskRunner<R: Reporter> {
    args: Args,
    p: Paths,
    world: Arc<dyn World>,
    judge: Arc<dyn Judge>,
    control: Option<Arc<control::ControlState>>,
    heartbeat: Option<Arc<crate::control::heartbeat::Heartbeat>>,
    pub(crate) started: Instant,
    pub(crate) r: R,
    it: u32,
    prompt: String,
    resume_prompt: String,
    rows: Vec<Row>,
    ctx: MeasureCtx,
    best_score: f64,
    best_tiebreak: Option<f64>,
    spent_before: f64,
    signal: Option<Signal>,
    measured: BTreeMap<TaskName, Measured>,
    decided: BTreeMap<TaskName, Decided>,
    fatal: Option<anyhow::Error>,
    /// Settled-task dispositions, fed by the executor's `on_result` callback; read by
    /// `grade` to record which declared evidence tasks actually ran.
    task_states: TaskStates,
    workflow_runner: crate::plan::harness::HarnessRunner,
}

fn harness_runner(args: &Args, p: &Paths) -> crate::plan::harness::HarnessRunner {
    crate::plan::harness::HarnessRunner {
        args: args.clone(),
        paths: p.clone(),
        commit_per_task: false,
        captured_bytes: std::sync::atomic::AtomicU64::new(0),
        staged: Default::default(),
    }
}

impl<R: Reporter> LoopTaskRunner<R> {
    /// One runner per run. It owns the reporter for the run's whole life: the driver borrows
    /// the runner between iterations and the executor borrows it during one, so the two can
    /// never touch the reporter at the same time.
    pub(crate) fn new(
        args: &Args,
        p: &Paths,
        r: R,
        world: Arc<dyn World>,
        judge: Arc<dyn Judge>,
        control: Option<Arc<control::ControlState>>,
        heartbeat: Option<Arc<crate::control::heartbeat::Heartbeat>>,
    ) -> Self {
        Self {
            args: args.clone(),
            p: p.clone(),
            world,
            judge,
            control,
            heartbeat,
            started: Instant::now(),
            r,
            it: 0,
            prompt: String::new(),
            resume_prompt: String::new(),
            rows: Vec::new(),
            ctx: MeasureCtx {
                baseline_score: None,
                baseline_total: None,
                best_score: None,
            },
            best_score: 0.0,
            best_tiebreak: None,
            spent_before: 0.0,
            signal: None,
            measured: BTreeMap::new(),
            decided: BTreeMap::new(),
            fatal: None,
            task_states: TaskStates::default(),
            workflow_runner: harness_runner(args, p),
        }
    }

    /// Point the runner at one iteration. The `measured`/`decided` slots flow between the
    /// engine tasks in dispatch order (serial executor), and `signal`/`fatal` carry loop
    /// control back to [`run_iteration`]; all of it starts empty each round.
    fn arm(&mut self, cx: &IterCtx<'_>) {
        self.it = cx.it;
        self.prompt = cx.prompt.to_string();
        self.resume_prompt = cx.resume_prompt.to_string();
        self.rows = cx.rows.to_vec();
        self.ctx = MeasureCtx {
            baseline_score: Some(cx.baseline_score),
            baseline_total: Some(cx.baseline_total),
            best_score: Some(cx.best_score),
        };
        self.best_score = cx.best_score;
        self.best_tiebreak = cx.best_tiebreak;
        self.spent_before = cx.spent_before;
        self.signal = None;
        self.measured.clear();
        self.decided.clear();
        self.fatal = None;
        self.task_states = TaskStates::default();
        self.workflow_runner = harness_runner(&self.args, &self.p);
    }

    fn propose(&mut self, task: &Task, prompt: &str) -> Attempt {
        let turn = self.r.run_agent(
            &self.args,
            &self.p,
            self.it,
            prompt,
            Some(&self.resume_prompt),
            task.session.as_deref(),
            TurnBudget {
                spent_before: self.spent_before,
                started: self.started,
                max_cost: crate::runloop::step::live_max_cost(&self.args, self.control.as_deref()),
                heartbeat: self.heartbeat.clone(),
            },
        );
        let cost = turn.cost;
        if let Some(control) = self.control.as_ref() {
            control.set_spend(self.spent_before + cost);
        }
        self.r
            .budget(self.spent_before + cost, self.started.elapsed());
        let turn_verdict = crate::runloop::step::drain_turn_markers(
            &mut self.r,
            &self.p,
            self.control.as_deref(),
            self.it,
            &turn,
            &self.rows,
        );
        match turn_verdict {
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
        match crate::runloop::step::measure_candidate(
            &*self.judge,
            &self.ctx,
            &self.p,
            &*self.world,
        ) {
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
        let states = states_of(&self.task_states);
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
        let mut measured =
            crate::runloop::step::measured_from_reading(reading, &self.p, &*self.world);
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
        let d = crate::runloop::step::decide_row(
            &*self.judge,
            self.best_score,
            self.best_tiebreak,
            self.it,
            m,
        );
        let out = serde_json::json!({
            "keep": d.verdict.keep,
            "solved": d.verdict.solved,
            "score": d.reading.score,
        });
        self.decided.insert(task.name.clone(), d);
        pass(out)
    }
}

impl<R: Reporter> TaskRunner for LoopTaskRunner<R> {
    fn run(&mut self, task: &Task, attempt: u32, inputs: &BTreeMap<TaskName, Value>) -> Attempt {
        match &task.task {
            TaskKind::Agent { .. }
            | TaskKind::Command { .. }
            | TaskKind::Evaluate { .. }
            | TaskKind::Report { .. } => self.workflow_runner.run(task, attempt, inputs),
            TaskKind::Engine {
                op: EngineOp::Propose,
                ..
            } => {
                let prompt = self.prompt.clone();
                self.propose(task, &prompt)
            }
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
            TaskKind::Engine {
                op: EngineOp::MeasureDiff,
                ..
            }
            | TaskKind::TopK { .. } => fail(
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

    fn stage(&mut self, task: &Task, producers: &[&Task]) -> Result<(), String> {
        self.workflow_runner.stage(task, producers)
    }

    fn settled(&mut self, task: &Task, passed: bool) {
        self.workflow_runner.settled(task, passed);
    }

    fn has_captured_files(&self, task: &Task) -> bool {
        self.workflow_runner.has_captured_files(task)
    }

    fn drop_captured(&mut self, task: &Task) {
        self.workflow_runner.drop_captured(task);
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

// ---------------------------------------------------------------------------
// The wide tournament as a template: N isolated propose tasks fan out in
// parallel worktrees, their diffs are scored serially on the shared deployment
// by MeasureDiff tasks, and an engine top_k with a lossy join ranks whatever
// survived. The driver seeds the deep loop from the winner's diff text.
// ---------------------------------------------------------------------------

/// The resolved wide-round config, merged from CLI flags + manifest `[search]`. CLI wins.
pub struct WideConfig {
    pub n: u32,
    pub k: u32,
    pub approaches: Vec<String>,
}

impl WideConfig {
    /// Merge CLI flags (`--wide`, `--wide-keep`) with the manifest's `[search]`. CLI wins.
    pub fn resolve(args: &Args, search: Option<&crate::manifest::SearchCfg>) -> Option<Self> {
        let n = if args.wide > 0 {
            args.wide
        } else {
            search.map(|s| s.wide).unwrap_or(0)
        };
        if n == 0 {
            return None;
        }
        let k = if args.wide_keep > 0 && args.wide > 0 {
            args.wide_keep
        } else {
            search.map(|s| s.policy_k).unwrap_or(1)
        };
        let approaches = search.map(|s| s.approaches.clone()).unwrap_or_default();
        if approaches.len() < n as usize {
            return None;
        }
        Some(WideConfig { n, k, approaches })
    }
}

/// What the wide tournament left behind for the driver.
pub(crate) struct WideOutcome {
    /// Winning candidate ids, best first.
    pub winners: Vec<u32>,
    /// Every candidate row (skip/fail/measured), for the session log.
    pub rows: Vec<Row>,
    /// Candidate id → captured diff text, for seeding the deep loop. Only winners are
    /// resolved here.
    pub diffs: BTreeMap<u32, String>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_wide_tournament<R: Reporter>(
    cfg: &WideConfig,
    args: &Args,
    p: &Paths,
    prep: &Prepared,
    r: &mut R,
    world: &Arc<dyn World>,
    judge: &Arc<dyn Judge>,
    baseline_score: f64,
) -> Result<WideOutcome> {
    r.note(&format!(
        "wide round: fanning out {} candidates (top-{} advance)",
        cfg.n, cfg.k
    ));
    let wide_dir = p.state.join("wide").join(&prep.run_id);
    std::fs::create_dir_all(&wide_dir)
        .with_context(|| format!("creating wide-round dir {}", wide_dir.display()))?;

    let direction = judge.direction();
    let plan = wide_template(cfg, prep, direction)?;
    r.plan_event(&crate::plan::events::plan_admitted_event(&plan));
    r.note("wide: starting parallel PROPOSE turns");
    let snap = world
        .snapshot("wide-pre-measure")
        .context("wide pre-measure snapshot")?;

    let mut runner = WideRunner {
        args,
        p,
        world: Arc::clone(world),
        judge: Arc::clone(judge),
        r,
        wide_dir,
        baseline_score,
        approaches: (0..cfg.n)
            .map(|id| (id, cfg.approaches[id as usize].clone()))
            .collect(),
        snap,
        measure_note_emitted: false,
        rows: Vec::new(),
        fatal: None,
    };
    let mut task_events = Vec::new();
    let outcome = execute(
        &plan,
        &Substrate::default(),
        ExecCfg::default(),
        &mut runner,
        |task, result| {
            task_events.push(crate::plan::events::task_result_event(
                plan.plan().version,
                0,
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

    let mut winners: Vec<u32> = Vec::new();
    if let Some(kept) = outcome
        .results
        .get(&"pick".into())
        .and_then(|r| r.output.as_ref())
        .and_then(|o| o.get("kept"))
        .and_then(Value::as_array)
    {
        for (rank, entry) in kept.iter().enumerate() {
            let id = entry
                .get("task")
                .and_then(Value::as_str)
                .and_then(|n| n.rsplit('-').next())
                .and_then(|s| s.parse::<u32>().ok());
            let score = entry.get("score").and_then(Value::as_f64);
            if let (Some(id), Some(score)) = (id, score) {
                runner.r.note(&format!(
                    "wide round: rank {} = candidate {id} (score: {score:.2})",
                    rank + 1
                ));
                winners.push(id);
            }
        }
    }
    if winners.is_empty() {
        runner
            .r
            .note("wide round: no candidates scored, deep loop starts from baseline");
    }

    let diffs: BTreeMap<u32, String> = winners
        .iter()
        .filter_map(|id| {
            outcome
                .results
                .get(&TaskName(format!("propose-{id}")))
                .and_then(|r| r.output.as_ref())
                .and_then(|o| o.get("diff"))
                .and_then(Value::as_str)
                .map(|d| (*id, d.to_string()))
        })
        .collect();
    Ok(WideOutcome {
        winners,
        rows: runner.rows,
        diffs,
    })
}

/// The wide tournament template: `propose-{i} (isolated, advisory) → measure-{i}
/// (advisory) → pick (top_k over whatever passed)`. Everything is advisory: a dead
/// candidate never gates the tournament, matching the loop's skip/fail rows. The budget
/// stays driver-owned, like the iteration template's.
fn wide_template(cfg: &WideConfig, prep: &Prepared, direction: Direction) -> Result<ValidPlan> {
    let mut tasks: Vec<Task> = Vec::new();
    for id in 0..cfg.n {
        tasks.push(Task {
            name: TaskName(format!("propose-{id}")),
            task: TaskKind::Agent {
                prompt: render_wide_prompt(
                    &prep.template,
                    &prep.goal,
                    &cfg.approaches[id as usize],
                ),
                harness: None,
                model: None,
                effort: None,
            },
            depends_on: vec![],
            session: None,
            needs: "any".to_string(),
            required: false,
            isolation: Some(Isolation::Worktree),
            join: Join::All,
            stage: Stage::Iteration,
            emits: Vec::new(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
        });
    }
    for id in 0..cfg.n {
        tasks.push(Task {
            name: TaskName(format!("measure-{id}")),
            task: TaskKind::Engine {
                op: EngineOp::MeasureDiff,
                source: None,
                tiebreak: None,
            },
            depends_on: vec![TaskName(format!("propose-{id}"))],
            session: None,
            needs: "any".to_string(),
            required: false,
            isolation: None,
            join: Join::All,
            stage: Stage::Iteration,
            emits: Vec::new(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
        });
    }
    tasks.push(Task {
        name: "pick".into(),
        task: TaskKind::TopK {
            k: cfg.k,
            direction,
        },
        depends_on: (0..cfg.n)
            .map(|id| TaskName(format!("measure-{id}")))
            .collect(),
        session: None,
        needs: "any".to_string(),
        required: false,
        isolation: None,
        join: Join::Passed,
        stage: Stage::Iteration,
        emits: Vec::new(),
        emits_files: Vec::new(),
        over: None,
        max_fanout: None,
    });
    Plan {
        version: 1,
        reason: None,
        budget: PlanBudget { usd: f64::MAX },
        tasks,
    }
    .validate()
    .context("building the wide tournament template")
}

/// [`TaskRunner`] for the wide template. Propose batches run threaded (one worktree per
/// candidate); MeasureDiff serializes on the main workspace.
struct WideRunner<'a, R: Reporter> {
    args: &'a Args,
    p: &'a Paths,
    world: Arc<dyn World>,
    judge: Arc<dyn Judge>,
    r: &'a mut R,
    wide_dir: PathBuf,
    baseline_score: f64,
    approaches: BTreeMap<u32, String>,
    /// The pre-measure rollback token every scoring pass restores to.
    snap: String,
    measure_note_emitted: bool,
    rows: Vec<Row>,
    /// A failed world restore leaves the workspace in an unknown state; abort the run.
    fatal: Option<anyhow::Error>,
}

impl<R: Reporter> WideRunner<'_, R> {
    fn restore_or_fatal(&mut self) -> bool {
        match self.world.restore(&self.snap) {
            Ok(()) => true,
            Err(e) => {
                self.fatal = Some(e.context("restoring after a wide candidate measure"));
                false
            }
        }
    }

    fn measure_diff(&mut self, task: &Task, inputs: &BTreeMap<TaskName, Value>) -> Attempt {
        if !self.measure_note_emitted {
            self.r.note("wide: measuring candidates serially");
            self.measure_note_emitted = true;
        }
        if STOP.load(Ordering::SeqCst) {
            return fail(0.0, "stop requested".to_string());
        }
        let Some(id) = candidate_id(&task.name) else {
            return fail(0.0, format!("task {} has no candidate id", task.name));
        };
        let approach = self.approaches.get(&id).cloned().unwrap_or_default();
        let diff = inputs
            .values()
            .next()
            .and_then(|v| v.get("diff"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if diff.trim().is_empty() {
            self.r
                .note(&format!("wide candidate {id}: no diff produced, skipping"));
            self.rows.push(Row {
                iter: 0,
                decision: "wide-skip".into(),
                note: format!("candidate {id} ({approach}) produced no diff"),
                phase: Some("wide".into()),
                ..Default::default()
            });
            return fail(0.0, "no diff produced".to_string());
        }
        self.r.note(&format!(
            "wide candidate {id}: measuring (approach: {approach})"
        ));
        if let Err(e) = crate::plan::worktree::apply(&self.p.workspace, &diff) {
            self.r
                .note(&format!("wide candidate {id}: apply failed: {e:#}"));
            if !self.restore_or_fatal() {
                return fail(0.0, "restore failed".to_string());
            }
            self.rows.push(Row {
                iter: 0,
                decision: "wide-fail".into(),
                note: format!("candidate {id} apply failed: {e:#}"),
                phase: Some("wide".into()),
                ..Default::default()
            });
            return fail(0.0, format!("apply failed: {e:#}"));
        }
        if let Err(e) = self.world.apply() {
            self.r
                .note(&format!("wide candidate {id}: world apply failed: {e:#}"));
            if !self.restore_or_fatal() {
                return fail(0.0, "restore failed".to_string());
            }
            self.rows.push(Row {
                iter: 0,
                decision: "wide-fail".into(),
                note: format!("candidate {id} world apply failed: {e:#}"),
                phase: Some("wide".into()),
                ..Default::default()
            });
            return fail(0.0, format!("world apply failed: {e:#}"));
        }
        let ctx = MeasureCtx {
            baseline_score: Some(self.baseline_score),
            baseline_total: None,
            best_score: Some(self.baseline_score),
        };
        match self.judge.measure(&ctx) {
            Ok(reading) => {
                let (diff_text, diffstat) = self.world.staged_diff();
                let decision = if self.judge.decide(&reading, self.baseline_score, None).keep {
                    "wide-keep"
                } else {
                    "wide-discard"
                };
                let row = Row {
                    iter: 0,
                    decision: decision.into(),
                    note: format!("[wide candidate {id}] {approach} — {}", reading.note),
                    detail: self.judge.detail(&reading),
                    diff: diff_text,
                    diffstat,
                    score: reading.score,
                    tiebreak: reading.tiebreak,
                    total: reading.detail.get("total").and_then(Value::as_u64),
                    phase: Some("wide".into()),
                    kept_snap: None,
                    evidence: Vec::new(),
                    candidate_md: String::new(),
                };
                self.r.row(&row, false);
                self.rows.push(row);
                if !self.restore_or_fatal() {
                    return fail(0.0, "restore failed".to_string());
                }
                match reading.score {
                    Some(s) if s.is_finite() => pass(serde_json::json!({ "score": s })),
                    // The reducer needs a finite number, so a scoreless candidate
                    // does not rank.
                    _ => fail(0.0, "no finite score to rank".to_string()),
                }
            }
            Err(e) => {
                self.r
                    .note(&format!("wide candidate {id}: measure failed: {e:#}"));
                self.rows.push(Row {
                    iter: 0,
                    decision: "wide-fail".into(),
                    note: format!("candidate {id} measure failed: {e:#}"),
                    phase: Some("wide".into()),
                    ..Default::default()
                });
                if !self.restore_or_fatal() {
                    return fail(0.0, "restore failed".to_string());
                }
                fail(0.0, format!("measure failed: {e:#}"))
            }
        }
    }
}

impl<R: Reporter> TaskRunner for WideRunner<'_, R> {
    fn run(&mut self, task: &Task, _attempt: u32, inputs: &BTreeMap<TaskName, Value>) -> Attempt {
        match &task.task {
            TaskKind::Agent { prompt, .. } => {
                // A batch of one (wide n=1) lands here instead of run_many.
                let Some(id) = candidate_id(&task.name) else {
                    return fail(0.0, format!("task {} has no candidate id", task.name));
                };
                let pending = match crate::plan::worktree::capture_diff(&self.p.workspace) {
                    Ok(p) => p,
                    Err(e) => {
                        return fail(0.0, format!("capturing the workspace state failed: {e:#}"));
                    }
                };
                wide_propose(
                    self.args,
                    &self.p.workspace,
                    &self.wide_dir,
                    self.p.skills.clone(),
                    id,
                    prompt,
                    &pending,
                )
            }
            TaskKind::Engine {
                op: EngineOp::MeasureDiff,
                ..
            } => self.measure_diff(task, inputs),
            other => fail(
                0.0,
                format!(
                    "unexpected task kind in the wide template: {}",
                    other.label()
                ),
            ),
        }
    }

    fn run_many(&mut self, batch: &[BatchItem<'_>]) -> Vec<Attempt> {
        let workspace = self.p.workspace.clone();
        let wide_dir = self.wide_dir.clone();
        let skills = self.p.skills.clone();
        let args = self.args;
        // Every candidate clones the same workspace, so its pending state is captured once
        // here: concurrent `git add -A` in one repo races on `.git/index.lock`.
        let pending = match crate::plan::worktree::capture_diff(&workspace) {
            Ok(p) => p,
            Err(e) => {
                let note = format!("capturing the workspace state failed: {e:#}");
                return batch.iter().map(|_| fail(0.0, note.clone())).collect();
            }
        };
        std::thread::scope(|s| {
            let handles: Vec<_> = batch
                .iter()
                .map(|b| {
                    let workspace = workspace.clone();
                    let wide_dir = wide_dir.clone();
                    let skills = skills.clone();
                    let args = args.clone();
                    let pending = pending.as_str();
                    let parsed = match (&b.task.task, candidate_id(&b.task.name)) {
                        (TaskKind::Agent { prompt, .. }, Some(id)) => Some((id, prompt.clone())),
                        _ => None,
                    };
                    s.spawn(move || match parsed {
                        Some((id, prompt)) => {
                            wide_propose(&args, &workspace, &wide_dir, skills, id, &prompt, pending)
                        }
                        None => fail(0.0, "non-agent task in a propose batch".to_string()),
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| fail(0.0, "propose thread panicked".to_string()))
                })
                .collect()
        })
    }
}

/// One candidate's PROPOSE turn in a private worktree: clone, run the turn, capture the
/// staged diff as the task output, clean up. The diff text is the only thing that leaves
/// the worktree. Wide turns are not streamed and their cost is not booked against the run
/// budget.
fn wide_propose(
    args: &Args,
    workspace: &Path,
    wide_dir: &Path,
    skills: Option<PathBuf>,
    id: u32,
    prompt: &str,
    pending: &str,
) -> Attempt {
    if STOP.load(Ordering::SeqCst) {
        return pass(serde_json::json!({ "diff": "" }));
    }
    let worktree = wide_dir.join(format!("candidate-{id}"));
    if let Err(e) = crate::plan::worktree::setup(workspace, &worktree, pending) {
        return fail(0.0, format!("worktree setup failed: {e:#}"));
    }
    let cand_paths = Paths::for_worktree(worktree.clone(), skills);
    let _ = std::fs::create_dir_all(&cand_paths.state);
    let turn = agent::run_turn(args, &cand_paths, prompt, false, |_line, _stream, _ev| {});
    if let Some(failure) = turn.failure() {
        let _ = std::fs::remove_dir_all(&worktree);
        return fail(turn.cost_usd, failure.to_string());
    }
    let diff = crate::plan::worktree::capture_diff(&worktree);
    let _ = std::fs::remove_dir_all(&worktree);
    match diff {
        Ok(diff) => pass(serde_json::json!({ "diff": diff })),
        Err(e) => fail(0.0, format!("capturing the candidate diff failed: {e:#}")),
    }
}

/// The numeric suffix of `propose-{id}` / `measure-{id}` task names.
fn candidate_id(name: &TaskName) -> Option<u32> {
    name.0.rsplit('-').next().and_then(|s| s.parse().ok())
}

/// Build the wide-round prompt. The approach biases the candidate; the template provides
/// the domain's structure. The status is omitted (no prior score in the wide round).
fn render_wide_prompt(template: &str, goal: &str, approach: &str) -> String {
    let status = "no prior score (wide round, first attempt)";
    let out = template
        .replace("{{GOAL}}", goal.trim())
        .replace("{{STATUS}}", status)
        .replace("{{BEST_SCORE}}", status)
        .replace("{{STEER}}", "");
    format!(
        "## Approach constraint (MANDATORY)\n\
         You MUST use this specific approach: {approach}\n\
         Implement a minimal, working version of this approach. Do not deviate.\n\n\
         {out}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::Prepared;
    use crate::manifest;
    use crate::report;
    use crate::report::stream;
    use crate::runloop::driver::{LoopRuntime, run_loop};
    use clap::Parser;
    use std::path::Path;

    /// The point of the ownership lift: an executor can hold the runner without borrowing the
    /// driver's frame, so the loop's graph is no longer welded to the process that started it.
    #[test]
    fn the_loop_task_runner_is_send_and_static() {
        const fn assert_send_static<T: Send + 'static>() {}
        assert_send_static::<LoopTaskRunner<crate::report::console::ConsoleReporter>>();
    }

    /// The one-task plan the forwarding tests pull a `Task` out of.
    fn task_named(name: &str) -> Task {
        let plan: crate::plan::ir::Plan = toml::from_str(&format!(
            "version = 1\n[budget]\nusd = 1.0\n[[task]]\nname = \"{name}\"\nkind = \"command\"\ncommand = \"true\"\nemits_files = [\"out.txt\"]\n"
        ))
        .unwrap();
        plan.validate()
            .unwrap()
            .tasks_topo()
            .find(|t| t.name.0 == name)
            .unwrap()
            .clone()
    }

    fn harness_runner(state: &Path) -> crate::plan::harness::HarnessRunner {
        crate::plan::harness::HarnessRunner {
            args: <crate::cli::Cli as Parser>::try_parse_from(["crucible"])
                .unwrap()
                .run,
            paths: crate::args::Paths::for_manifest(
                state.to_path_buf(),
                state.to_path_buf(),
                state,
                None,
            ),
            commit_per_task: false,
            captured_bytes: std::sync::atomic::AtomicU64::new(0),
            staged: Default::default(),
        }
    }

    /// An epilogue task consumes what the main graph declared, so the wrapper has to answer
    /// from the runner that did the capturing. Inheriting the trait default reports `false`
    /// for a set that is on disk, and the executor then stages nothing into `inputs/`.
    #[test]
    fn an_epilogue_runner_answers_captured_files_from_its_inner_runner() {
        let dir = tempfile::tempdir().unwrap();
        let task = task_named("probe");
        std::fs::create_dir_all(dir.path().join("files").join("probe")).unwrap();

        let runner = EpilogueRunner {
            inner: harness_runner(dir.path()),
            kept: Value::Null,
        };

        assert!(
            runner.has_captured_files(&task),
            "the epilogue wrapper reported no captured set for a task whose files are on disk"
        );
        assert!(
            !runner.has_captured_files(&task_named("never-ran")),
            "the wrapper claimed a captured set for a task that captured nothing"
        );
    }

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
            false,
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
        let trace = run_counter_cfg(true, 2, BUMP, false, Some(workflow));
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
        let trace = run_counter_cfg(true, 1, BUMP, false, Some(workflow));
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
        let trace = run_counter_cfg(true, 1, BUMP, false, Some(workflow));
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
        run_counter_cfg(graph_loop, iterations, bump, false, None)
    }

    fn run_counter_cfg(
        graph_loop: bool,
        iterations: u32,
        bump: &str,
        wide: bool,
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
        let search_block = if wide {
            "\n[search]\nwide = 3\napproaches = [\"plus one\", \"add a unit\", \"increment\"]\npolicy_k = 1\n"
        } else {
            ""
        };
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
            {search_block}{workflow_block}"#
            ),
        )
        .unwrap();

        let m = manifest::Manifest::load_frozen(&manifest_path).unwrap();
        let workspace = dir.join("workspace");
        crate::cli::workspace::manifest_setup(&m, &dir, &workspace).unwrap();
        // The real run path does this right after setup. Without it the workspace has no
        // committer identity of its own, so every keep's snapshot fails on a machine with
        // no global git config and the run silently stops ratcheting.
        crucible_vcs::vcs::ensure_repo(&workspace).unwrap();
        let state = dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let p = crate::args::Paths::for_manifest(workspace.clone(), state, &dir, None);

        let mut args = crate::cli::Cli::parse_from(["crucible"]).run;
        args.manifest = Some(manifest_path);
        args.agent_backend = manifest::AgentBackend::Command;
        args.agent_cmd = m.agent.agent_cmd.clone();
        args.iterations = iterations;
        args.graph_loop = graph_loop;
        args.search = m.search.clone();
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
        let r = stream::SessionReporter::stream(&p, report::RunMeta::from_args(&args)).unwrap();
        let (_r, outcome) = run_loop(&args, &p, &prep, r, &world, &judge, LoopRuntime::default());
        let outcome = outcome.unwrap();
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
        // Both traces may carry additive plan lines (the wide tournament is graph-shaped
        // on both loop paths); the parity claim is about everything else.
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

    /// The wide tournament as a template: parallel isolated proposes, serial
    /// diff scoring, top_k, winner seed: produces the same rows, notes order, seed
    /// state, and event sequence under both paths, then the deep loop runs on
    /// top of the seeded workspace under both paths.
    #[test]
    fn counter_parity_wide_tournament() {
        let legacy = run_counter_cfg(false, 2, BUMP, true, None);
        let graph = run_counter_cfg(true, 2, BUMP, true, None);

        // Shape pinned on the legacy path first: 3 measured candidates at value 2 (each
        // row appears twice on the wire: once at measure time, once in the driver's
        // fold), then the seeded deep loop solves at 3 on its first iteration.
        let decisions: Vec<&str> = legacy.rows.iter().map(|(_, d, _)| d.as_str()).collect();
        assert_eq!(
            decisions,
            [
                "baseline",
                "wide-keep",
                "wide-keep",
                "wide-keep", // measure-time emissions
                "wide-keep",
                "wide-keep",
                "wide-keep", // driver re-emissions
                "keep",      // deep iteration on the seeded workspace
            ]
        );
        let scores: Vec<Option<f64>> = legacy.rows.iter().map(|(_, _, s)| *s).collect();
        assert_eq!(
            scores,
            [
                Some(1.0),
                Some(2.0),
                Some(2.0),
                Some(2.0),
                Some(2.0),
                Some(2.0),
                Some(2.0),
                Some(3.0)
            ],
            "the winner seed must actually land: the deep iteration starts from 2"
        );
        assert_eq!(legacy.shutdown, "solved");

        assert_parity(&legacy, &graph);
        assert_eq!(
            graph.kinds.iter().filter(|k| *k == "plan_admitted").count(),
            2,
            "one admitted plan for the tournament, one for the deep round"
        );
        assert_eq!(
            graph.kinds.iter().filter(|k| *k == "task_result").count(),
            7 + 4,
            "3 proposes + 3 measures + pick, then the 4 deep-round tasks"
        );
    }

    /// The full round loop under the graph path: keep, a regression that
    /// exercises discard/restore, then the solve that stops the run early (3 of 5
    /// budgeted iterations): identical to the typestate path.
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

    fn default_args() -> crate::args::Args {
        crate::cli::Cli::parse_from(["crucible"]).run
    }

    fn search_cfg(wide: u32, approaches: &[&str], k: u32) -> crate::manifest::SearchCfg {
        crate::manifest::SearchCfg {
            wide,
            approaches: approaches.iter().map(|s| s.to_string()).collect(),
            policy: "top-k".into(),
            policy_k: k,
        }
    }

    #[test]
    fn wide_config_resolve_none_when_no_wide() {
        assert!(WideConfig::resolve(&default_args(), None).is_none());
    }

    #[test]
    fn wide_config_resolve_from_cli() {
        let search = search_cfg(3, &["a", "b", "c"], 1);
        let args = crate::args::Args {
            wide: 3,
            wide_keep: 2,
            ..default_args()
        };
        let cfg = WideConfig::resolve(&args, Some(&search)).unwrap();
        assert_eq!(cfg.n, 3);
        assert_eq!(cfg.k, 2);
    }

    #[test]
    fn wide_config_resolve_from_manifest() {
        let search = search_cfg(4, &["a", "b", "c", "d"], 2);
        let cfg = WideConfig::resolve(&default_args(), Some(&search)).unwrap();
        assert_eq!(cfg.n, 4);
        assert_eq!(cfg.k, 2);
    }

    #[test]
    fn wide_config_cli_wide_wins_over_manifest_wide() {
        let search = search_cfg(5, &["a", "b", "c"], 1);
        let args = crate::args::Args {
            wide: 3,
            wide_keep: 1,
            ..default_args()
        };
        assert_eq!(WideConfig::resolve(&args, Some(&search)).unwrap().n, 3);
    }

    #[test]
    fn wide_config_none_when_too_few_approaches() {
        let search = search_cfg(3, &["a"], 1);
        let args = crate::args::Args {
            wide: 3,
            wide_keep: 1,
            ..default_args()
        };
        assert!(WideConfig::resolve(&args, Some(&search)).is_none());
    }

    #[test]
    fn wide_prompt_includes_approach_constraint() {
        let prompt = render_wide_prompt(
            "goal={{GOAL}} status={{BEST_SCORE}}",
            "lower p99",
            "metrics-scrape approach",
        );
        assert!(prompt.contains("metrics-scrape approach"));
        assert!(prompt.contains("MUST use this specific approach"));
        assert!(prompt.contains("goal=lower p99"));
    }

    #[test]
    fn wide_prompt_replaces_steer_placeholder() {
        let prompt = render_wide_prompt(
            "{{GOAL}} {{STATUS}} steer:{{STEER}}",
            "improve throughput",
            "batch-all",
        );
        assert!(prompt.contains("improve throughput"));
        assert!(prompt.contains("steer:"));
        assert!(!prompt.contains("{{STEER}}"));
    }

    #[test]
    fn wide_prompt_without_steer_placeholder() {
        let prompt = render_wide_prompt("goal={{GOAL}}", "my goal", "approach-x");
        assert!(prompt.contains("goal=my goal"));
        assert!(!prompt.contains("{{STEER}}"));
    }
}
