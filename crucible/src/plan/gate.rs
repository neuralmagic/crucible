//! The `approve` task at dispatch time: resolve where its decision comes from, and either
//! settle it from a resolution the run already holds or report it as an open gate.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::plan::exec::{Attempt, AttemptOutcome, Gate};
use crate::plan::ir::{ApprovalSourceSpec, RefOrLiteral, Task, TaskKind, TaskName};
use crucible_contract::{GateDecision, GateResolution, GateSource, gate_trace_id};

/// What a runner needs to settle gates: the run they belong to, and the resolutions the run
/// holds (from `--approval`, the admission ledger, or a park that just ended).
#[derive(Debug, Clone, Default)]
pub struct GateCtx {
    pub run_id: String,
    pub resolutions: BTreeMap<String, GateResolution>,
}

impl GateCtx {
    pub fn new(run_id: impl Into<String>) -> Self {
        GateCtx {
            run_id: run_id.into(),
            resolutions: BTreeMap::new(),
        }
    }

    pub fn resolve(&mut self, trace_id: impl Into<String>, resolution: GateResolution) {
        self.resolutions.insert(trace_id.into(), resolution);
    }

    /// Apply the held resolution, or report that the task is waiting for one.
    pub fn attempt(
        &self,
        task: &Task,
        inputs: &BTreeMap<TaskName, Value>,
    ) -> Attempt {
        let TaskKind::Approve {
            summary,
            source,
            timeout_secs,
        } = &task.task
        else {
            return Attempt {
                outcome: AttemptOutcome::fail(format!("task {} is not an approve task", task.name)),
                cost_usd: 0.0,
            };
        };
        let source = match source.resolve(inputs) {
            Ok(source) => source,
            Err(why) => {
                return Attempt {
                    outcome: AttemptOutcome::fail(why.to_string()),
                    cost_usd: 0.0,
                };
            }
        };
        let trace_id = gate_trace_id(&self.run_id, &task.name.0);
        let outcome = match self.resolutions.get(&trace_id) {
            Some(resolution) => settle(resolution, &source),
            None => AttemptOutcome::Await(Gate {
                task: task.name.clone(),
                trace_id,
                handle: source
                    .handle()
                    .map(str::to_string)
                    .unwrap_or_else(|| task.name.0.clone()),
                source,
                summary: summary.clone(),
                timeout_secs: *timeout_secs,
            }),
        };
        Attempt {
            outcome,
            cost_usd: 0.0,
        }
    }
}

/// The env var the launcher sets to name the run a gate belongs to. It must be stable across a
/// suspend and its redispatch, or the resolution the controller hands back names a trace the
/// resumed run never opened.
pub const ENV_RUN_ID: &str = "CRUCIBLE_RUN_ID";

/// The run id gates are keyed under: `CRUCIBLE_RUN_ID`, else the launcher's `CRUCIBLE_RUN_NAME`,
/// else `local`.
pub fn run_id_from_env() -> String {
    for key in [ENV_RUN_ID, "CRUCIBLE_RUN_NAME"] {
        if let Ok(v) = std::env::var(key)
            && !v.trim().is_empty()
        {
            return v.trim().to_string();
        }
    }
    "local".to_string()
}

/// Why a gate's source could not be resolved from its inputs.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SourceError {
    #[error("gate reads {field} from {task}, which produced no passing output")]
    ProducerMissing { task: String, field: String },
    #[error("gate reads {field} from {task}, whose output has no string {field}")]
    FieldMissing { task: String, field: String },
}

impl ApprovalSourceSpec {
    /// Resolve upstream references in a gate source against settled task outputs.
    pub fn resolve(
        &self,
        inputs: &BTreeMap<TaskName, Value>,
    ) -> Result<GateSource, SourceError> {
        let read = |r: &RefOrLiteral| -> Result<String, SourceError> {
            match r {
                RefOrLiteral::Literal(s) => Ok(s.clone()),
                RefOrLiteral::Output(reference) => {
                    let producer = inputs.get(&reference.task).ok_or_else(|| {
                        SourceError::ProducerMissing {
                            task: reference.task.0.clone(),
                            field: reference.field.0.clone(),
                        }
                    })?;
                    producer
                        .get(&reference.field.0)
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .ok_or_else(|| SourceError::FieldMissing {
                            task: reference.task.0.clone(),
                            field: reference.field.0.clone(),
                        })
                }
            }
        };
        Ok(match self {
            ApprovalSourceSpec::Native => GateSource::Native,
            ApprovalSourceSpec::GithubPr { url, until } => GateSource::GithubPr {
                url: read(url)?,
                until: *until,
            },
            ApprovalSourceSpec::Jira { key, until } => GateSource::Jira {
                key: read(key)?,
                until: until.clone(),
            },
        })
    }
}

