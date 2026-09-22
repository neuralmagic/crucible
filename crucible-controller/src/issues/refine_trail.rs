//! The measurement refine trail, as read back for a scope pack's approval evidence. `GET
//! /api/approvals/{scope_id}/evidence` serves this: the ordered round-by-round record a
//! propose/refine/adversary pipeline left behind, parsed straight out of the frozen `SCOPE.md`.
//!
//! Schema is MIRRORED from `crucible/src/refine.rs` (the controller can't depend on the `crucible`
//! bin crate — same decoupling `export.rs`'s parquet mirror and `ingest.rs`'s session-log wire shape
//! use). Every type here — [`RoundRecord`], [`RoundKind`], [`RoundOutcome`], [`FailureEvidence`],
//! [`Attack`], [`AttackKind`], [`SelftestEvidence`], [`ControlEvidence`], [`ReadingEvidence`] —
//! must keep field names, tags, and variants byte-identical to their `refine.rs` counterparts, since
//! [`extract_trail`] deserializes the exact same fenced JSON `refine::render_rounds_json` wrote.
//! Any change to `refine.rs`'s wire shape must be mirrored here (and vice-versa) or the approval UI
//! silently stops parsing packs written after the drift.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Mirrors `crucible::refine::RoundKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RoundKind {
    Propose,
    Refine,
    Adversary,
}

/// Mirrors `crucible::refine::RoundRecord`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct RoundRecord {
    round: u32,
    pub(crate) kind: RoundKind,
    judge_block: String,
    cost: f64,
    pub(crate) outcome: RoundOutcome,
}

/// Mirrors `crucible::refine::RoundOutcome`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum RoundOutcome {
    Passed,
    Failed { evidence: FailureEvidence },
    Error { detail: String },
}

/// Mirrors `crucible::refine::FailureEvidence`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum FailureEvidence {
    Structure {
        detail: String,
    },
    Contract {
        findings: Vec<String>,
        stderr_tail: Vec<String>,
    },
    Selftest(SelftestEvidence),
    Adversary {
        attacks: Vec<Attack>,
    },
}

/// Mirrors `crucible::refine::Attack`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Attack {
    kind: AttackKind,
    narrative: String,
    suggestion: String,
}

/// Mirrors `crucible::refine::AttackKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AttackKind {
    SelfReport,
    UncountedPath,
    Boundary,
    SelftestPair,
    FrozenLeak,
}

/// Mirrors `crucible::refine::SelftestEvidence`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SelftestEvidence {
    direction: String,
    runs: u32,
    good: ControlEvidence,
    bad: ControlEvidence,
}

/// Mirrors `crucible::refine::ControlEvidence`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ControlEvidence {
    cmd: String,
    mean: f64,
    all_valid: bool,
    readings: Vec<ReadingEvidence>,
}

/// Mirrors `crucible::refine::ReadingEvidence`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ReadingEvidence {
    valid: bool,
    score: Option<f64>,
    note: String,
}

/// The approval evidence for one scope pack: its `scopes.check_outcome` (the measure contract probe's
/// plain verdict string) plus the full round trail. `rounds` is empty for a hand-authored pack
/// frozen before the refine loop shipped, or a propose-pipeline pack whose `SCOPE.md` carries no
/// fenced trail for any other reason — never an error; there's simply nothing to show.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ScopeEvidenceDto {
    pub(crate) scope_id: i64,
    pub(crate) check_outcome: Option<String>,
    pub(crate) rounds: Vec<RoundRecord>,
}

/// Mirrors `crucible::scope::StageResult` — one stage of the scope pipeline
/// (ingest/propose/validate/freeze), as stored in `scope_reports.report_json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ScopeStage {
    name: String,
    passed: bool,
    detail: String,
}

/// The stored `crucible scope --json` object (`scope_reports.report_json`), decoded on read.
/// `rounds` is `skip_serializing_if = "Vec::is_empty"` on the engine side, so it defaults here.
#[derive(Debug, Clone, Default, Deserialize)]
struct StoredScopeReport {
    #[serde(default)]
    stages: Vec<ScopeStage>,
    digest: Option<String>,
    cost: Option<f64>,
    #[serde(default)]
    rounds: Vec<RoundRecord>,
}

