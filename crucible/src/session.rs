//! The `Row`/`Phase` bridge for the session log wire format.
//!
//! The wire format itself ([`SessionEvent`], [`RowWire`], [`PrLinkWire`], `encode`/`decode`) lives
//! in `crucible_contract::session` and is re-exported here unchanged. This module adds only the
//! two conversions that need [`crate::reporter::{Row, Phase}`], CLI/in-process types that must
//! stay out of the wire-format leaf: [`IntoRow`] (`RowWire` -> `Row`) and [`SessionPhase`]
//! (`SessionEvent` <-> `Phase`).

use crate::reporter::{Phase, Row};

pub use crucible_contract::session::*;

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
            total: r.total,
            phase: r.phase.clone(),
            kept_snap: r.kept_snap.clone(),
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
            total: self.total,
            phase: self.phase,
            kept_snap: self.kept_snap,
        }
    }
}

/// Bridges [`Phase`] (in-process state) to [`SessionEvent`]'s wire `Phase { phase, iter }` shape.
/// A trait rather than an inherent impl since `SessionEvent` is defined in `crucible-contract`.
pub trait SessionPhase: Sized {
    /// Encode a [`Phase`] into the wire shape.
    fn phase(p: Phase) -> Self;
}

impl SessionPhase for SessionEvent {
    fn phase(p: Phase) -> Self {
        match p {
            Phase::Baseline => SessionEvent::Phase {
                phase: "baseline".into(),
                iter: 0,
            },
            Phase::Iteration(it) => SessionEvent::Phase {
                phase: "iteration".into(),
                iter: it,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_contract::session::{decode, encode};

    /// The `Row`/`Phase` bridge round-trips through the wire codec exactly like every other
    /// variant (the pure-wire round-trip is covered in `crucible_contract::session`'s own tests).
    #[test]
    fn phase_bridge_round_trips() {
        for (phase, want_phase, want_iter) in [
            (Phase::Baseline, "baseline", 0),
            (Phase::Iteration(2), "iteration", 2),
        ] {
            let ev = SessionEvent::phase(phase);
            let line = encode(&ev);
            let decoded = decode(&line).unwrap_or_else(|| panic!("decode failed: {line}"));
            let SessionEvent::Phase {
                phase: wire_phase,
                iter,
            } = decoded
            else {
                panic!("expected a Phase event, got {line}");
            };
            assert_eq!(wire_phase, want_phase);
            assert_eq!(iter, want_iter);
        }
    }

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
            total: None,
            phase: None,
            kept_snap: Some("abc123".into()),
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
        assert_eq!(back.total, row.total);
        assert_eq!(back.phase, row.phase);
        assert_eq!(back.kept_snap, row.kept_snap);
    }
}
