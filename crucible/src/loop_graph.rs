//! The loop's work-graph templates and the runners that adapt the executor's task
//! contract onto the loop's `World`/`Judge`/`Reporter`. Cross-round state, keep/discard,
//! and every between-round drain stay in [`crate::loop_driver`].
//!
//! The templates carry no budget of their own (`f64::MAX`): the driver owns the run budget
//! and checks it between rounds, so a turn that blows the cap is still measured and decided.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::crucible::{Judge, MeasureCtx, World};
use crate::loop_driver::{self, Decided, IterStep, Measured, TurnVerdict};
use crate::manifest::{WorkflowCaps, WorkflowCfg, WorkflowType};
use crate::plan::exec::{
    Attempt, AttemptOutcome, BatchItem, ExecCfg, Substrate, TaskRunner, execute,
};
use crate::plan::ir::{
    Direction, EngineOp, Isolation, Join, Plan, PlanBudget, Task, TaskKind, TaskName, ValidPlan,
};
use crate::reporter::{Reporter, Row};
use crate::{Args, Paths, Prepared, STOP, agent, control};

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
    /// Run spend before this iteration, so the runner reports the same cumulative budget
    /// the typestate path would after the turn.
    pub spent_before: f64,
    pub started: Instant,
    pub workflow: Option<&'a WorkflowCfg>,
}

/// Run one admitted autoresearch iteration and return its step and cost.
pub(crate) fn run_iteration<R: Reporter>(cx: IterCtx<'_>, r: &mut R) -> Result<(IterStep, f64)> {
    let mut caps = WorkflowCaps::autoresearch_engine();
    if agent::supports_persistent_sessions(cx.args) {
        caps = caps.with_persistent_sessions();
    }
    let plan = iteration_template(cx.workflow, &caps)?;
    let result_task = cx
        .workflow
        .filter(|workflow| !workflow.is_legacy_splice())
        .and_then(|workflow| workflow.result.clone())
        .unwrap_or_else(|| "decide".into());
    r.plan_event(&crate::plan::cli::plan_admitted_event(&plan));

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
        spent_before: cx.spent_before,
        started: cx.started,
        signal: None,
        measured: BTreeMap::new(),
        decided: BTreeMap::new(),
        fatal: None,
        workflow_runner: crate::plan::harness::HarnessRunner {
            args: cx.args.clone(),
            paths: cx.p.clone(),
        },
    };
    // The runner and the on_result hook both need the reporter; collect the wire lines
    // here and append them after the executor returns (they're additive either way).
    let mut task_events = Vec::new();
    let outcome = execute(
        &plan,
        &Substrate::default(),
        ExecCfg::default(),
        &mut runner,
        |task, result| {
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
        Some(Signal::Discard) => IterStep::Discarded,
        Some(Signal::Escalate) => IterStep::Escalated,
        Some(Signal::Park(pp)) => IterStep::Parked(pp),
        Some(Signal::Stop) => IterStep::Stopped,
        None => match runner.decided.remove(&result_task) {
            Some(d) => IterStep::Decided(Box::new(d)),
            // A pre-gate rejection discards the candidate.
            None => match &outcome.exit {
                crate::plan::exec::PlanExit::ShortCircuit { task } => {
                    let why = outcome
                        .results
                        .get(task)
                        .and_then(|r| r.note.clone())
                        .unwrap_or_default();
                    runner.r.note(&format!(
                        "workflow task {task} rejected the candidate (discarding iter {}): {why}",
                        cx.it
                    ));
                    IterStep::Discarded
                }
                exit => anyhow::bail!(
                    "graph iteration ended with neither a decision nor a control signal (exit: {exit:?})"
                ),
            },
        },
    };
    Ok((step, outcome.spent_usd))
}

