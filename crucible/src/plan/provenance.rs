//! The reserved provenance input: what runs a task, as the engine and the controller that
//! dispatched it know it.

use std::collections::BTreeMap;

use crucible_contract::provenance::{DispatchProvenance, ENV_PROVENANCE};
use crucible_contract::session::TaskAgent;
use serde::Serialize;
use serde_json::Value;

use crate::plan::ir::{Task, TaskName};

#[derive(Debug, thiserror::Error)]
pub enum BadDispatchProvenance {
    #[error("{ENV_PROVENANCE} is not a dispatch provenance document: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("{ENV_PROVENANCE} is not valid unicode")]
    NotUnicode,
}

/// One run's provenance, resolved once before the first dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunProvenance {
    run: String,
    dispatch: DispatchProvenance,
    agents: BTreeMap<String, AgentInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct AgentInput {
    harness: String,
    model: String,
}

#[derive(Serialize)]
struct ProvenanceInput<'a> {
    run: &'a str,
    task: &'a str,
    #[serde(flatten)]
    dispatch: &'a DispatchProvenance,
    agents: &'a BTreeMap<String, AgentInput>,
}

impl RunProvenance {
    /// `agents` is each agent task's resolved harness and model, by task name.
    pub fn new(
        run: &str,
        dispatch: DispatchProvenance,
        agents: impl IntoIterator<Item = (TaskName, TaskAgent)>,
    ) -> Self {
        RunProvenance {
            run: run.to_owned(),
            dispatch,
            agents: agents
                .into_iter()
                .map(|(task, agent)| {
                    (
                        task.0,
                        AgentInput {
                            harness: agent.harness,
                            model: agent.model,
                        },
                    )
                })
                .collect(),
        }
    }

    /// The controller's half, read from [`ENV_PROVENANCE`]. Unset reads as empty.
    pub fn dispatch_from_env() -> Result<DispatchProvenance, BadDispatchProvenance> {
        dispatch_from(std::env::var(ENV_PROVENANCE))
    }

    /// The input `task` receives.
    pub fn input(&self, task: &TaskName) -> Value {
        serde_json::to_value(ProvenanceInput {
            run: &self.run,
            task: &task.0,
            dispatch: &self.dispatch,
            agents: &self.agents,
        })
        .unwrap_or(Value::Null)
    }

    /// Write the reserved input into `inputs`.
    pub fn add(&self, task: &Task, inputs: &mut BTreeMap<TaskName, Value>) {
        inputs.insert(
            TaskName(crate::plan::ir::PROVENANCE_INPUT.to_string()),
            self.input(&task.name),
        );
    }
}

fn dispatch_from(
    var: Result<String, std::env::VarError>,
) -> Result<DispatchProvenance, BadDispatchProvenance> {
    match var {
        Ok(raw) if !raw.trim().is_empty() => {
            serde_json::from_str(&raw).map_err(BadDispatchProvenance::Decode)
        }
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(DispatchProvenance::default()),
        Err(std::env::VarError::NotUnicode(_)) => Err(BadDispatchProvenance::NotUnicode),
    }
}

#[cfg(test)]
mod tests {
    use crate::plan::provenance::*;
    use serde_json::json;

    #[test]
    fn the_input_names_the_run_the_task_the_dispatch_and_every_agent() {
        let provenance = RunProvenance::new(
            "run-1",
            DispatchProvenance {
                cluster: Some("east".into()),
                provider: Some("p".into()),
                sandbox_image: Some("img@sha256:abc".into()),
            },
            [(
                TaskName("author".into()),
                TaskAgent {
                    harness: "claude".into(),
                    model: "m".into(),
                    effort: "high".into(),
                },
            )],
        );
        assert_eq!(
            provenance.input(&TaskName("push".into())),
            json!({
                "run": "run-1",
                "task": "push",
                "cluster": "east",
                "provider": "p",
                "sandbox_image": "img@sha256:abc",
                "agents": {"author": {"harness": "claude", "model": "m"}},
            })
        );
    }

    #[test]
    fn an_undispatched_run_names_only_what_the_engine_knows() {
        let provenance = RunProvenance::new("local", DispatchProvenance::default(), []);
        assert_eq!(
            provenance.input(&TaskName("push".into())),
            json!({"run": "local", "task": "push", "agents": {}})
        );
    }

    #[test]
    fn the_dispatch_half_reads_the_controllers_document() {
        let unset = dispatch_from(Err(std::env::VarError::NotPresent));
        assert_eq!(unset.ok(), Some(DispatchProvenance::default()));
        let blank = dispatch_from(Ok("  ".into()));
        assert_eq!(blank.ok(), Some(DispatchProvenance::default()));
        let set = dispatch_from(Ok(r#"{"cluster":"east"}"#.into()));
        assert_eq!(
            set.ok(),
            Some(DispatchProvenance {
                cluster: Some("east".into()),
                ..DispatchProvenance::default()
            })
        );
        assert!(matches!(
            dispatch_from(Ok(r#"{"cluster":"east","extra":1}"#.into())),
            Err(BadDispatchProvenance::Decode(_))
        ));
        assert!(matches!(
            dispatch_from(Err(std::env::VarError::NotUnicode("x".into()))),
            Err(BadDispatchProvenance::NotUnicode)
        ));
    }
}
