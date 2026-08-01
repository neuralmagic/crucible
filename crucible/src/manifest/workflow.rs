//! `[[workflow.task]]`: pack-authored tasks spliced into each loop iteration.
//!
//! A pack may add work between the agent's turn and the gate, and only there. The engine
//! always appends `apply -> measure -> decide` after whatever the pack declares, so a pack
//! can reject its own candidate early but can never remove, reorder, or precede the gate it
//! is scored by. The agent gets a World, never the Judge, and this keeps that true when the
//! agent is also the one authoring the harness.
//!
//! The useful shape is a check that is cheaper than measuring: a reviewer, a linter, a
//! static analysis. A required task that fails discards the iteration before any measurement
//! spend.

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::plan::ir::{Task, TaskName};

/// Task names the engine owns. A pack task may depend on `propose`; it may not take any of
/// these names, because the engine constructs them itself.
pub const RESERVED: [&str; 4] = ["propose", "apply", "measure", "decide"];

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCfg {
    /// Tasks in dependency order-independent form; the engine wires the ends.
    #[serde(rename = "task", default)]
    pub tasks: Vec<Task>,
}

impl WorkflowCfg {
    /// Reject anything the splice cannot wire safely. Runs at manifest load, so a bad
    /// workflow is a config error rather than a failure a paid run discovers.
    pub fn validate(&self) -> Result<()> {
        let mut seen: Vec<&str> = Vec::new();
        for t in &self.tasks {
            let name = t.name.0.as_str();
            if name.trim().is_empty() {
                bail!("[[workflow.task]] has an empty name");
            }
            if RESERVED.contains(&name) {
                bail!(
                    "[[workflow.task]] name {name:?} is reserved for the engine's own stages \
                     ({})",
                    RESERVED.join(", ")
                );
            }
            if seen.contains(&name) {
                bail!("duplicate [[workflow.task]] name {name:?}");
            }
            seen.push(name);
        }
        // Edges may point at another workflow task or at `propose`, nothing else: depending
        // on `measure` or `decide` would put pack work after the gate.
        for t in &self.tasks {
            for d in &t.depends_on {
                let dep = d.0.as_str();
                if dep == "propose" {
                    continue;
                }
                if RESERVED.contains(&dep) {
                    bail!(
                        "[[workflow.task]] {:?} depends on {dep:?}; pack tasks run before the \
                         gate, so they may only depend on `propose` or on each other",
                        t.name.0
                    );
                }
                if !seen.contains(&dep) {
                    bail!(
                        "[[workflow.task]] {:?} depends on unknown task {dep:?}",
                        t.name.0
                    );
                }
            }
        }
        Ok(())
    }

    /// The tasks nothing else depends on. `apply` waits on these, so every declared task is
    /// upstream of the gate whether or not the author wired it explicitly.
    pub fn sinks(&self) -> Vec<TaskName> {
        self.tasks
            .iter()
            .filter(|t| {
                !self
                    .tasks
                    .iter()
                    .any(|o| o.depends_on.iter().any(|d| d == &t.name))
            })
            .map(|t| t.name.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(src: &str) -> WorkflowCfg {
        toml::from_str(src).expect("parse")
    }

    #[test]
    fn reserved_names_are_refused() {
        for name in RESERVED {
            let c = cfg(&format!(
                "[[task]]\nname = \"{name}\"\nkind = \"command\"\ncommand = \"true\"\n"
            ));
            let err = c.validate().unwrap_err().to_string();
            assert!(err.contains("reserved"), "{name}: {err}");
        }
    }

    #[test]
    fn depending_on_the_gate_is_refused() {
        let c = cfg(
            "[[task]]\nname = \"late\"\nkind = \"command\"\ncommand = \"true\"\ndepends_on = [\"measure\"]\n",
        );
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("before the gate"), "{err}");
    }

    #[test]
    fn unknown_dependency_is_refused() {
        let c = cfg(
            "[[task]]\nname = \"a\"\nkind = \"command\"\ncommand = \"true\"\ndepends_on = [\"ghost\"]\n",
        );
        assert!(
            c.validate()
                .unwrap_err()
                .to_string()
                .contains("unknown task")
        );
    }

    #[test]
    fn duplicate_names_are_refused() {
        let c = cfg(
            "[[task]]\nname = \"a\"\nkind = \"command\"\ncommand = \"true\"\n\
                     [[task]]\nname = \"a\"\nkind = \"command\"\ncommand = \"true\"\n",
        );
        assert!(c.validate().unwrap_err().to_string().contains("duplicate"));
    }

    #[test]
    fn sinks_are_the_tasks_nothing_depends_on() {
        let c = cfg(
            "[[task]]\nname = \"a\"\nkind = \"command\"\ncommand = \"true\"\n\
                     [[task]]\nname = \"b\"\nkind = \"command\"\ncommand = \"true\"\ndepends_on = [\"a\"]\n\
                     [[task]]\nname = \"c\"\nkind = \"command\"\ncommand = \"true\"\n",
        );
        c.validate().unwrap();
        let sinks: Vec<String> = c.sinks().into_iter().map(|n| n.0).collect();
        assert_eq!(
            sinks,
            ["b", "c"],
            "`a` feeds `b`, so only b and c gate apply"
        );
    }

    #[test]
    fn an_empty_workflow_has_no_sinks() {
        assert!(WorkflowCfg::default().sinks().is_empty());
        WorkflowCfg::default().validate().unwrap();
    }
}
