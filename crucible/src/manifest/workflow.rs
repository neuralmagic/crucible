//! Pack-authored graphs. Workflow invariants and orchestrator capabilities are checked separately.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::plan::ir::{
    EngineOp, Join, Plan, PlanBudget, PlanError, Stage, Task, TaskKind, TaskName,
};

/// Reserved epilogue input key: `loop_graph` injects the kept candidate's
/// context into every epilogue task's inputs under this name.
pub const KEPT_INPUT: &str = "kept";

/// Names used only by the compatibility template.
const LEGACY_NAMES: [&str; 4] = ["propose", "apply", "measure", "decide"];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowType {
    /// The candidate/apply/measure/decision protocol.
    #[default]
    Autoresearch,
    /// An arbitrary capability-admitted DAG.
    Custom,
    /// One pass over the graph, no score, no judge.
    Playbook,
}

impl WorkflowType {
    /// The wire spelling, parsed. `None` for anything else, so a caller can leave the real
    /// diagnostic to whoever owns it.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "autoresearch" => Some(WorkflowType::Autoresearch),
            "custom" => Some(WorkflowType::Custom),
            "playbook" => Some(WorkflowType::Playbook),
            _ => None,
        }
    }

    pub fn capability(self) -> &'static str {
        match self {
            WorkflowType::Autoresearch => "workflow.autoresearch",
            WorkflowType::Custom => "workflow.custom",
            WorkflowType::Playbook => "workflow.playbook",
        }
    }
}

impl EngineOp {
    pub fn capability(self) -> &'static str {
        match self {
            EngineOp::Propose => "engine.propose",
            EngineOp::Apply => "engine.apply",
            EngineOp::Measure => "engine.measure",
            EngineOp::Grade => "engine.grade",
            EngineOp::Decide => "engine.decide",
            EngineOp::MeasureDiff => "engine.measure_diff",
        }
    }
}

/// Capabilities advertised by an engine or outer orchestrator at admission time.
#[derive(Debug, Clone, Default)]
pub struct WorkflowCaps {
    names: BTreeSet<String>,
}

impl WorkflowCaps {
    pub fn new(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            names: names.into_iter().map(Into::into).collect(),
        }
    }

    /// Capabilities implemented by Crucible's repeating autoresearch loop.
    pub fn autoresearch_engine() -> Self {
        Self::new([
            "workflow.autoresearch",
            "engine.propose",
            "engine.apply",
            "engine.measure",
            "engine.grade",
            "engine.decide",
        ])
    }

    /// Capabilities the one-pass playbook lane implements. No engine operations: a playbook has
    /// no scored loop to run them in, and [`WorkflowCfg::validate`] refuses a task naming one.
    pub fn playbook_engine() -> Self {
        Self::new(["workflow.playbook"])
    }

    /// What the engine implements for the lane a workflow declares. Granting the scored loop's
    /// capabilities to every lane would admit a playbook whose type the engine never checked,
    /// and refuse one whose type it did.
    pub fn for_lane(lane: WorkflowType) -> Self {
        match lane {
            WorkflowType::Playbook => Self::playbook_engine(),
            WorkflowType::Autoresearch | WorkflowType::Custom => Self::autoresearch_engine(),
        }
    }

    pub fn with_persistent_sessions(mut self) -> Self {
        self.names.insert("agent.session.persist".to_string());
        self
    }

    fn require(&self, capability: &'static str) -> Result<(), WorkflowError> {
        if self.names.contains(capability) {
            Ok(())
        } else {
            Err(WorkflowError::MissingCapability { capability })
        }
    }
}

