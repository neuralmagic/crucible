//! Tournament control: which candidates advance, when to stop fanning out.
//!
//! v1 ships only [`top_k`] (rank after one measurement round, return the K best).
//! Round-robin and successive-halving are future impls that re-measure survivors
//! each round to reduce noise.

/// A measured candidate the policy ranks.
pub struct ScoredCandidate {
    pub id: u32,
    pub score: f64,
}

/// Select the top-K candidate IDs by score, using `direction` to sort. "lower" = lower wins
/// (bench p99); "higher" = higher wins (throughput).
pub fn top_k(scored: &[ScoredCandidate], k: u32, direction: &str) -> Vec<u32> {
    let mut indexed: Vec<(u32, f64)> = scored.iter().map(|c| (c.id, c.score)).collect();
    match direction {
        "lower" => {
            indexed.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        }
        _ => indexed.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)),
    }
    indexed
        .into_iter()
        .take(k as usize)
        .map(|(id, _)| id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_k_picks_lowest_for_lower_direction() {
        let scored = vec![
            ScoredCandidate {
                id: 0,
                score: 300.0,
            },
            ScoredCandidate {
                id: 1,
                score: 200.0,
            },
            ScoredCandidate {
                id: 2,
                score: 250.0,
            },
        ];
        let winners = top_k(&scored, 2, "lower");
        assert_eq!(winners, vec![1, 2]);
    }

    #[test]
    fn top_k_picks_highest_for_higher_direction() {
        let scored = vec![
            ScoredCandidate { id: 0, score: 10.0 },
            ScoredCandidate { id: 1, score: 30.0 },
            ScoredCandidate { id: 2, score: 20.0 },
        ];
        let winners = top_k(&scored, 1, "higher");
        assert_eq!(winners, vec![1]);
    }

    #[test]
    fn top_k_empty_scored_returns_empty() {
        let winners = top_k(&[], 3, "lower");
        assert!(winners.is_empty());
    }

    #[test]
    fn top_k_k_exceeds_candidates_returns_all() {
        let scored = vec![
            ScoredCandidate { id: 0, score: 10.0 },
            ScoredCandidate { id: 1, score: 5.0 },
        ];
        let winners = top_k(&scored, 5, "lower");
        assert_eq!(winners.len(), 2);
        assert_eq!(winners[0], 1);
        assert_eq!(winners[1], 0);
    }

    #[test]
    fn top_k_tied_scores_deterministic() {
        let scored = vec![
            ScoredCandidate {
                id: 0,
                score: 100.0,
            },
            ScoredCandidate {
                id: 1,
                score: 100.0,
            },
            ScoredCandidate {
                id: 2,
                score: 100.0,
            },
        ];
        let w1 = top_k(&scored, 2, "lower");
        let w2 = top_k(&scored, 2, "lower");
        assert_eq!(w1, w2);
    }

    #[test]
    fn top_k_nan_score_does_not_panic() {
        let scored = vec![
            ScoredCandidate {
                id: 0,
                score: f64::NAN,
            },
            ScoredCandidate { id: 1, score: 10.0 },
        ];
        let winners = top_k(&scored, 1, "lower");
        assert_eq!(winners.len(), 1);
    }
}