/// Build and admit the default or authored iteration graph.
fn iteration_template(workflow: Option<&WorkflowCfg>, caps: &WorkflowCaps) -> Result<ValidPlan> {
    if let Some(workflow) = workflow.filter(|workflow| !workflow.is_legacy_splice()) {
        workflow
            .admit(caps)
            .context("admitting authored workflow into the autoresearch loop")?;
        return Plan {
            version: 1,
            reason: None,
            budget: PlanBudget { usd: f64::MAX },
            tasks: workflow.tasks.clone(),
        }
        .validate()
        .context("building authored iteration workflow");
    }

    let engine =
        |name: &str, op: EngineOp, source: Option<TaskName>, deps: Vec<TaskName>| -> Task {
            Task {
                name: name.into(),
                task: TaskKind::Engine { op, source },
                depends_on: deps,
                session: None,
                needs: "any".to_string(),
                required: true,
                isolation: None,
                join: Join::default(),
            }
        };
    let mut tasks = vec![engine("propose", EngineOp::Propose, None, vec![])];

    // Legacy splice tasks run between `propose` and `apply`; `apply` waits on every sink.
    let mut apply_deps = vec![TaskName("propose".to_string())];
    if let Some(w) = workflow.filter(|w| !w.tasks.is_empty()) {
        for t in &w.tasks {
            let mut t = t.clone();
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

/// What the propose task's post-turn drains decided, parked here for the driver: the
/// executor only sees pass/fail, the loop control travels out of band.
enum Signal {
    Discard,
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
    spent_before: f64,
    started: Instant,
    signal: Option<Signal>,
    measured: BTreeMap<TaskName, Measured>,
    decided: BTreeMap<TaskName, Decided>,
    fatal: Option<anyhow::Error>,
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
                self.signal = Some(Signal::Discard);
                fail(cost, "turn failed; iteration discarded".to_string())
            }
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
                self.signal = Some(Signal::Discard);
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
        let d = loop_driver::decide_row(self.judge, self.best_score, self.it, m);
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
            TaskKind::Agent { .. } | TaskKind::Command { .. } => {
                self.workflow_runner.run(task, attempt, inputs)
            }
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
                op: EngineOp::Decide,
                source,
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
}

fn pass(v: Value) -> Attempt {
    Attempt {
        outcome: AttemptOutcome::Pass(v),
        cost_usd: 0.0,
    }
}

fn fail(cost_usd: f64, note: String) -> Attempt {
    Attempt {
        outcome: AttemptOutcome::Fail(note),
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
    world: &dyn World,
    judge: &dyn Judge,
    baseline_score: f64,
) -> Result<WideOutcome> {
    r.note(&format!(
        "wide round: fanning out {} candidates (top-{} advance)",
        cfg.n, cfg.k
    ));
    let wide_dir = p.state.join("wide").join(&prep.run_id);
    std::fs::create_dir_all(&wide_dir)
        .with_context(|| format!("creating wide-round dir {}", wide_dir.display()))?;

    let direction = match judge.direction() {
        crate::command_judge::Direction::Lower => Direction::Lower,
        crate::command_judge::Direction::Higher => Direction::Higher,
    };
    let plan = wide_template(cfg, prep, direction)?;
    r.plan_event(&crate::plan::cli::plan_admitted_event(&plan));
    r.note("wide: starting parallel PROPOSE turns");
    let snap = world
        .snapshot("wide-pre-measure")
        .context("wide pre-measure snapshot")?;

    let mut runner = WideRunner {
        args,
        p,
        world,
        judge,
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
            task_events.push(crate::plan::cli::task_result_event(
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
        });
    }
    for id in 0..cfg.n {
        tasks.push(Task {
            name: TaskName(format!("measure-{id}")),
            task: TaskKind::Engine {
                op: EngineOp::MeasureDiff,
                source: None,
            },
            depends_on: vec![TaskName(format!("propose-{id}"))],
            session: None,
            needs: "any".to_string(),
            required: false,
            isolation: None,
            join: Join::All,
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
    world: &'a dyn World,
    judge: &'a dyn Judge,
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
                let decision = if self.judge.decide(&reading, self.baseline_score).keep {
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
                    total: reading.detail.get("total").and_then(Value::as_u64),
                    phase: Some("wide".into()),
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
    let _cost = agent::run_turn(args, &cand_paths, prompt, false, |_line, _stream, _ev| {});
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
    use crate::loop_driver::{LoopRuntime, run_loop};
    use crate::{Prepared, agent, manifest, reporter, run, stream};
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
        assert_eq!(
            decisions,
            ["baseline"],
            "every iteration is discarded before it can be measured: {}",
            describe(&trace)
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
        args.agent_backend = agent::AgentBackend::Command;
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
            identity: crate::identity::RunIdentity {
                components: Vec::new(),
                manifest_hash: "h".into(),
                inject_hash: "h".into(),
                measure_cmd: "./measure.sh".into(),
                direction: "higher".into(),
                rig: Default::default(),
                digest: "v1:0".into(),
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
        for line in log.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            let kind = v["kind"].as_str().unwrap().to_string();
            match kind.as_str() {
                "row" => rows.push((
                    v["row"]["iter"].as_u64().unwrap() as u32,
                    v["row"]["decision"].as_str().unwrap().to_string(),
                    v["row"]["score"].as_f64(),
                )),
                "summary" => best = v["best_score"].as_f64().unwrap(),
                "shutdown" => shutdown = v["outcome"].as_str().unwrap().to_string(),
                "note" => notes.push(v["msg"].as_str().unwrap_or("").to_string()),
                "task_result" => task_iters.push(v["iter"].as_u64().unwrap() as u32),
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

    fn default_args() -> crate::Args {
        crate::Cli::parse_from(["crucible"]).run
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
        let args = crate::Args {
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
        let args = crate::Args {
            wide: 3,
            wide_keep: 1,
            ..default_args()
        };
        assert_eq!(WideConfig::resolve(&args, Some(&search)).unwrap().n, 3);
    }

    #[test]
    fn wide_config_none_when_too_few_approaches() {
        let search = search_cfg(3, &["a"], 1);
        let args = crate::Args {
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
