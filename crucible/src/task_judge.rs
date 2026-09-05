//! The task lane's judge: a manifest with no `[judge]` runs the normal loop with this
//! keep-everything stand-in. No measure command runs and no score is fabricated; rows keep
//! the ordinary `decision: "keep"` shape (score `None`) so publish, resume, and the flow
//! renderer work unchanged.

use crate::crucible::Direction;
use crate::crucible::{Decision, Judge, MeasureCtx, Reading};
use anyhow::Result;

pub struct TaskJudge;

impl Judge for TaskJudge {
    fn measure(&self, _ctx: &MeasureCtx) -> Result<Reading> {
        Ok(Reading {
            valid: true,
            score: None,
            tiebreak: None,
            solved: false,
            note: "turn complete (task mode: no gate)".to_string(),
            detail: serde_json::Value::Object(serde_json::Map::new()),
        })
    }

    fn decide(
        &self,
        _reading: &Reading,
        _best_score: f64,
        _best_tiebreak: Option<f64>,
    ) -> Decision {
        Decision {
            keep: true,
            solved: false,
        }
    }

    fn status(&self, _best_score: f64) -> String {
        "task mode: no objective score, completed work is kept each turn".to_string()
    }

    fn improved(&self, _best_score: f64, _baseline_score: f64, _solved_any: bool) -> bool {
        true
    }

    fn detail(&self, _reading: &Reading) -> String {
        "task".to_string()
    }

    fn objective(&self) -> String {
        "task".to_string()
    }

    fn direction(&self) -> Direction {
        Direction::Lower
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measure_emits_no_score_and_never_solves() {
        let r = TaskJudge.measure(&MeasureCtx::default()).unwrap();
        assert!(r.valid);
        assert_eq!(r.score, None);
        assert_eq!(r.tiebreak, None);
        assert!(!r.solved);
    }

    #[test]
    fn decide_always_keeps_and_never_solves() {
        let r = TaskJudge.measure(&MeasureCtx::default()).unwrap();
        for (best, tiebreak) in [(f64::INFINITY, None), (0.0, Some(1.0)), (-3.5, None)] {
            let d = TaskJudge.decide(&r, best, tiebreak);
            assert!(d.keep);
            assert!(!d.solved);
        }
    }

    #[test]
    fn improved_is_unconditional_so_a_finished_run_exits_zero() {
        assert!(TaskJudge.improved(f64::INFINITY, f64::INFINITY, false));
    }

    #[test]
    fn objective_labels_the_run_as_a_task() {
        assert_eq!(TaskJudge.objective(), "task");
        assert_eq!(TaskJudge.direction(), Direction::Lower);
    }
}
