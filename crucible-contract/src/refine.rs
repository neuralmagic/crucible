//! The refine loop's trail: what each scope round tried, cost, and how it was judged.
//!
//! The ordered `Vec<RoundRecord>` is frozen into `SCOPE.md` (on success) or `REJECTED.md` (on
//! exhaustion) as one fenced JSON block, which [`render_rounds_json`] writes and [`parse_rounds`]
//! reads. Both sides of the boundary use these types, so a checkpoint UI reads a trail without
//! redeclaring its schema.

use serde::{Deserialize, Serialize};

/// Which kind of agent turn produced a round: the first is the ordinary propose turn, every one
/// after is a refine turn seeded with the prior round's failure evidence, and an `Adversary` round
/// is the read-only gaming-review turn run after a pack validates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundKind {
    Propose,
    Refine,
    Adversary,
}

impl RoundKind {
    pub fn label(self) -> &'static str {
        match self {
            RoundKind::Propose => "propose",
            RoundKind::Refine => "refine",
            RoundKind::Adversary => "adversary",
        }
    }
}

/// One round of the refine loop: the turn that ran, the `[judge]` block it left on disk, the turn
/// cost, and the validation verdict. The full ordered `Vec<RoundRecord>` is the trail written into
/// `SCOPE.md` (on success) or `REJECTED.md` (on exhaustion).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoundRecord {
    /// 1-based round number.
    pub round: u32,
    pub kind: RoundKind,
    /// The `[judge]`/`[judge.selftest]` region of the manifest this round left on disk (verbatim
    /// TOML), or empty if the turn wrote no parseable manifest.
    pub judge_block: String,
    /// The turn's cost in USD (0.0 for the scripted `command` backend).
    pub cost: f64,
    pub outcome: RoundOutcome,
}

/// A round's validation verdict: it either passed (loop freezes, or the gaming review proceeds),
/// failed with concrete evidence (loop refines, or rejects if rounds are exhausted), or, a
/// gaming-review-only outcome, errored because the adversary turn's output didn't parse as a
/// verdict at all. `Error` is deliberately distinct from `Failed`: there is no evidence to refine
/// on, only a malformed turn, and the pack must NOT freeze on it (fail-closed, never treated as a
/// pass).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum RoundOutcome {
    Passed,
    Failed { evidence: FailureEvidence },
    Error { detail: String },
}

/// Why a round's pack failed validation, tagged by the stage that rejected it. This is what the
/// refine turn is handed verbatim, concrete enough to diagnose from, structured enough for the
/// controller's checkpoint UI to render.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum FailureEvidence {
    /// Nothing usable to validate: no manifest written, it didn't parse, no `[judge.selftest]`, or
    /// the proposed `runs` is below the pipeline's floor. The refiner has to fix the pack's shape
    /// before the gate can even be measured.
    Structure { detail: String },
    /// The measure contract probe (`crucible check`'s `check_measure_once`) rejected the gate: a
    /// nonzero exit, no JSON contract line, or a malformed one, with the tail of the gate's own
    /// stderr so the refiner can see why it blew up.
    Contract {
        findings: Vec<String>,
        stderr_tail: Vec<String>,
    },
    /// The self-test ran end to end but the gate didn't discriminate: the good control wasn't
    /// strictly better than the bad one (per `direction`), or a reading came back invalid.
    Selftest(SelftestEvidence),
    /// The adversarial gaming-review turn found concrete ways an optimizing agent
    /// could score well without genuinely addressing the goal. Feeds exactly one additional
    /// refine round; still-concerns after that round is terminal (REJECTED, not another refine).
    Adversary { attacks: Vec<Attack> },
}

/// One concrete way an optimizing agent could game the measurement, per the adversary's
/// checklist. `narrative` names the exact mechanism; `suggestion` is the concrete
/// fix to the manifest/gate/controls that would close it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attack {
    pub kind: AttackKind,
    pub narrative: String,
    pub suggestion: String,
}

/// Which of the adversary's five checklist categories an attack falls under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AttackKind {
    SelfReport,
    UncountedPath,
    Boundary,
    SelftestPair,
    FrozenLeak,
}

/// A serde-friendly snapshot of a self-test that failed to discriminate: the direction and run
/// count it was judged by, plus each control's readings and mean.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelftestEvidence {
    /// `"higher"` or `"lower"`, how the controls were compared.
    pub direction: String,
    pub runs: u32,
    pub good: ControlEvidence,
    pub bad: ControlEvidence,
}

/// One control's evidence: the staging command, its per-run readings, the mean, and whether every
/// reading was valid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlEvidence {
    pub cmd: String,
    pub mean: f64,
    pub all_valid: bool,
    pub readings: Vec<ReadingEvidence>,
}

