//! MCP tasks (`io.modelcontextprotocol/tasks`) over batch Kueue workloads. A task id
//! names the submitted object including its UID, so the id is unguessable and a recreated object
//! of the same name is a different task. Task state is derived from the cluster on every
//! `tasks/get`; nothing is held in the server. Serving kinds are not tasks: they never finish, and
//! report readiness through `workload_status`.

use crate::manifest::WorkloadKind;
use crate::status::{Phase, Rollup};
use serde_json::{Value, json};

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
            .find(|k| k.kind() == kind_str && k.is_batch())?;
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

/// The error for a task whose object no longer exists. A finished batch workload is deleted after
/// its TTL, `default_ttl` unless its manifest set one, and its outcome with it.
pub fn gone(t: &TaskRef, default_ttl: Option<i64>) -> String {
    let default = default_ttl
        .map(|s| format!(" (default {s}s)"))
        .unwrap_or_default();
    format!(
        "task {}: {} {} no longer exists. It finished and was deleted after its ttlSecondsAfterFinished{default}, or it was deleted; its outcome is gone",
        t.encode(),
        t.kind.kind(),
        t.name
    )
}

pub fn replaced(t: &TaskRef) -> String {
    format!(
        "task {}: {} {} was deleted and created again under the same name; this task's outcome is gone",
        t.encode(),
        t.kind.kind(),
        t.name
    )
}

/// A completed task's result text. A failed workload is a tool error with status `failed`.
pub fn completed_body(failed: bool, message: &str, report: Value) -> Value {
    json!({
        "status": if failed { "failed" } else { "ok" },
        "outcome": if failed { "failed" } else { "succeeded" },
        "message": message,
        "report": report,
    })
}

fn annotation<'a>(object: &'a Value, key: &str) -> Option<&'a str> {
    object
        .pointer("/metadata/annotations")
        .and_then(|a| a.get(key))
        .and_then(Value::as_str)
}

/// A batch workload's task completes when Kueue marks its Workload finished; it failed unless the
/// Workload succeeded. `object` is the full object, whose `cancelled` annotation records
/// cancellation.
pub fn outcome(object: &Value, rollup: &Rollup, cancelled: &str) -> Outcome {
    if annotation(object, cancelled).is_some() {
        return Outcome::Cancelled;
    }
    let head = &rollup.headline;
    if head.phase == Phase::Finished {
        return Outcome::Completed {
            failed: head.reason != "Succeeded",
            message: head.message.clone(),
        };
    }
    Outcome::Working {
        message: format!("{}: {}", head.phase.as_str(), head.message),
    }
}

#[cfg(test)]
mod tests {
    use crate::manifest::WorkloadKind;
    use crate::status::{PhaseReport, Rollup};
    use crate::task::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    const CANCELLED: &str = "crucible.dev/task-cancelled";

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
            kind: WorkloadKind::RayJob,
            name: "train".into(),
            uid: "6f1c-uid".into(),
        };
        assert_eq!(t.encode(), "team-a/RayJob/train/6f1c-uid");
        assert_eq!(TaskRef::parse(&t.encode()), Some(t));
        let legacy = TaskRef::parse("Job/train/6f1c-uid").unwrap();
        assert_eq!(legacy.namespace, None);
        assert_eq!(legacy.encode(), "Job/train/6f1c-uid");
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
            "team-a/Deployment/svc/u",
            "StatefulSet/db/u",
            "team-a/LeaderWorkerSet/serve/u",
        ] {
            assert_eq!(TaskRef::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_job_completes_when_kueue_finishes_it_and_fails_unless_it_succeeded() {
        let obj = json!({});
        assert!(matches!(
            outcome(&obj, &rollup(Phase::Admitted, "Admitted"), CANCELLED),
            Outcome::Working { .. }
        ));
        assert_eq!(
            outcome(&obj, &rollup(Phase::Finished, "Succeeded"), CANCELLED),
            Outcome::Completed {
                failed: false,
                message: "msg".into()
            }
        );
        assert!(matches!(
            outcome(&obj, &rollup(Phase::Finished, "Failed"), CANCELLED),
            Outcome::Completed { failed: true, .. }
        ));
    }

    #[test]
    fn a_gone_task_says_it_was_deleted_after_its_ttl() {
        let t = TaskRef::parse("team-a/Job/train/6f1c-uid").unwrap();
        let msg = gone(&t, Some(3600));
        assert!(
            msg.starts_with("task team-a/Job/train/6f1c-uid: Job train no longer exists"),
            "{msg}"
        );
        assert!(
            msg.contains("ttlSecondsAfterFinished (default 3600s)"),
            "{msg}"
        );
        assert!(!msg.contains("unknown task"), "{msg}");
        assert!(!gone(&t, None).contains("default"), "{}", gone(&t, None));
        assert!(replaced(&t).contains("created again under the same name"));
    }

    #[test]
    fn a_completed_body_never_says_ok_for_a_failed_workload() {
        let failed = completed_body(true, "BackoffLimitExceeded", json!({}));
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["outcome"], "failed");
        let ok = completed_body(false, "done", json!({"phase": "finished"}));
        assert_eq!(ok["status"], "ok");
        assert_eq!(ok["outcome"], "succeeded");
        assert_eq!(ok["report"]["phase"], "finished");
    }

    #[test]
    fn a_cancelled_task_stays_cancelled_whatever_kueue_says() {
        let obj = json!({"metadata": {"annotations": {CANCELLED: "true"}}});
        assert_eq!(
            outcome(&obj, &rollup(Phase::Finished, "Succeeded"), CANCELLED),
            Outcome::Cancelled
        );
    }
}
