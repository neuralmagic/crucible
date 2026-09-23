use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::crucible::Direction;
use anyhow::{Context, Result};
use crucible_contract::decision::{
    Label, Question, QuestionError, QuestionId, QuestionKind, UNCERTAIN,
};
use crucible_contract::emits::{FieldType, FieldTypeError};
use serde::{Deserialize, Serialize};

/// The reserved input a mapped instance receives its own item under. Reserved like the
/// epilogue's kept-candidate input: a task may not declare a dependency by this name.
pub const ITEM_INPUT: &str = "item";
/// Reserved epilogue input key: `loop_graph` injects the kept candidate's
/// context into every epilogue task's inputs under this name.
pub const KEPT_INPUT: &str = "kept";
/// The reserved input every epilogue task receives the main graph's outcome under.
pub const OUTCOME_INPUT: &str = "outcome";
/// The reserved input a revised task receives its reviewer's last verdict under, from its second
/// round on.
pub const REVISION_INPUT: &str = "revision";
/// Every key the engine writes into a task's inputs itself. A dependency named after one of
/// them would have its entry overwritten, so [`crate::plan::ir::Plan::validate`] refuses it.
pub const RESERVED_INPUTS: [&str; 4] = [ITEM_INPUT, KEPT_INPUT, OUTCOME_INPUT, REVISION_INPUT];

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

/// The fields a task's JSON output promises: names alone (`emits = ["a"]`), or names with the
/// type each holds (`emits = {"a": "string"}`). Empty in either form declares nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Emits {
    Fields(Vec<OutputField>),
    Typed(BTreeMap<OutputField, FieldType>),
}

/// What a task's emits say about one field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Declared<'a> {
    /// The task declares no emits, so nothing is promised or checked.
    Unchecked,
    /// The task declares emits without this field.
    Omitted,
    /// The field is promised present, of any type.
    Untyped,
    /// The field is promised present and of this type.
    Typed(&'a FieldType),
}

impl Default for Emits {
    fn default() -> Self {
        Emits::Fields(Vec::new())
    }
}

impl Emits {
    pub fn is_empty(&self) -> bool {
        match self {
            Emits::Fields(fields) => fields.is_empty(),
            Emits::Typed(fields) => fields.is_empty(),
        }
    }

    /// Every declared field in declaration order (key order for the typed form), with its type.
    pub fn fields(&self) -> Vec<(&OutputField, Option<&FieldType>)> {
        match self {
            Emits::Fields(fields) => fields.iter().map(|f| (f, None)).collect(),
            Emits::Typed(fields) => fields.iter().map(|(f, ty)| (f, Some(ty))).collect(),
        }
    }

    pub fn names(&self) -> Vec<String> {
        self.fields()
            .into_iter()
            .map(|(f, _)| f.0.clone())
            .collect()
    }

    pub fn field(&self, name: &str) -> Declared<'_> {
        if self.is_empty() {
            return Declared::Unchecked;
        }
        match self.fields().into_iter().find(|(f, _)| f.0 == name) {
            None => Declared::Omitted,
            Some((_, None)) => Declared::Untyped,
            Some((_, Some(ty))) => Declared::Typed(ty),
        }
    }
}

impl Serialize for Emits {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Emits::Fields(fields) => fields.serialize(serializer),
            Emits::Typed(fields) => fields.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Emits {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{Error, MapAccess, SeqAccess, Visitor};

        struct EmitsVisitor;

        impl<'de> Visitor<'de> for EmitsVisitor {
            type Value = Emits;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a list of field names, or a table from field name to field type")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Emits, A::Error> {
                let mut fields = Vec::new();
                while let Some(field) = seq.next_element::<OutputField>()? {
                    fields.push(field);
                }
                Ok(Emits::Fields(fields))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Emits, A::Error> {
                let mut fields = BTreeMap::new();
                while let Some((field, ty)) = map.next_entry::<OutputField, FieldType>()? {
                    if fields.contains_key(&field) {
                        return Err(A::Error::custom(format!(
                            "output field {:?} is declared twice",
                            field.0
                        )));
                    }
                    fields.insert(field, ty);
                }
                Ok(Emits::Typed(fields))
            }
        }

        deserializer.deserialize_any(EmitsVisitor)
    }
}

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
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "number::optional"
        )]
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
    /// Engine-owned call to a decision model; the output is one typed answer per question.
    Route {
        questions: BTreeMap<QuestionId, Question>,
        decider: Decider,
    },
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
            TaskKind::Route { .. } => "route",
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

/// `f64` fields deserialized through [`serde_json::Number`].
mod number {
    use serde::{Deserialize, Deserializer, de::Error};

    fn finite<E: Error>(n: serde_json::Number) -> Result<f64, E> {
        n.as_f64()
            .ok_or_else(|| E::custom(format!("{n} is not representable as a float")))
    }

    pub fn required<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        finite(serde_json::Number::deserialize(d)?)
    }

    pub fn optional<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
        Option::<serde_json::Number>::deserialize(d)?
            .map(finite)
            .transpose()
    }
}

/// What answers a route's questions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Decider {
    /// A System One decision model, reached through the broker.
    Model {
        #[serde(deserialize_with = "number::required")]
        min_confidence: f64,
    },
    /// A dependency's output, which carries one declared label per question id.
    Output { task: TaskName },
}

/// Run a task only when one question of a route it depends on resolved to a listed label.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct When {
    pub task: TaskName,
    pub question: QuestionId,
    pub is: Vec<Label>,
}

impl fmt::Display for When {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let labels: Vec<&str> = self.is.iter().map(Label::as_str).collect();
        write!(f, "{}.{} in {}", self.task, self.question, labels.join("|"))
    }
}

/// The capability a model-decided route task needs.
pub const NEEDS_SYSTEMONE: &str = "systemone";

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
    /// Fields the task's JSON output promises to include, optionally typed. Presence and type
    /// are checked at runtime; consumer contracts (`top_k`, grade sources, output-decided
    /// routes, `over`) at validation. Empty = undeclared.
    #[serde(default, skip_serializing_if = "Emits::is_empty")]
    pub emits: Emits,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<When>,
    /// Sends a failing verdict back to a dependency (see [`Revise`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revise: Option<Revise>,
}

/// A reviewer's bounded send-back: when the reviewer settles failing, `task` runs again with the
/// verdict, then the reviewer does, for at most `max_rounds` rounds in all.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revise {
    pub task: TaskName,
    pub max_rounds: u32,
}

