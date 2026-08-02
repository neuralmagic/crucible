//! Pack-authored workflow graphs and their admission contract.
//!
//! Topology is data. Safety comes from two independent checks: a workflow type chooses
//! the semantic invariants the graph must satisfy, and the admitting orchestrator must
//! advertise capabilities for that type and every engine operation it will execute.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::plan::ir::{EngineOp, Join, Plan, PlanBudget, Task, TaskKind, TaskName};

/// Task names used by the compatibility template. Fully-authored workflows may reuse
/// these names; they are not engine-owned identities.
const LEGACY_NAMES: [&str; 4] = ["propose", "apply", "measure", "decide"];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowType {
    /// A repeating candidate/apply/measure/decision protocol.
    #[default]
    Autoresearch,
    /// An arbitrary DAG. Only universal graph and operation-authority rules apply.
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
            "engine.decide",
        ])
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
    /// The semantic contract an admitting orchestrator promises to enforce.
    #[serde(rename = "type", default)]
    pub workflow_type: WorkflowType,
    /// The task whose typed output completes the workflow. Required for fully-authored
    /// autoresearch graphs; absent preserves the original splice-only manifest format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<TaskName>,
    #[serde(rename = "task", default)]
    pub tasks: Vec<Task>,
}

impl WorkflowCfg {
    /// Validate graph structure plus the invariants selected by `type`. This does not
    /// grant execution authority; call [`WorkflowCfg::admit`] at the orchestrator edge.
    pub fn validate(&self) -> Result<()> {
        if self.is_legacy_splice() {
            return self.validate_legacy_splice();
        }

        let plan = Plan {
            version: 1,
            reason: None,
            budget: PlanBudget { usd: f64::MAX },
            tasks: self.tasks.clone(),
        };
        plan.validate()?;

        let tasks: BTreeMap<&TaskName, &Task> =
            self.tasks.iter().map(|task| (&task.name, task)).collect();
        for task in &self.tasks {
            if let TaskKind::Engine { op, source } = &task.task {
                if !task.required {
                    bail!("engine task {:?} must be required", task.name.0);
                }
                if task.isolation.is_some() {
                    bail!(
                        "engine task {:?} cannot run in an isolated worktree",
                        task.name.0
                    );
                }
                if task.join != Join::All {
                    bail!("engine task {:?} must use join = \"all\"", task.name.0);
                }
                match (op, source) {
                    (EngineOp::Decide, None) => {
                        bail!("engine decide task {:?} requires source", task.name.0)
                    }
                    (EngineOp::Decide, Some(_)) | (_, None) => {}
                    (_, Some(_)) => bail!(
                        "engine {:?} task {:?} does not accept source",
                        op,
                        task.name.0
                    ),
                }
                if let Some(source) = source {
                    if !tasks.contains_key(source) {
                        bail!(
                            "engine task {:?} names unknown source {:?}",
                            task.name.0,
                            source.0
                        );
                    }
                    if !is_ancestor(&tasks, source, &task.name) {
                        bail!(
                            "engine task source {:?} must be an ancestor of {:?}",
                            source.0,
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

    /// Validate and prove that the admitting orchestrator owns every requested authority.
    pub fn admit(&self, caps: &WorkflowCaps) -> Result<()> {
        self.validate()?;
        caps.require(self.workflow_type.capability())?;
        for task in &self.tasks {
            if let TaskKind::Engine { op, .. } = task.task {
                caps.require(op.capability())?;
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
                op: EngineOp::Measure,
                ..
            }
        ) {
            bail!(
                "autoresearch decision source {:?} must be an engine measure task",
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

    /// The tasks nothing else depends on, used only by the legacy splice adapter.
    pub fn sinks(&self) -> Vec<TaskName> {
        self.tasks
            .iter()
            .filter(|task| {
                !self.tasks.iter().any(|other| {
                    other
                        .depends_on
                        .iter()
                        .any(|dependency| dependency == &task.name)
                })
            })
            .map(|task| task.name.clone())
            .collect()
    }
}

fn is_ancestor(tasks: &BTreeMap<&TaskName, &Task>, ancestor: &TaskName, node: &TaskName) -> bool {
    let Some(task) = tasks.get(node) else {
        return false;
    };
    task.depends_on
        .iter()
        .any(|dependency| dependency == ancestor || is_ancestor(tasks, ancestor, dependency))
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
    fn autoresearch_shape_is_semantic_not_name_based() {
        full_autoresearch().validate().unwrap();
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
