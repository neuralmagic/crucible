//! Pack-authored graphs. Workflow invariants and orchestrator capabilities are checked separately.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::plan::ir::{EngineOp, Join, Plan, PlanBudget, Stage, Task, TaskKind, TaskName};

/// Reserved epilogue input key: [`crate::loop_graph`] injects the kept candidate's
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
}

impl WorkflowType {
    pub fn capability(self) -> &'static str {
        match self {
            WorkflowType::Autoresearch => "workflow.autoresearch",
            WorkflowType::Custom => "workflow.custom",
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

    pub fn with_persistent_sessions(mut self) -> Self {
        self.names.insert("agent.session.persist".to_string());
        self
    }

    fn require(&self, capability: &str) -> Result<()> {
        if !self.names.contains(capability) {
            bail!("workflow requires unavailable orchestrator capability {capability:?}");
        }
        Ok(())
    }
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
    #[serde(rename = "task", default)]
    pub tasks: Vec<Task>,
}

impl WorkflowCfg {
    /// Validate structure and type-specific invariants, without granting authority.
    pub fn validate(&self) -> Result<()> {
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
            if let TaskKind::Engine {
                op,
                source,
                tiebreak,
            } = &task.task
            {
                if !task.required {
                    bail!("engine task {:?} must be required", task.name.0);
                }
                if task.isolation.is_some() {
                    bail!(
                        "engine task {:?} cannot run in an isolated worktree",
                        task.name.0
                    );
                }
                if task.join != Join::All && *op != EngineOp::Grade {
                    bail!("engine task {:?} must use join = \"all\"", task.name.0);
                }
                match (op, source) {
                    (EngineOp::Decide | EngineOp::Grade, None) => {
                        bail!("engine {op:?} task {:?} requires source", task.name.0)
                    }
                    (EngineOp::Decide | EngineOp::Grade, Some(_)) | (_, None) => {}
                    (_, Some(_)) => bail!(
                        "engine {:?} task {:?} does not accept source",
                        op,
                        task.name.0
                    ),
                }
                if let Some(source) = source {
                    let Some(source_task) = tasks.get(source) else {
                        bail!(
                            "engine task {:?} names unknown source {:?}",
                            task.name.0,
                            source.0
                        );
                    };
                    if !is_ancestor(&tasks, source, &task.name) {
                        bail!(
                            "engine task source {:?} must be an ancestor of {:?}",
                            source.0,
                            task.name.0
                        );
                    }
                    if *op == EngineOp::Decide
                        && let Some(first) = decided_sources.insert(source, &task.name)
                    {
                        bail!(
                            "engine decide tasks {:?} and {:?} share measurement source {:?}; a \
                             measurement can be graded once",
                            first.0,
                            task.name.0,
                            source.0
                        );
                    }
                    if *op == EngineOp::Grade
                        && !matches!(source_task.task, TaskKind::Evaluate { .. })
                    {
                        bail!(
                            "engine grade source {:?} must be an evaluate task",
                            source.0
                        );
                    }
                    if *op == EngineOp::Grade && !source_task.required {
                        bail!("engine grade score source {:?} must be required", source.0);
                    }
                }
                if let Some(tiebreak) = tiebreak {
                    if *op != EngineOp::Grade {
                        bail!(
                            "engine {op:?} task {:?} does not accept tiebreak",
                            task.name.0
                        );
                    }
                    let Some(tiebreak_task) = tasks.get(tiebreak) else {
                        bail!(
                            "engine grade task {:?} names unknown tiebreak {:?}",
                            task.name.0,
                            tiebreak.0
                        );
                    };
                    if !matches!(tiebreak_task.task, TaskKind::Evaluate { .. }) {
                        bail!(
                            "engine grade tiebreak {:?} must be an evaluate task",
                            tiebreak.0
                        );
                    }
                    // Unlike the score source, the tiebreak may be advisory: a rung that
                    // failed or never ran just leaves the reading without a secondary.
                    if !is_ancestor(&tasks, tiebreak, &task.name) {
                        bail!(
                            "engine grade tiebreak {:?} must be an ancestor of {:?}",
                            tiebreak.0,
                            task.name.0
                        );
                    }
                }
            }
        }

