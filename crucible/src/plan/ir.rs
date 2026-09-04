use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Task identity: cache key component, wire label, UI label. Unique within a plan.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskName(pub String);

impl fmt::Display for TaskName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for TaskName {
    fn from(s: &str) -> Self {
        TaskName(s.to_string())
    }
}

/// A field name a task promises to include in its JSON output.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OutputField(pub String);

/// One declared output field of one task: what a mapped task fans out over.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputRef {
    pub task: TaskName,
    pub field: OutputField,
}

impl std::fmt::Display for OutputRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.task, self.field.0)
    }
}

/// Grading direction for reducers, mirroring the judge's convention.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Lower,
    Higher,
}

/// A controller-configured publication sink. The workflow selects a key, never an endpoint or
/// credential; adding a destination is an engine change with an explicit transport policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportDestination {
    Slack(SlackDestination),
}

/// Slack destination parameters. Empty in v1; the object shape allows additive options without
/// changing `report()` or admitting caller-supplied webhook URLs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackDestination {}

/// Authorable operations that require orchestrator capabilities to execute.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineOp {
    /// Run the loop's candidate-producing turn.
    Propose,
    /// `World::apply`: make the candidate live (a failure = unscoreable, discard).
    Apply,
    /// `Judge::measure`: score the live candidate.
    Measure,
    /// Fold evaluation evidence into a measurement; `source` supplies its score.
    Grade,
    /// `Judge::decide`: rule keep/discard against the run's best.
    Decide,
    /// The wide tournament's scoring stage: apply an upstream candidate diff to the main
    /// workspace, `World::apply`, measure with the frozen judge, restore. Serialized by
    /// construction (never isolation-marked), because candidates share one deployment.
    MeasureDiff,
}

/// Where a task executes. Authorable (`isolation = "worktree"`); a runner that cannot
/// honor it must refuse the task loudly rather than silently ignore it: see
/// [`crate::plan::runner::ShellRunner`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// A private clone of the workspace; the task's edits travel out as a captured diff
    /// in its output, never as workspace state.
    Worktree,
}

/// When the loop schedules a workflow task (`stage = "iteration" | "epilogue"`). Only the
/// loop's workflow admission gives this meaning; the plain plan executor runs whatever
/// tasks it is handed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Rides the per-iteration graph (the default, and the only behavior before epilogues).
    #[default]
    Iteration,
    /// Excluded from the per-iteration graph; runs once after the loop concludes, against
    /// the final kept candidate, and only if the run kept something. Advisory by contract:
    /// it cannot un-keep the candidate.
    Epilogue,
}

impl Stage {
    /// Keeps the default off the wire: frozen packs' canonical JSON predates the field.
    fn is_iteration(&self) -> bool {
        *self == Stage::Iteration
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Stage::Iteration => "iteration",
            Stage::Epilogue => "epilogue",
        }
    }
}

/// How dependency outputs join into a task's inputs (`join = "all" | "passed" | "settled"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Join {
    /// Every dependency must pass; anything else blocks the task (the default, and the
    /// only behavior before isolated fan-out).
    #[default]
    All,
    /// Dispatch once every dependency is terminal, folding only the passing outputs: a
    /// reducer over a lossy fan-out (the wide `top_k`: skipped/failed candidates just
    /// don't rank), or a join over reviewers where one being advisory must not stop the run.
    Passed,
    /// Dispatch once every dependency is terminal whatever it settled as, unless the run has
    /// halted, forwarding each one as an entry carrying its status, note, output, and whether
    /// a file set was staged for this consumer.
    Settled,
}

impl Join {
    pub fn as_str(&self) -> &'static str {
        match self {
            Join::All => "all",
            Join::Passed => "passed",
            Join::Settled => "settled",
        }
    }
}

/// What a task *is*. The executor owns advancement; agents only ever run inside `Agent` tasks.
///
/// Internally tagged so TOML and JSON authoring read naturally:
/// `kind = "agent"` / `kind = "command"` / `kind = "top_k"`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskKind {
    /// An agent turn. Harness, model family, and effort are per-task knobs: the openshell
    /// heterogeneity axis. `None` inherits the manifest's `[agent]` defaults.
    Agent {
        prompt: String,
        #[serde(default)]
        harness: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        effort: Option<String>,
    },
    /// A plan-authored command. Trusted scripts require frozen manifest injects.
    Command { command: String },
    /// A command whose final JSON object is graded by `pass` or a threshold.
    Evaluate {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        threshold: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        direction: Option<Direction>,
    },
    /// Engine-owned publication of the bounded run report. The template is pack-authored at
    /// compile time; its context remains the fixed typed report contract.
    Report {
        destination: ReportDestination,
        template: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<TaskName>,
    },
    /// Engine-builtin deterministic fold: keep the k best upstream outputs by `score`.
    TopK { k: u32, direction: Direction },
    /// A capability-owned engine operation.
    Engine {
        op: EngineOp,
        /// Typed input; dependencies still control scheduling.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<TaskName>,
        /// Grade only: the evaluate task whose score becomes the reading's secondary
        /// `tiebreak` scalar (breaks primary-score ties in the keep rule).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tiebreak: Option<TaskName>,
    },
}

impl TaskKind {
    /// Stable wire label for the kind (`SessionEvent::TaskResult.task_kind`, UI classes).
    pub fn label(&self) -> &'static str {
        match self {
            TaskKind::Agent { .. } => "agent",
            TaskKind::Command { .. } => "command",
            TaskKind::Evaluate { .. } => "evaluate",
            TaskKind::Report { .. } => "report",
            TaskKind::TopK { .. } => "top_k",
            TaskKind::Engine { op, .. } => match op {
                EngineOp::Propose => "engine_propose",
                EngineOp::Apply => "engine_apply",
                EngineOp::Measure => "engine_measure",
                EngineOp::Grade => "engine_grade",
                EngineOp::Decide => "engine_decide",
                EngineOp::MeasureDiff => "engine_measure_diff",
            },
        }
    }
}

