//! The measurement refine loop: the strongly-typed evidence a failed
//! validation hands back to a bounded retry turn, and the per-round record trail that both the
//! refine prompt and the frozen `SCOPE.md`/`REJECTED.md` are rendered from.
//!
//! Ownership split: this module holds the *types* + the pure rendering (prompt text, the fenced
//! JSON trail, the human summaries) and nothing that touches an agent or the filesystem, the
//! loop driver itself lives in `scope.rs`, where the propose plumbing (scratch checkout, seed
//! context, `agent::run_turn`) already sits. Keeping the records here means `scope.rs` stays about
//! orchestration and this stays trivially unit-testable. The records themselves are
//! [`crucible_contract::refine`], which a controller depends on directly to read a frozen trail.

use crate::runloop::selftest::SelftestReport;
use crucible_contract::refine::{
    Attack, ControlEvidence, FailureEvidence, ReadingEvidence, RoundOutcome, RoundRecord,
    SelftestEvidence,
};
use serde::{Deserialize, Serialize};

/// The engine-embedded refine prompt: seeded from `scope-propose.md`'s contract sections, focused
/// on diagnosing and fixing a gate that didn't validate, and explicit that weakening the gate to
/// pass (inverting direction, making the controls trivially different) is not a fix.
pub(crate) const SCOPE_REFINE_PROMPT: &str = include_str!("../prompts/scope-refine.md");

/// The `{{GOAL_GUARD}}` block: keep `goal.md` de-prescribed (the default), or, for an
/// authoritative brief, preserve its prescriptions and fix the gate instead.
pub(crate) const GOAL_GUARD: &str = include_str!("../prompts/scope-refine-goal-guard.md");
pub(crate) const GOAL_GUARD_AUTHORITATIVE: &str =
    include_str!("../prompts/scope-refine-goal-guard-authoritative.md");

/// The engine-embedded adversarial gaming-review prompt: a read-only
/// turn that attacks a pack that already passed validation, looking for ways an optimizing agent
/// could score well without genuinely addressing the goal.
const JUDGE_ADVERSARY_PROMPT: &str = include_str!("../prompts/judge-adversary.md");

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
    let line_err = match crucible_contract::json::from_str(last) {
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
        if let Ok(v) = crucible_contract::json::from_str::<AdversaryVerdict>(tail[i..].trim_end()) {
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
impl From<&SelftestReport> for SelftestEvidence {
    fn from(r: &SelftestReport) -> Self {
        let direction = match r.direction {
            crucible::crucible::Direction::Higher => "higher",
            crucible::crucible::Direction::Lower => "lower",
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

fn control_evidence(c: &crate::runloop::selftest::ControlResult) -> ControlEvidence {
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

/// Render the refine prompt for `round`: the goal, the pack's on-disk location (the agent edits in
/// place), the concrete failure evidence from the prior round, the round number, and the
/// confirmed tier so a refine turn doesn't quietly slide a T1 harness back toward a
/// T0 shape while "fixing" it.
pub fn render_refine_prompt(
    goal: &str,
    out_dir: &std::path::Path,
    evidence: &FailureEvidence,
    round: u32,
    tier: crate::deploy::ProposeTier,
    authoritative: bool,
) -> String {
    let guard = if authoritative {
        GOAL_GUARD_AUTHORITATIVE
    } else {
        GOAL_GUARD
    };
    SCOPE_REFINE_PROMPT
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
    use crucible_contract::refine::AttackKind;

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
}