/// The most rounds one revise loop may run. Operator-owned, like [`MAX_FANOUT_CEILING`].
pub const MAX_ROUNDS_CEILING: u32 = 5;

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
/// autoresearch shape live in [`crate::plan::workflow::WorkflowCfg`].
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
    #[error("task {task:?} declares output field {field:?} with an invalid type: {error}")]
    InvalidFieldType {
        task: String,
        field: String,
        error: FieldTypeError,
    },
    #[error(
        "task {task:?} reads a numeric `score` from {source_task:?}, which declares it {declared}; \
         declare it \"number\""
    )]
    ScoreNotNumeric {
        task: String,
        source_task: String,
        declared: FieldType,
    },
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
    #[error(
        "task {task:?} (stage {stage:?}) depends on {dependency:?} (stage {dependency_stage:?}); \
         dependencies cannot cross stages, and each would wait for the other forever"
    )]
    CrossStageDependency {
        task: String,
        stage: Stage,
        dependency: String,
        dependency_stage: Stage,
    },
    #[error("route task {task:?} declares no questions")]
    RouteWithoutQuestions { task: String },
    #[error("route task {task:?}, question {question:?}: {error}")]
    InvalidQuestion {
        task: String,
        question: String,
        error: QuestionError,
    },
    #[error("route task {task:?}: min_confidence must be in (0, 1], got {got}")]
    MinConfidenceOutOfRange { task: String, got: f64 },
    #[error(
        "model-decided route task {task:?} must declare needs = \"{NEEDS_SYSTEMONE}\", got {got:?}"
    )]
    RouteNeeds { task: String, got: String },
    #[error(
        "route task {task:?} decides from {source_task:?}, which is not one of its dependencies"
    )]
    RouteSourceNotADependency { task: String, source_task: String },
    #[error(
        "route task {task:?} reads question {question:?} from {source_task:?}, which declares emits without it"
    )]
    RouteSourceOmitsQuestion {
        task: String,
        source_task: String,
        question: String,
    },
    #[error(
        "route task {task:?} reads question {question:?} from {source_task:?}, which declares it \
         {declared}; {question:?} answers {expected}"
    )]
    RouteSourceUnanswerable {
        task: String,
        source_task: String,
        question: String,
        declared: Box<FieldType>,
        expected: String,
    },
    #[error(
        "task {task:?} maps over {reference}, but {producer:?} declares emits without {field:?}"
    )]
    OverFieldOmitted {
        task: String,
        reference: String,
        producer: String,
        field: String,
    },
    #[error(
        "task {task:?} maps over {reference}, which {producer:?} declares {declared}; `over` \
         needs a list"
    )]
    OverNotAList {
        task: String,
        reference: String,
        producer: String,
        declared: FieldType,
    },
    #[error("route task {task:?} declares `over`; routing each element of a list is not supported")]
    RouteWithOver { task: String },
    #[error("task {task:?}: when names {route:?}, which is not one of its dependencies")]
    WhenNotADependency { task: String, route: String },
    #[error("task {task:?}: when names {route:?}, which is a {kind} task, not a route")]
    WhenNotARoute {
        task: String,
        route: String,
        kind: &'static str,
    },
    #[error(
        "task {task:?}: route {route:?} has no question {question:?}; it declares: {}",
        .declared.join(", ")
    )]
    WhenUnknownQuestion {
        task: String,
        route: String,
        question: String,
        declared: Vec<String>,
    },
    #[error("task {task:?}: when on {route}.{question} lists no labels")]
    WhenWithoutLabels {
        task: String,
        route: String,
        question: String,
    },
    #[error("task {task:?}: when on {route}.{question} lists {label:?} twice")]
    WhenRepeatedLabel {
        task: String,
        route: String,
        question: String,
        label: String,
    },
    #[error(
        "task {task:?}: {route}.{question} cannot answer {label:?}; its labels are: {}",
        .declared.join(", ")
    )]
    WhenUnknownLabel {
        task: String,
        route: String,
        question: String,
        label: String,
        declared: Vec<String>,
    },
    #[error(
        "route {route:?}, question {question:?}: no task runs on {}. Add a task with a matching \
         when, or list them under the question's drop",
        .labels.join(", ")
    )]
    UnroutedLabels {
        route: String,
        question: String,
        labels: Vec<String>,
    },
    #[error(
        "task {task:?} revises {target:?} but does not depend on it; a reviewer reads the work it \
         sends back from a direct dependency"
    )]
    ReviseTargetNotADependency { task: String, target: String },
    #[error("task {task:?}: max_rounds = {got} is outside 2..={MAX_ROUNDS_CEILING}")]
    RoundsOutOfRange { task: String, got: u32 },
    #[error(
        "task {task:?} revises {target:?}, a {kind} task; only agent, command, and evaluate tasks \
         take part in a revise loop"
    )]
    ReviseOnUnsupportedTask {
        task: String,
        target: String,
        kind: &'static str,
    },
    #[error(
        "task {task:?} revises {target:?}, and one of them maps over a list; a round re-runs one \
         task, not a fan-out"
    )]
    ReviseWithFanout { task: String, target: String },
    #[error(
        "task {task:?} revises {target:?} and declares when; a reviewer runs whenever what it \
         revises does, so put the when on {target:?}"
    )]
    WhenOnReviewer { task: String, target: String },
    #[error("tasks {left:?} and {right:?} both revise {target:?}; a task has at most one reviewer")]
    ReviseTargetRevisedTwice {
        left: String,
        right: String,
        target: String,
    },
    #[error(
        "task {task:?} revises {target:?}, which is itself in another revise loop; revise loops \
         do not nest or chain"
    )]
    NestedRevise { task: String, target: String },
    #[error(
        "task {task:?} revises {target:?} and also depends on {dependency:?}, which depends on \
         {target:?}; a round re-runs only {target:?} and {task:?}, so {dependency:?} would read \
         a draft the loop replaces"
    )]
    ReviseAroundADependent {
        task: String,
        target: String,
        dependency: String,
    },
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

/// Whether every value of `declared` is an answer an output-decided route accepts for
/// `question`: one of its labels or `uncertain`, or, for a noul, a boolean.
fn answers(question: &Question, declared: &FieldType) -> bool {
    match (declared, &question.kind) {
        (FieldType::Boolean, QuestionKind::Noul) => true,
        (FieldType::OneOf(labels), _) => labels.iter().all(|l| question.resolves_to(l)),
        _ => false,
    }
}