fn default_needs() -> String {
    "any".to_string()
}
fn default_required() -> bool {
    true
}

/// One unit of work in a plan. Always "task", never node/stage/step/rung.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub name: TaskName,
    #[serde(flatten)]
    pub task: TaskKind,
    #[serde(default)]
    pub depends_on: Vec<TaskName>,
    /// Durable logical session; shared names must be dependency-ordered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Substrate capability this task needs; `"any"` runs everywhere.
    #[serde(default = "default_needs")]
    pub needs: String,
    /// Required tasks gate plan validity; advisory failures block dependents only.
    #[serde(default = "default_required")]
    pub required: bool,
    /// Isolated execution (see [`Isolation`]); absent = run in the shared workspace.
    #[serde(default)]
    pub isolation: Option<Isolation>,
    /// Dependency-join semantics (see [`Join`]).
    #[serde(default)]
    pub join: Join,
    /// Loop scheduling (see [`Stage`]): epilogue tasks leave the per-iteration graph and
    /// run once post-loop against the kept best.
    #[serde(default, skip_serializing_if = "Stage::is_iteration")]
    pub stage: Stage,
    /// Fields the task's JSON output promises to include. Presence is checked at
    /// runtime; consumer contracts (`top_k`, grade sources) at validation. Empty =
    /// undeclared.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emits: Vec<OutputField>,
    /// Workspace-relative paths this task's output includes as files. A declared file is part
    /// of the task's output, not part of the workspace state that isolation discards, so a
    /// dependent receives it either way.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emits_files: Vec<String>,
    /// An upstream list this task runs once per element of. The task is one node in the graph
    /// however many elements arrive, so the graph stays renderable before any spend; only the
    /// number of instances is decided at run time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub over: Option<OutputRef>,
    /// The most instances `over` may produce. Required alongside it and never defaulted: an
    /// author who has not said how wide their fan-out gets has not thought about it, and a
    /// discovery task that returns more than expected should fail loudly rather than fan out to
    /// whatever a global default happened to be.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_fanout: Option<u32>,
}

/// Executor-enforced accounting limit; overruns fail the plan.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PlanBudget {
    pub usd: f64,
}

/// A versioned work graph. `reason` is reserved for replanning.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub version: u32,
    #[serde(default)]
    pub reason: Option<String>,
    pub budget: PlanBudget,
    #[serde(rename = "task", default)]
    pub tasks: Vec<Task>,
}

/// A plan that passed structural validation, carrying its topological order.
/// The executor only accepts this type: an unvalidated `Plan` cannot run.
#[derive(Debug)]
pub struct ValidPlan {
    plan: Plan,
    /// Indices into `plan.tasks`, dependency-first.
    topo: Vec<usize>,
}

impl ValidPlan {
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Replace the plan's budget with the launcher's. A pack's own figure is authoring data;
    /// a ceiling is the operator's, and this is where the second overrides the first.
    pub fn with_budget(mut self, usd: f64) -> Result<Self, PlanError> {
        self.plan.budget = PlanBudget { usd };
        self.plan.validate()
    }

    pub fn tasks_topo(&self) -> impl Iterator<Item = &Task> {
        self.topo.iter().map(|&i| &self.plan.tasks[i])
    }

    pub fn get(&self, name: &TaskName) -> Option<&Task> {
        self.plan.tasks.iter().find(|t| &t.name == name)
    }
}

/// The most instances one mapped node may produce. Operator-owned, not author-owned: a bound a
/// pack could raise is not a bound.
pub const MAX_FANOUT_CEILING: u32 = 256;