/// `GET /api/issues/{key}/scope-report`: the latest scope turn's structured report for one issue —
/// per-stage pass/fail, the refine round trail, and the dispatch context (which pod ran it) an
/// operator correlates with cluster logs. Tolerant of wire drift the same way [`extract_trail`]
/// is: a stored JSON this mirror can no longer decode yields empty `stages`/`rounds`, never an
/// error — `survived` and the park reason still tell the story.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ScopeReportDto {
    issue_key: String,
    /// The scope turn's work pod, or null for the local subprocess executor.
    pod_name: Option<String>,
    survived: bool,
    created_at: String,
    stages: Vec<ScopeStage>,
    digest: Option<String>,
    /// The turn's total cost (USD), summed across refine rounds.
    cost: Option<f64>,
    rounds: Vec<RoundRecord>,
}

impl ScopeReportDto {
    /// Assemble the DTO from a stored row, decoding `report_json` tolerantly.
    pub(crate) fn from_row(row: crate::issues::model::ScopeReportRow) -> ScopeReportDto {
        let report: StoredScopeReport =
            crucible_contract::json::from_str(&row.report_json).unwrap_or_default();
        ScopeReportDto {
            issue_key: row.issue_key,
            pod_name: row.pod_name,
            survived: row.survived,
            created_at: row.created_at,
            stages: report.stages,
            digest: report.digest,
            cost: report.cost,
            rounds: report.rounds,
        }
    }
}

