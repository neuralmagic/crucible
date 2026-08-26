//! The scope pipeline's result, as the `--json` document and the `SCOPE_REPORT_MARKER` line carry
//! it.
//!
//! The engine writes it and a controller reads it, so it deserializes as well as it serializes and
//! every field is constructible by either side.

use serde::{Deserialize, Serialize};

use crate::refine::RoundRecord;

/// Which stage of `ingest [-> propose] -> validate -> freeze` a result belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageName {
    Ingest,
    Propose,
    Validate,
    Freeze,
}

impl StageName {
    pub const fn as_str(self) -> &'static str {
        match self {
            StageName::Ingest => "ingest",
            StageName::Propose => "propose",
            StageName::Validate => "validate",
            StageName::Freeze => "freeze",
        }
    }
}

impl std::fmt::Display for StageName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One stage's result, independent of how the pipeline renders it (console lines or one JSON
/// object).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageResult {
    pub name: StageName,
    pub passed: bool,
    pub detail: String,
}

/// The whole pipeline's result as one JSON object, for `--json`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ScopeReport {
    pub stages: Vec<StageResult>,
    pub digest: Option<String>,
    /// The `--propose` turn's cost (USD), summed across refine rounds; `None` outside `--propose`.
    pub cost: Option<f64>,
    /// The refine loop's per-round trail; empty outside `--propose`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rounds: Vec<RoundRecord>,
    /// The turns' preserved session NDJSON. Never serialized into the report JSON (it can be MBs);
    /// it rides its own delivery path, the `--marker` transcript line or `--transcript-out`.
    #[serde(skip)]
    pub transcript: String,
}

impl ScopeReport {
    /// Whether every stage that ran passed. An empty report is not a pass: the pipeline always
    /// records at least the ingest stage.
    pub fn passed(&self) -> bool {
        !self.stages.is_empty() && self.stages.iter().all(|s| s.passed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refine::{RoundKind, RoundOutcome};

    fn report() -> ScopeReport {
        ScopeReport {
            stages: vec![
                StageResult {
                    name: StageName::Ingest,
                    passed: true,
                    detail: "goal from issue".to_string(),
                },
                StageResult {
                    name: StageName::Freeze,
                    passed: false,
                    detail: "SCOPE.md exists".to_string(),
                },
            ],
            digest: Some("sha256:abc".to_string()),
            cost: Some(1.5),
            rounds: vec![RoundRecord {
                round: 1,
                kind: RoundKind::Propose,
                judge_block: "[judge]".to_string(),
                cost: 1.5,
                outcome: RoundOutcome::Passed,
            }],
            transcript: "{}\n".to_string(),
        }
    }

    #[test]
    fn a_report_decodes_and_keeps_its_rounds() {
        let encoded = serde_json::to_string(&report()).expect("serialize");
        let decoded: ScopeReport = crate::json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded.stages, report().stages);
        assert_eq!(decoded.digest.as_deref(), Some("sha256:abc"));
        assert_eq!(decoded.cost, Some(1.5));
        assert_eq!(decoded.rounds, report().rounds);
        // The transcript rides its own delivery path, so it is absent from the document by design.
        assert!(decoded.transcript.is_empty());
    }

    /// The bytes a deployed engine already emits. Decoding was added without moving them, so an
    /// engine and a controller on either side of this change still agree.
    #[test]
    fn the_document_shape_is_unchanged() {
        let json = serde_json::to_value(report()).expect("serialize");
        assert_eq!(json["stages"][0]["name"], "ingest");
        assert_eq!(json["stages"][0]["passed"], true);
        assert_eq!(json["stages"][1]["name"], "freeze");
        assert_eq!(json["digest"], "sha256:abc");
        assert_eq!(json["cost"], 1.5);
        assert_eq!(json["rounds"][0]["kind"], "propose");
        assert!(json.get("transcript").is_none());

        let mut minimal = ScopeReport::default();
        minimal.stages.push(StageResult {
            name: StageName::Validate,
            passed: true,
            detail: "ok".to_string(),
        });
        assert_eq!(
            serde_json::to_string(&minimal).expect("serialize"),
            r#"{"stages":[{"name":"validate","passed":true,"detail":"ok"}],"digest":null,"cost":null}"#
        );
    }

    /// An empty `rounds` stays absent, the shape a non-propose scope has always emitted.
    #[test]
    fn an_empty_trail_is_omitted() {
        let json = serde_json::to_value(ScopeReport::default()).expect("serialize");
        assert!(json.get("rounds").is_none());
        let decoded: ScopeReport = crate::json::from_str(&json.to_string()).expect("deserialize");
        assert!(decoded.rounds.is_empty());
        assert!(!decoded.passed());
    }

    #[test]
    fn passed_is_every_stage() {
        let mut r = report();
        assert!(!r.passed());
        r.stages[1].passed = true;
        assert!(r.passed());
    }
}