/// Everything [`WorkflowCfg::validate`] and [`WorkflowCfg::admit`] can reject. Structural
/// graph errors come through [`PlanError`]; the rest are workflow-shape and capability rules.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum WorkflowError {
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("workflow requires unavailable orchestrator capability {capability:?}")]
    MissingCapability { capability: &'static str },
    #[error("engine task {task:?} must be required")]
    EngineTaskOptional { task: String },
    #[error("engine task {task:?} cannot run in an isolated worktree")]
    EngineTaskIsolated { task: String },
    #[error("engine task {task:?} must use join = \"all\"")]
    EngineTaskJoin { task: String },
    #[error("engine {op:?} task {task:?} requires source")]
    EngineSourceRequired { op: EngineOp, task: String },
    #[error("engine {op:?} task {task:?} does not accept source")]
    EngineSourceUnexpected { op: EngineOp, task: String },
    #[error("engine task {task:?} names unknown source {source_task:?}")]
    UnknownEngineSource { task: String, source_task: String },
    #[error("engine task source {source_task:?} must be an ancestor of {task:?}")]
    EngineSourceNotAncestor { source_task: String, task: String },
    #[error(
        "engine decide tasks {first:?} and {second:?} share measurement source {source_task:?}; a \
         measurement can be graded once"
    )]
    SharedDecideSource {
        first: String,
        second: String,
        source_task: String,
    },
    #[error("engine grade source {source_task:?} must be an evaluate task")]
    GradeSourceNotEvaluate { source_task: String },
    #[error("engine grade score source {source_task:?} must be required")]
    GradeSourceOptional { source_task: String },
    #[error("engine {op:?} task {task:?} does not accept tiebreak")]
    TiebreakUnexpected { op: EngineOp, task: String },
    #[error("engine grade task {task:?} names unknown tiebreak {tiebreak:?}")]
    UnknownTiebreak { task: String, tiebreak: String },
    #[error("engine grade tiebreak {tiebreak:?} must be an evaluate task")]
    TiebreakNotEvaluate { tiebreak: String },
    #[error("engine grade tiebreak {tiebreak:?} must be an ancestor of {task:?}")]
    TiebreakNotAncestor { tiebreak: String, task: String },
    #[error("custom workflow result {result:?} names an unknown task")]
    UnknownCustomResult { result: String },
    #[error("playbook task {task:?} names engine operation {op:?}: a playbook has no scored loop")]
    PlaybookEngineTask { task: String, op: EngineOp },
    #[error("engine task {task:?} cannot run in the epilogue (the loop is over)")]
    EngineTaskInEpilogue { task: String },
    #[error("report task {task:?} must run in the epilogue")]
    ReportOutsideEpilogue { task: String },
    #[error("epilogue task name {KEPT_INPUT:?} is reserved for the kept-candidate input")]
    ReservedEpilogueName,
    #[error(
        "task {task:?} (stage {stage:?}) depends on {dependency:?} (stage {dependency_stage:?}); \
         dependencies cannot cross stages"
    )]
    CrossStageDependency {
        task: String,
        stage: Stage,
        dependency: String,
        dependency_stage: Stage,
    },
    #[error("workflow result {result:?} cannot be an epilogue task")]
    EpilogueResult { result: String },
    #[error("[[workflow.task]] has an empty name")]
    LegacyEmptyName,
    #[error("legacy [[workflow.task]] name {name:?} collides with its compatibility template")]
    LegacyReservedName { name: String },
    #[error("duplicate [[workflow.task]] name {name:?}")]
    LegacyDuplicateName { name: String },
    #[error("legacy [[workflow.task]] {task:?} depends on unknown task {dependency:?}")]
    LegacyUnknownDependency { task: String, dependency: String },
    #[error("fully-authored autoresearch workflow requires result")]
    AutoresearchMissingResult,
    #[error("autoresearch result {result:?} names an unknown task")]
    AutoresearchUnknownResult { result: String },
    #[error("autoresearch result {result:?} must be an engine decide task with source")]
    AutoresearchResultNotDecide { result: String },
    #[error("autoresearch decision {result:?} names unknown measurement source {measurement:?}")]
    AutoresearchUnknownMeasurement { result: String, measurement: String },
    #[error("autoresearch decision source {measurement:?} must be an engine measure or grade task")]
    AutoresearchMeasurementNotEngine { measurement: String },
    #[error("measurement {measurement:?} must be an ancestor of decision {result:?}")]
    MeasurementNotAncestor { measurement: String, result: String },
    #[error("autoresearch measurement {measurement:?} requires an engine apply ancestor")]
    AutoresearchNoApply { measurement: String },
    #[error("autoresearch apply path requires an engine propose ancestor")]
    AutoresearchNoPropose,
    #[error("[workflow] declares both file = {file:?} and inline tasks; it is one or the other")]
    FileAndTasks { file: String },
    #[error("[workflow].file is empty")]
    FileEmpty,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCfg {
    /// Invariants enforced by the admitting orchestrator.
    #[serde(rename = "type", default)]
    pub workflow_type: WorkflowType,
    /// Result task; absent selects the compatibility splice format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<TaskName>,
    /// A workflow source to compile, relative to the manifest directory, instead of inline
    /// tasks. A parameterised graph is a function of its launch arguments, so it cannot be a
    /// committed artifact and must be compiled per run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Set when [`file`] has been compiled into `tasks`, and never read from or written to
    /// TOML. The graph is then absent from the manifest text, so whatever hashes a run has to
    /// hash the graph itself or the hash stops discriminating between two different graphs.
    #[serde(skip)]
    pub resolved_from: Option<String>,
    #[serde(rename = "task", default)]
    pub tasks: Vec<Task>,
}

impl WorkflowCfg {
    /// True while the graph is still a path rather than tasks. The manifest is valid in this
    /// state and the graph is validated once [`WorkflowCfg::file`] has been compiled into it.
    pub fn is_unresolved(&self) -> bool {
        self.file.is_some() && self.tasks.is_empty()
    }

    /// True once a named source has been compiled into tasks.
    pub fn resolved_from_file(&self) -> bool {
        self.resolved_from.is_some()
    }

    /// True when the graph carries a task that runs outside the sandbox. An unresolved graph
    /// counts as carrying one: what a source compiles to is not known until it compiles, and
    /// under-disclosing reach is the worse error.
    pub fn runs_host_commands(&self) -> bool {
        self.is_unresolved()
            || self
                .tasks
                .iter()
                .any(|t| matches!(t.task, TaskKind::Command { .. } | TaskKind::Evaluate { .. }))
    }

