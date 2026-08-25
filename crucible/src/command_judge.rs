//! The any-repo [`Judge`]: fitness *is* a command. Run `measure_cmd` in the workspace, read
//! `{valid, score, solved?, note?, detail?}` from its last JSON stdout line, and decide
//! keep/discard generically by `direction`. No domain Rust, the win condition lives in the
//! command (which the engine feeds `CRUCIBLE_BASELINE_*`/`CRUCIBLE_BEST_SCORE`).

use crate::crucible::{Decision, Judge, MeasureCtx, Reading};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;
use std::process::Command;

/// Which way is better. `lower` (latency, failures) or `higher` (throughput, score).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Lower,
    Higher,
}

impl Direction {
    /// Strictly-better test for the keep rule (and the gate self-test's discrimination check).
    pub fn better(self, score: f64, best: f64) -> bool {
        match self {
            Direction::Lower => score < best,
            Direction::Higher => score > best,
        }
    }
}

/// The shape a measure command prints on its last JSON line (the rest is free-form `detail`).
#[derive(Deserialize, Default)]
struct MeasureOut {
    valid: bool,
    #[serde(default)]
    score: Option<f64>,
    #[serde(default)]
    tiebreak: Option<f64>,
    #[serde(default)]
    solved: bool,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    detail: serde_json::Value,
}

pub struct CommandJudge {
    pub workspace: PathBuf,
    pub measure_cmd: String,
    pub direction: Direction,
    /// Which way the secondary `tiebreak` scalar improves. `None` inherits `direction`,
    /// so a pack only declares it when the two axes disagree (a pass/fail primary that is
    /// lower-is-better with a higher-is-better throughput tiebreak).
    pub tiebreak_direction: Option<Direction>,
    pub objective: String,
    /// Frozen judge files (absolute `src`, `dst`) re-copied before each scored measure so a candidate
    /// can't edit the gate (a tuning harness, a regression test) to game it. Empty for most domains.
    pub frozen_injects: Vec<(PathBuf, PathBuf)>,
}

/// The last stdout line that looks like a JSON object (the contract's measurement line).
fn last_json_line(stdout: &str) -> Option<&str> {
    stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
}

impl CommandJudge {
    fn run_measure(&self, ctx: &MeasureCtx) -> Result<Reading> {
        // Re-establish the frozen judge files before scoring, so any edit the candidate made to the
        // gate (the harness, the regression test) is overwritten and can't game the measurement.
        for (src, dst) in &self.frozen_injects {
            crate::manifest::apply_inject(src, dst)
                .context("re-establishing frozen judge before measure")?;
        }
        let mut c = Command::new("sh");
        c.arg("-c")
            .arg(&self.measure_cmd)
            .current_dir(&self.workspace);
        if let Some(s) = ctx.baseline_score {
            c.env("CRUCIBLE_BASELINE_SCORE", s.to_string());
        }
        if let Some(t) = ctx.baseline_total {
            c.env("CRUCIBLE_BASELINE_TOTAL", t.to_string());
        }
        if let Some(b) = ctx.best_score {
            c.env("CRUCIBLE_BEST_SCORE", b.to_string());
        }
        let out = c
            .output()
            .with_context(|| format!("exec measure `{}`", self.measure_cmd))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let Some(line) = last_json_line(&stdout) else {
            return Ok(Reading {
                valid: false,
                note: "measure produced no JSON line".into(),
                ..Default::default()
            });
        };
        let parsed: MeasureOut =
            serde_json::from_str(line).with_context(|| format!("parsing measure JSON: {line}"))?;
        // A nonzero exit forces the reading invalid regardless of what it printed.
        let valid = parsed.valid && out.status.success();
        Ok(Reading {
            valid,
            score: parsed.score,
            tiebreak: parsed.tiebreak,
            solved: parsed.solved,
            note: parsed.note.unwrap_or_else(|| match parsed.score {
                Some(s) => format!("{} = {s}", self.objective),
                None => "invalid".into(),
            }),
            detail: parsed.detail,
        })
    }
}

impl Judge for CommandJudge {
    fn measure(&self, ctx: &MeasureCtx) -> Result<Reading> {
        self.run_measure(ctx)
    }

    fn decide(&self, r: &Reading, best_score: f64, best_tiebreak: Option<f64>) -> Decision {
        // Universal rule: keep a valid candidate that either strictly beats best by direction
        // OR the measure command declared `solved`. The win condition is the whole point, so a
        // domain whose win lands at an *equal* score must still be kept and terminate the loop
        // (a test gate: a green suite is 0 failures == the baseline's 0, yet `solved` once a
        // new regression test is added and passes). Without `|| solved` that win gets discarded
        // and the loop never finishes. No per-domain logic; the measure command owns `solved`.
        let better = r
            .score
            .map(|s| self.direction.better(s, best_score))
            .unwrap_or(false);
        // Lexicographic fallback for functional gates: on an exact primary tie, a candidate
        // reporting a strictly better `tiebreak` still keeps. A missing best tiebreak counts
        // as the worst (the kept best never declared one, so any declared value beats it);
        // a candidate without one falls through to the plain strictly-better rule.
        let tie_break_better = r.score == Some(best_score)
            && r.tiebreak.is_some_and(|t| {
                let dir = self.tiebreak_direction.unwrap_or(self.direction);
                let best = best_tiebreak.unwrap_or(match dir {
                    Direction::Lower => f64::INFINITY,
                    Direction::Higher => f64::NEG_INFINITY,
                });
                dir.better(t, best)
            });
        let keep = r.valid && r.score.is_some() && (better || tie_break_better || r.solved);
        Decision {
            keep,
            solved: keep && r.solved,
        }
    }