/// Everything [`Plan::validate`] can reject. Structural only: capability admission and
/// autoresearch shape live in [`crate::manifest::WorkflowCfg`].
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PlanError {
    #[error("unsupported plan version {version}; this build supports only version 1")]
    UnsupportedVersion { version: u32 },
    #[error("plan declares no tasks")]
    NoTasks,
    #[error("plan budget.usd must be a positive number, got {usd}")]
    NonPositiveBudget { usd: f64 },
    #[error("task #{index} has an empty name")]
    EmptyTaskName { index: usize },
    #[error("duplicate task name {task:?}")]
    DuplicateTask { task: String },
    #[error("task {task:?} depends on itself")]
    SelfDependency { task: String },
    #[error("task {task:?} depends on unknown task {dependency:?}")]
    UnknownDependency { task: String, dependency: String },
    #[error("report task {task:?} selects unknown result task {result:?}")]
    UnknownReportResult { task: String, result: String },
    #[error(
        "report task {task:?} selects epilogue task {result:?}; report results must come from the main graph"
    )]
    EpilogueReportResult { task: String, result: String },
    #[error("task {task:?} lists dependency {dependency:?} twice")]
    RepeatedDependency { task: String, dependency: String },
    #[error("task {task:?}: join = \"passed\" needs at least one dependency")]
    JoinPassedWithoutDependencies { task: String },
    #[error("task {task:?}: join = \"settled\" needs at least one dependency")]
    JoinSettledWithoutDependencies { task: String },
    #[error(
        "task {task:?}: join = \"settled\" is not accepted on {kind} tasks, whose inputs are a \
         fixed typed context rather than one entry per dependency. Use join = \"all\" or \
         join = \"passed\""
    )]
    SettledJoinOnFixedContext { task: String, kind: &'static str },
    #[error(
        "task {task:?} is required and joins on {dependency:?} with join = \"all\", but \
         {dependency:?} is advisory and allowed to fail. Set join = \"passed\" on {task:?}, or \
         make {dependency:?} required"
    )]
    AdvisoryGatesRequired { task: String, dependency: String },
    #[error("task {task:?}: top_k k must be >= 1")]
    TopKZero { task: String },
    #[error("task {task:?}: top_k needs at least one dependency to fold")]
    TopKWithoutDependencies { task: String },
    #[error("task {task:?}: evaluate threshold and direction must be set together")]
    ThresholdWithoutDirection { task: String },
    #[error("task {task:?}: evaluate threshold must be finite")]
    NonFiniteThreshold { task: String },
    #[error(
        "task {task:?}: emits is not accepted on {kind} tasks; their outputs are engine-defined"
    )]
    EmitsOnEngineTask { task: String, kind: &'static str },
    #[error(
        "task {task:?} declares invalid output field {field:?}; use 1-64 ASCII letters, digits, or `_`"
    )]
    InvalidOutputField { task: String, field: String },
    #[error("task {task:?} declares output field {field:?} twice")]
    DuplicateOutputField { task: String, field: String },
    #[error(
        "task {task:?}: top_k ranks by `score`, but dependency {dependency:?} declares emits without it"
    )]
    TopKSourceOmitsScore { task: String, dependency: String },
    #[error("task {task:?}: grade reads `score` from {from:?}, which declares emits without it")]
    GradeSourceOmitsScore { task: String, from: String },
    #[error("task {task:?}: a thresholded evaluate grades `score`, but its emits omits it")]
    ThresholdedEvaluateOmitsScore { task: String },
    #[error(
        "task {task:?} has invalid session {session:?}; use 1-64 ASCII letters, digits, `.`, `_`, or `-`"
    )]
    InvalidSessionName { task: String, session: String },
    #[error(
        "task {task:?} sets session, but only agent and engine propose tasks can resume an agent"
    )]
    SessionOnUnsupportedTask { task: String },
    #[error(
        "task {task:?} sets session {session:?}, but durable sessions cannot use disposable isolation"
    )]
    SessionWithIsolation { task: String, session: String },
    #[error(
        "task {task:?} contains `[` or `]`; those are reserved for a mapped node's instance \
         names, which are synthesized as `node[item]`"
    )]
    BracketInTaskName { task: String },
    #[error(
        "task {task:?} maps over {reference} but does not depend on {producer:?}; a fan-out \
         reads its items from a dependency's output"
    )]
    OverNotADependency {
        task: String,
        reference: String,
        producer: String,
    },
    #[error(
        "task {task:?} maps over {reference} without max_fanout; a fan-out states how wide it \
         may get before it runs, not after"
    )]
    OverWithoutFanout { task: String, reference: String },
    #[error("task {task:?} declares max_fanout without `over`; there is nothing to bound")]
    FanoutWithoutOver { task: String },
    #[error("task {task:?}: max_fanout = {got} is outside 1..={MAX_FANOUT_CEILING}")]
    FanoutOutOfRange { task: String, got: u32 },
    #[error(
        "task {task:?} maps over {reference} and resumes session {session:?}; instances run one \
         at a time and would interleave a single transcript"
    )]
    OverWithSession {
        task: String,
        reference: String,
        session: String,
    },
    #[error(
        "task {task:?} depends on {dependency:?}, which is a key the engine writes into a task's \
         inputs itself and would overwrite the dependency's entry with; rename the dependency"
    )]
    ReservedDependencyName { task: String, dependency: String },
    #[error("plan has a dependency cycle involving: {}", .tasks.join(", "))]
    DependencyCycle { tasks: Vec<String> },
    #[error(
        "tasks {left:?} and {right:?} share session {session:?} but are not dependency-ordered"
    )]
    UnorderedSession {
        left: String,
        right: String,
        session: String,
    },
}

impl Plan {
    /// Parse JSON without validating it.
    pub fn from_json_str(s: &str) -> Result<Plan> {
        serde_json::from_str(s).context("PLAN.json does not parse as a plan")
    }

    /// Parse the pack-authored TOML form (`version`, `[budget]`, `[[task]]`).
    pub fn from_toml_str(s: &str) -> Result<Plan> {
        toml::from_str(s).context("plan TOML does not parse")
    }