        if self.workflow_type == WorkflowType::Autoresearch {
            self.validate_autoresearch()?;
        } else if let Some(result) = &self.result
            && !self.tasks.iter().any(|task| &task.name == result)
        {
            bail!(
                "custom workflow result {:?} names an unknown task",
                result.0
            );
        }
        Ok(())
    }

    /// Require orchestrator authority for the workflow and its engine operations.
    pub fn admit(&self, caps: &WorkflowCaps) -> Result<()> {
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
    fn validate_stages(&self) -> Result<()> {
        let stages: BTreeMap<&TaskName, Stage> = self
            .tasks
            .iter()
            .map(|task| (&task.name, task.stage))
            .collect();
        for task in &self.tasks {
            if task.stage == Stage::Epilogue {
                if matches!(task.task, TaskKind::Engine { .. }) {
                    bail!(
                        "engine task {:?} cannot run in the epilogue (the loop is over)",
                        task.name.0
                    );
                }
                if task.name.0 == KEPT_INPUT {
                    bail!(
                        "epilogue task name {KEPT_INPUT:?} is reserved for the kept-candidate input"
                    );
                }
            }
            for dependency in &task.depends_on {
                // Unknown names default to Iteration here; the legacy splice's implicit
                // "propose" is an iteration task, and the authored path rejects truly
                // unknown dependencies in its plan validation.
                let dependency_stage = stages.get(dependency).copied().unwrap_or_default();
                if dependency_stage != task.stage {
                    bail!(
                        "task {:?} (stage {:?}) depends on {:?} (stage {:?}); dependencies cannot cross stages",
                        task.name.0,
                        task.stage,
                        dependency.0,
                        dependency_stage
                    );
                }
            }
        }
        if let Some(result) = &self.result
            && stages.get(result).copied().unwrap_or_default() == Stage::Epilogue
        {
            bail!("workflow result {:?} cannot be an epilogue task", result.0);
        }
        Ok(())
    }

    fn validate_legacy_splice(&self) -> Result<()> {
        let mut seen = BTreeSet::new();
        for task in &self.tasks {
            let name = task.name.0.as_str();
            if name.trim().is_empty() {
                bail!("[[workflow.task]] has an empty name");
            }
            if LEGACY_NAMES.contains(&name) {
                bail!(
                    "legacy [[workflow.task]] name {name:?} collides with its compatibility template"
                );
            }
            if !seen.insert(name) {
                bail!("duplicate [[workflow.task]] name {name:?}");
            }
        }
        for task in &self.tasks {
            for dependency in &task.depends_on {
                let name = dependency.0.as_str();
                if name != "propose" && !seen.contains(name) {
                    bail!(
                        "legacy [[workflow.task]] {:?} depends on unknown task {name:?}",
                        task.name.0
                    );
                }
            }
        }
        Ok(())
    }

    fn validate_autoresearch(&self) -> Result<()> {
        let result = self.result.as_ref().ok_or_else(|| {
            anyhow::anyhow!("fully-authored autoresearch workflow requires result")
        })?;
        let tasks: BTreeMap<&TaskName, &Task> =
            self.tasks.iter().map(|task| (&task.name, task)).collect();
        let decision = tasks.get(result).ok_or_else(|| {
            anyhow::anyhow!("autoresearch result {:?} names an unknown task", result.0)
        })?;
        let TaskKind::Engine {
            op: EngineOp::Decide,
            source: Some(measurement),
            ..
        } = &decision.task
        else {
            bail!(
                "autoresearch result {:?} must be an engine decide task with source",
                result.0
            );
        };
        let measured = tasks.get(measurement).ok_or_else(|| {
            anyhow::anyhow!(
                "autoresearch decision {:?} names unknown measurement source {:?}",
                result.0,
                measurement.0
            )
        })?;
        if !matches!(
            measured.task,
            TaskKind::Engine {
                op: EngineOp::Measure | EngineOp::Grade,
                ..
            }
        ) {
            bail!(
                "autoresearch decision source {:?} must be an engine measure or grade task",
                measurement.0
            );
        }
        if !is_ancestor(&tasks, measurement, result) {
            bail!(
                "measurement {:?} must be an ancestor of decision {:?}",
                measurement.0,
                result.0
            );
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
            bail!(
                "autoresearch measurement {:?} requires an engine apply ancestor",
                measurement.0
            );
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
            bail!("autoresearch apply path requires an engine propose ancestor");
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
        let error = workflow
            .admit(&WorkflowCaps::new(["workflow.autoresearch"]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("engine.propose"), "{error}");
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
        let error = workflow
            .admit(&WorkflowCaps::new(["workflow.custom"]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("agent.session.persist"), "{error}");
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
        let error = workflow.validate().unwrap_err().to_string();
        assert!(error.contains("share measurement source"), "{error}");
        assert!(error.contains("score"), "{error}");
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
        let error = workflow.validate().unwrap_err().to_string();
        assert!(error.contains("does not accept tiebreak"), "{error}");

        // A tiebreak naming a non-evaluate task is rejected.
        let mut workflow = full_autoresearch();
        workflow.tasks[2].task = TaskKind::Engine {
            op: EngineOp::Measure,
            source: None,
            tiebreak: Some("deploy".into()),
        };
        let error = workflow.validate().unwrap_err().to_string();
        assert!(error.contains("does not accept tiebreak"), "{error}");
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
        let error = parse(&unknown).validate().unwrap_err().to_string();
        assert!(error.contains("unknown tiebreak"), "{error}");

        let not_evaluate = format!(
            "{base}[[task]]\nname = \"grade\"\nkind = \"engine\"\nop = \"grade\"\nsource = \"correctness\"\ntiebreak = \"deploy\"\ndepends_on = [\"correctness\"]\njoin = \"passed\"\n"
        );
        let error = parse(&not_evaluate).validate().unwrap_err().to_string();
        assert!(error.contains("must be an evaluate task"), "{error}");
    }

    #[test]
    fn grade_requires_an_evaluation_score_source() {
        let mut workflow = full_autoresearch();
        workflow.tasks[2].task = TaskKind::Engine {
            op: EngineOp::Grade,
            source: Some("deploy".into()),
            tiebreak: None,
        };
        let error = workflow.validate().unwrap_err().to_string();
        assert!(error.contains("must be an evaluate task"), "{error}");
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
        let error = workflow.validate().unwrap_err().to_string();
        assert!(error.contains("cannot cross stages"), "{error}");

        // Iteration depending on an epilogue task would deadlock every iteration.
        let workflow = parse(
            "[[task]]\nname = \"racecheck\"\nkind = \"command\"\ncommand = \"true\"\nstage = \"epilogue\"\n\
             [[task]]\nname = \"review\"\nkind = \"command\"\ncommand = \"true\"\ndepends_on = [\"racecheck\"]\n",
        );
        let error = workflow.validate().unwrap_err().to_string();
        assert!(error.contains("cannot cross stages"), "{error}");

        // The legacy splice's implicit propose dependency is an iteration task too.
        let workflow = parse(
            "[[task]]\nname = \"racecheck\"\nkind = \"command\"\ncommand = \"true\"\nstage = \"epilogue\"\ndepends_on = [\"propose\"]\n",
        );
        let error = workflow.validate().unwrap_err().to_string();
        assert!(error.contains("cannot cross stages"), "{error}");
    }

    #[test]
    fn engine_tasks_cannot_be_epilogue() {
        let mut workflow = full_autoresearch();
        workflow.tasks[2].stage = Stage::Epilogue;
        let error = workflow.validate().unwrap_err().to_string();
        assert!(error.contains("cannot run in the epilogue"), "{error}");
    }

    #[test]
    fn epilogue_cannot_be_the_result_or_claim_the_kept_name() {
        let workflow = parse(
            "type = \"custom\"\nresult = \"racecheck\"\n\
             [[task]]\nname = \"solve\"\nkind = \"command\"\ncommand = \"true\"\n\
             [[task]]\nname = \"racecheck\"\nkind = \"command\"\ncommand = \"true\"\nstage = \"epilogue\"\n",
        );
        let error = workflow.validate().unwrap_err().to_string();
        assert!(error.contains("cannot be an epilogue task"), "{error}");

        let workflow = parse(
            "[[task]]\nname = \"kept\"\nkind = \"command\"\ncommand = \"true\"\nstage = \"epilogue\"\n",
        );
        let error = workflow.validate().unwrap_err().to_string();
        assert!(error.contains("reserved"), "{error}");
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