    /// Validate structure and type-specific invariants, without granting authority.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if let Some(file) = &self.file {
            if file.is_empty() {
                return Err(WorkflowError::FileEmpty);
            }
            if !self.tasks.is_empty() {
                return Err(WorkflowError::FileAndTasks { file: file.clone() });
            }
            // Nothing to check until it is compiled; the compiled graph validates then.
            return Ok(());
        }

        if self.is_legacy_splice() {
            self.validate_stages()?;
            return self.validate_legacy_splice();
        }

        let plan = Plan {
            version: 1,
            reason: None,
            budget: PlanBudget { usd: f64::MAX },
            tasks: self.tasks.clone(),
        };
        plan.validate()?;
        self.validate_stages()?;

        let tasks: BTreeMap<&TaskName, &Task> =
            self.tasks.iter().map(|task| (&task.name, task)).collect();
        // A measurement source may feed only one decision.
        let mut decided_sources: BTreeMap<&TaskName, &TaskName> = BTreeMap::new();
        for task in &self.tasks {
            if matches!(task.task, TaskKind::Report { .. }) && task.stage != Stage::Epilogue {
                return Err(WorkflowError::ReportOutsideEpilogue {
                    task: task.name.0.clone(),
                });
            }
            if let TaskKind::Engine {
                op,
                source,
                tiebreak,
            } = &task.task
            {
                let name = || task.name.0.clone();
                if !task.required {
                    return Err(WorkflowError::EngineTaskOptional { task: name() });
                }
                if task.isolation.is_some() {
                    return Err(WorkflowError::EngineTaskIsolated { task: name() });
                }
                if task.join != Join::All && *op != EngineOp::Grade {
                    return Err(WorkflowError::EngineTaskJoin { task: name() });
                }
                match (op, source) {
                    (EngineOp::Decide | EngineOp::Grade, None) => {
                        return Err(WorkflowError::EngineSourceRequired {
                            op: *op,
                            task: name(),
                        });
                    }
                    (EngineOp::Decide | EngineOp::Grade, Some(_)) | (_, None) => {}
                    (_, Some(_)) => {
                        return Err(WorkflowError::EngineSourceUnexpected {
                            op: *op,
                            task: name(),
                        });
                    }
                }
                if let Some(source) = source {
                    let Some(source_task) = tasks.get(source) else {
                        return Err(WorkflowError::UnknownEngineSource {
                            task: name(),
                            source_task: source.0.clone(),
                        });
                    };
                    if !is_ancestor(&tasks, source, &task.name) {
                        return Err(WorkflowError::EngineSourceNotAncestor {
                            source_task: source.0.clone(),
                            task: name(),
                        });
                    }
                    if *op == EngineOp::Decide
                        && let Some(first) = decided_sources.insert(source, &task.name)
                    {
                        return Err(WorkflowError::SharedDecideSource {
                            first: first.0.clone(),
                            second: name(),
                            source_task: source.0.clone(),
                        });
                    }
                    if *op == EngineOp::Grade
                        && !matches!(source_task.task, TaskKind::Evaluate { .. })
                    {
                        return Err(WorkflowError::GradeSourceNotEvaluate {
                            source_task: source.0.clone(),
                        });
                    }
                    if *op == EngineOp::Grade && !source_task.required {
                        return Err(WorkflowError::GradeSourceOptional {
                            source_task: source.0.clone(),
                        });
                    }
                }
                if let Some(tiebreak) = tiebreak {
                    if *op != EngineOp::Grade {
                        return Err(WorkflowError::TiebreakUnexpected {
                            op: *op,
                            task: name(),
                        });
                    }
                    let Some(tiebreak_task) = tasks.get(tiebreak) else {
                        return Err(WorkflowError::UnknownTiebreak {
                            task: name(),
                            tiebreak: tiebreak.0.clone(),
                        });
                    };
                    if !matches!(tiebreak_task.task, TaskKind::Evaluate { .. }) {
                        return Err(WorkflowError::TiebreakNotEvaluate {
                            tiebreak: tiebreak.0.clone(),
                        });
                    }
                    // Unlike the score source, the tiebreak may be advisory: a rung that
                    // failed or never ran just leaves the reading without a secondary.
                    if !is_ancestor(&tasks, tiebreak, &task.name) {
                        return Err(WorkflowError::TiebreakNotAncestor {
                            tiebreak: tiebreak.0.clone(),
                            task: name(),
                        });
                    }
                }
            }
        }

        if self.workflow_type == WorkflowType::Playbook {
            for task in &self.tasks {
                if let TaskKind::Engine { op, .. } = task.task {
                    return Err(WorkflowError::PlaybookEngineTask {
                        task: task.name.0.clone(),
                        op,
                    });
                }
            }
        }

        if self.workflow_type == WorkflowType::Autoresearch {
            self.validate_autoresearch()?;
        } else if let Some(result) = &self.result
            && !self.tasks.iter().any(|task| &task.name == result)
        {
            return Err(WorkflowError::UnknownCustomResult {
                result: result.0.clone(),
            });
        }
        Ok(())
    }

    /// Require orchestrator authority for the workflow and its engine operations.
    pub fn admit(&self, caps: &WorkflowCaps) -> Result<(), WorkflowError> {
        self.validate()?;
        caps.require(self.workflow_type.capability())?;
        for task in &self.tasks {
            if let TaskKind::Engine { op, .. } = task.task {
                caps.require(op.capability())?;
            }
            if task.session.is_some() {
                caps.require("agent.session.persist")?;
            }
        }
        Ok(())
    }

    pub fn is_legacy_splice(&self) -> bool {
        self.workflow_type == WorkflowType::Autoresearch
            && self.result.is_none()
            && self
                .tasks
                .iter()
                .all(|task| !matches!(task.task, TaskKind::Engine { .. }))
    }

    /// Stage rules, shared by both authoring forms. The two stages compile into separate
    /// plans (per-iteration vs. once post-loop), so a dependency cannot cross them, the
    /// loop's engine ops have no post-run meaning, and the decision task must iterate.
    fn validate_stages(&self) -> Result<(), WorkflowError> {
        let stages: BTreeMap<&TaskName, Stage> = self
            .tasks
            .iter()
            .map(|task| (&task.name, task.stage))
            .collect();
        for task in &self.tasks {
            if task.stage == Stage::Epilogue {
                if matches!(task.task, TaskKind::Engine { .. }) {
                    return Err(WorkflowError::EngineTaskInEpilogue {
                        task: task.name.0.clone(),
                    });
                }
                if task.name.0 == KEPT_INPUT {
                    return Err(WorkflowError::ReservedEpilogueName);
                }
            }
            for dependency in &task.depends_on {
                // Unknown names default to Iteration here; the legacy splice's implicit
                // "propose" is an iteration task, and the authored path rejects truly
                // unknown dependencies in its plan validation.
                let dependency_stage = stages.get(dependency).copied().unwrap_or_default();
                if dependency_stage != task.stage {
                    return Err(WorkflowError::CrossStageDependency {
                        task: task.name.0.clone(),
                        stage: task.stage,
                        dependency: dependency.0.clone(),
                        dependency_stage,
                    });
                }
            }
        }
        if let Some(result) = &self.result
            && stages.get(result).copied().unwrap_or_default() == Stage::Epilogue
        {
            return Err(WorkflowError::EpilogueResult {
                result: result.0.clone(),
            });
        }
        Ok(())
    }

    fn validate_legacy_splice(&self) -> Result<(), WorkflowError> {
        let mut seen = BTreeSet::new();
        for task in &self.tasks {
            let name = task.name.0.as_str();
            if name.trim().is_empty() {
                return Err(WorkflowError::LegacyEmptyName);
            }
            if LEGACY_NAMES.contains(&name) {
                return Err(WorkflowError::LegacyReservedName {
                    name: name.to_owned(),
                });
            }
            if !seen.insert(name) {
                return Err(WorkflowError::LegacyDuplicateName {
                    name: name.to_owned(),
                });
            }
        }
        for task in &self.tasks {
            for dependency in &task.depends_on {
                let name = dependency.0.as_str();
                if name != "propose" && !seen.contains(name) {
                    return Err(WorkflowError::LegacyUnknownDependency {
                        task: task.name.0.clone(),
                        dependency: name.to_owned(),
                    });
                }
            }
        }
        Ok(())
    }

    fn validate_autoresearch(&self) -> Result<(), WorkflowError> {
        let result = self
            .result
            .as_ref()
            .ok_or(WorkflowError::AutoresearchMissingResult)?;
        let tasks: BTreeMap<&TaskName, &Task> =
            self.tasks.iter().map(|task| (&task.name, task)).collect();
        let decision =
            tasks
                .get(result)
                .ok_or_else(|| WorkflowError::AutoresearchUnknownResult {
                    result: result.0.clone(),
                })?;
        let TaskKind::Engine {
            op: EngineOp::Decide,
            source: Some(measurement),
            ..
        } = &decision.task
        else {
            return Err(WorkflowError::AutoresearchResultNotDecide {
                result: result.0.clone(),
            });
        };
        let measured = tasks.get(measurement).ok_or_else(|| {
            WorkflowError::AutoresearchUnknownMeasurement {
                result: result.0.clone(),
                measurement: measurement.0.clone(),
            }
        })?;
        if !matches!(
            measured.task,
            TaskKind::Engine {
                op: EngineOp::Measure | EngineOp::Grade,
                ..
            }
        ) {
            return Err(WorkflowError::AutoresearchMeasurementNotEngine {
                measurement: measurement.0.clone(),
            });
        }
        if !is_ancestor(&tasks, measurement, result) {
            return Err(WorkflowError::MeasurementNotAncestor {
                measurement: measurement.0.clone(),
                result: result.0.clone(),
            });
        }

        let applies: Vec<&TaskName> = self
            .tasks
            .iter()
            .filter(|task| {
                matches!(
                    task.task,
                    TaskKind::Engine {
                        op: EngineOp::Apply,
                        ..
                    }
                ) && is_ancestor(&tasks, &task.name, measurement)
            })
            .map(|task| &task.name)
            .collect();
        if applies.is_empty() {
            return Err(WorkflowError::AutoresearchNoApply {
                measurement: measurement.0.clone(),
            });
        }
        let has_proposal = self.tasks.iter().any(|task| {
            matches!(
                task.task,
                TaskKind::Engine {
                    op: EngineOp::Propose,
                    ..
                }
            ) && applies
                .iter()
                .any(|apply| is_ancestor(&tasks, &task.name, apply))
        });
        if !has_proposal {
            return Err(WorkflowError::AutoresearchNoPropose);
        }
        Ok(())
    }

    /// Terminal tasks used by the compatibility splice adapter. Epilogue tasks never
    /// splice, so they are neither sinks nor dependents here.
    pub fn sinks(&self) -> Vec<TaskName> {
        let depended: BTreeSet<&TaskName> = self
            .tasks
            .iter()
            .flat_map(|task| &task.depends_on)
            .collect();
        self.tasks
            .iter()
            .filter(|task| task.stage == Stage::Iteration && !depended.contains(&task.name))
            .map(|task| task.name.clone())
            .collect()
    }

    /// The per-iteration subset: everything not marked `stage = "epilogue"`.
    pub fn iteration_tasks(&self) -> Vec<Task> {
        self.tasks
            .iter()
            .filter(|task| task.stage == Stage::Iteration)
            .cloned()
            .collect()
    }

    /// Run-scoped tasks that fire once post-loop against the final kept candidate.
    pub fn epilogue_tasks(&self) -> Vec<Task> {
        self.tasks
            .iter()
            .filter(|task| task.stage == Stage::Epilogue)
            .cloned()
            .collect()
    }

    pub fn has_epilogue(&self) -> bool {
        self.tasks.iter().any(|task| task.stage == Stage::Epilogue)
    }
}

