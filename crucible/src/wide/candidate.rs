//! One wide-round candidate: its identity, the approach it tried, its result.

use crate::reporter::Row;

/// A candidate in the wide round. Each gets one PROPOSE turn in its own worktree,
/// biased to a distinct approach from `[search].approaches`.
pub struct WideCandidate {
    /// 0-indexed candidate slot.
    pub id: u32,
    /// The approach description this candidate was biased toward.
    pub approach: String,
    /// The worktree path this candidate's agent operated in.
    pub worktree: std::path::PathBuf,
    /// The candidate's proposal result (set after the PROPOSE turn).
    pub result: Option<CandidateResult>,
}

/// The outcome of one candidate's PROPOSE + MEASURE cycle.
pub struct CandidateResult {
    pub row: Row,
    pub score: Option<f64>,
    /// The diff the candidate produced (staged from its worktree).
    pub diff: String,
    pub diffstat: String,
}