/// A single measurement, as the refiner should see it: valid flag, score (absent when the gate
/// reported invalid), and the note the gate emitted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadingEvidence {
    pub valid: bool,
    pub score: Option<f64>,
    pub note: String,
}

/// The fenced-JSON trail block for a `SCOPE.md`/`REJECTED.md` section: a ```json block holding the
/// ordered `Vec<RoundRecord>`. Pretty-printed for a human skimming the frozen report.
pub fn render_rounds_json(records: &[RoundRecord]) -> String {
    let body = serde_json::to_string_pretty(records)
        .unwrap_or_else(|e| format!("[/* round trail failed to serialize: {e} */]"));
    format!("```json\n{body}\n```")
}

/// The inverse of [`render_rounds_json`]: pull the round records back out of a rendered fenced
/// block.
pub fn parse_rounds(fenced: &str) -> Result<Vec<RoundRecord>, serde_json::Error> {
    let inner = fenced
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    crate::json::from_str(inner)
}

impl FailureEvidence {
    /// A human-and-agent-readable summary of the failure, embedded in the refine prompt and the
    /// per-round bullet lines. Concrete numbers, no JSON noise, the machine-readable copy rides
    /// the fenced trail alongside it.
    pub fn describe(&self) -> String {
        match self {
            FailureEvidence::Structure { detail } => {
                format!("STRUCTURE — the pack isn't shaped to validate: {detail}")
            }
            FailureEvidence::Contract {
                findings,
                stderr_tail,
            } => {
                let mut s = format!(
                    "CONTRACT — the measure gate failed its contract probe:\n  - {}",
                    findings.join("\n  - ")
                );
                if !stderr_tail.is_empty() {
                    s.push_str(&format!(
                        "\n  measure_cmd stderr (last {} line(s)):\n{}",
                        stderr_tail.len(),
                        stderr_tail
                            .iter()
                            .map(|l| format!("    | {l}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                }
                s
            }
            FailureEvidence::Selftest(e) => {
                let control = |label: &str, c: &ControlEvidence| {
                    let reads = c
                        .readings
                        .iter()
                        .map(|r| match r.score {
                            _ if !r.valid => format!("INVALID({})", r.note),
                            Some(s) => format!("{s}"),
                            None => "no-score".to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(
                        "  {label}: cmd=`{}` mean={:.4} valid={} readings=[{reads}]",
                        c.cmd, c.mean, c.all_valid
                    )
                };
                format!(
                    "SELFTEST — the gate didn't discriminate ({} wins, {} run(s)); the good control \
                     must be STRICTLY better than the bad one and every reading valid:\n{}\n{}",
                    e.direction,
                    e.runs,
                    control("good", &e.good),
                    control("bad", &e.bad),
                )
            }
            FailureEvidence::Adversary { attacks } => {
                let lines = attacks
                    .iter()
                    .map(|a| {
                        format!(
                            "  - [{:?}] {}\n    suggestion: {}",
                            a.kind, a.narrative, a.suggestion
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "ADVERSARY — the gaming review found {} concrete way(s) an optimizing agent \
                     could win without addressing the goal:\n{lines}",
                    attacks.len()
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(outcome: RoundOutcome) -> RoundRecord {
        RoundRecord {
            round: 1,
            kind: RoundKind::Propose,
            judge_block: "[judge]\nmeasure_cmd = \"./m.sh\"".to_string(),
            cost: 0.25,
            outcome,
        }
    }

    #[test]
    fn every_outcome_survives_the_fenced_block() {
        let records = vec![
            record(RoundOutcome::Passed),
            record(RoundOutcome::Error {
                detail: "no verdict line".to_string(),
            }),
            record(RoundOutcome::Failed {
                evidence: FailureEvidence::Structure {
                    detail: "no manifest".to_string(),
                },
            }),
            record(RoundOutcome::Failed {
                evidence: FailureEvidence::Contract {
                    findings: vec!["exit 1".to_string()],
                    stderr_tail: vec!["boom".to_string()],
                },
            }),
            record(RoundOutcome::Failed {
                evidence: FailureEvidence::Adversary {
                    attacks: vec![Attack {
                        kind: AttackKind::SelftestPair,
                        narrative: "n".to_string(),
                        suggestion: "s".to_string(),
                    }],
                },
            }),
            record(RoundOutcome::Failed {
                evidence: FailureEvidence::Selftest(SelftestEvidence {
                    direction: "higher".to_string(),
                    runs: 3,
                    good: ControlEvidence {
                        cmd: "./good.sh".to_string(),
                        mean: 1.0,
                        all_valid: true,
                        readings: vec![ReadingEvidence {
                            valid: true,
                            score: Some(1.0),
                            note: "ok".to_string(),
                        }],
                    },
                    bad: ControlEvidence {
                        cmd: "./bad.sh".to_string(),
                        mean: 1.0,
                        all_valid: false,
                        readings: vec![ReadingEvidence {
                            valid: false,
                            score: None,
                            note: "invalid".to_string(),
                        }],
                    },
                }),
            }),
        ];
        let parsed = parse_rounds(&render_rounds_json(&records)).expect("round-trip");
        assert_eq!(parsed, records);
    }

    /// A trail frozen before the gaming review shipped: only Propose/Refine kinds, only
    /// Passed/Failed outcomes, no Adversary/Error variants anywhere. Must still parse under the
    /// current types (new enum variants are additive, never renamed/removed).
    #[test]
    fn parse_rounds_stays_backward_compatible_with_pre_adversary_trails() {
        let legacy = r#"[
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
        ]"#;
        let parsed = parse_rounds(legacy).expect("legacy trail parses under the current types");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].kind, RoundKind::Propose);
        assert_eq!(parsed[1].outcome, RoundOutcome::Passed);
    }

    fn selftest_ev() -> FailureEvidence {
        FailureEvidence::Selftest(SelftestEvidence {
            direction: "higher".to_string(),
            runs: 3,
            good: ControlEvidence {
                cmd: "stage-good".to_string(),
                mean: 10.0,
                all_valid: true,
                readings: vec![ReadingEvidence {
                    valid: true,
                    score: Some(10.0),
                    note: String::new(),
                }],
            },
            bad: ControlEvidence {
                cmd: "stage-bad".to_string(),
                mean: 100.0,
                all_valid: true,
                readings: vec![ReadingEvidence {
                    valid: true,
                    score: Some(100.0),
                    note: String::new(),
                }],
            },
        })
    }

    #[test]
    fn describe_selftest_quotes_the_numbers() {
        let d = selftest_ev().describe();
        assert!(d.contains("higher wins"), "{d}");
        assert!(d.contains("mean=10.0000"), "good mean quoted: {d}");
        assert!(d.contains("mean=100.0000"), "bad mean quoted: {d}");
        assert!(d.contains("stage-good") && d.contains("stage-bad"), "{d}");
    }

    #[test]
    fn describe_contract_includes_stderr_tail() {
        let d = FailureEvidence::Contract {
            findings: vec!["measure_cmd `./m.sh` exited nonzero: exit status: 1".to_string()],
            stderr_tail: vec!["line1".to_string(), "boom: not found".to_string()],
        }
        .describe();
        assert!(d.contains("exited nonzero"), "{d}");
        assert!(d.contains("boom: not found"), "stderr tail carried: {d}");
    }

    #[test]
    fn describe_adversary_lists_every_attack() {
        let d = FailureEvidence::Adversary {
            attacks: vec![
                Attack {
                    kind: AttackKind::UncountedPath,
                    narrative: "work moves to setup_cmd".to_string(),
                    suggestion: "time setup_cmd too".to_string(),
                },
                Attack {
                    kind: AttackKind::FrozenLeak,
                    narrative: "fixtures live in the editable workspace".to_string(),
                    suggestion: "freeze-inject the fixtures".to_string(),
                },
            ],
        }
        .describe();
        assert!(d.contains("2 concrete way"), "{d}");
        assert!(d.contains("work moves to setup_cmd"), "{d}");
        assert!(d.contains("freeze-inject the fixtures"), "{d}");
    }

    /// The tags a frozen trail carries are the wire, so a rename would strand every `SCOPE.md`
    /// already on disk.
    #[test]
    fn the_tags_are_pinned() {
        let json = serde_json::to_value(record(RoundOutcome::Failed {
            evidence: FailureEvidence::Selftest(SelftestEvidence {
                direction: "lower".to_string(),
                runs: 1,
                good: ControlEvidence {
                    cmd: "g".to_string(),
                    mean: 0.0,
                    all_valid: true,
                    readings: vec![],
                },
                bad: ControlEvidence {
                    cmd: "b".to_string(),
                    mean: 0.0,
                    all_valid: true,
                    readings: vec![],
                },
            }),
        }))
        .expect("serialize");
        assert_eq!(json["kind"], "propose");
        assert_eq!(json["outcome"]["result"], "failed");
        assert_eq!(json["outcome"]["evidence"]["stage"], "selftest");
        assert_eq!(
            serde_json::to_value(AttackKind::UncountedPath).expect("serialize"),
            "uncounted-path"
        );
    }
}