/// Iterative to remain linear across diamond-shaped DAGs.
fn is_ancestor(tasks: &BTreeMap<&TaskName, &Task>, ancestor: &TaskName, node: &TaskName) -> bool {
    let mut stack = vec![node];
    let mut seen = BTreeSet::new();
    while let Some(current) = stack.pop() {
        if !seen.insert(current) {
            continue;
        }
        let Some(task) = tasks.get(current) else {
            continue;
        };
        for dependency in &task.depends_on {
            if dependency == ancestor {
                return true;
            }
            stack.push(dependency);
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> WorkflowCfg {
        toml::from_str(source).expect("parse workflow")
    }

    #[test]
    fn legacy_splice_remains_valid() {
        let workflow = parse(
            "[[task]]\nname = \"review\"\nkind = \"command\"\ncommand = \"true\"\ndepends_on = [\"propose\"]\n",
        );
        workflow.validate().unwrap();
        assert!(workflow.is_legacy_splice());
    }

    #[test]
    fn a_required_gate_joining_all_on_an_advisory_reviewer_is_rejected() {
        let workflow = parse(
            "type = \"custom\"\nresult = \"gate\"\n\
             [[task]]\nname = \"implement\"\nkind = \"command\"\ncommand = \"true\"\n\
             [[task]]\nname = \"review-copy\"\nkind = \"agent\"\nprompt = \"copy\"\ndepends_on = [\"implement\"]\nrequired = false\n\
             [[task]]\nname = \"gate\"\nkind = \"command\"\ncommand = \"./join_gate.sh\"\ndepends_on = [\"review-copy\"]\n",
        );
        assert_eq!(
            workflow.validate().unwrap_err(),
            WorkflowError::Plan(PlanError::AdvisoryGatesRequired {
                task: "gate".to_owned(),
                dependency: "review-copy".to_owned(),
            })
        );

        let folded = parse(
            "type = \"custom\"\nresult = \"gate\"\n\
             [[task]]\nname = \"implement\"\nkind = \"command\"\ncommand = \"true\"\n\
             [[task]]\nname = \"review-copy\"\nkind = \"agent\"\nprompt = \"copy\"\ndepends_on = [\"implement\"]\nrequired = false\n\
             [[task]]\nname = \"gate\"\nkind = \"command\"\ncommand = \"./join_gate.sh\"\ndepends_on = [\"review-copy\"]\njoin = \"passed\"\n",
        );
        folded.validate().unwrap();
    }

    /// The shipped panel is the shape the rule must not fire on: an advisory copy editor
    /// reaching a required gate through `join = "passed"`.
    #[test]
    fn the_shipped_adversarial_review_panel_still_validates() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crate dir has a parent")
            .join("examples/adversarial-review/crucible.toml");
        let workflow = crate::manifest::Manifest::load(&manifest)
            .unwrap()
            .workflow
            .expect("the panel manifest declares a workflow");
        workflow.validate().unwrap();
        let task = |name: &str| {
            workflow
                .tasks
                .iter()
                .find(|t| t.name.0 == name)
                .unwrap_or_else(|| panic!("{name} is missing from the panel"))
        };
        assert!(!task("review-copy").required);
        assert!(task("gate").required);
        assert_eq!(task("gate").join, Join::Passed);
    }

    #[test]
    fn custom_graph_needs_no_autoresearch_shape() {
        let workflow = parse(
            "type = \"custom\"\nresult = \"publish\"\n[[task]]\nname = \"publish\"\nkind = \"command\"\ncommand = \"true\"\n",
        );
        workflow.validate().unwrap();
        workflow
            .admit(&WorkflowCaps::new(["workflow.custom"]))
            .unwrap();
    }

    #[test]
    fn admission_checks_type_and_operation_caps() {
        let workflow = full_autoresearch();
        assert_eq!(
            workflow
                .admit(&WorkflowCaps::new(["workflow.autoresearch"]))
                .unwrap_err(),
            WorkflowError::MissingCapability {
                capability: "engine.propose"
            }
        );
        workflow
            .admit(&WorkflowCaps::autoresearch_engine())
            .unwrap();
    }

    #[test]
    fn persistent_session_is_separately_capability_gated() {
        let workflow = parse(
            "type = \"custom\"\nresult = \"solve\"\n\
             [[task]]\nname = \"solve\"\nkind = \"agent\"\nprompt = \"go\"\nsession = \"solver\"\n",
        );
        assert_eq!(
            workflow
                .admit(&WorkflowCaps::new(["workflow.custom"]))
                .unwrap_err(),
            WorkflowError::MissingCapability {
                capability: "agent.session.persist"
            }
        );
        workflow
            .admit(&WorkflowCaps::new([
                "workflow.custom",
                "agent.session.persist",
            ]))
            .unwrap();
    }

    #[test]
    fn autoresearch_shape_is_semantic_not_name_based() {
        full_autoresearch().validate().unwrap();
    }

    /// A second decision on one measurement finds nothing to grade, mid-run.
    #[test]
    fn two_decisions_may_not_share_one_measurement() {
        let workflow = parse(
            "type = \"autoresearch\"\nresult = \"choose\"\n\
             [[task]]\nname = \"invent\"\nkind = \"engine\"\nop = \"propose\"\n\
             [[task]]\nname = \"deploy\"\nkind = \"engine\"\nop = \"apply\"\ndepends_on = [\"invent\"]\n\
             [[task]]\nname = \"score\"\nkind = \"engine\"\nop = \"measure\"\ndepends_on = [\"deploy\"]\n\
             [[task]]\nname = \"choose\"\nkind = \"engine\"\nop = \"decide\"\nsource = \"score\"\ndepends_on = [\"score\"]\n\
             [[task]]\nname = \"second-guess\"\nkind = \"engine\"\nop = \"decide\"\nsource = \"score\"\ndepends_on = [\"choose\"]\n",
        );
        assert_eq!(
            workflow.validate().unwrap_err(),
            WorkflowError::SharedDecideSource {
                first: "choose".to_owned(),
                second: "second-guess".to_owned(),
                source_task: "score".to_owned(),
            }
        );
    }

    /// N diamonds means 2^N paths. `off-path` forces an ancestry question whose answer is
    /// false, since `.any()` short-circuits on a true one and never fans out.
    #[test]
    fn ancestry_does_not_blow_up_on_a_diamond_chain() {
        const DIAMONDS: usize = 26;
        let mut source = String::from("type = \"autoresearch\"\nresult = \"choose\"\n");
        source.push_str("[[task]]\nname = \"invent\"\nkind = \"engine\"\nop = \"propose\"\n");
        let mut previous = "invent".to_string();
        for i in 0..DIAMONDS {
            for side in ["l", "r"] {
                source.push_str(&format!(
                    "[[task]]\nname = \"{side}{i}\"\nkind = \"command\"\ncommand = \"true\"\ndepends_on = [\"{previous}\"]\n"
                ));
            }
            previous = format!("join{i}");
            source.push_str(&format!(
                "[[task]]\nname = \"{previous}\"\nkind = \"command\"\ncommand = \"true\"\ndepends_on = [\"l{i}\", \"r{i}\"]\n"
            ));
        }
        source.push_str(&format!(
            "[[task]]\nname = \"deploy\"\nkind = \"engine\"\nop = \"apply\"\ndepends_on = [\"{previous}\"]\n"
        ));
        source.push_str("[[task]]\nname = \"score\"\nkind = \"engine\"\nop = \"measure\"\ndepends_on = [\"deploy\"]\n");
        source.push_str("[[task]]\nname = \"choose\"\nkind = \"engine\"\nop = \"decide\"\nsource = \"score\"\ndepends_on = [\"score\"]\n");
        source.push_str(
            "[[task]]\nname = \"off-path\"\nkind = \"engine\"\nop = \"apply\"\ndepends_on = [\"invent\"]\n",
        );
        let workflow = parse(&source);

        let started = std::time::Instant::now();
        workflow.validate().unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "ancestry walk took {elapsed:?}; it is re-exploring paths instead of visited nodes"
        );
    }

    #[test]
    fn autoresearch_accepts_a_typed_measurement_subgraph() {
        let workflow = parse(
            "type = \"autoresearch\"\nresult = \"choose\"\n\
             [[task]]\nname = \"invent\"\nkind = \"engine\"\nop = \"propose\"\n\
             [[task]]\nname = \"deploy\"\nkind = \"engine\"\nop = \"apply\"\ndepends_on = [\"invent\"]\n\
             [[task]]\nname = \"correctness\"\nkind = \"evaluate\"\ncommand = \"./correctness.sh\"\ndepends_on = [\"deploy\"]\nisolation = \"worktree\"\n\
             [[task]]\nname = \"latency\"\nkind = \"evaluate\"\ncommand = \"./latency.sh\"\ndepends_on = [\"correctness\"]\nisolation = \"worktree\"\nthreshold = 10.0\ndirection = \"lower\"\n\
             [[task]]\nname = \"grade\"\nkind = \"engine\"\nop = \"grade\"\nsource = \"latency\"\ndepends_on = [\"correctness\", \"latency\"]\njoin = \"passed\"\n\
             [[task]]\nname = \"choose\"\nkind = \"engine\"\nop = \"decide\"\nsource = \"grade\"\ndepends_on = [\"grade\"]\n",
        );
        workflow.validate().unwrap();
        workflow
            .admit(&WorkflowCaps::autoresearch_engine())
            .unwrap();
    }

    #[test]
    fn grade_accepts_an_advisory_tiebreak_evaluate_source() {
        let workflow = parse(
            "type = \"autoresearch\"\nresult = \"choose\"\n\
             [[task]]\nname = \"invent\"\nkind = \"engine\"\nop = \"propose\"\n\
             [[task]]\nname = \"deploy\"\nkind = \"engine\"\nop = \"apply\"\ndepends_on = [\"invent\"]\n\
             [[task]]\nname = \"correctness\"\nkind = \"evaluate\"\ncommand = \"./correctness.sh\"\ndepends_on = [\"deploy\"]\nisolation = \"worktree\"\n\
             [[task]]\nname = \"latency\"\nkind = \"evaluate\"\ncommand = \"./latency.sh\"\ndepends_on = [\"correctness\"]\nisolation = \"worktree\"\nrequired = false\n\
             [[task]]\nname = \"grade\"\nkind = \"engine\"\nop = \"grade\"\nsource = \"correctness\"\ntiebreak = \"latency\"\ndepends_on = [\"correctness\", \"latency\"]\njoin = \"passed\"\n\
             [[task]]\nname = \"choose\"\nkind = \"engine\"\nop = \"decide\"\nsource = \"grade\"\ndepends_on = [\"grade\"]\n",
        );
        workflow.validate().unwrap();
    }

    #[test]
    fn tiebreak_is_rejected_outside_grade() {
        let mut workflow = full_autoresearch();
        // tiebreak on a decide task is rejected.
        workflow.tasks[3].task = TaskKind::Engine {
            op: EngineOp::Decide,
            source: Some("score".into()),
            tiebreak: Some("score".into()),
        };
        assert_eq!(
            workflow.validate().unwrap_err(),
            WorkflowError::TiebreakUnexpected {
                op: EngineOp::Decide,
                task: "choose".to_owned(),
            }
        );

        // A tiebreak naming a non-evaluate task is rejected.
        let mut workflow = full_autoresearch();
        workflow.tasks[2].task = TaskKind::Engine {
            op: EngineOp::Measure,
            source: None,
            tiebreak: Some("deploy".into()),
        };
        assert_eq!(
            workflow.validate().unwrap_err(),
            WorkflowError::TiebreakUnexpected {
                op: EngineOp::Measure,
                task: "score".to_owned(),
            }
        );
    }

    #[test]
    fn grade_tiebreak_must_name_a_known_evaluate_task() {
        let base = "type = \"autoresearch\"\nresult = \"choose\"\n\
             [[task]]\nname = \"invent\"\nkind = \"engine\"\nop = \"propose\"\n\
             [[task]]\nname = \"deploy\"\nkind = \"engine\"\nop = \"apply\"\ndepends_on = [\"invent\"]\n\
             [[task]]\nname = \"correctness\"\nkind = \"evaluate\"\ncommand = \"./correctness.sh\"\ndepends_on = [\"deploy\"]\nisolation = \"worktree\"\n\
             [[task]]\nname = \"choose\"\nkind = \"engine\"\nop = \"decide\"\nsource = \"grade\"\ndepends_on = [\"grade\"]\n";
        let unknown = format!(
            "{base}[[task]]\nname = \"grade\"\nkind = \"engine\"\nop = \"grade\"\nsource = \"correctness\"\ntiebreak = \"ghost\"\ndepends_on = [\"correctness\"]\njoin = \"passed\"\n"
        );
        assert_eq!(
            parse(&unknown).validate().unwrap_err(),
            WorkflowError::UnknownTiebreak {
                task: "grade".to_owned(),
                tiebreak: "ghost".to_owned(),
            }
        );

        let not_evaluate = format!(
            "{base}[[task]]\nname = \"grade\"\nkind = \"engine\"\nop = \"grade\"\nsource = \"correctness\"\ntiebreak = \"deploy\"\ndepends_on = [\"correctness\"]\njoin = \"passed\"\n"
        );
        assert_eq!(
            parse(&not_evaluate).validate().unwrap_err(),
            WorkflowError::TiebreakNotEvaluate {
                tiebreak: "deploy".to_owned()
            }
        );
    }

    #[test]
    fn grade_requires_an_evaluation_score_source() {
        let mut workflow = full_autoresearch();
        workflow.tasks[2].task = TaskKind::Engine {
            op: EngineOp::Grade,
            source: Some("deploy".into()),
            tiebreak: None,
        };
        assert_eq!(
            workflow.validate().unwrap_err(),
            WorkflowError::GradeSourceNotEvaluate {
                source_task: "deploy".to_owned()
            }
        );
    }

    #[test]
    fn epilogue_tasks_split_cleanly_from_the_iteration_graph() {
        let workflow = parse(
            "type = \"autoresearch\"\nresult = \"choose\"\n\
             [[task]]\nname = \"invent\"\nkind = \"engine\"\nop = \"propose\"\n\
             [[task]]\nname = \"deploy\"\nkind = \"engine\"\nop = \"apply\"\ndepends_on = [\"invent\"]\n\
             [[task]]\nname = \"score\"\nkind = \"engine\"\nop = \"measure\"\ndepends_on = [\"deploy\"]\n\
             [[task]]\nname = \"choose\"\nkind = \"engine\"\nop = \"decide\"\nsource = \"score\"\ndepends_on = [\"score\"]\n\
             [[task]]\nname = \"racecheck\"\nkind = \"command\"\ncommand = \"true\"\nstage = \"epilogue\"\nrequired = false\n",
        );
        workflow.validate().unwrap();
        assert!(workflow.has_epilogue());
        let iteration: Vec<String> = workflow
            .iteration_tasks()
            .into_iter()
            .map(|t| t.name.0)
            .collect();
        assert_eq!(iteration, ["invent", "deploy", "score", "choose"]);
        let epilogue: Vec<String> = workflow
            .epilogue_tasks()
            .into_iter()
            .map(|t| t.name.0)
            .collect();
        assert_eq!(epilogue, ["racecheck"]);
    }

    /// Legacy splice: an epilogue task is neither spliced nor a sink `apply` waits on.
    #[test]
    fn legacy_splice_sinks_exclude_epilogue_tasks() {
        let workflow = parse(
            "[[task]]\nname = \"review\"\nkind = \"command\"\ncommand = \"true\"\n\
             [[task]]\nname = \"racecheck\"\nkind = \"command\"\ncommand = \"true\"\nstage = \"epilogue\"\n",
        );
        workflow.validate().unwrap();
        assert!(workflow.is_legacy_splice());
        assert_eq!(workflow.sinks(), vec![TaskName::from("review")]);
    }

    #[test]
    fn dependencies_cannot_cross_stages() {
        // Epilogue depending on an iteration task: the iteration output is long gone.
        let workflow = parse(
            "[[task]]\nname = \"review\"\nkind = \"command\"\ncommand = \"true\"\n\
             [[task]]\nname = \"racecheck\"\nkind = \"command\"\ncommand = \"true\"\nstage = \"epilogue\"\ndepends_on = [\"review\"]\n",
        );
        assert_eq!(
            workflow.validate().unwrap_err(),
            WorkflowError::CrossStageDependency {
                task: "racecheck".to_owned(),
                stage: Stage::Epilogue,
                dependency: "review".to_owned(),
                dependency_stage: Stage::Iteration,
            }
        );

        // Iteration depending on an epilogue task would deadlock every iteration.
        let workflow = parse(
            "[[task]]\nname = \"racecheck\"\nkind = \"command\"\ncommand = \"true\"\nstage = \"epilogue\"\n\
             [[task]]\nname = \"review\"\nkind = \"command\"\ncommand = \"true\"\ndepends_on = [\"racecheck\"]\n",
        );
        assert_eq!(
            workflow.validate().unwrap_err(),
            WorkflowError::CrossStageDependency {
                task: "review".to_owned(),
                stage: Stage::Iteration,
                dependency: "racecheck".to_owned(),
                dependency_stage: Stage::Epilogue,
            }
        );

        // The legacy splice's implicit propose dependency is an iteration task too.
        let workflow = parse(
            "[[task]]\nname = \"racecheck\"\nkind = \"command\"\ncommand = \"true\"\nstage = \"epilogue\"\ndepends_on = [\"propose\"]\n",
        );
        assert_eq!(
            workflow.validate().unwrap_err(),
            WorkflowError::CrossStageDependency {
                task: "racecheck".to_owned(),
                stage: Stage::Epilogue,
                dependency: "propose".to_owned(),
                dependency_stage: Stage::Iteration,
            }
        );
    }

    #[test]
    fn engine_tasks_cannot_be_epilogue() {
        let mut workflow = full_autoresearch();
        workflow.tasks[2].stage = Stage::Epilogue;
        assert_eq!(
            workflow.validate().unwrap_err(),
            WorkflowError::EngineTaskInEpilogue {
                task: "score".to_owned()
            }
        );
    }

    #[test]
    fn epilogue_cannot_be_the_result_or_claim_the_kept_name() {
        let workflow = parse(
            "type = \"custom\"\nresult = \"racecheck\"\n\
             [[task]]\nname = \"solve\"\nkind = \"command\"\ncommand = \"true\"\n\
             [[task]]\nname = \"racecheck\"\nkind = \"command\"\ncommand = \"true\"\nstage = \"epilogue\"\n",
        );
        assert_eq!(
            workflow.validate().unwrap_err(),
            WorkflowError::EpilogueResult {
                result: "racecheck".to_owned()
            }
        );

        let workflow = parse(
            "[[task]]\nname = \"kept\"\nkind = \"command\"\ncommand = \"true\"\nstage = \"epilogue\"\n",
        );
        assert_eq!(
            workflow.validate().unwrap_err(),
            WorkflowError::ReservedEpilogueName
        );
    }

    #[test]
    fn emits_round_trips_toml_and_does_not_affect_admission() {
        let workflow = parse(
            "type = \"custom\"\nresult = \"check\"\n\
             [[task]]\nname = \"check\"\nkind = \"evaluate\"\ncommand = \"true\"\nemits = [\"score\", \"pass\"]\n",
        );
        workflow.validate().unwrap();
        workflow
            .admit(&WorkflowCaps::new(["workflow.custom"]))
            .unwrap();
        let toml = toml::to_string(&workflow).unwrap();
        assert!(toml.contains("emits = [\"score\", \"pass\"]"), "{toml}");
        let back: WorkflowCfg = toml::from_str(&toml).unwrap();
        assert_eq!(back.tasks[0].emits.len(), 2);

        // Undeclared emits stays off the wire, keeping existing manifests byte-identical.
        let bare = parse(
            "type = \"custom\"\nresult = \"c\"\n\
             [[task]]\nname = \"c\"\nkind = \"command\"\ncommand = \"true\"\n",
        );
        assert!(!toml::to_string(&bare).unwrap().contains("emits"));
    }

    fn full_autoresearch() -> WorkflowCfg {
        parse(
            "type = \"autoresearch\"\nresult = \"choose\"\n\
             [[task]]\nname = \"invent\"\nkind = \"engine\"\nop = \"propose\"\n\
             [[task]]\nname = \"deploy\"\nkind = \"engine\"\nop = \"apply\"\ndepends_on = [\"invent\"]\n\
             [[task]]\nname = \"score\"\nkind = \"engine\"\nop = \"measure\"\ndepends_on = [\"deploy\"]\n\
             [[task]]\nname = \"choose\"\nkind = \"engine\"\nop = \"decide\"\nsource = \"score\"\ndepends_on = [\"score\"]\n",
        )
    }
}