    fn status(&self, best_score: f64) -> String {
        if best_score.is_finite() {
            format!("best {} = {best_score}", self.objective)
        } else {
            format!("no valid {} yet", self.objective)
        }
    }

    fn improved(&self, best_score: f64, baseline_score: f64, solved_any: bool) -> bool {
        solved_any || self.direction.better(best_score, baseline_score)
    }

    fn detail(&self, r: &Reading) -> String {
        // Surface the detail JSON compactly (empty when the command emitted none).
        match &r.detail {
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        }
    }

    fn objective(&self) -> String {
        self.objective.clone()
    }

    fn direction(&self) -> Direction {
        self.direction
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn judge(dir: Direction) -> CommandJudge {
        CommandJudge {
            workspace: PathBuf::from("/tmp"),
            measure_cmd: String::new(),
            direction: dir,
            tiebreak_direction: None,
            objective: "score".into(),
            frozen_injects: Vec::new(),
        }
    }

    fn reading(valid: bool, score: f64, solved: bool) -> Reading {
        Reading {
            valid,
            score: Some(score),
            solved,
            ..Default::default()
        }
    }

    fn reading_tb(score: f64, tiebreak: Option<f64>) -> Reading {
        Reading {
            valid: true,
            score: Some(score),
            tiebreak,
            ..Default::default()
        }
    }

    #[test]
    fn lower_keeps_strictly_smaller() {
        let j = judge(Direction::Lower);
        assert!(j.decide(&reading(true, 10.0, false), 20.0, None).keep);
        assert!(!j.decide(&reading(true, 20.0, false), 20.0, None).keep);
        assert!(
            !j.decide(&reading(false, 1.0, false), 20.0, None).keep,
            "invalid never keeps"
        );
    }

    #[test]
    fn higher_keeps_strictly_larger_and_passes_solved() {
        let j = judge(Direction::Higher);
        let d = j.decide(&reading(true, 5.0, true), 4.0, None);
        assert!(d.keep && d.solved, "better + solved -> kept + solved");
        // A plain (unsolved) candidate that isn't strictly better is dropped.
        let d = j.decide(&reading(true, 3.0, false), 4.0, None);
        assert!(!d.keep && !d.solved);
    }

    #[test]
    fn solved_forces_keep_even_at_equal_or_worse_score() {
        // The test-gate case: a win lands at a score that doesn't strictly beat best
        // (green suite == 0 failures == baseline's 0). `solved` must still keep + terminate.
        let lo = judge(Direction::Lower);
        let d = lo.decide(&reading(true, 0.0, true), 0.0, None);
        assert!(d.keep && d.solved, "solved at an equal score is kept");
        // Solved never rescues an invalid reading, though.
        let d = lo.decide(&reading(false, 0.0, true), 0.0, None);
        assert!(!d.keep && !d.solved, "invalid never keeps, solved or not");
    }

    #[test]
    fn primary_tie_keeps_on_strictly_better_tiebreak() {
        // The functional-gate case: every passing candidate scores 0.0, so the secondary
        // scalar is the only gradient. Tiebreak direction inherits the judge's (lower).
        let j = judge(Direction::Lower);
        let d = j.decide(&reading_tb(0.0, Some(10.0)), 0.0, Some(12.0));
        assert!(d.keep, "tie + better tiebreak keeps");
        // The kept best never declared a tiebreak: any declared value beats absent.
        assert!(j.decide(&reading_tb(0.0, Some(10.0)), 0.0, None).keep);
    }

    #[test]
    fn primary_tie_discards_on_worse_or_absent_tiebreak() {
        let j = judge(Direction::Lower);
        assert!(
            !j.decide(&reading_tb(0.0, Some(12.0)), 0.0, Some(10.0)).keep,
            "tie + worse tiebreak discards"
        );
        assert!(
            !j.decide(&reading_tb(0.0, Some(10.0)), 0.0, Some(10.0)).keep,
            "tie on both axes discards"
        );
        assert!(
            !j.decide(&reading_tb(0.0, None), 0.0, Some(10.0)).keep,
            "tie with no candidate tiebreak behaves as before: discard"
        );
        assert!(
            !j.decide(&reading_tb(0.0, None), 0.0, None).keep,
            "no tiebreak anywhere: today's behavior exactly"
        );
    }

    #[test]
    fn strictly_better_primary_keeps_regardless_of_tiebreak() {
        let j = judge(Direction::Lower);
        assert!(j.decide(&reading_tb(1.0, Some(99.0)), 2.0, Some(0.0)).keep);
        // And a strictly worse primary never keeps, however good the tiebreak.
        assert!(!j.decide(&reading_tb(3.0, Some(0.0)), 2.0, Some(99.0)).keep);
    }

    #[test]
    fn declared_tiebreak_direction_overrides_the_judges() {
        // Lower-is-better primary (failures) with a higher-is-better tiebreak (throughput).
        let mut j = judge(Direction::Lower);
        j.tiebreak_direction = Some(Direction::Higher);
        assert!(
            j.decide(&reading_tb(0.0, Some(200.0)), 0.0, Some(100.0))
                .keep
        );
        assert!(
            !j.decide(&reading_tb(0.0, Some(50.0)), 0.0, Some(100.0))
                .keep
        );
        // An invalid reading never keeps, tiebreak or not.
        let mut r = reading_tb(0.0, Some(200.0));
        r.valid = false;
        assert!(!j.decide(&r, 0.0, Some(100.0)).keep);
    }

    #[test]
    fn parses_last_json_line_only() {
        assert_eq!(
            last_json_line("noise\n{\"valid\":true}\n"),
            Some("{\"valid\":true}")
        );
        assert_eq!(last_json_line("no json here"), None);
    }
}
