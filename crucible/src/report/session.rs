//! The in-process [`Row`] and its bridge to the session log wire format.
//!
//! The wire format itself ([`SessionEvent`], [`RowWire`], [`PrLinkWire`], `encode`/`decode`) lives
//! in `crucible_contract::session` and is re-exported here unchanged. This module adds the one
//! CLI/in-process type that must stay out of the wire-format leaf and its conversion:
//! [`IntoRow`] (`RowWire` -> `Row`).

pub use crucible_contract::session::*;

/// One row in the results log / final summary.
#[derive(Clone, Debug, Default)]
pub struct Row {
    pub iter: u32,
    pub decision: String,
    pub note: String,
    pub detail: String,
    /// The agent's full staged diff for this iteration (captured before
    /// keep/discard, so it survives the commit/reset). Empty for the baseline.
    pub diff: String,
    /// One-line `git --shortstat` for the diff (files changed, +/-).
    pub diffstat: String,
    /// The measured fitness for this row (bench: p99 ms; test: failed count). Carried
    /// numerically (not just in `note`) so a resumed run can restore baseline/best.
    pub score: Option<f64>,
    /// Secondary scalar for functional gates: breaks primary-score ties in the keep rule.
    /// Carried numerically so a resume restores the kept best's tiebreak with its score.
    pub tiebreak: Option<f64>,
    /// Total test count for this row (test gate), for the same reason.
    pub total: Option<u64>,
    /// `Some("wide")` for wide-round rows, `Some("infra")` for never-started turn
    /// records; `None` for the deep (default) loop.
    pub phase: Option<String>,
    /// The World snapshot token committed when this row was kept (a git world packs the
    /// commit sha). Carried on the wire so a resume can restore the kept-best tree instead
    /// of pairing the logged best score with whatever the re-prepared checkout holds.
    /// `None` on non-keep rows and on logs written before the field existed.
    pub kept_snap: Option<String>,
    /// The grade step's declared evidence set with per-task dispositions, so the row
    /// says which declared checks never ran instead of presenting a partially graded
    /// candidate as fully graded. Empty on ungraded rows.
    pub evidence: Vec<crate::report::session::EvidenceEntry>,
    /// The agent's whole CANDIDATE.md (`note` is its 120-char single-line fold). The PR
    /// body prints this; every table keeps using `note`. Empty when the agent wrote none.
    pub candidate_md: String,
}

impl From<&Row> for RowWire {
    fn from(r: &Row) -> Self {
        Self {
            iter: r.iter,
            decision: r.decision.clone(),
            note: r.note.clone(),
            detail: r.detail.clone(),
            diff: r.diff.clone(),
            diffstat: r.diffstat.clone(),
            score: r.score,
            tiebreak: r.tiebreak,
            total: r.total,
            phase: r.phase.clone(),
            kept_snap: r.kept_snap.clone(),
            evidence: r.evidence.clone(),
            candidate_md: r.candidate_md.clone(),
        }
    }
}

/// Bridges [`RowWire`] (the wire mirror) back to [`Row`] (in-process state). A trait rather than
/// an inherent method since `RowWire` is defined in `crucible-contract`.
pub trait IntoRow {
    fn into_row(self) -> Row;
}

impl IntoRow for RowWire {
    fn into_row(self) -> Row {
        Row {
            iter: self.iter,
            decision: self.decision,
            note: self.note,
            detail: self.detail,
            diff: self.diff,
            diffstat: self.diffstat,
            score: self.score,
            tiebreak: self.tiebreak,
            total: self.total,
            phase: self.phase,
            kept_snap: self.kept_snap,
            evidence: self.evidence,
            candidate_md: self.candidate_md,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_bridge_round_trips() {
        let row = Row {
            iter: 1,
            decision: "keep".into(),
            note: "p99=210 ms".into(),
            detail: "cache_hit=0.83".into(),
            diff: "diff --git a/p.go b/p.go\n@@ -1 +1 @@\n-old\n+new\n".into(),
            diffstat: "1 file changed, 1 insertion(+), 1 deletion(-)".into(),
            score: Some(210.0),
            tiebreak: Some(0.25),
            total: None,
            phase: None,
            kept_snap: Some("abc123".into()),
            evidence: vec![EvidenceEntry {
                task: "refcheck".into(),
                disposition: EvidenceDisposition::Passed,
                note: String::new(),
            }],
            candidate_md: "# Candidate\n\nfull writeup".into(),
        };
        let wire = RowWire::from(&row);
        let back = wire.into_row();
        assert_eq!(back.iter, row.iter);
        assert_eq!(back.decision, row.decision);
        assert_eq!(back.note, row.note);
        assert_eq!(back.detail, row.detail);
        assert_eq!(back.diff, row.diff);
        assert_eq!(back.diffstat, row.diffstat);
        assert_eq!(back.score, row.score);
        assert_eq!(back.tiebreak, row.tiebreak);
        assert_eq!(back.total, row.total);
        assert_eq!(back.phase, row.phase);
        assert_eq!(back.kept_snap, row.kept_snap);
        assert_eq!(back.evidence, row.evidence);
        assert_eq!(back.candidate_md, row.candidate_md);
    }
}
