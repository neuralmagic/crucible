//! MCP tasks (`io.modelcontextprotocol/tasks`) over Kueue workloads. A task id names the submitted
//! object including its UID, so the id is unguessable and a recreated object of the same name is
//! a different task. Task state is derived from the cluster on every `tasks/get`; nothing is held
//! in the server.

use crate::manifest::WorkloadKind;
use crate::status::{Phase, Rollup};
use serde_json::Value;

pub const CANCELLED_ANNOTATION: &str = "crucible.dev/task-cancelled";
/// Set the first time a serving task is seen complete, so a later scale-down cannot move a
/// terminal task back to working.
pub const COMPLETED_ANNOTATION: &str = "crucible.dev/task-completed";
pub const POLL_INTERVAL_MS: u64 = 5_000;

/// `<namespace>/<kind>/<name>/<uid>`. Ids minted before namespaces were per call have no
/// namespace segment and resolve in the caller's own namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRef {
    pub namespace: Option<String>,
    pub kind: WorkloadKind,
    pub name: String,
    pub uid: String,
}

impl TaskRef {
    pub fn encode(&self) -> String {
        match &self.namespace {
            Some(ns) => format!("{ns}/{}/{}/{}", self.kind.kind(), self.name, self.uid),
            None => format!("{}/{}/{}", self.kind.kind(), self.name, self.uid),
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        let segments: Vec<&str> = id.splitn(4, '/').collect();
        let (namespace, kind_str, name, uid) = match segments[..] {
            [ns, kind, name, uid] if !ns.is_empty() => (Some(ns.to_string()), kind, name, uid),
            [kind, name, uid] => (None, kind, name, uid),
            _ => return None,
        };
        let kind = WorkloadKind::ALL
            .into_iter()
            .find(|k| k.kind() == kind_str)?;
        if name.is_empty() || uid.is_empty() || uid.contains('/') {
            return None;
        }
        Some(Self {
            namespace,
            kind,
            name: name.to_string(),
            uid: uid.to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Working {
        message: String,
    },
    /// Terminal. `failed` marks a workload that ran and failed, reported as a tool error result.
    Completed {
        failed: bool,
        message: String,
    },
    Cancelled,
}

fn annotation<'a>(object: &'a Value, key: &str) -> Option<&'a str> {
    object
        .pointer("/metadata/annotations")
        .and_then(|a| a.get(key))
        .and_then(Value::as_str)
}

/// Batch kinds complete when Kueue marks the Workload finished. Serving kinds never finish, so
/// their task completes once every replica is admitted and ready. `object` is the full object,
/// whose annotations record cancellation and an earlier completion.
pub fn outcome(kind: WorkloadKind, object: &Value, rollup: &Rollup) -> Outcome {
    if annotation(object, CANCELLED_ANNOTATION).is_some() {
        return Outcome::Cancelled;
    }
    if let Some(message) = annotation(object, COMPLETED_ANNOTATION) {
        return Outcome::Completed {
            failed: false,
            message: message.to_string(),
        };
    }
    let head = &rollup.headline;
    let working = || Outcome::Working {
        message: format!("{}: {}", head.phase.as_str(), head.message),
    };
    match kind {
        WorkloadKind::Job | WorkloadKind::RayJob => {
            if head.phase == Phase::Finished {
                Outcome::Completed {
                    failed: head.reason != "Succeeded",
                    message: head.message.clone(),
                }
            } else {
                working()
            }
        }
        WorkloadKind::Deployment | WorkloadKind::StatefulSet | WorkloadKind::LeaderWorkerSet => {
            let want = object
                .pointer("/spec/replicas")
                .and_then(Value::as_i64)
                .unwrap_or(1);
            let ready = object
                .pointer("/status/readyReplicas")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if want > 0 && ready >= want && head.phase == Phase::Admitted {
                Outcome::Completed {
                    failed: false,
                    message: format!("{ready}/{want} replicas admitted and ready"),
                }
            } else {
                Outcome::Working {
                    message: format!(
                        "{ready}/{want} ready; {}: {}",
                        head.phase.as_str(),
                        head.message
                    ),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::manifest::WorkloadKind;
    use crate::status::{PhaseReport, Rollup};
    use crate::task::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn rollup(phase: Phase, reason: &str) -> Rollup {
        Rollup {
            headline: PhaseReport {
                phase,
                reason: reason.into(),
                message: "msg".into(),
            },
            counts: BTreeMap::from([(phase.as_str(), 1)]),
        }
    }

    #[test]
    fn task_ids_round_trip_and_reject_garbage() {
        let t = TaskRef {
            namespace: Some("team-a".into()),
            kind: WorkloadKind::Deployment,
            name: "svc".into(),
            uid: "6f1c-uid".into(),
        };
        assert_eq!(t.encode(), "team-a/Deployment/svc/6f1c-uid");
        assert_eq!(TaskRef::parse(&t.encode()), Some(t));
        let legacy = TaskRef::parse("Deployment/svc/6f1c-uid").unwrap();
        assert_eq!(legacy.namespace, None);
        assert_eq!(legacy.encode(), "Deployment/svc/6f1c-uid");
        for bad in [
            "",
            "Job",
            "Job/x",
            "Job//u",
            "Pod/x/u",
            "Job/x/u/extra",
            "team-a/Job/x/u/extra",
            "/Job/x/u",
            "job/x/u",
        ] {
            assert_eq!(TaskRef::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_job_completes_when_kueue_finishes_it_and_fails_unless_it_succeeded() {
        let obj = json!({});
        assert!(matches!(
            outcome(
                WorkloadKind::Job,
                &obj,
                &rollup(Phase::Admitted, "Admitted")
            ),
            Outcome::Working { .. }
        ));
        assert_eq!(
            outcome(
                WorkloadKind::Job,
                &obj,
                &rollup(Phase::Finished, "Succeeded")
            ),
            Outcome::Completed {
                failed: false,
                message: "msg".into()
            }
        );
        assert!(matches!(
            outcome(
                WorkloadKind::RayJob,
                &obj,
                &rollup(Phase::Finished, "Failed")
            ),
            Outcome::Completed { failed: true, .. }
        ));
    }

    #[test]
    fn a_deployment_completes_once_every_replica_is_admitted_and_ready() {
        let partly = json!({"spec": {"replicas": 3}, "status": {"readyReplicas": 2}});
        let fully = json!({"spec": {"replicas": 3}, "status": {"readyReplicas": 3}});
        let admitted = rollup(Phase::Admitted, "Admitted");
        assert!(matches!(
            outcome(WorkloadKind::Deployment, &partly, &admitted),
            Outcome::Working { .. }
        ));
        assert!(matches!(
            outcome(WorkloadKind::Deployment, &fully, &admitted),
            Outcome::Completed { failed: false, .. }
        ));
        assert!(matches!(
            outcome(
                WorkloadKind::Deployment,
                &fully,
                &rollup(Phase::Pending, "Pending")
            ),
            Outcome::Working { .. }
        ));
        let zero = json!({"spec": {"replicas": 0}, "status": {}});
        assert!(matches!(
            outcome(WorkloadKind::StatefulSet, &zero, &admitted),
            Outcome::Working { .. }
        ));
    }

    #[test]
    fn a_cancelled_task_stays_cancelled_whatever_kueue_says() {
        let obj = json!({"metadata": {"annotations": {CANCELLED_ANNOTATION: "true"}}});
        assert_eq!(
            outcome(
                WorkloadKind::Job,
                &obj,
                &rollup(Phase::Finished, "Succeeded")
            ),
            Outcome::Cancelled
        );
    }

    #[test]
    fn a_recorded_completion_survives_a_later_scale_down() {
        let obj = json!({
            "metadata": {"annotations": {COMPLETED_ANNOTATION: "3/3 replicas admitted and ready"}},
            "spec": {"replicas": 0},
            "status": {}
        });
        assert_eq!(
            outcome(
                WorkloadKind::Deployment,
                &obj,
                &rollup(Phase::NoWorkload, "")
            ),
            Outcome::Completed {
                failed: false,
                message: "3/3 replicas admitted and ready".into()
            }
        );
    }
}
