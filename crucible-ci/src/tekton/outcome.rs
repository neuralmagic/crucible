//! How a PipelineRun ended, read from its `Succeeded` condition and results.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::tekton::model::{PipelineRun, succeeded};

const CANCELLED: [&str; 4] = [
    "Cancelled",
    "CancelledRunFinally",
    "StoppedRunFinally",
    "PipelineRunCancelled",
];

/// A PipelineRun's state.
#[derive(Clone, Debug, PartialEq)]
pub enum RunState {
    Running,
    Ended(Ended),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Ended {
    pub verdict: Verdict,
    pub note: String,
    pub results: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Passed,
    Failed,
    Cancelled,
}

pub fn run_state(run: &PipelineRun) -> RunState {
    let Some(status) = &run.status else {
        return RunState::Running;
    };
    let Some(cond) = succeeded(&status.conditions) else {
        return RunState::Running;
    };
    let reason = cond.reason.as_deref().unwrap_or_default();
    let verdict = match cond.status.as_str() {
        "True" => Verdict::Passed,
        "False" if CANCELLED.contains(&reason) => Verdict::Cancelled,
        "False" => Verdict::Failed,
        _ => return RunState::Running,
    };
    let note = match cond.message.as_deref() {
        Some(m) if !m.is_empty() => m.to_owned(),
        _ => reason.to_owned(),
    };
    RunState::Ended(Ended {
        verdict,
        note,
        results: status
            .results
            .iter()
            .map(|r| (r.name.clone(), r.value.clone()))
            .collect(),
    })
}

/// Why an ended run cannot give the results a watch declared.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("the run {run} ended without the result(s) {}", missing.join(", "))]
pub struct MissingResults {
    pub run: String,
    pub missing: Vec<String>,
}

pub fn declared_results(
    run: &str,
    ended: &Ended,
    declared: &[String],
) -> Result<BTreeMap<String, String>, MissingResults> {
    let mut found = BTreeMap::new();
    let mut missing = Vec::new();
    for name in declared {
        match ended.results.get(name) {
            Some(Value::String(v)) => {
                found.insert(name.clone(), v.clone());
            }
            Some(other) => {
                found.insert(name.clone(), other.to_string());
            }
            None => missing.push(name.clone()),
        }
    }
    match missing.is_empty() {
        true => Ok(found),
        false => Err(MissingResults {
            run: run.to_owned(),
            missing,
        }),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::tekton::model::{List, PipelineRun};
    use crate::tekton::outcome::{MissingResults, RunState, Verdict, declared_results, run_state};

    fn fixture(name: &str) -> PipelineRun {
        let raw = include_str!("../../testdata/tekton/pipelineruns.json");
        let list: List<PipelineRun> = serde_json::from_str(raw).expect("fixture parses");
        list.items
            .into_iter()
            .find(|r| r.metadata.name == name)
            .expect("fixture holds the run")
    }

    fn with_condition(status: &str, reason: &str, message: &str) -> PipelineRun {
        serde_json::from_value(json!({
            "metadata": {"name": "r"},
            "status": {"conditions": [{"type": "Succeeded", "status": status, "reason": reason, "message": message}]}
        }))
        .expect("run parses")
    }

    #[test]
    fn a_completed_run_passes_with_its_image_results() {
        let RunState::Ended(ended) =
            run_state(&fixture("odh-openshell-sandbox-openclaw-on-push-wtb9f"))
        else {
            panic!("the run completed");
        };
        assert_eq!(ended.verdict, Verdict::Passed);
        assert!(ended.note.starts_with("Tasks Completed: 21"));
        let declared = vec!["IMAGE_URL".to_owned(), "IMAGE_DIGEST".to_owned()];
        let results = declared_results("openclaw", &ended, &declared).expect("both results");
        assert_eq!(
            results.get("IMAGE_URL").map(String::as_str),
            Some("quay.io/opendatahub/odh-openshell-sandbox-openclaw:odh-stable")
        );
        assert!(
            results
                .get("IMAGE_DIGEST")
                .is_some_and(|d| d.starts_with("sha256:"))
        );
        let wanted = vec!["IMAGE_DIGEST".to_owned(), "SBOM".to_owned()];
        assert_eq!(
            declared_results("openclaw", &ended, &wanted),
            Err(MissingResults {
                run: "openclaw".to_owned(),
                missing: vec!["SBOM".to_owned()]
            })
        );
    }

    #[test]
    fn running_and_pending_runs_have_not_ended() {
        assert_eq!(
            run_state(&fixture("odh-openshell-gateway-on-push-wcsxc")),
            RunState::Running
        );
        assert_eq!(
            run_state(&fixture("early-gate-ci-build-c2k2h")),
            RunState::Running
        );
    }

    #[test]
    fn a_false_condition_fails_or_cancels_by_its_reason() {
        let verdict = |reason| match run_state(&with_condition("False", reason, "m")) {
            RunState::Ended(e) => Some(e.verdict),
            RunState::Running => None,
        };
        assert_eq!(verdict("Failed"), Some(Verdict::Failed));
        assert_eq!(verdict("PipelineRunTimeout"), Some(Verdict::Failed));
        assert_eq!(verdict("CouldntGetPipeline"), Some(Verdict::Failed));
        for reason in [
            "Cancelled",
            "CancelledRunFinally",
            "StoppedRunFinally",
            "PipelineRunCancelled",
        ] {
            assert_eq!(verdict(reason), Some(Verdict::Cancelled), "{reason}");
        }
        let RunState::Ended(e) = run_state(&with_condition("False", "PipelineRunTimeout", ""))
        else {
            panic!("ended");
        };
        assert_eq!(e.note, "PipelineRunTimeout");
    }
}
