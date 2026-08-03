//! The measurement refine loop: the strongly-typed evidence a failed
//! validation hands back to a bounded retry turn, and the per-round record trail that both the
//! refine prompt and the frozen `SCOPE.md`/`REJECTED.md` are rendered from.
//!
//! Ownership split: this module holds the *types* + the pure rendering (prompt text, the fenced
//! JSON trail, the human summaries) and nothing that touches an agent or the filesystem, the
//! loop driver itself lives in `scope.rs`, where the propose plumbing (scratch checkout, seed
//! context, `agent::run_turn`) already sits. Keeping the records here means `scope.rs` stays about
//! orchestration and this stays trivially unit-testable; the controller's checkpoint UI mirrors these same
//! [`RoundRecord`] structs (the controller's `refine_trail` module) to deserialize straight out
//! of the `SCOPE.md` fenced block, since the controller can't depend back on this bin crate.

use crate::selftest::SelftestReport;
use serde::{Deserialize, Serialize};

/// The engine-embedded refine prompt: seeded from `scope-propose.md`'s contract sections, focused
/// on diagnosing and fixing a gate that didn't validate, and explicit that weakening the gate to
/// pass (inverting direction, making the controls trivially different) is not a fix.
pub(crate) const SCOPE_REFINE_PROMPT: &str = include_str!("prompts/scope-refine.md");

/// The `{{GOAL_GUARD}}` block: keep `goal.md` de-prescribed (the default), or, for an
/// authoritative brief, preserve its prescriptions and fix the gate instead.
pub(crate) const GOAL_GUARD: &str = include_str!("prompts/scope-refine-goal-guard.md");
pub(crate) const GOAL_GUARD_AUTHORITATIVE: &str =
    include_str!("prompts/scope-refine-goal-guard-authoritative.md");

/// The `{{MEASURE_MODE}}` block: hold the drafted local T0/T1 shape (the default), or hold the
/// broker-measured shape, whose gate validation never executed because it needs GPU hardware.
const MEASURE_MODE: &str = include_str!("prompts/scope-refine-measure-local.md");
const MEASURE_MODE_BROKER: &str = include_str!("prompts/scope-refine-measure-broker.md");

/// The engine-embedded adversarial gaming-review prompt: a read-only
/// turn that attacks a pack that already passed validation, looking for ways an optimizing agent
/// could score well without genuinely addressing the goal.
const JUDGE_ADVERSARY_PROMPT: &str = include_str!("prompts/judge-adversary.md");

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

/// The adversary turn's output contract: the single JSON verdict its last line must carry. `Pass`
/// means no attacks found; `Concerns` carries the attack list `FailureEvidence::Adversary` wraps
/// for the refine turn. Untagged on `verdict` so `{"verdict":"pass"}` and
/// `{"verdict":"concerns","attacks":[...]}` both decode directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum AdversaryVerdict {
    Pass,
    Concerns { attacks: Vec<Attack> },
}

/// Parse the adversary turn's output contract: the verdict JSON ends the transcript. The last
/// non-blank line is the fast path; when the backend's event stream glues messages together
/// without newlines (the openshell path, a live turn died on exactly that), fall back to the
/// last parseable JSON object in the tail. No verdict anywhere is an `Err`, which the pipeline
/// treats as fail-closed (an `Error` round, never a pass).
pub fn parse_adversary_verdict(transcript: &str) -> Result<AdversaryVerdict, String> {
    let last = transcript
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .ok_or_else(|| "the adversary turn produced no output".to_string())?
        .trim();
    let line_err = match serde_json::from_str(last) {
        Ok(v) => return Ok(v),
        Err(e) => e,
    };
    // Fallback: scan the tail backwards for the last `{`-opened object that parses as a verdict.
    // Bounded to the final 16 KiB, the verdict is the turn's closing act, never buried deep.
    let mut start = transcript.len().saturating_sub(16 * 1024);
    while !transcript.is_char_boundary(start) {
        start += 1;
    }
    let tail = &transcript[start..];
    for (i, _) in tail.char_indices().rev().filter(|(_, c)| *c == '{') {
        if let Ok(v) = serde_json::from_str::<AdversaryVerdict>(tail[i..].trim_end()) {
            return Ok(v);
        }
    }
    Err(format!(
        "the adversary turn's last line did not parse as a verdict ({line_err}), and no verdict \
         object was found in the transcript tail: {last:?}"
    ))
}