    /// Validate structure and compute dependency order.
    pub fn validate(self) -> Result<ValidPlan, PlanError> {
        if self.version != 1 {
            return Err(PlanError::UnsupportedVersion {
                version: self.version,
            });
        }
        if self.tasks.is_empty() {
            return Err(PlanError::NoTasks);
        }
        if !self.budget.usd.is_finite() || self.budget.usd <= 0.0 {
            return Err(PlanError::NonPositiveBudget {
                usd: self.budget.usd,
            });
        }
        let mut index: BTreeMap<&TaskName, usize> = BTreeMap::new();
        for (i, t) in self.tasks.iter().enumerate() {
            if t.name.0.trim().is_empty() {
                return Err(PlanError::EmptyTaskName { index: i });
            }
            if t.name.0.contains(['[', ']']) {
                return Err(PlanError::BracketInTaskName {
                    task: t.name.0.clone(),
                });
            }
            if index.insert(&t.name, i).is_some() {
                return Err(PlanError::DuplicateTask {
                    task: t.name.0.clone(),
                });
            }
        }
        for t in &self.tasks {
            let task = || t.name.0.clone();
            let mut seen = BTreeSet::new();
            for d in &t.depends_on {
                if d == &t.name {
                    return Err(PlanError::SelfDependency { task: task() });
                }
                if !index.contains_key(d) {
                    return Err(PlanError::UnknownDependency {
                        task: task(),
                        dependency: d.0.clone(),
                    });
                }
                if !seen.insert(d) {
                    return Err(PlanError::RepeatedDependency {
                        task: task(),
                        dependency: d.0.clone(),
                    });
                }
                if crate::plan::exec::RESERVED_INPUTS.contains(&d.0.as_str()) {
                    return Err(PlanError::ReservedDependencyName {
                        task: task(),
                        dependency: d.0.clone(),
                    });
                }
            }
            if t.join == Join::Passed && t.depends_on.is_empty() {
                return Err(PlanError::JoinPassedWithoutDependencies { task: task() });
            }
            if t.join == Join::Settled {
                if t.depends_on.is_empty() {
                    return Err(PlanError::JoinSettledWithoutDependencies { task: task() });
                }
                if matches!(
                    t.task,
                    TaskKind::TopK { .. } | TaskKind::Report { .. } | TaskKind::Engine { .. }
                ) {
                    return Err(PlanError::SettledJoinOnFixedContext {
                        task: task(),
                        kind: t.task.label(),
                    });
                }
            }
            if t.required && t.join == Join::All {
                for d in &t.depends_on {
                    if index.get(d).is_some_and(|&i| !self.tasks[i].required) {
                        return Err(PlanError::AdvisoryGatesRequired {
                            task: task(),
                            dependency: d.0.clone(),
                        });
                    }
                }
            }
            if let TaskKind::TopK { k, .. } = &t.task {
                if *k == 0 {
                    return Err(PlanError::TopKZero { task: task() });
                }
                if t.depends_on.is_empty() {
                    return Err(PlanError::TopKWithoutDependencies { task: task() });
                }
            }
            if let TaskKind::Evaluate {
                threshold,
                direction,
                ..
            } = &t.task
            {
                if threshold.is_some() != direction.is_some() {
                    return Err(PlanError::ThresholdWithoutDirection { task: task() });
                }
                if threshold.is_some_and(|value| !value.is_finite()) {
                    return Err(PlanError::NonFiniteThreshold { task: task() });
                }
            }
            if let TaskKind::Report {
                result: Some(result),
                ..
            } = &t.task
            {
                let Some(&result_index) = index.get(result) else {
                    return Err(PlanError::UnknownReportResult {
                        task: task(),
                        result: result.0.clone(),
                    });
                };
                if self.tasks[result_index].stage == Stage::Epilogue {
                    return Err(PlanError::EpilogueReportResult {
                        task: task(),
                        result: result.0.clone(),
                    });
                }
            }
            if !t.emits.is_empty() {
                if matches!(t.task, TaskKind::TopK { .. } | TaskKind::Engine { .. }) {
                    return Err(PlanError::EmitsOnEngineTask {
                        task: task(),
                        kind: t.task.label(),
                    });
                }
                let mut fields = BTreeSet::new();
                for field in &t.emits {
                    if field.0.is_empty()
                        || field.0.len() > 64
                        || !field
                            .0
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_')
                    {
                        return Err(PlanError::InvalidOutputField {
                            task: task(),
                            field: field.0.clone(),
                        });
                    }
                    if !fields.insert(&field.0) {
                        return Err(PlanError::DuplicateOutputField {
                            task: task(),
                            field: field.0.clone(),
                        });
                    }
                }
            }
            // Consumer contracts are presence-only: a declared emits that omits `score`
            // where a score is read is a wiring bug worth failing before any spend.
            let score_declared = |name: &TaskName| {
                index.get(name).is_none_or(|&i| {
                    let emits = &self.tasks[i].emits;
                    emits.is_empty() || emits.iter().any(|f| f.0 == "score")
                })
            };
            if matches!(t.task, TaskKind::TopK { .. }) {
                for d in &t.depends_on {
                    if !score_declared(d) {
                        return Err(PlanError::TopKSourceOmitsScore {
                            task: task(),
                            dependency: d.0.clone(),
                        });
                    }
                }
            }
            if let TaskKind::Engine {
                op: EngineOp::Grade,
                source: Some(source),
                ..
            } = &t.task
                && !score_declared(source)
            {
                return Err(PlanError::GradeSourceOmitsScore {
                    task: task(),
                    from: source.0.clone(),
                });
            }
            if let TaskKind::Evaluate {
                threshold: Some(_), ..
            } = &t.task
                && !t.emits.is_empty()
                && !t.emits.iter().any(|f| f.0 == "score")
            {
                return Err(PlanError::ThresholdedEvaluateOmitsScore { task: task() });
            }
            if let Some(session) = &t.session {
                if session.is_empty()
                    || session.len() > 64
                    || !session
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                {
                    return Err(PlanError::InvalidSessionName {
                        task: task(),
                        session: session.clone(),
                    });
                }
                if !matches!(
                    t.task,
                    TaskKind::Agent { .. }
                        | TaskKind::Engine {
                            op: EngineOp::Propose,
                            ..
                        }
                ) {
                    return Err(PlanError::SessionOnUnsupportedTask { task: task() });
                }
                if t.isolation.is_some() {
                    return Err(PlanError::SessionWithIsolation {
                        task: task(),
                        session: session.clone(),
                    });
                }
            }
            match (&t.over, t.max_fanout) {
                (None, None) => {}
                (None, Some(_)) => return Err(PlanError::FanoutWithoutOver { task: task() }),
                (Some(reference), None) => {
                    return Err(PlanError::OverWithoutFanout {
                        task: task(),
                        reference: reference.to_string(),
                    });
                }
                (Some(reference), Some(width)) => {
                    if !t.depends_on.contains(&reference.task) {
                        return Err(PlanError::OverNotADependency {
                            task: task(),
                            reference: reference.to_string(),
                            producer: reference.task.0.clone(),
                        });
                    }
                    if width == 0 || width > MAX_FANOUT_CEILING {
                        return Err(PlanError::FanoutOutOfRange {
                            task: task(),
                            got: width,
                        });
                    }
                    if let Some(session) = &t.session {
                        return Err(PlanError::OverWithSession {
                            task: task(),
                            reference: reference.to_string(),
                            session: session.clone(),
                        });
                    }
                }
            }
        }
        // Kahn's algorithm; leftovers mean a cycle. The ready set is a min-heap on the
        // declaration index so the order is deterministic and declaration-stable: ties
        // dispatch in the order the author wrote them, which the UI, the cache, and the
        // budget cutoff all depend on.
        let n = self.tasks.len();
        let mut indegree = vec![0usize; n];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, t) in self.tasks.iter().enumerate() {
            indegree[i] = t.depends_on.len();
            for d in &t.depends_on {
                dependents[index[d]].push(i);
            }
        }
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;
        let mut ready: BinaryHeap<Reverse<usize>> =
            (0..n).filter(|&i| indegree[i] == 0).map(Reverse).collect();
        let mut topo = Vec::with_capacity(n);
        while let Some(Reverse(i)) = ready.pop() {
            topo.push(i);
            for &j in &dependents[i] {
                indegree[j] -= 1;
                if indegree[j] == 0 {
                    ready.push(Reverse(j));
                }
            }
        }
        if topo.len() != n {
            return Err(PlanError::DependencyCycle {
                tasks: (0..n)
                    .filter(|&i| indegree[i] > 0)
                    .map(|i| self.tasks[i].name.0.clone())
                    .collect(),
            });
        }
        // One native conversation is serial, so shared sessions require an ordering path.
        let reaches = |from: usize, to: usize| {
            let mut stack = vec![from];
            let mut seen = BTreeSet::new();
            while let Some(i) = stack.pop() {
                if !seen.insert(i) {
                    continue;
                }
                for dependent in &dependents[i] {
                    if *dependent == to {
                        return true;
                    }
                    stack.push(*dependent);
                }
            }
            false
        };
        let mut sessions: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for (i, task) in self.tasks.iter().enumerate() {
            if let Some(session) = task.session.as_deref() {
                sessions.entry(session).or_default().push(i);
            }
        }
        for (session, tasks) in sessions {
            for (offset, left) in tasks.iter().enumerate() {
                for right in &tasks[offset + 1..] {
                    if !reaches(*left, *right) && !reaches(*right, *left) {
                        return Err(PlanError::UnorderedSession {
                            left: self.tasks[*left].name.0.clone(),
                            right: self.tasks[*right].name.0.clone(),
                            session: session.to_owned(),
                        });
                    }
                }
            }
        }
        Ok(ValidPlan { plan: self, topo })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str, deps: &[&str]) -> Task {
        Task {
            name: name.into(),
            task: TaskKind::Agent {
                prompt: "p".into(),
                harness: None,
                model: None,
                effort: None,
            },
            depends_on: deps.iter().map(|d| (*d).into()).collect(),
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
        }
    }

    fn plan(tasks: Vec<Task>) -> Plan {
        Plan {
            version: 1,
            reason: None,
            budget: PlanBudget { usd: 5.0 },
            tasks,
        }
    }

    #[test]
    fn valid_chain_topo_orders_dependencies_first() {
        let p = plan(vec![
            agent("b", &["a"]),
            agent("a", &[]),
            agent("c", &["b"]),
        ]);
        let v = p.validate().unwrap();
        let order: Vec<&str> = v.tasks_topo().map(|t| t.name.0.as_str()).collect();
        let pos = |n: &str| order.iter().position(|x| *x == n).unwrap();
        assert!(pos("a") < pos("b"));
        assert!(pos("b") < pos("c"));
    }

    #[test]
    fn topo_is_declaration_stable_for_independent_tasks() {
        let p = plan(vec![agent("z", &[]), agent("m", &[]), agent("a", &[])]);
        let v = p.validate().unwrap();
        let order: Vec<&str> = v.tasks_topo().map(|t| t.name.0.as_str()).collect();
        assert_eq!(
            order,
            vec!["z", "m", "a"],
            "ties break in declaration order"
        );
    }

    #[test]
    fn duplicate_names_rejected() {
        let err = plan(vec![agent("a", &[]), agent("a", &[])])
            .validate()
            .unwrap_err();
        assert!(matches!(err, PlanError::DuplicateTask { task } if task == "a"));
    }

    #[test]
    fn unknown_dependency_rejected() {
        let err = plan(vec![agent("a", &["ghost"])]).validate().unwrap_err();
        assert_eq!(
            err,
            PlanError::UnknownDependency {
                task: "a".to_owned(),
                dependency: "ghost".to_owned(),
            }
        );
    }

    #[test]
    fn self_dependency_rejected() {
        let err = plan(vec![agent("a", &["a"])]).validate().unwrap_err();
        assert_eq!(
            err,
            PlanError::SelfDependency {
                task: "a".to_owned()
            }
        );
    }

    #[test]
    fn cycle_rejected_and_named() {
        let err = plan(vec![agent("a", &["b"]), agent("b", &["a"])])
            .validate()
            .unwrap_err();
        let PlanError::DependencyCycle { tasks } = err else {
            panic!("expected a cycle, got {err}");
        };
        assert_eq!(tasks, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn unsupported_plan_versions_are_rejected() {
        for version in [0, 2, u32::MAX] {
            let mut p = plan(vec![agent("a", &[])]);
            p.version = version;
            assert_eq!(
                p.validate().unwrap_err(),
                PlanError::UnsupportedVersion { version }
            );
        }
    }

    #[test]
    fn shared_sessions_must_be_serial_and_nonisolated() {
        let mut first = agent("first", &[]);
        first.session = Some("solver".into());
        let mut next = agent("next", &["first"]);
        next.session = Some("solver".into());
        assert!(plan(vec![first.clone(), next]).validate().is_ok());

        let mut racing = agent("racing", &[]);
        racing.session = Some("solver".into());
        let err = plan(vec![first.clone(), racing]).validate().unwrap_err();
        assert_eq!(
            err,
            PlanError::UnorderedSession {
                left: "first".to_owned(),
                right: "racing".to_owned(),
                session: "solver".to_owned(),
            }
        );

        first.isolation = Some(Isolation::Worktree);
        let err = plan(vec![first]).validate().unwrap_err();
        assert_eq!(
            err,
            PlanError::SessionWithIsolation {
                task: "first".to_owned(),
                session: "solver".to_owned(),
            }
        );
    }

    #[test]
    fn zero_or_negative_budget_rejected() {
        for usd in [0.0, -1.0, f64::NAN] {
            let mut p = plan(vec![agent("a", &[])]);
            p.budget = PlanBudget { usd };
            assert!(p.validate().is_err(), "budget {usd} should be rejected");
        }
    }

    #[test]
    fn top_k_without_dependencies_rejected() {
        let t = Task {
            name: "pick".into(),
            task: TaskKind::TopK {
                k: 1,
                direction: Direction::Lower,
            },
            depends_on: vec![],
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
        };
        let err = plan(vec![t]).validate().unwrap_err();
        assert_eq!(
            err,
            PlanError::TopKWithoutDependencies {
                task: "pick".to_owned()
            }
        );
    }

    #[test]
    fn engine_tasks_are_authorable_data_but_legacy_kind_aliases_are_rejected() {
        let authored = "version = 1\n[budget]\nusd = 1.0\n[[task]]\nname = \"score\"\nkind = \"engine\"\nop = \"measure\"\n";
        let plan = Plan::from_toml_str(authored).unwrap();
        assert!(matches!(
            plan.tasks[0].task,
            TaskKind::Engine {
                op: EngineOp::Measure,
                source: None,
                tiebreak: None
            }
        ));

        for kind in ["engine_apply", "engine_measure", "engine_decide"] {
            let src = format!(
                "version = 1\n[budget]\nusd = 1.0\n[[task]]\nname = \"x\"\nkind = \"{kind}\"\n"
            );
            assert!(
                Plan::from_toml_str(&src).is_err(),
                "legacy kind alias {kind:?} must not parse"
            );
        }
    }

    #[test]
    fn toml_front_end_parses_all_kinds() {
        let src = r#"
            version = 1
            [budget]
            usd = 2.5

            [[task]]
            name = "propose-a"
            kind = "agent"
            prompt = "try the cache approach"
            model = "opus"
            effort = "high"

            [[task]]
            name = "propose-b"
            kind = "agent"
            prompt = "try the algorithm swap"
            harness = "hermes"

            [[task]]
            name = "measure-a"
            kind = "command"
            command = "bench.sh"
            depends_on = ["propose-a"]
            needs = "gpu"

            [[task]]
            name = "measure-b"
            kind = "command"
            command = "bench.sh"
            depends_on = ["propose-b"]
            needs = "gpu"

            [[task]]
            name = "pick"
            kind = "top_k"
            k = 1
            direction = "lower"
            depends_on = ["measure-a", "measure-b"]
        "#;
        let v = Plan::from_toml_str(src).unwrap().validate().unwrap();
        assert_eq!(v.plan().tasks.len(), 5);
        let pick = v.get(&"pick".into()).unwrap();
        assert!(matches!(
            pick.task,
            TaskKind::TopK {
                k: 1,
                direction: Direction::Lower
            }
        ));
        let b = v.get(&"propose-b".into()).unwrap();
        match &b.task {
            TaskKind::Agent { harness, .. } => assert_eq!(harness.as_deref(), Some("hermes")),
            other => panic!("expected agent task, got {other:?}"),
        }
    }

    #[test]
    fn json_front_end_round_trips() {
        let p = plan(vec![agent("a", &[]), agent("b", &["a"])]);
        let json = serde_json::to_string(&p).unwrap();
        let back = Plan::from_json_str(&json).unwrap().validate().unwrap();
        assert_eq!(back.plan().tasks.len(), 2);
    }

    fn emitting(name: &str, deps: &[&str], emits: &[&str]) -> Task {
        let mut t = agent(name, deps);
        t.emits = emits
            .iter()
            .map(|f| OutputField((*f).to_string()))
            .collect();
        t
    }

    fn top_k(name: &str, deps: &[&str]) -> Task {
        Task {
            name: name.into(),
            task: TaskKind::TopK {
                k: 1,
                direction: Direction::Higher,
            },
            depends_on: deps.iter().map(|d| (*d).into()).collect(),
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
        }
    }

    #[test]
    fn emits_fields_must_be_short_identifiers_without_duplicates() {
        for bad in ["", "has-dash", "sp ace", &"x".repeat(65)] {
            let err = plan(vec![emitting("a", &[], &[bad])])
                .validate()
                .unwrap_err();
            assert!(
                matches!(err, PlanError::InvalidOutputField { ref field, .. } if field == bad),
                "{bad:?}: {err}"
            );
        }
        let err = plan(vec![emitting("a", &[], &["score", "score"])])
            .validate()
            .unwrap_err();
        assert_eq!(
            err,
            PlanError::DuplicateOutputField {
                task: "a".to_owned(),
                field: "score".to_owned(),
            }
        );
        assert!(
            plan(vec![emitting("a", &[], &["score", "pass", "note_1"])])
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn emits_is_rejected_on_top_k_and_engine_tasks() {
        let mut pick = top_k("pick", &["a"]);
        pick.emits = vec![OutputField("kept".into())];
        let err = plan(vec![agent("a", &[]), pick]).validate().unwrap_err();
        assert_eq!(
            err,
            PlanError::EmitsOnEngineTask {
                task: "pick".to_owned(),
                kind: "top_k",
            }
        );

        let mut measure = agent("measure", &[]);
        measure.task = TaskKind::Engine {
            op: EngineOp::Measure,
            source: None,
            tiebreak: None,
        };
        measure.emits = vec![OutputField("score".into())];
        let err = plan(vec![measure]).validate().unwrap_err();
        assert!(matches!(err, PlanError::EmitsOnEngineTask { .. }), "{err}");
    }

    #[test]
    fn top_k_dependency_declaring_emits_must_include_score() {
        let err = plan(vec![
            emitting("m", &[], &["latency_ms"]),
            top_k("pick", &["m"]),
        ])
        .validate()
        .unwrap_err();
        assert_eq!(
            err,
            PlanError::TopKSourceOmitsScore {
                task: "pick".to_owned(),
                dependency: "m".to_owned(),
            }
        );

        assert!(
            plan(vec![emitting("m", &[], &["score"]), top_k("pick", &["m"])])
                .validate()
                .is_ok()
        );
        // Empty emits = undeclared = unchecked.
        assert!(
            plan(vec![agent("m", &[]), top_k("pick", &["m"])])
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn grade_source_declaring_emits_must_include_score() {
        let mut source = emitting("score", &[], &["latency_ms"]);
        source.task = TaskKind::Evaluate {
            command: "./x.sh".into(),
            threshold: None,
            direction: None,
        };
        let mut grade = agent("grade", &["score"]);
        grade.task = TaskKind::Engine {
            op: EngineOp::Grade,
            source: Some("score".into()),
            tiebreak: None,
        };
        let err = plan(vec![source, grade]).validate().unwrap_err();
        assert_eq!(
            err,
            PlanError::GradeSourceOmitsScore {
                task: "grade".to_owned(),
                from: "score".to_owned(),
            }
        );
    }

    #[test]
    fn thresholded_evaluate_declaring_emits_must_include_score() {
        let mut t = emitting("latency", &[], &["latency_ms"]);
        t.task = TaskKind::Evaluate {
            command: "./x.sh".into(),
            threshold: Some(10.0),
            direction: Some(Direction::Lower),
        };
        let err = plan(vec![t.clone()]).validate().unwrap_err();
        assert_eq!(
            err,
            PlanError::ThresholdedEvaluateOmitsScore {
                task: "latency".to_owned()
            }
        );

        t.emits.push(OutputField("score".into()));
        assert!(plan(vec![t]).validate().is_ok());
    }

    fn advisory(name: &str, deps: &[&str]) -> Task {
        let mut t = agent(name, deps);
        t.required = false;
        t
    }

    fn folding(name: &str, deps: &[&str]) -> Task {
        let mut t = agent(name, deps);
        t.join = Join::Passed;
        t
    }

    #[test]
    fn a_required_task_cannot_join_all_on_an_advisory_dependency() {
        let err = plan(vec![advisory("copy", &[]), agent("gate", &["copy"])])
            .validate()
            .unwrap_err();
        assert_eq!(
            err,
            PlanError::AdvisoryGatesRequired {
                task: "gate".to_owned(),
                dependency: "copy".to_owned(),
            }
        );
        let rendered = err.to_string();
        assert!(rendered.contains("\"gate\""), "{rendered}");
        assert!(rendered.contains("\"copy\""), "{rendered}");
    }

    #[test]
    fn advisory_gating_through_all_join_hops_is_reported_at_the_closest_edge() {
        // root -> mid -> near -> copy, every hop join = "all" and everything but the
        // advisory tail required: the violation is the near/copy edge.
        let err = plan(vec![
            advisory("copy", &[]),
            agent("near", &["copy"]),
            agent("mid", &["near"]),
            agent("root", &["mid"]),
        ])
        .validate()
        .unwrap_err();
        assert_eq!(
            err,
            PlanError::AdvisoryGatesRequired {
                task: "near".to_owned(),
                dependency: "copy".to_owned(),
            }
        );
    }

    #[test]
    fn a_passed_join_exempts_the_gate_and_everything_above_it() {
        let p = plan(vec![
            agent("propose", &[]),
            agent("correctness", &["propose"]),
            advisory("copy", &["propose"]),
            folding("gate", &["correctness", "copy"]),
            agent("apply", &["gate"]),
            agent("measure", &["apply"]),
        ]);
        assert!(p.validate().is_ok());
    }

    /// A settled join names the dependencies it reports on, so a task that names none has
    /// nothing to wait for and nothing to read.
    #[test]
    fn a_settled_join_needs_at_least_one_dependency() {
        let mut tip = agent("report", &[]);
        tip.join = Join::Settled;
        assert_eq!(
            plan(vec![tip]).validate().unwrap_err(),
            PlanError::JoinSettledWithoutDependencies {
                task: "report".to_owned()
            }
        );
    }

    /// The reducer, the engine-owned report, and every engine operation read their inputs
    /// against a fixed typed context, so a per-dependency envelope has nowhere to land. `top_k`
    /// and `report` never accept `join` from the DSL at all, so this arm is the JSON route's.
    #[test]
    fn a_settled_join_is_refused_on_a_task_with_a_fixed_input_context() {
        let settled = |kind: TaskKind| {
            let mut t = agent("tip", &["source"]);
            t.task = kind;
            t.join = Join::Settled;
            let json = serde_json::to_string(&plan(vec![agent("source", &[]), t])).unwrap();
            Plan::from_json_str(&json)
                .unwrap()
                .validate()
                .expect_err("the plan validated")
        };
        for (kind, label) in [
            (
                TaskKind::TopK {
                    k: 1,
                    direction: Direction::Higher,
                },
                "top_k",
            ),
            (
                TaskKind::Report {
                    destination: ReportDestination::Slack(SlackDestination {}),
                    template: "t".into(),
                    result: None,
                },
                "report",
            ),
            (
                TaskKind::Engine {
                    op: EngineOp::Grade,
                    source: None,
                    tiebreak: None,
                },
                "engine_grade",
            ),
        ] {
            assert_eq!(
                settled(kind),
                PlanError::SettledJoinOnFixedContext {
                    task: "tip".to_owned(),
                    kind: label,
                }
            );
        }
    }

    /// A required task joining settled declares that it runs on whatever settled, so the
    /// advisory-gates-required rule does not reach it.
    #[test]
    fn a_required_settled_task_may_join_an_advisory_dependency() {
        let mut tip = agent("report", &["copy"]);
        tip.join = Join::Settled;
        assert!(plan(vec![advisory("copy", &[]), tip]).validate().is_ok());
    }

    #[test]
    fn advisory_work_may_gate_advisory_consumers() {
        let p = plan(vec![
            agent("propose", &[]),
            advisory("copy", &["propose"]),
            advisory("summarize", &["copy"]),
            agent("apply", &["propose"]),
        ]);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn empty_emits_is_omitted_from_the_wire_and_round_trips() {
        let bare = serde_json::to_value(agent("a", &[])).unwrap();
        assert!(bare.get("emits").is_none(), "{bare}");

        let declared = emitting("a", &[], &["score"]);
        let json = serde_json::to_string(&plan(vec![declared])).unwrap();
        let back = Plan::from_json_str(&json).unwrap().validate().unwrap();
        assert_eq!(
            back.plan().tasks[0].emits,
            vec![OutputField("score".into())]
        );
    }
    /// A plan that arrives as JSON never passes through the starlark front end, so the
    /// structural facts about a mapped node are checked here or nowhere.
    #[test]
    fn validate_checks_a_mapped_node_on_the_json_route() {
        let mapped = |over: Option<OutputRef>, max_fanout: Option<u32>, deps: &[&str]| {
            let mut t = agent("audit", deps);
            t.over = over;
            t.max_fanout = max_fanout;
            t
        };
        let targets = OutputRef {
            task: "discover".into(),
            field: OutputField("targets".into()),
        };
        let refuse = |tasks: Vec<Task>| -> PlanError {
            let json = serde_json::to_string(&plan(tasks)).unwrap();
            Plan::from_json_str(&json)
                .unwrap()
                .validate()
                .expect_err("the plan validated")
        };

        assert_eq!(
            refuse(vec![agent("audit[x]", &[])]),
            PlanError::BracketInTaskName {
                task: "audit[x]".into()
            }
        );
        assert_eq!(
            refuse(vec![
                emitting("discover", &[], &["targets"]),
                mapped(Some(targets.clone()), Some(4), &[]),
            ]),
            PlanError::OverNotADependency {
                task: "audit".into(),
                reference: "discover.targets".into(),
                producer: "discover".into(),
            }
        );
        assert_eq!(
            refuse(vec![
                emitting("discover", &[], &["targets"]),
                mapped(Some(targets.clone()), None, &["discover"]),
            ]),
            PlanError::OverWithoutFanout {
                task: "audit".into(),
                reference: "discover.targets".into(),
            }
        );
        assert_eq!(
            refuse(vec![agent("a", &[]), mapped(None, Some(4), &["a"])]),
            PlanError::FanoutWithoutOver {
                task: "audit".into()
            }
        );
        for got in [0, MAX_FANOUT_CEILING + 1] {
            assert_eq!(
                refuse(vec![
                    emitting("discover", &[], &["targets"]),
                    mapped(Some(targets.clone()), Some(got), &["discover"]),
                ]),
                PlanError::FanoutOutOfRange {
                    task: "audit".into(),
                    got
                }
            );
        }
        let mut with_session = mapped(Some(targets.clone()), Some(4), &["discover"]);
        with_session.session = Some("scribe".into());
        assert_eq!(
            refuse(vec![emitting("discover", &[], &["targets"]), with_session]),
            PlanError::OverWithSession {
                task: "audit".into(),
                reference: "discover.targets".into(),
                session: "scribe".into(),
            }
        );

        let json = serde_json::to_string(&plan(vec![
            emitting("discover", &[], &["targets"]),
            mapped(Some(targets), Some(MAX_FANOUT_CEILING), &["discover"]),
        ]))
        .unwrap();
        let ok = Plan::from_json_str(&json).unwrap().validate().unwrap();
        assert_eq!(ok.plan().tasks.len(), 2);
    }

    /// The engine writes its own keys into a task's inputs after the dependency envelope is
    /// built, so a dependency named after one of them loses the entry the plan promised it.
    #[test]
    fn a_dependency_named_after_a_reserved_input_is_refused() {
        for reserved in crate::plan::exec::RESERVED_INPUTS {
            assert_eq!(
                plan(vec![agent(reserved, &[]), agent("consumer", &[reserved])])
                    .validate()
                    .unwrap_err(),
                PlanError::ReservedDependencyName {
                    task: "consumer".to_owned(),
                    dependency: reserved.to_owned(),
                }
            );
        }
        let ok = plan(vec![agent("items", &[]), agent("consumer", &["items"])])
            .validate()
            .expect("a name that only looks like a reserved one");
        assert_eq!(ok.plan().tasks.len(), 2);
    }
}