fn settle(resolution: &GateResolution, source: &GateSource) -> AttemptOutcome {
    match resolution.decision {
        GateDecision::Granted => AttemptOutcome::Pass(serde_json::json!({
            "approved_by": resolution.by,
            "source": resolution.source.clone().unwrap_or_else(|| source.kind().to_string()),
            "reason": resolution.reason,
        })),
        GateDecision::Denied => AttemptOutcome::Fail {
            note: format!(
                "denied: {}",
                resolution
                    .reason
                    .as_deref()
                    .filter(|r| !r.is_empty())
                    .unwrap_or("no reason given")
            ),
            output: Some(serde_json::json!({
                "denied_by": resolution.by,
                "source": resolution.source,
                "reason": resolution.reason,
            })),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::ir::{Join, OutputField, OutputRef, Stage};
    use crucible_contract::{JiraUntil, PrUntil};

    fn gate(source: ApprovalSourceSpec, deps: &[&str]) -> Task {
        Task {
            name: TaskName("review".into()),
            task: TaskKind::Approve {
                summary: Some("ship?".into()),
                source,
                timeout_secs: None,
            },
            depends_on: deps.iter().map(|d| TaskName((*d).into())).collect(),
            session: None,
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::All,
            stage: Stage::Iteration,
            emits: Vec::new(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
        }
    }

    fn pr_from(task: &str, field: &str) -> ApprovalSourceSpec {
        ApprovalSourceSpec::GithubPr {
            url: RefOrLiteral::Output(OutputRef {
                task: TaskName(task.into()),
                field: OutputField(field.into()),
            }),
            until: PrUntil::Approved,
        }
    }

    #[test]
    fn an_unresolved_gate_is_reported_open_with_its_resolved_source() {
        let ctx = GateCtx::new("run-1");
        let inputs = BTreeMap::from([(
            TaskName("open_pr".into()),
            serde_json::json!({"pr_url": "https://github.com/o/r/pull/9"}),
        )]);
        let a = attempt(
            &ctx,
            &gate(pr_from("open_pr", "pr_url"), &["open_pr"]),
            &inputs,
        );
        assert_eq!(a.cost_usd, 0.0);
        let AttemptOutcome::Await(g) = a.outcome else {
            panic!("an unresolved gate awaits");
        };
        assert_eq!(g.trace_id, "approve:run-1:review");
        assert_eq!(g.handle, "https://github.com/o/r/pull/9");
        assert_eq!(g.summary.as_deref(), Some("ship?"));
        assert_eq!(
            g.source,
            GateSource::GithubPr {
                url: "https://github.com/o/r/pull/9".into(),
                until: PrUntil::Approved
            }
        );
    }

    #[test]
    fn a_native_gate_uses_the_task_name_as_its_handle() {
        let ctx = GateCtx::new("run-1");
        let a = attempt(
            &ctx,
            &gate(ApprovalSourceSpec::Native, &[]),
            &BTreeMap::new(),
        );
        let AttemptOutcome::Await(g) = a.outcome else {
            panic!("awaits");
        };
        assert_eq!(g.handle, "review");
        assert_eq!(g.source, GateSource::Native);
    }

    #[test]
    fn a_held_grant_passes_and_a_held_denial_fails_with_the_reason() {
        let mut ctx = GateCtx::new("run-1");
        ctx.resolve(
            "approve:run-1:review",
            GateResolution::granted(Some("alice".parse().unwrap()), "native"),
        );
        let a = attempt(
            &ctx,
            &gate(ApprovalSourceSpec::Native, &[]),
            &BTreeMap::new(),
        );
        let AttemptOutcome::Pass(out) = a.outcome else {
            panic!("a grant passes");
        };
        assert_eq!(out["approved_by"], "alice");
        assert_eq!(out["source"], "native");

        ctx.resolve(
            "approve:run-1:review",
            GateResolution::denied(
                "changes requested",
                Some("bob".parse().unwrap()),
                "github_pr",
            ),
        );
        let a = attempt(
            &ctx,
            &gate(ApprovalSourceSpec::Native, &[]),
            &BTreeMap::new(),
        );
        let AttemptOutcome::Fail { note, output } = a.outcome else {
            panic!("a denial fails");
        };
        assert_eq!(note, "denied: changes requested");
        assert_eq!(output.expect("carries the denial")["denied_by"], "bob");

        ctx.resolve("approve:run-1:review", GateResolution::timeout());
        let a = attempt(
            &ctx,
            &gate(ApprovalSourceSpec::Native, &[]),
            &BTreeMap::new(),
        );
        assert!(matches!(a.outcome, AttemptOutcome::Fail { .. }));
    }

    #[test]
    fn a_source_read_from_a_missing_producer_or_field_fails_the_gate() {
        let ctx = GateCtx::new("run-1");
        let a = attempt(
            &ctx,
            &gate(pr_from("open_pr", "pr_url"), &["open_pr"]),
            &BTreeMap::new(),
        );
        let AttemptOutcome::Fail { note, .. } = a.outcome else {
            panic!("fails");
        };
        assert!(note.contains("no passing output"), "{note}");
        let inputs =
            BTreeMap::from([(TaskName("open_pr".into()), serde_json::json!({"number": 9}))]);
        let a = attempt(
            &ctx,
            &gate(pr_from("open_pr", "pr_url"), &["open_pr"]),
            &inputs,
        );
        let AttemptOutcome::Fail { note, .. } = a.outcome else {
            panic!("fails");
        };
        assert!(note.contains("no string pr_url"), "{note}");
        let jira = ApprovalSourceSpec::Jira {
            key: RefOrLiteral::Literal("PROJ-1".into()),
            until: JiraUntil::Status("Ready".into()),
        };
        let AttemptOutcome::Await(g) = attempt(&ctx, &gate(jira, &[]), &BTreeMap::new()).outcome
        else {
            panic!("awaits");
        };
        assert_eq!(g.handle, "PROJ-1");
    }
}