/// Pull the round trail back out of a rendered `SCOPE.md`/`REJECTED.md`: find the fenced ```` ```json
/// ```` block `refine::render_rounds_json` wrote and deserialize it. Tolerant by design — a pack
/// with no fenced block (no `[refine loop]` section at all) or one that fails to parse (an older,
/// incompatible shape) yields an empty trail rather than an error, since the approval's job is to show
/// whatever provenance exists, not to gate on it.
pub(crate) fn extract_trail(scope_md: &str) -> Vec<RoundRecord> {
    let Some(start) = scope_md.find("```json") else {
        return Vec::new();
    };
    let after_fence = &scope_md[start + "```json".len()..];
    let Some(end) = after_fence.find("```") else {
        return Vec::new();
    };
    let body = after_fence[..end].trim();
    crucible_contract::json::from_str(body).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"# SCOPE.md

Some prose the propose turn wrote.

## Refine loop

**Round trail:**

```json
[
  {
    "round": 1,
    "kind": "propose",
    "judge_block": "[judge]\nmeasure_cmd = \"./m.sh\"\ndirection = \"higher\"",
    "cost": 0.0,
    "outcome": {
      "result": "failed",
      "evidence": {
        "stage": "selftest",
        "direction": "higher",
        "runs": 3,
        "good": {
          "cmd": "stage-good",
          "mean": 10.0,
          "all_valid": true,
          "readings": [{"valid": true, "score": 10.0, "note": ""}]
        },
        "bad": {
          "cmd": "stage-bad",
          "mean": 100.0,
          "all_valid": true,
          "readings": [{"valid": true, "score": 100.0, "note": ""}]
        }
      }
    }
  },
  {
    "round": 2,
    "kind": "adversary",
    "judge_block": "",
    "cost": 0.02,
    "outcome": {
      "result": "failed",
      "evidence": {
        "stage": "adversary",
        "attacks": [
          {"kind": "uncounted-path", "narrative": "n", "suggestion": "s"}
        ]
      }
    }
  }
]
```

More prose after the fence.
"#;

    #[test]
    fn extract_trail_parses_the_fenced_round_trip() {
        let rounds = extract_trail(FIXTURE);
        assert_eq!(rounds.len(), 2, "both rounds parsed");
        assert_eq!(rounds[0].kind, RoundKind::Propose);
        assert!(matches!(
            rounds[0].outcome,
            RoundOutcome::Failed {
                evidence: FailureEvidence::Selftest(_)
            }
        ));
        assert_eq!(rounds[1].kind, RoundKind::Adversary);
        match &rounds[1].outcome {
            RoundOutcome::Failed {
                evidence: FailureEvidence::Adversary { attacks },
            } => {
                assert_eq!(attacks.len(), 1);
                assert_eq!(attacks[0].kind, AttackKind::UncountedPath);
            }
            other => panic!("expected an adversary failure, got {other:?}"),
        }
    }

    #[test]
    fn extract_trail_is_empty_for_a_hand_authored_pack() {
        let scope_md = "# SCOPE.md\n\nNo refine loop here, just prose.\n";
        assert_eq!(extract_trail(scope_md), Vec::new());
    }

    #[test]
    fn extract_trail_is_empty_for_a_malformed_fence() {
        let scope_md = "```json\nnot valid json at all\n```\n";
        assert_eq!(extract_trail(scope_md), Vec::new());
    }

    #[test]
    fn scope_report_dto_decodes_a_stored_failure_report() {
        let row = crate::issues::model::ScopeReportRow {
            id: 1,
            issue_key: "o/r#7".into(),
            pod_name: Some("crucible-scope-o-r-7".into()),
            survived: false,
            report_json: r#"{
                "stages": [
                    {"name": "ingest", "passed": true, "detail": "goal from issue"},
                    {"name": "propose", "passed": false, "detail": "scope refine exhausted 3 round(s)"}
                ],
                "digest": null,
                "cost": 0.0,
                "rounds": [
                    {
                        "round": 1,
                        "kind": "propose",
                        "judge_block": "",
                        "cost": 0.0,
                        "outcome": {
                            "result": "failed",
                            "evidence": {"stage": "structure", "detail": "no crucible.toml was written"}
                        }
                    }
                ]
            }"#
            .into(),
            created_at: "2026-07-05T00:00:00Z".into(),
        };
        let dto = ScopeReportDto::from_row(row);
        assert_eq!(dto.stages.len(), 2);
        assert!(!dto.stages[1].passed);
        assert_eq!(dto.rounds.len(), 1);
        assert!(matches!(
            &dto.rounds[0].outcome,
            RoundOutcome::Failed {
                evidence: FailureEvidence::Structure { detail }
            } if detail == "no crucible.toml was written"
        ));
        assert_eq!(dto.pod_name.as_deref(), Some("crucible-scope-o-r-7"));
        assert!(!dto.survived);
    }

    #[test]
    fn scope_report_dto_is_empty_not_an_error_for_garbled_json() {
        let row = crate::issues::model::ScopeReportRow {
            id: 2,
            issue_key: "o/r#8".into(),
            pod_name: None,
            survived: true,
            report_json: "not json".into(),
            created_at: "2026-07-05T00:00:00Z".into(),
        };
        let dto = ScopeReportDto::from_row(row);
        assert!(dto.stages.is_empty());
        assert!(dto.rounds.is_empty());
        assert!(dto.survived, "the denormalized verdict survives the drift");
    }

    #[test]
    fn scope_report_dto_defaults_absent_rounds() {
        let row = crate::issues::model::ScopeReportRow {
            id: 3,
            issue_key: "o/r#9".into(),
            pod_name: None,
            survived: true,
            report_json: r#"{"stages": [{"name": "freeze", "passed": true, "detail": "v1:abc"}], "digest": "v1:abc", "cost": null}"#.into(),
            created_at: "2026-07-05T00:00:00Z".into(),
        };
        let dto = ScopeReportDto::from_row(row);
        assert_eq!(dto.digest.as_deref(), Some("v1:abc"));
        assert!(dto.rounds.is_empty());
    }

    #[test]
    fn extract_trail_round_trips_the_pre_adversary_compat_shape() {
        // A trail frozen before the adversarial gaming review shipped: only propose/refine kinds and
        // passed/failed outcomes, exactly the shape `refine.rs`'s own backward-compat test freezes.
        let scope_md = r#"```json
[
    {
        "round": 1,
        "kind": "propose",
        "judge_block": "[judge]\nmeasure_cmd = \"./m.sh\"",
        "cost": 0.0,
        "outcome": {
            "result": "failed",
            "evidence": {
                "stage": "contract",
                "findings": ["boom"],
                "stderr_tail": []
            }
        }
    },
    {
        "round": 2,
        "kind": "refine",
        "judge_block": "[judge]\nmeasure_cmd = \"./m.sh\"",
        "cost": 0.01,
        "outcome": { "result": "passed" }
    }
]
```"#;
        let rounds = extract_trail(scope_md);
        assert_eq!(rounds.len(), 2);
        assert_eq!(rounds[0].kind, RoundKind::Propose);
        assert!(matches!(
            rounds[0].outcome,
            RoundOutcome::Failed {
                evidence: FailureEvidence::Contract { .. }
            }
        ));
        assert_eq!(rounds[1].kind, RoundKind::Refine);
        assert!(matches!(rounds[1].outcome, RoundOutcome::Passed));
    }
}