fn answerable_types(question: &Question) -> String {
    let labels: Vec<String> = question
        .labels()
        .iter()
        .map(ToString::to_string)
        .chain([UNCERTAIN.to_owned()])
        .collect();
    let labels = format!("labels from {}", labels.join("|"));
    match question.kind {
        QuestionKind::Noul => format!("\"boolean\" or {labels}"),
        QuestionKind::Choice { .. } => labels,
    }
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
                if crate::plan::ir::RESERVED_INPUTS.contains(&d.0.as_str()) {
                    return Err(PlanError::ReservedDependencyName {
                        task: task(),
                        dependency: d.0.clone(),
                    });
                }
                let dependency_stage = index.get(d).map_or(t.stage, |&i| self.tasks[i].stage);
                if dependency_stage != t.stage {
                    return Err(PlanError::CrossStageDependency {
                        task: task(),
                        stage: t.stage,
                        dependency: d.0.clone(),
                        dependency_stage,
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
            if let TaskKind::Route { questions, decider } = &t.task {
                if questions.is_empty() {
                    return Err(PlanError::RouteWithoutQuestions { task: task() });
                }
                for (id, question) in questions {
                    question
                        .validate()
                        .map_err(|error| PlanError::InvalidQuestion {
                            task: task(),
                            question: id.to_string(),
                            error,
                        })?;
                }
                match decider {
                    Decider::Model { min_confidence } => {
                        if !(*min_confidence > 0.0 && *min_confidence <= 1.0) {
                            return Err(PlanError::MinConfidenceOutOfRange {
                                task: task(),
                                got: *min_confidence,
                            });
                        }
                        if t.needs != NEEDS_SYSTEMONE {
                            return Err(PlanError::RouteNeeds {
                                task: task(),
                                got: t.needs.clone(),
                            });
                        }
                    }
                    Decider::Output { task: source } => {
                        if !t.depends_on.contains(source) {
                            return Err(PlanError::RouteSourceNotADependency {
                                task: task(),
                                source_task: source.0.clone(),
                            });
                        }
                        let emits = &self.tasks[index[source]].emits;
                        for (id, question) in questions {
                            match emits.field(id.as_str()) {
                                Declared::Unchecked | Declared::Untyped => {}
                                Declared::Omitted => {
                                    return Err(PlanError::RouteSourceOmitsQuestion {
                                        task: task(),
                                        source_task: source.0.clone(),
                                        question: id.to_string(),
                                    });
                                }
                                Declared::Typed(declared) if answers(question, declared) => {}
                                Declared::Typed(declared) => {
                                    return Err(PlanError::RouteSourceUnanswerable {
                                        task: task(),
                                        source_task: source.0.clone(),
                                        question: id.to_string(),
                                        declared: Box::new(declared.clone()),
                                        expected: answerable_types(question),
                                    });
                                }
                            }
                        }
                    }
                }
                if t.over.is_some() {
                    return Err(PlanError::RouteWithOver { task: task() });
                }
            }
            if let Some(when) = &t.when {
                let route = || when.task.0.clone();
                let question = || when.question.to_string();
                if !t.depends_on.contains(&when.task) {
                    return Err(PlanError::WhenNotADependency {
                        task: task(),
                        route: route(),
                    });
                }
                let target = &self.tasks[index[&when.task]];
                let TaskKind::Route { questions, .. } = &target.task else {
                    return Err(PlanError::WhenNotARoute {
                        task: task(),
                        route: route(),
                        kind: target.task.label(),
                    });
                };
                let Some(asked) = questions.get(&when.question) else {
                    return Err(PlanError::WhenUnknownQuestion {
                        task: task(),
                        route: route(),
                        question: question(),
                        declared: questions.keys().map(ToString::to_string).collect(),
                    });
                };
                if when.is.is_empty() {
                    return Err(PlanError::WhenWithoutLabels {
                        task: task(),
                        route: route(),
                        question: question(),
                    });
                }
                let mut listed = BTreeSet::new();
                for label in &when.is {
                    if !asked.resolves_to(label) {
                        return Err(PlanError::WhenUnknownLabel {
                            task: task(),
                            route: route(),
                            question: question(),
                            label: label.to_string(),
                            declared: asked
                                .labels()
                                .iter()
                                .map(ToString::to_string)
                                .chain([UNCERTAIN.to_owned()])
                                .collect(),
                        });
                    }
                    if !listed.insert(label) {
                        return Err(PlanError::WhenRepeatedLabel {
                            task: task(),
                            route: route(),
                            question: question(),
                            label: label.to_string(),
                        });
                    }
                }
            }
            if !t.emits.is_empty() {
                if matches!(
                    t.task,
                    TaskKind::TopK { .. } | TaskKind::Route { .. } | TaskKind::Engine { .. }
                ) {
                    return Err(PlanError::EmitsOnEngineTask {
                        task: task(),
                        kind: t.task.label(),
                    });
                }
                let mut fields = BTreeSet::new();
                for (field, ty) in t.emits.fields() {
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
                    if let Some(ty) = ty {
                        ty.validate().map_err(|error| PlanError::InvalidFieldType {
                            task: task(),
                            field: field.0.clone(),
                            error,
                        })?;
                    }
                }
            }
            // A declared emits that omits `score`, or types it as something other than a number,
            // where a score is read is a wiring bug worth failing before any spend.
            let score = |name: &TaskName| {
                index
                    .get(name)
                    .map_or(Declared::Unchecked, |&i| self.tasks[i].emits.field("score"))
            };
            let not_numeric =
                |source: &TaskName, declared: &FieldType| PlanError::ScoreNotNumeric {
                    task: task(),
                    source_task: source.0.clone(),
                    declared: declared.clone(),
                };
            if matches!(t.task, TaskKind::TopK { .. }) {
                for d in &t.depends_on {
                    match score(d) {
                        Declared::Omitted => {
                            return Err(PlanError::TopKSourceOmitsScore {
                                task: task(),
                                dependency: d.0.clone(),
                            });
                        }
                        Declared::Typed(declared) if !declared.is_numeric() => {
                            return Err(not_numeric(d, declared));
                        }
                        _ => {}
                    }
                }
            }
            if let TaskKind::Engine {
                op: EngineOp::Grade,
                source,
                tiebreak,
            } = &t.task
            {
                for source in source.iter().chain(tiebreak) {
                    match score(source) {
                        Declared::Omitted => {
                            return Err(PlanError::GradeSourceOmitsScore {
                                task: task(),
                                from: source.0.clone(),
                            });
                        }
                        Declared::Typed(declared) if !declared.is_numeric() => {
                            return Err(not_numeric(source, declared));
                        }
                        _ => {}
                    }
                }
            }
            if let TaskKind::Evaluate {
                threshold: Some(_), ..
            } = &t.task
            {
                match t.emits.field("score") {
                    Declared::Omitted => {
                        return Err(PlanError::ThresholdedEvaluateOmitsScore { task: task() });
                    }
                    Declared::Typed(declared) if !declared.is_numeric() => {
                        return Err(not_numeric(&t.name, declared));
                    }
                    _ => {}
                }
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
                    let producer = &self.tasks[index[&reference.task]];
                    match producer.emits.field(&reference.field.0) {
                        Declared::Unchecked
                        | Declared::Untyped
                        | Declared::Typed(FieldType::List) => {}
                        Declared::Omitted => {
                            return Err(PlanError::OverFieldOmitted {
                                task: task(),
                                reference: reference.to_string(),
                                producer: reference.task.0.clone(),
                                field: reference.field.0.clone(),
                            });
                        }
                        Declared::Typed(declared) => {
                            return Err(PlanError::OverNotAList {
                                task: task(),
                                reference: reference.to_string(),
                                producer: reference.task.0.clone(),
                                declared: declared.clone(),
                            });
                        }
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
            if let Some(revise) = &t.revise {
                let target = || revise.task.0.clone();
                if t.when.is_some() {
                    return Err(PlanError::WhenOnReviewer {
                        task: task(),
                        target: target(),
                    });
                }
                if !t.depends_on.contains(&revise.task) {
                    return Err(PlanError::ReviseTargetNotADependency {
                        task: task(),
                        target: target(),
                    });
                }
                if !(2..=MAX_ROUNDS_CEILING).contains(&revise.max_rounds) {
                    return Err(PlanError::RoundsOutOfRange {
                        task: task(),
                        got: revise.max_rounds,
                    });
                }
                let Some(&proposer) = index.get(&revise.task) else {
                    return Err(PlanError::UnknownDependency {
                        task: task(),
                        dependency: target(),
                    });
                };
                let proposer = &self.tasks[proposer];
                for member in [t, proposer] {
                    if !matches!(
                        member.task,
                        TaskKind::Agent { .. }
                            | TaskKind::Command { .. }
                            | TaskKind::Evaluate { .. }
                    ) {
                        return Err(PlanError::ReviseOnUnsupportedTask {
                            task: task(),
                            target: target(),
                            kind: member.task.label(),
                        });
                    }
                    if member.over.is_some() {
                        return Err(PlanError::ReviseWithFanout {
                            task: task(),
                            target: target(),
                        });
                    }
                }
                if proposer.revise.is_some() {
                    return Err(PlanError::NestedRevise {
                        task: task(),
                        target: target(),
                    });
                }
                for other in &self.tasks {
                    let Some(other_revise) = &other.revise else {
                        continue;
                    };
                    if other.name == t.name {
                        continue;
                    }
                    if other_revise.task == revise.task {
                        return Err(PlanError::ReviseTargetRevisedTwice {
                            left: task(),
                            right: other.name.0.clone(),
                            target: target(),
                        });
                    }
                    if other_revise.task == t.name {
                        return Err(PlanError::NestedRevise {
                            task: other.name.0.clone(),
                            target: task(),
                        });
                    }
                }
            }
        }
        let mut handled: BTreeMap<(&TaskName, &QuestionId), BTreeSet<&Label>> = BTreeMap::new();
        for t in &self.tasks {
            if let Some(when) = &t.when {
                handled
                    .entry((&when.task, &when.question))
                    .or_default()
                    .extend(&when.is);
            }
        }
        for ((route, question), listed) in &handled {
            let TaskKind::Route { questions, .. } = &self.tasks[index[route]].task else {
                continue;
            };
            let Some(asked) = questions.get(question) else {
                continue;
            };
            let unrouted: Vec<String> = asked
                .labels()
                .into_iter()
                .chain([Label::uncertain()])
                .filter(|l| !listed.contains(l) && !asked.drop.contains(l))
                .map(|l| l.to_string())
                .collect();
            if !unrouted.is_empty() {
                return Err(PlanError::UnroutedLabels {
                    route: route.0.clone(),
                    question: question.to_string(),
                    labels: unrouted,
                });
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
        for t in &self.tasks {
            let Some(revise) = &t.revise else { continue };
            let Some(&proposer) = index.get(&revise.task) else {
                continue;
            };
            for d in t.depends_on.iter().filter(|d| **d != revise.task) {
                if index.get(d).is_some_and(|&i| reaches(proposer, i)) {
                    return Err(PlanError::ReviseAroundADependent {
                        task: t.name.0.clone(),
                        target: revise.task.0.clone(),
                        dependency: d.0.clone(),
                    });
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
            emits: crate::plan::ir::Emits::default(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
            when: None,
            revise: None,
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

    fn label(s: &str) -> Label {
        Label::new(s).unwrap()
    }

    fn qid(s: &str) -> QuestionId {
        QuestionId::new(s).unwrap()
    }

    fn area_question(drop: &[&str]) -> Question {
        use crucible_contract::decision::{ChoiceOption, QuestionKind};
        Question {
            instructions: "Which component?".into(),
            kind: QuestionKind::Choice {
                options: ["scheduler", "frontend"]
                    .iter()
                    .map(|o| ChoiceOption {
                        label: label(o),
                        description: None,
                    })
                    .collect(),
            },
            drop: drop.iter().map(|l| label(l)).collect(),
        }
    }

    fn model_route(name: &str, deps: &[&str], drop: &[&str]) -> Task {
        Task {
            task: TaskKind::Route {
                questions: BTreeMap::from([(qid("area"), area_question(drop))]),
                decider: Decider::Model {
                    min_confidence: 0.8,
                },
            },
            needs: NEEDS_SYSTEMONE.into(),
            ..agent(name, deps)
        }
    }

    fn output_route(name: &str, source: &str, drop: &[&str]) -> Task {
        Task {
            task: TaskKind::Route {
                questions: BTreeMap::from([(qid("area"), area_question(drop))]),
                decider: Decider::Output {
                    task: source.into(),
                },
            },
            ..agent(name, &[source])
        }
    }

    fn on(mut task: Task, route: &str, question: &str, is: &[&str]) -> Task {
        task.when = Some(When {
            task: route.into(),
            question: qid(question),
            is: is.iter().map(|l| label(l)).collect(),
        });
        task
    }

    fn routed() -> Vec<Task> {
        vec![
            agent("scan", &[]),
            model_route("gate", &["scan"], &[]),
            on(agent("fix", &["gate"]), "gate", "area", &["scheduler"]),
            on(
                agent("punt", &["gate"]),
                "gate",
                "area",
                &["frontend", "uncertain"],
            ),
        ]
    }

    #[test]
    fn a_thresholded_evaluate_parses_from_json() {
        let json = r#"{"version":1,"budget":{"usd":5.0},"task":[{"name":"e","kind":"evaluate","command":"x","threshold":0.5,"direction":"higher"}]}"#;
        let plan = Plan::from_json_str(json).unwrap();
        let TaskKind::Evaluate { threshold, .. } = &plan.tasks[0].task else {
            panic!("e is not an evaluate");
        };
        assert_eq!(*threshold, Some(0.5));
    }

    #[test]
    fn a_fully_routed_question_validates() {
        plan(routed()).validate().unwrap();
    }

    #[test]
    fn a_route_and_its_when_round_trip_through_toml_and_json() {
        let original = plan(routed());
        let text = toml::to_string(&original).unwrap();
        let from_toml = Plan::from_toml_str(&text).unwrap();
        let from_json = Plan::from_json_str(&serde_json::to_string(&original).unwrap()).unwrap();
        for back in [from_toml, from_json] {
            assert_eq!(back.tasks[2].when, original.tasks[2].when);
            let TaskKind::Route { questions, decider } = &back.tasks[1].task else {
                panic!("gate is not a route");
            };
            assert_eq!(questions[&qid("area")], area_question(&[]));
            assert_eq!(
                *decider,
                Decider::Model {
                    min_confidence: 0.8
                }
            );
            back.validate().unwrap();
        }
    }

    #[test]
    fn an_integer_min_confidence_parses_from_toml() {
        let text = toml::to_string(&plan(routed()))
            .unwrap()
            .replace("min_confidence = 0.8", "min_confidence = 1");
        Plan::from_toml_str(&text).unwrap().validate().unwrap();
    }

    #[test]
    fn a_task_without_when_serializes_without_the_key() {
        let text = toml::to_string(&plan(vec![agent("a", &[])])).unwrap();
        assert!(!text.contains("when"), "{text}");
    }

    #[test]
    fn a_route_needs_questions() {
        let mut gate = model_route("gate", &[], &[]);
        gate.task = TaskKind::Route {
            questions: BTreeMap::new(),
            decider: Decider::Model {
                min_confidence: 0.8,
            },
        };
        assert_eq!(
            plan(vec![gate]).validate().unwrap_err(),
            PlanError::RouteWithoutQuestions {
                task: "gate".into()
            }
        );
    }

    #[test]
    fn a_route_rejects_an_invalid_question() {
        let mut gate = model_route("gate", &[], &["ghost"]);
        assert_eq!(
            plan(vec![gate.clone()]).validate().unwrap_err(),
            PlanError::InvalidQuestion {
                task: "gate".into(),
                question: "area".into(),
                error: QuestionError::UnknownDrop {
                    label: label("ghost")
                },
            }
        );
        if let TaskKind::Route { questions, .. } = &mut gate.task {
            let q = questions.get_mut(&qid("area")).unwrap();
            q.drop.clear();
            q.instructions = String::new();
        }
        assert!(matches!(
            plan(vec![gate]).validate().unwrap_err(),
            PlanError::InvalidQuestion {
                error: QuestionError::EmptyInstructions,
                ..
            }
        ));
    }

    #[test]
    fn min_confidence_must_lie_in_zero_exclusive_to_one_inclusive() {
        for (value, ok) in [
            (0.0, false),
            (-0.5, false),
            (1.01, false),
            (f64::NAN, false),
            (0.01, true),
            (1.0, true),
        ] {
            let mut gate = model_route("gate", &[], &[]);
            if let TaskKind::Route { decider, .. } = &mut gate.task {
                *decider = Decider::Model {
                    min_confidence: value,
                };
            }
            let got = plan(vec![gate]).validate();
            assert_eq!(got.is_ok(), ok, "min_confidence = {value}");
            if !ok {
                assert!(matches!(
                    got.unwrap_err(),
                    PlanError::MinConfidenceOutOfRange { .. }
                ));
            }
        }
    }

    #[test]
    fn a_model_route_must_need_systemone_and_an_output_route_need_not() {
        let mut gate = model_route("gate", &[], &[]);
        gate.needs = "any".into();
        assert_eq!(
            plan(vec![gate]).validate().unwrap_err(),
            PlanError::RouteNeeds {
                task: "gate".into(),
                got: "any".into()
            }
        );
        plan(vec![
            agent("classify", &[]),
            output_route("gate", "classify", &[]),
        ])
        .validate()
        .unwrap();
    }

    #[test]
    fn a_route_rejects_over_and_emits() {
        let mut gate = model_route("gate", &["scan"], &[]);
        gate.over = Some(OutputRef {
            task: "scan".into(),
            field: OutputField("issues".into()),
        });
        gate.max_fanout = Some(4);
        assert_eq!(
            plan(vec![agent("scan", &[]), gate]).validate().unwrap_err(),
            PlanError::RouteWithOver {
                task: "gate".into()
            }
        );
        let mut gate = model_route("gate", &[], &[]);
        gate.emits = fields(&["area"]);
        assert_eq!(
            plan(vec![gate]).validate().unwrap_err(),
            PlanError::EmitsOnEngineTask {
                task: "gate".into(),
                kind: "route"
            }
        );
    }

    #[test]
    fn an_output_route_reads_a_dependency_that_declares_the_question() {
        let mut gate = output_route("gate", "classify", &[]);
        gate.depends_on.clear();
        assert_eq!(
            plan(vec![agent("classify", &[]), gate])
                .validate()
                .unwrap_err(),
            PlanError::RouteSourceNotADependency {
                task: "gate".into(),
                source_task: "classify".into()
            }
        );
        let mut classify = agent("classify", &[]);
        classify.emits = fields(&["severity"]);
        assert_eq!(
            plan(vec![
                classify.clone(),
                output_route("gate", "classify", &[])
            ])
            .validate()
            .unwrap_err(),
            PlanError::RouteSourceOmitsQuestion {
                task: "gate".into(),
                source_task: "classify".into(),
                question: "area".into()
            }
        );
        classify.emits = fields(&["severity", "area"]);
        plan(vec![classify, output_route("gate", "classify", &[])])
            .validate()
            .unwrap();
    }

    #[test]
    fn when_must_name_a_route_among_the_dependencies() {
        let mut tasks = routed();
        tasks[2].depends_on = vec!["scan".into()];
        assert_eq!(
            plan(tasks).validate().unwrap_err(),
            PlanError::WhenNotADependency {
                task: "fix".into(),
                route: "gate".into()
            }
        );
        let mut tasks = routed();
        tasks[2] = on(
            agent("fix", &["scan", "gate"]),
            "scan",
            "area",
            &["scheduler"],
        );
        assert_eq!(
            plan(tasks).validate().unwrap_err(),
            PlanError::WhenNotARoute {
                task: "fix".into(),
                route: "scan".into(),
                kind: "agent"
            }
        );
    }

    #[test]
    fn when_must_name_a_declared_question_and_labels() {
        let mut tasks = routed();
        tasks[2] = on(agent("fix", &["gate"]), "gate", "aria", &["scheduler"]);
        assert_eq!(
            plan(tasks).validate().unwrap_err(),
            PlanError::WhenUnknownQuestion {
                task: "fix".into(),
                route: "gate".into(),
                question: "aria".into(),
                declared: vec!["area".into()]
            }
        );
        let mut tasks = routed();
        tasks[2] = on(agent("fix", &["gate"]), "gate", "area", &["schedular"]);
        assert_eq!(
            plan(tasks).validate().unwrap_err(),
            PlanError::WhenUnknownLabel {
                task: "fix".into(),
                route: "gate".into(),
                question: "area".into(),
                label: "schedular".into(),
                declared: vec!["scheduler".into(), "frontend".into(), "uncertain".into()]
            }
        );
        let mut tasks = routed();
        tasks[2] = on(agent("fix", &["gate"]), "gate", "area", &[]);
        assert!(matches!(
            plan(tasks).validate().unwrap_err(),
            PlanError::WhenWithoutLabels { .. }
        ));
        let mut tasks = routed();
        tasks[2] = on(
            agent("fix", &["gate"]),
            "gate",
            "area",
            &["scheduler", "scheduler"],
        );
        assert!(matches!(
            plan(tasks).validate().unwrap_err(),
            PlanError::WhenRepeatedLabel { .. }
        ));
    }

    #[test]
    fn every_label_of_a_routed_question_is_handled_or_dropped() {
        let mut tasks = routed();
        tasks.pop();
        assert_eq!(
            plan(tasks.clone()).validate().unwrap_err(),
            PlanError::UnroutedLabels {
                route: "gate".into(),
                question: "area".into(),
                labels: vec!["frontend".into(), "uncertain".into()]
            }
        );
        tasks[1] = model_route("gate", &["scan"], &["frontend", "uncertain"]);
        plan(tasks).validate().unwrap();
    }

    #[test]
    fn a_question_no_when_refers_to_need_not_be_routed() {
        plan(vec![
            agent("scan", &[]),
            model_route("gate", &["scan"], &[]),
        ])
        .validate()
        .unwrap();
    }

    #[test]
    fn a_when_displays_as_route_question_in_labels() {
        let task = on(
            agent("punt", &["gate"]),
            "gate",
            "area",
            &["frontend", "uncertain"],
        );
        assert_eq!(
            task.when.unwrap().to_string(),
            "gate.area in frontend|uncertain"
        );
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
            emits: crate::plan::ir::Emits::default(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
            when: None,
            revise: None,
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
        t.emits = fields(emits);
        t
    }

    fn fields(names: &[&str]) -> Emits {
        Emits::Fields(
            names
                .iter()
                .map(|f| OutputField((*f).to_string()))
                .collect(),
        )
    }

    fn typed(pairs: &[(&str, FieldType)]) -> Emits {
        Emits::Typed(
            pairs
                .iter()
                .map(|(f, ty)| (OutputField((*f).to_string()), ty.clone()))
                .collect(),
        )
    }

    fn one_of(labels: &[&str]) -> FieldType {
        FieldType::OneOf(labels.iter().map(|l| Label::new(*l).unwrap()).collect())
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
            emits: crate::plan::ir::Emits::default(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
            when: None,
            revise: None,
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
        pick.emits = fields(&["kept"]);
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
        measure.emits = fields(&["score"]);
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

        t.emits = fields(&["latency_ms", "score"]);
        assert!(plan(vec![t]).validate().is_ok());
    }

    #[test]
    fn typed_emits_parse_from_toml_and_json_and_keep_their_form() {
        let toml = r#"
version = 1
[budget]
usd = 1.0
[[task]]
name = "classify"
kind = "command"
command = "./c.sh"
emits = { score = "number", tier = ["high", "low"], notes = "list" }
[[task]]
name = "legacy"
kind = "command"
command = "./l.sh"
emits = ["lines"]
"#;
        let plan = Plan::from_toml_str(toml).unwrap().validate().unwrap();
        let tasks = &plan.plan().tasks;
        assert_eq!(
            tasks[0].emits,
            typed(&[
                ("score", FieldType::Number),
                ("tier", one_of(&["high", "low"])),
                ("notes", FieldType::List),
            ])
        );
        assert_eq!(tasks[1].emits, fields(&["lines"]));

        let json = serde_json::to_value(&tasks[0]).unwrap();
        assert_eq!(
            json["emits"],
            serde_json::json!({"notes": "list", "score": "number", "tier": ["high", "low"]})
        );
        let back: Task = serde_json::from_value(json).unwrap();
        assert_eq!(back.emits, tasks[0].emits);
        let legacy = serde_json::to_value(&tasks[1]).unwrap();
        assert_eq!(legacy["emits"], serde_json::json!(["lines"]));

        let text = toml::to_string(plan.plan()).unwrap();
        let again = Plan::from_toml_str(&text).unwrap();
        assert_eq!(again.tasks[0].emits, tasks[0].emits);
        assert_eq!(again.tasks[1].emits, tasks[1].emits);
    }

    #[test]
    fn a_malformed_typed_emits_does_not_parse_or_validate() {
        let parse = |emits: &str| {
            let toml = format!(
                "version = 1\n[budget]\nusd = 1.0\n[[task]]\nname = \"a\"\nkind = \"command\"\ncommand = \"x\"\nemits = {emits}\n"
            );
            format!("{:#}", Plan::from_toml_str(&toml).unwrap_err())
        };
        for (emits, needle) in [
            (r#"{ score = "float" }"#, "a field type"),
            (r#"{ score = 3 }"#, "a field type"),
            (r#"{ tier = ["hi-gh"] }"#, "not an identifier"),
            (r#"{ tier = [] }"#, "names no labels"),
            (r#"{ tier = ["a", "a"] }"#, "listed twice"),
            (r#""score""#, "a list of field names, or a table"),
        ] {
            let err = parse(emits);
            assert!(err.contains(needle), "{emits}: {err}");
        }
        let duplicate = serde_json::from_str::<Emits>(r#"{"a": "string", "a": "list"}"#)
            .unwrap_err()
            .to_string();
        assert!(duplicate.contains("declared twice"), "{duplicate}");

        let mut t = agent("a", &[]);
        t.emits = typed(&[("tier", FieldType::OneOf(Vec::new()))]);
        assert_eq!(
            plan(vec![t]).validate().unwrap_err(),
            PlanError::InvalidFieldType {
                task: "a".into(),
                field: "tier".into(),
                error: FieldTypeError::NoLabels,
            }
        );
        let mut t = agent("a", &[]);
        t.emits = typed(&[("bad-name", FieldType::String)]);
        assert!(matches!(
            plan(vec![t]).validate().unwrap_err(),
            PlanError::InvalidOutputField { .. }
        ));
    }

    #[test]
    fn a_score_source_that_types_its_score_must_type_it_numeric() {
        for ty in [FieldType::Number, FieldType::Integer] {
            let mut m = agent("m", &[]);
            m.emits = typed(&[("score", ty)]);
            plan(vec![m, top_k("pick", &["m"])]).validate().unwrap();
        }
        for ty in [
            FieldType::String,
            FieldType::Boolean,
            FieldType::List,
            FieldType::Object,
            one_of(&["high"]),
        ] {
            let mut m = agent("m", &[]);
            m.emits = typed(&[("score", ty.clone())]);
            assert_eq!(
                plan(vec![m, top_k("pick", &["m"])]).validate().unwrap_err(),
                PlanError::ScoreNotNumeric {
                    task: "pick".into(),
                    source_task: "m".into(),
                    declared: ty,
                }
            );
        }
        let mut m = agent("m", &[]);
        m.emits = typed(&[("latency_ms", FieldType::Number)]);
        assert_eq!(
            plan(vec![m, top_k("pick", &["m"])]).validate().unwrap_err(),
            PlanError::TopKSourceOmitsScore {
                task: "pick".into(),
                dependency: "m".into(),
            }
        );

        let evaluate = |emits: Emits| {
            let mut e = agent("e", &[]);
            e.task = TaskKind::Evaluate {
                command: "./x.sh".into(),
                threshold: None,
                direction: None,
            };
            e.emits = emits;
            e
        };
        let mut grade = agent("grade", &["e"]);
        grade.task = TaskKind::Engine {
            op: EngineOp::Grade,
            source: Some("e".into()),
            tiebreak: None,
        };
        assert_eq!(
            plan(vec![
                evaluate(typed(&[("score", FieldType::Boolean)])),
                grade.clone()
            ])
            .validate()
            .unwrap_err(),
            PlanError::ScoreNotNumeric {
                task: "grade".into(),
                source_task: "e".into(),
                declared: FieldType::Boolean,
            }
        );
        assert_eq!(
            plan(vec![
                evaluate(typed(&[("pass", FieldType::Boolean)])),
                grade.clone()
            ])
            .validate()
            .unwrap_err(),
            PlanError::GradeSourceOmitsScore {
                task: "grade".into(),
                from: "e".into(),
            }
        );
        plan(vec![
            evaluate(typed(&[("score", FieldType::Integer)])),
            grade,
        ])
        .validate()
        .unwrap();

        let mut thresholded = evaluate(typed(&[("score", FieldType::String)]));
        thresholded.task = TaskKind::Evaluate {
            command: "./x.sh".into(),
            threshold: Some(1.0),
            direction: Some(Direction::Lower),
        };
        assert_eq!(
            plan(vec![thresholded.clone()]).validate().unwrap_err(),
            PlanError::ScoreNotNumeric {
                task: "e".into(),
                source_task: "e".into(),
                declared: FieldType::String,
            }
        );
        thresholded.emits = typed(&[("pass", FieldType::Boolean)]);
        assert_eq!(
            plan(vec![thresholded.clone()]).validate().unwrap_err(),
            PlanError::ThresholdedEvaluateOmitsScore { task: "e".into() }
        );
        thresholded.emits = typed(&[("score", FieldType::Number)]);
        plan(vec![thresholded]).validate().unwrap();
    }

    #[test]
    fn a_grade_tiebreak_that_declares_emits_must_declare_a_numeric_score() {
        let evaluate = |name: &str, emits: Emits| {
            let mut e = agent(name, &[]);
            e.task = TaskKind::Evaluate {
                command: "./x.sh".into(),
                threshold: None,
                direction: None,
            };
            e.emits = emits;
            e
        };
        let mut grade = agent("grade", &["e", "tb"]);
        grade.task = TaskKind::Engine {
            op: EngineOp::Grade,
            source: Some("e".into()),
            tiebreak: Some("tb".into()),
        };
        let with = |tiebreak: Emits| {
            plan(vec![
                evaluate("e", typed(&[("score", FieldType::Number)])),
                evaluate("tb", tiebreak),
                grade.clone(),
            ])
            .validate()
        };
        with(Emits::default()).unwrap();
        with(fields(&["score"])).unwrap();
        with(typed(&[("score", FieldType::Integer)])).unwrap();
        with(typed(&[("score", FieldType::Number)])).unwrap();
        assert_eq!(
            with(typed(&[("score", FieldType::String)])).unwrap_err(),
            PlanError::ScoreNotNumeric {
                task: "grade".into(),
                source_task: "tb".into(),
                declared: FieldType::String,
            }
        );
        for omitted in [fields(&["pass"]), typed(&[("pass", FieldType::Boolean)])] {
            assert_eq!(
                with(omitted).unwrap_err(),
                PlanError::GradeSourceOmitsScore {
                    task: "grade".into(),
                    from: "tb".into(),
                }
            );
        }
    }

    fn noul_route(name: &str, source: &str) -> Task {
        Task {
            task: TaskKind::Route {
                questions: BTreeMap::from([(
                    qid("urgent"),
                    Question {
                        instructions: "Urgent?".into(),
                        kind: QuestionKind::Noul,
                        drop: Vec::new(),
                    },
                )]),
                decider: Decider::Output {
                    task: source.into(),
                },
            },
            ..agent(name, &[source])
        }
    }

    #[test]
    fn an_output_route_question_must_be_answerable_by_the_declared_type() {
        let check = |question: &str, ty: FieldType| {
            let mut classify = agent("classify", &[]);
            classify.emits = typed(&[(question, ty)]);
            let gate = if question == "area" {
                output_route("gate", "classify", &[])
            } else {
                noul_route("gate", "classify")
            };
            plan(vec![classify, gate]).validate()
        };
        for ok in [
            one_of(&["scheduler", "frontend"]),
            one_of(&["frontend"]),
            one_of(&["scheduler", "uncertain"]),
        ] {
            check("area", ok).unwrap();
        }
        for (bad, expected) in [
            (
                FieldType::String,
                "labels from scheduler|frontend|uncertain",
            ),
            (
                FieldType::Boolean,
                "labels from scheduler|frontend|uncertain",
            ),
            (
                one_of(&["scheduler", "backend"]),
                "labels from scheduler|frontend|uncertain",
            ),
        ] {
            assert_eq!(
                check("area", bad.clone()).unwrap_err(),
                PlanError::RouteSourceUnanswerable {
                    task: "gate".into(),
                    source_task: "classify".into(),
                    question: "area".into(),
                    declared: Box::new(bad),
                    expected: expected.into(),
                }
            );
        }
        for ok in [
            FieldType::Boolean,
            one_of(&["yes", "no"]),
            one_of(&["no", "uncertain"]),
        ] {
            check("urgent", ok).unwrap();
        }
        for bad in [
            FieldType::String,
            FieldType::Integer,
            one_of(&["yes", "maybe"]),
        ] {
            let err = check("urgent", bad.clone()).unwrap_err();
            assert_eq!(
                err,
                PlanError::RouteSourceUnanswerable {
                    task: "gate".into(),
                    source_task: "classify".into(),
                    question: "urgent".into(),
                    declared: Box::new(bad),
                    expected: "\"boolean\" or labels from yes|no|uncertain".into(),
                }
            );
        }
        let mut classify = agent("classify", &[]);
        classify.emits = typed(&[("severity", one_of(&["high"]))]);
        assert_eq!(
            plan(vec![classify, output_route("gate", "classify", &[])])
                .validate()
                .unwrap_err(),
            PlanError::RouteSourceOmitsQuestion {
                task: "gate".into(),
                source_task: "classify".into(),
                question: "area".into(),
            }
        );
        let message = check("area", FieldType::String).unwrap_err().to_string();
        assert!(
            message.contains("which declares it string; \"area\" answers labels from"),
            "{message}"
        );
    }

    #[test]
    fn over_must_name_a_declared_list() {
        let targets = OutputRef {
            task: "discover".into(),
            field: OutputField("targets".into()),
        };
        let audit = || {
            let mut t = agent("audit", &["discover"]);
            t.over = Some(targets.clone());
            t.max_fanout = Some(4);
            t
        };
        let with = |emits: Emits| {
            let mut discover = agent("discover", &[]);
            discover.emits = emits;
            plan(vec![discover, audit()]).validate()
        };
        with(Emits::default()).unwrap();
        with(fields(&["targets"])).unwrap();
        with(typed(&[("targets", FieldType::List)])).unwrap();
        for bad in [FieldType::Object, FieldType::String, one_of(&["a"])] {
            assert_eq!(
                with(typed(&[("targets", bad.clone())])).unwrap_err(),
                PlanError::OverNotAList {
                    task: "audit".into(),
                    reference: "discover.targets".into(),
                    producer: "discover".into(),
                    declared: bad,
                }
            );
        }
        for omitted in [fields(&["other"]), typed(&[("other", FieldType::List)])] {
            assert_eq!(
                with(omitted).unwrap_err(),
                PlanError::OverFieldOmitted {
                    task: "audit".into(),
                    reference: "discover.targets".into(),
                    producer: "discover".into(),
                    field: "targets".into(),
                }
            );
        }
    }

    #[test]
    fn declared_names_the_field_in_either_form() {
        assert_eq!(Emits::default().field("a"), Declared::Unchecked);
        assert_eq!(typed(&[]).field("a"), Declared::Unchecked);
        assert_eq!(fields(&["a"]).field("a"), Declared::Untyped);
        assert_eq!(fields(&["a"]).field("b"), Declared::Omitted);
        assert_eq!(
            typed(&[("a", FieldType::List)]).field("a"),
            Declared::Typed(&FieldType::List)
        );
        assert_eq!(
            typed(&[("a", FieldType::List)]).field("b"),
            Declared::Omitted
        );
        assert_eq!(
            typed(&[("b", FieldType::List), ("a", FieldType::String)]).names(),
            ["a", "b"]
        );
        assert_eq!(fields(&["b", "a"]).names(), ["b", "a"]);
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
        assert_eq!(back.plan().tasks[0].emits, fields(&["score"]));
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
        for reserved in crate::plan::ir::RESERVED_INPUTS {
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

    /// The two stages are scheduled against each other: an epilogue task waits for every
    /// main-graph task to settle, and a main-graph task waits for its dependencies. An edge
    /// either way is a deadlock the executor cannot see, so it dispatches nothing and reports a
    /// completed run with no rows at all.
    #[test]
    fn a_dependency_that_crosses_a_stage_is_refused() {
        let mut wrap = agent("wrap", &[]);
        wrap.stage = Stage::Epilogue;
        assert_eq!(
            plan(vec![wrap.clone(), agent("build", &["wrap"])])
                .validate()
                .unwrap_err(),
            PlanError::CrossStageDependency {
                task: "build".to_owned(),
                stage: Stage::Iteration,
                dependency: "wrap".to_owned(),
                dependency_stage: Stage::Epilogue,
            }
        );

        let mut wrap_on_build = agent("wrap", &["build"]);
        wrap_on_build.stage = Stage::Epilogue;
        assert_eq!(
            plan(vec![agent("build", &[]), wrap_on_build])
                .validate()
                .unwrap_err(),
            PlanError::CrossStageDependency {
                task: "wrap".to_owned(),
                stage: Stage::Epilogue,
                dependency: "build".to_owned(),
                dependency_stage: Stage::Iteration,
            }
        );

        let mut publish = agent("publish", &["wrap"]);
        publish.stage = Stage::Epilogue;
        let ok = plan(vec![wrap, publish])
            .validate()
            .expect("one stage, one graph");
        assert_eq!(ok.plan().tasks.len(), 2);
    }

    fn reviewer(name: &str, deps: &[&str], target: &str, max_rounds: u32) -> Task {
        let mut t = agent(name, deps);
        t.revise = Some(Revise {
            task: target.into(),
            max_rounds,
        });
        t
    }

    fn refused(tasks: Vec<Task>) -> PlanError {
        plan(tasks).validate().unwrap_err()
    }

    /// A plan that arrives as JSON never passes through the starlark front end, so every
    /// structural fact about a revise loop is checked here or nowhere.
    #[test]
    fn a_revise_loop_round_trips_and_validates_on_the_json_route() {
        let p = plan(vec![
            agent("author", &[]),
            reviewer("repro", &["author"], "author", 3),
        ]);
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(
            json["task"][1]["revise"],
            serde_json::json!({"task": "author", "max_rounds": 3})
        );
        assert!(json["task"][0].get("revise").is_none(), "{json}");
        let back = Plan::from_json_str(&json.to_string())
            .unwrap()
            .validate()
            .unwrap();
        assert_eq!(
            back.get(&"repro".into()).and_then(|t| t.revise.clone()),
            Some(Revise {
                task: "author".into(),
                max_rounds: 3
            })
        );
    }

    #[test]
    fn a_reviewer_cannot_be_conditional_and_what_it_revises_can() {
        let gate = output_route("gate", "classify", &["frontend", "uncertain"]);
        let base = || vec![agent("classify", &[]), gate.clone()];
        let mut tasks = base();
        tasks.push(on(
            agent("author", &["gate"]),
            "gate",
            "area",
            &["scheduler"],
        ));
        tasks.push(reviewer("repro", &["author"], "author", 3));
        plan(tasks).validate().unwrap();

        let mut tasks = base();
        tasks.push(agent("author", &[]));
        tasks.push(on(
            reviewer("repro", &["author", "gate"], "author", 3),
            "gate",
            "area",
            &["scheduler"],
        ));
        assert_eq!(
            plan(tasks).validate().unwrap_err(),
            PlanError::WhenOnReviewer {
                task: "repro".into(),
                target: "author".into()
            }
        );
    }

    #[test]
    fn a_reviewer_must_depend_on_what_it_revises() {
        assert_eq!(
            refused(vec![
                agent("author", &[]),
                reviewer("repro", &[], "author", 3)
            ]),
            PlanError::ReviseTargetNotADependency {
                task: "repro".into(),
                target: "author".into()
            }
        );
    }

    #[test]
    fn max_rounds_is_bounded_on_both_sides() {
        for got in [0, 1, MAX_ROUNDS_CEILING + 1] {
            assert_eq!(
                refused(vec![
                    agent("author", &[]),
                    reviewer("repro", &["author"], "author", got)
                ]),
                PlanError::RoundsOutOfRange {
                    task: "repro".into(),
                    got
                }
            );
        }
        for ok in [2, MAX_ROUNDS_CEILING] {
            assert!(
                plan(vec![
                    agent("author", &[]),
                    reviewer("repro", &["author"], "author", ok)
                ])
                .validate()
                .is_ok()
            );
        }
    }

    #[test]
    fn only_agent_command_and_evaluate_tasks_take_part_in_a_revise_loop() {
        let mut reducer = top_k("pick", &["author"]);
        reducer.revise = Some(Revise {
            task: "author".into(),
            max_rounds: 2,
        });
        assert_eq!(
            refused(vec![emitting("author", &[], &["score"]), reducer]),
            PlanError::ReviseOnUnsupportedTask {
                task: "pick".into(),
                target: "author".into(),
                kind: "top_k"
            }
        );
        assert_eq!(
            refused(vec![
                emitting("author", &[], &["score"]),
                top_k("fold", &["author"]),
                reviewer("repro", &["fold"], "fold", 2),
            ]),
            PlanError::ReviseOnUnsupportedTask {
                task: "repro".into(),
                target: "fold".into(),
                kind: "top_k"
            }
        );
    }

    #[test]
    fn a_revise_loop_does_not_run_over_a_fanout() {
        let targets = OutputRef {
            task: "discover".into(),
            field: OutputField("targets".into()),
        };
        let mut mapped = agent("audit", &["discover"]);
        mapped.over = Some(targets);
        mapped.max_fanout = Some(4);
        assert_eq!(
            refused(vec![
                emitting("discover", &[], &["targets"]),
                mapped,
                reviewer("repro", &["audit"], "audit", 2),
            ]),
            PlanError::ReviseWithFanout {
                task: "repro".into(),
                target: "audit".into()
            }
        );
    }

    #[test]
    fn a_task_has_at_most_one_reviewer_and_loops_do_not_chain() {
        assert_eq!(
            refused(vec![
                agent("author", &[]),
                reviewer("left", &["author"], "author", 2),
                reviewer("right", &["author"], "author", 2),
            ]),
            PlanError::ReviseTargetRevisedTwice {
                left: "left".into(),
                right: "right".into(),
                target: "author".into()
            }
        );
        assert_eq!(
            refused(vec![
                agent("author", &[]),
                reviewer("repro", &["author"], "author", 2),
                reviewer("confirm", &["repro"], "repro", 2),
            ]),
            PlanError::NestedRevise {
                task: "confirm".into(),
                target: "repro".into()
            }
        );
    }

    #[test]
    fn a_reviewer_cannot_read_the_draft_through_another_dependency() {
        assert_eq!(
            refused(vec![
                agent("author", &[]),
                agent("build", &["author"]),
                reviewer("repro", &["author", "build"], "author", 2),
            ]),
            PlanError::ReviseAroundADependent {
                task: "repro".into(),
                target: "author".into(),
                dependency: "build".into()
            }
        );
    }

    #[test]
    fn revision_is_a_reserved_input_name() {
        assert_eq!(
            refused(vec![agent("revision", &[]), agent("author", &["revision"])]),
            PlanError::ReservedDependencyName {
                task: "author".into(),
                dependency: "revision".into()
            }
        );
    }
}
