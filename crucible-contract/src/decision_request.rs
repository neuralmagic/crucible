//! Decision requests: what a human-decided route opens with the orchestrator, what the orchestrator
//! answers when the run polls, and the evidence an approver decides on (RFC-0002 C-HUMAN-DECISION,
//! C-DECISION-EVIDENCE). The engine and the controller round-trip these same types, and both
//! compute the evidence digest with [`Evidence::digest`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::decision::{Label, Question, QuestionId};

/// The route output key the decision record is filed under, beside the per-question answers.
pub const DECISION_KEY: &str = "decision";

/// `POST`ed by the run when a human-decided route is dispatched. Opening the same run and task
/// again returns the request already open, with any answer recorded on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenRequest {
    pub task: String,
    pub questions: BTreeMap<QuestionId, Question>,
    pub evidence: Evidence,
    /// [`Evidence::digest`] of `evidence`.
    pub evidence_digest: String,
    /// Seconds from the first open until the request expires unanswered.
    pub timeout_secs: u64,
}

/// What the approver sees. Nothing a task did not declare.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    /// Each dependency of the route, as its join hands it to the route.
    pub inputs: BTreeMap<String, InputEvidence>,
    pub run: RunEvidence,
    /// The tasks each answer starts.
    pub gated: Vec<GatedTask>,
    /// The rendered review, CommonMark.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
    /// Each pick question's options, read from its source when the request opened.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub choices: BTreeMap<QuestionId, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputEvidence {
    /// The dependency's terminal status, in the task-result vocabulary.
    pub status: String,
    /// Its declared JSON output, where it passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    /// Its declared files, where it passed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEvidence {
    /// Workspace-relative, `/`-separated.
    pub path: String,
    pub media_type: String,
    /// Standard base64 of the file's bytes.
    pub base64: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEvidence {
    pub spent_usd: f64,
    pub elapsed_secs: u64,
    pub max_cost_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_time_secs: Option<u64>,
}

/// A task whose `when` names a question of the route, and the labels that dispatch it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatedTask {
    pub name: String,
    pub kind: String,
    pub needs: String,
    pub question: QuestionId,
    pub labels: Vec<Label>,
}

impl Evidence {
    /// `sha256:<hex>` over the canonical JSON encoding. Maps are ordered, so the encoding is a
    /// function of the evidence alone.
    pub fn digest(&self) -> Result<String, serde_json::Error> {
        let bytes = serde_json::to_vec(self)?;
        Ok(format!("sha256:{:x}", Sha256::digest(&bytes)))
    }
}

/// The request as the orchestrator holds it, returned by open and by every poll.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestState {
    pub id: String,
    pub evidence_digest: String,
    /// RFC 3339; counted from the first open and never extended.
    pub expires_at: String,
    #[serde(flatten)]
    pub status: RequestStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RequestStatus {
    Open,
    Answered {
        answer: AnswerRecord,
    },
    Expired,
    /// The run stopped while the request was open.
    Withdrawn,
}

/// The accepted answer, filed in the route's output under [`DECISION_KEY`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnswerRecord {
    /// Every question's chosen values: one label for a single choice, one or more labels for a
    /// multiple choice, one or more of the request's options for a pick.
    pub labels: BTreeMap<QuestionId, Vec<String>>,
    /// The deciding user principal, `user:<login>`.
    pub decided_by: String,
    /// RFC 3339.
    pub decided_at: String,
    pub evidence_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// What a person submits. The orchestrator checks it against the open request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubmitAnswer {
    pub labels: BTreeMap<QuestionId, Vec<String>>,
    pub evidence_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[cfg(test)]
mod tests {
    use crate::decision_request::*;

    fn evidence() -> Evidence {
        Evidence {
            inputs: BTreeMap::from([(
                "plan".to_string(),
                InputEvidence {
                    status: "pass".into(),
                    output: Some(serde_json::json!({"gpus": 8, "est_usd": 412.5})),
                    files: vec![FileEvidence {
                        path: "out/chart.html".into(),
                        media_type: "text/html".into(),
                        base64: "PGgxPmhpPC9oMT4=".into(),
                    }],
                },
            )]),
            run: RunEvidence {
                spent_usd: 0.12,
                elapsed_secs: 30,
                max_cost_usd: 5.0,
                max_time_secs: Some(3600),
            },
            gated: vec![GatedTask {
                name: "launch".into(),
                kind: "command".into(),
                needs: "any".into(),
                question: QuestionId::new("go").unwrap(),
                labels: vec![Label::new("approve").unwrap()],
            }],
            review: Some("# Launch 8 GPUs?".into()),
            choices: BTreeMap::from([(
                QuestionId::new("regions").unwrap(),
                vec!["us-east-1".to_string(), "eu-west-1".to_string()],
            )]),
        }
    }

    #[test]
    fn the_digest_is_stable_and_tracks_every_field() {
        let a = evidence();
        assert_eq!(a.digest().unwrap(), evidence().digest().unwrap());
        assert!(a.digest().unwrap().starts_with("sha256:"));
        let mut b = evidence();
        b.review = Some("# Launch 9 GPUs?".into());
        assert_ne!(a.digest().unwrap(), b.digest().unwrap());
        let mut c = evidence();
        c.run.spent_usd = 0.13;
        assert_ne!(a.digest().unwrap(), c.digest().unwrap());
        let mut d = evidence();
        d.choices.clear();
        assert_ne!(
            a.digest().unwrap(),
            d.digest().unwrap(),
            "the pick options are evidence"
        );
    }

    #[test]
    fn request_states_round_trip_with_a_flat_state_tag() {
        let answered = RequestState {
            id: "r1".into(),
            evidence_digest: "sha256:ab".into(),
            expires_at: "2026-10-06T18:00:00Z".into(),
            status: RequestStatus::Answered {
                answer: AnswerRecord {
                    labels: BTreeMap::from([(
                        QuestionId::new("go").unwrap(),
                        vec!["approve".to_string()],
                    )]),
                    decided_by: "user:wseaton".into(),
                    decided_at: "2026-10-06T17:00:00Z".into(),
                    evidence_digest: "sha256:ab".into(),
                    note: None,
                },
            },
        };
        let text = serde_json::to_string(&answered).unwrap();
        assert!(text.contains(r#""state":"answered""#), "{text}");
        assert_eq!(
            serde_json::from_str::<RequestState>(&text).unwrap(),
            answered
        );
        for status in [
            RequestStatus::Open,
            RequestStatus::Expired,
            RequestStatus::Withdrawn,
        ] {
            let state = RequestState {
                status,
                ..answered.clone()
            };
            let text = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<RequestState>(&text).unwrap(), state);
        }
    }

    #[test]
    fn evidence_omits_what_is_absent() {
        let mut e = evidence();
        e.review = None;
        if let Some(plan) = e.inputs.get_mut("plan") {
            plan.files.clear();
            plan.output = None;
        }
        let text = serde_json::to_string(&e).unwrap();
        assert!(!text.contains("review") && !text.contains("files") && !text.contains("output"));
    }
}