/// Render the adversary prompt: the goal, the pack's on-disk location (the adversary's cwd, it
/// reads the manifest/gate/workspace itself, nothing is embedded here), and a human summary of
/// the rounds so far for context.
pub fn render_adversary_prompt(
    goal: &str,
    out_dir: &std::path::Path,
    rounds: &[RoundRecord],
) -> String {
    let trail = if rounds.is_empty() {
        "(no prior rounds recorded)".to_string()
    } else {
        rounds
            .iter()
            .map(|r| {
                format!(
                    "- round {} ({}): {}",
                    r.round,
                    r.kind.label(),
                    match &r.outcome {
                        RoundOutcome::Passed => "passed validation".to_string(),
                        RoundOutcome::Failed { evidence } => evidence.describe(),
                        RoundOutcome::Error { detail } => format!("ERROR — {detail}"),
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    JUDGE_ADVERSARY_PROMPT
        .replace("{{GOAL}}", goal)
        .replace("{{OUT_DIR}}", &out_dir.display().to_string())
        .replace("{{TRAIL}}", &trail)
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

impl From<&SelftestReport> for SelftestEvidence {
    fn from(r: &SelftestReport) -> Self {
        let direction = match r.direction {
            crate::command_judge::Direction::Higher => "higher",
            crate::command_judge::Direction::Lower => "lower",
        }
        .to_string();
        SelftestEvidence {
            direction,
            runs: r.runs,
            good: control_evidence(&r.good),
            bad: control_evidence(&r.bad),
        }
    }
}

fn control_evidence(c: &crate::selftest::ControlResult) -> ControlEvidence {
    ControlEvidence {
        cmd: c.cmd.clone(),
        mean: c.mean_score,
        all_valid: c.all_valid,
        readings: c
            .readings
            .iter()
            .map(|r| ReadingEvidence {
                valid: r.valid,
                score: r.score,
                note: r.note.clone(),
            })
            .collect(),
    }
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

/// Render the refine prompt for `round`: the goal, the pack's on-disk location (the agent edits in
/// place), the concrete failure evidence from the prior round, the round number, and the
/// confirmed tier so a refine turn doesn't quietly slide a T1 harness back toward a
/// T0 shape while "fixing" it.
pub fn render_refine_prompt(
    goal: &str,
    out_dir: &std::path::Path,
    evidence: &FailureEvidence,
    round: u32,
    tier: crate::scope::ProposeTier,
    authoritative: bool,
    broker_measure: bool,
) -> String {
    let guard = if authoritative {
        GOAL_GUARD_AUTHORITATIVE
    } else {
        GOAL_GUARD
    };
    let measure_mode = if broker_measure {
        MEASURE_MODE_BROKER
    } else {
        MEASURE_MODE
    };
    SCOPE_REFINE_PROMPT
        .replace("{{MEASURE_MODE}}", measure_mode.trim_end())
        .replace("{{GOAL_GUARD}}", guard.trim_end())
        .replace("{{GOAL}}", goal)
        .replace("{{OUT_DIR}}", &out_dir.display().to_string())
        .replace("{{ROUND}}", &round.to_string())
        .replace("{{EVIDENCE}}", &evidence.describe())
        .replace("{{TIER}}", tier.as_str())
}

/// The pipeline floor on self-test `runs` for a *proposed* pack: a single reading can't establish
/// that a gate discriminates a noisy metric. Hand-authored domains (the `crucible check` path)
/// keep the manifest default of 1; this floor is enforced only in the propose/refine pipeline.
pub const MIN_PROPOSED_SELFTEST_RUNS: u32 = 3;

/// The fenced-JSON trail block for a `SCOPE.md`/`REJECTED.md` section: a ```json block holding the
/// ordered `Vec<RoundRecord>`, so the controller's checkpoint UI can `parse_rounds` it straight back out of the
/// markdown. Pretty-printed for a human skimming the frozen report.
pub fn render_rounds_json(records: &[RoundRecord]) -> String {
    let body = serde_json::to_string_pretty(records)
        .unwrap_or_else(|e| format!("[/* round trail failed to serialize: {e} */]"));
    format!("```json\n{body}\n```")
}

/// The inverse of [`render_rounds_json`]: pull the round records back out of a rendered fenced
/// block. The controller's checkpoint UI wants exactly this deserialization, but `crucible-controller` can't
/// depend on this bin crate (the dependency runs the other way), so its `refine_trail` module
/// carries a byte-identical mirror of these types plus its own `extract_trail`, the schema is
/// pinned to this one. Here, `parse_rounds` backs the serde round-trip tests.
#[allow(dead_code)]
pub fn parse_rounds(fenced: &str) -> Result<Vec<RoundRecord>, serde_json::Error> {
    let inner = fenced
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    serde_json::from_str(inner)
}

/// Slice the `[judge]`/`[judge.selftest]` region out of a manifest's raw TOML for the round
/// record, from the first `[judge` header to the next top-level section that isn't a `[judge…]`
/// subtable. Best-effort: returns empty if there's no `[judge]` header (an unparseable/absent
/// manifest), which is exactly the STRUCTURE-failure case.
pub fn extract_judge_block(manifest_text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut in_judge = false;
    for line in manifest_text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            let is_judge = trimmed.starts_with("[judge]") || trimmed.starts_with("[judge.");
            if is_judge {
                in_judge = true;
                out.push(line);
                continue;
            }
            if in_judge {
                break;
            }
            continue;
        }
        if in_judge {
            out.push(line);
        }
    }
    out.join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn round_records_survive_a_fenced_json_round_trip() {
        let records = vec![
            RoundRecord {
                round: 1,
                kind: RoundKind::Propose,
                judge_block: "[judge]\nmeasure_cmd = \"./m.sh\"\ndirection = \"higher\""
                    .to_string(),
                cost: 0.0,
                outcome: RoundOutcome::Failed {
                    evidence: selftest_ev(),
                },
            },
            RoundRecord {
                round: 2,
                kind: RoundKind::Refine,
                judge_block: "[judge]\nmeasure_cmd = \"./m.sh\"\ndirection = \"higher\""
                    .to_string(),
                cost: 0.0,
                outcome: RoundOutcome::Passed,
            },
        ];
        let fenced = render_rounds_json(&records);
        assert!(fenced.starts_with("```json"), "must be a fenced block");
        let parsed = parse_rounds(&fenced).expect("round trail parses back");
        assert_eq!(parsed, records, "records survive the fenced round trip");
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
    fn extract_judge_block_captures_judge_and_selftest() {
        let manifest = r#"
[repo]
path = "."

[judge]
measure_cmd = "./measure.sh"
direction = "higher"
objective = "score"

[judge.selftest]
good_cmd = "true"
bad_cmd = "false"
runs = 3

[agent]
backend = "local"
goal = "g"
"#;
        let block = extract_judge_block(manifest);
        assert!(block.starts_with("[judge]"), "{block}");
        assert!(block.contains("[judge.selftest]"), "{block}");
        assert!(block.contains("runs = 3"), "{block}");
        assert!(
            !block.contains("[agent]"),
            "stops before the next section: {block}"
        );
        assert!(!block.contains("[repo]"), "doesn't reach back: {block}");
    }

    #[test]
    fn extract_judge_block_empty_when_absent() {
        assert_eq!(extract_judge_block("[repo]\npath = \".\"\n"), "");
    }

    #[test]
    fn parse_adversary_verdict_recovers_a_verdict_glued_to_prose() {
        // The openshell event stream concatenates messages without newlines, the live failure
        // shape: narration and the closing verdict fused into one giant "line".
        let glued = format!(
            "I'll read the pack files.Now let me check the workspace.Now I have the full \
             picture.{}",
            r#"{"verdict":"pass"}"#
        );
        let v = parse_adversary_verdict(&glued).expect("glued verdict recovers via the tail scan");
        assert_eq!(v, AdversaryVerdict::Pass);
    }

    #[test]
    fn parse_adversary_verdict_glued_concerns_with_braces_in_prose() {
        // Earlier `{` characters in the prose must not derail the backward scan.
        let glued = format!(
            "chatter about {{braces}} and code.{}",
            r#"{"verdict":"concerns","attacks":[]}"#
        );
        let v = parse_adversary_verdict(&glued).expect("concerns verdict recovers");
        assert!(matches!(v, AdversaryVerdict::Concerns { .. }));
    }

    #[test]
    fn parse_adversary_verdict_multibyte_tail_does_not_panic() {
        // The 16 KiB tail window must respect char boundaries.
        let mut big = "é".repeat(20 * 1024);
        big.push_str("no verdict here");
        assert!(parse_adversary_verdict(&big).is_err());
    }

    #[test]
    fn parse_adversary_verdict_reads_a_pass() {
        let v = parse_adversary_verdict("some chatter\n{\"verdict\":\"pass\"}\n")
            .expect("a pass verdict parses");
        assert_eq!(v, AdversaryVerdict::Pass);
    }

    #[test]
    fn parse_adversary_verdict_reads_concerns_with_attacks() {
        let transcript = "thinking out loud\n\
             {\"verdict\":\"concerns\",\"attacks\":[{\"kind\":\"self-report\",\"narrative\":\"n\",\"suggestion\":\"s\"}]}";
        let v = parse_adversary_verdict(transcript).expect("a concerns verdict parses");
        match v {
            AdversaryVerdict::Concerns { attacks } => {
                assert_eq!(attacks.len(), 1);
                assert_eq!(attacks[0].kind, AttackKind::SelfReport);
                assert_eq!(attacks[0].narrative, "n");
                assert_eq!(attacks[0].suggestion, "s");
            }
            other => panic!("expected Concerns, got {other:?}"),
        }
    }

    #[test]
    fn parse_adversary_verdict_rejects_no_output() {
        assert!(parse_adversary_verdict("").is_err());
        assert!(parse_adversary_verdict("   \n\n  ").is_err());
    }

    #[test]
    fn parse_adversary_verdict_rejects_non_json_last_line() {
        assert!(parse_adversary_verdict("looks fine to me, no concerns").is_err());
    }

    #[test]
    fn parse_adversary_verdict_rejects_bad_schema() {
        // Valid JSON, wrong shape (unknown verdict tag), still a fail-closed error, not a pass.
        assert!(parse_adversary_verdict("{\"verdict\":\"maybe\"}").is_err());
        assert!(parse_adversary_verdict("{\"ok\":true}").is_err());
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

    #[test]
    fn adversary_round_trail_round_trips_through_fenced_json() {
        let records = vec![RoundRecord {
            round: 3,
            kind: RoundKind::Adversary,
            judge_block: String::new(),
            cost: 0.0,
            outcome: RoundOutcome::Failed {
                evidence: FailureEvidence::Adversary {
                    attacks: vec![Attack {
                        kind: AttackKind::Boundary,
                        narrative: "n".to_string(),
                        suggestion: "s".to_string(),
                    }],
                },
            },
        }];
        let fenced = render_rounds_json(&records);
        let parsed = parse_rounds(&fenced).expect("adversary records round-trip");
        assert_eq!(parsed, records);
    }

    #[test]
    fn parse_rounds_stays_backward_compatible_with_pre_adversary_trails() {
        // A trail frozen before the gaming review shipped: only Propose/Refine kinds, only
        // Passed/Failed outcomes, no Adversary/Error variants anywhere. Must still parse under
        // the current types (new enum variants are additive, never renamed/removed).
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
        assert!(matches!(
            parsed[0].outcome,
            RoundOutcome::Failed {
                evidence: FailureEvidence::Contract { .. }
            }
        ));
        assert_eq!(parsed[1].kind, RoundKind::Refine);
        assert!(matches!(parsed[1].outcome, RoundOutcome::Passed));
    }
}
