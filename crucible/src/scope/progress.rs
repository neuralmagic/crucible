use crate::refine::RoundKind;
use serde::Serialize;

/// The prefix of the interim progress marker `--marker` emits at each refine-round boundary, so a
/// live log tail can show where a 5-15 minute scope turn is instead of a black box. The controller's
/// live turn relay matches the same shared literal from `crucible-contract`.
pub use crucible_contract::SCOPE_PROGRESS_MARKER;

/// One interim progress beat, emitted just before a round's agent turn starts. Closed and small on
/// purpose, a live-view hint, never a result (the terminal [`ScopeReport`] is the result).
#[derive(Debug, Clone, Serialize)]
pub struct ScopeProgress {
    /// 1-based round number, same numbering as [`RoundRecord::round`].
    pub round: u32,
    pub kind: RoundKind,
    /// What the round is about to do, one human-readable line.
    pub doing: String,
    /// Total turn cost (USD) accumulated before this round started.
    pub cost_so_far: f64,
}

impl ScopeProgress {
    /// The full single-line marker: `CRUCIBLE_SCOPE_PROGRESS: {json}`.
    pub fn marker_line(&self) -> String {
        let json = serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string());
        format!("{SCOPE_PROGRESS_MARKER} {json}")
    }
}

/// Cap on the `doing` field: failure evidence can be pages of selftest output, and a progress beat
/// is a hint, not the trail (`SCOPE.md`/`REJECTED.md` carry the full evidence).
pub(super) const PROGRESS_DOING_CAP: usize = 240;

/// Truncate a `doing` line to [`PROGRESS_DOING_CAP`] chars, marking the cut.
pub(super) fn cap_doing(doing: &str) -> String {
    let mut capped: String = doing.chars().take(PROGRESS_DOING_CAP).collect();
    if capped.len() < doing.len() {
        capped.push('…');
    }
    capped
}

/// Print one progress marker line (stdout, so it rides the same pod-log channel as the report
/// marker). No-op unless `--marker` asked for machine-readable output.
pub(super) fn emit_progress(
    enabled: bool,
    round: u32,
    kind: RoundKind,
    doing: &str,
    cost_so_far: f64,
) {
    if !enabled {
        return;
    }
    let beat = ScopeProgress {
        round,
        kind,
        doing: cap_doing(doing),
        cost_so_far,
    };
    println!("{}", beat.marker_line());
}
