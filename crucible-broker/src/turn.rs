//! Per-turn candidate budget for the broker: the "cap" / "wrap-up". Builds stay free (compile
//! feedback); what we bound is *candidates* (`deploy_candidate` calls) per turn, so the agent
//! commits one and lets the LOOP be the search instead of grinding a mini-search inside a turn.
//!
//! Turn boundaries come from crucible: the driver writes a fresh token to `<storage>/turn-token`
//! per turn sandbox, and the budget keys on that value changing. A missing token file degrades to
//! uncapped (an older driver, a local run) rather than over-capping the run.
//! `BROKER_MAX_DEPLOYS_PER_TURN` (0 / unset = unlimited) sets the cap; an operator can force an
//! immediate wrap-up for the current turn by `touch`-ing `<storage>/wrap-up`.

use std::path::{Path, PathBuf};

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct TurnState {
    /// Identifies the current turn (the live sandbox instance). Empty before the first call.
    token: String,
    /// Candidates (deploys) committed so far this turn.
    deploys: u32,
}

/// The budget decision for a build/deploy call.
pub(crate) enum Budget {
    /// Proceed. `remaining` is the candidates left this turn (None when uncapped); production
    /// callers only branch on the variant; the tests match on it to pin the countdown.
    Ok {
        #[cfg_attr(not(test), allow(dead_code))]
        remaining: Option<u32>,
    },
    /// Stop: the turn's candidate budget is spent (or an operator asked to wrap up). `reason` is the
    /// message handed to the agent so it commits its best candidate and ends the turn.
    Spent { reason: String },
}

fn state_path() -> PathBuf {
    forge::storage_root().join("turn-state.json")
}
fn wrap_flag() -> PathBuf {
    forge::storage_root().join("wrap-up")
}

fn max_deploys() -> u32 {
    std::env::var("BROKER_MAX_DEPLOYS_PER_TURN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// A token identifying the current turn: the value crucible wrote to `<storage>/turn-token` when it
/// created this turn's sandbox. Empty when the file is missing/unreadable (an older driver, a local
/// run); callers then skip the auto-cap (degrade to uncapped rather than over-cap the whole run).
fn turn_token() -> String {
    read_turn_token(&forge::storage_root().join("turn-token"))
}

/// The current turn token (empty when the driver hasn't written one) for other per-turn budgets that
/// key on the same turn boundary as the candidate cap. Same degrade-to-uncapped contract.
pub(crate) fn current_token() -> String {
    turn_token()
}

/// The engine-written W3C `traceparent` for the CURRENT turn (`<storage>/turn-traceparent`, the
/// per-turn sibling of `turn-token`). The long-lived broker re-reads it per tool call: a
/// process-level env would pin every turn's spans onto the first turn's trace. Empty when the
/// engine hasn't written one (an older driver, a local run); the caller then falls back or roots.
pub(crate) fn current_traceparent() -> String {
    read_turn_token(&forge::storage_root().join("turn-traceparent"))
}

/// Read + trim a turn-token file; empty string on any failure. Split from [`turn_token`] so the
/// file contract is testable without touching the process env.
fn read_turn_token(path: &Path) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn load_state() -> TurnState {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}
fn save_state(st: &TurnState) {
    if let Ok(s) = serde_json::to_string(st) {
        if let Some(p) = state_path().parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let _ = std::fs::write(state_path(), s);
    }
}

/// Pure budget transition: reset on a new turn, block once `max` candidates have deployed, else count
/// the candidate (when `consume`). Split out from the IO so the state machine is unit-tested.
fn decide(mut st: TurnState, token: &str, max: u32, consume: bool) -> (TurnState, Budget) {
    if st.token != token {
        st = TurnState {
            token: token.to_string(),
            deploys: 0,
        };
    }
    if max > 0 && st.deploys >= max {
        let reason = format!(
            "this turn's candidate budget is spent ({max} deployed). Commit your best candidate to \
             CANDIDATE.md and END your turn — the loop measures it and gives you a fresh turn for \
             the next idea."
        );
        return (st, Budget::Spent { reason });
    }
    if consume {
        st.deploys += 1;
    }
    let remaining = (max > 0).then(|| max.saturating_sub(st.deploys));
    (st, Budget::Ok { remaining })
}

/// Check (and update) the per-turn candidate budget. `consume = true` for a `deploy_candidate` (it
/// counts a candidate); `false` for a `build_epp` peek (compile feedback isn't itself capped, but it
/// reports `Spent` once the deploy budget is gone, so the agent stops building too).
pub(crate) fn check(consume: bool) -> Budget {
    let token = turn_token();
    let st = load_state();
    let new_turn = !token.is_empty() && st.token != token;
    // A new turn clears a stale manual wrap-up left over from a previous turn.
    if new_turn {
        let _ = std::fs::remove_file(wrap_flag());
    }
    if wrap_flag().exists() {
        return Budget::Spent {
            reason:
                "operator requested wrap-up. Commit your best candidate to CANDIDATE.md and END \
                     your turn."
                    .into(),
        };
    }
    let max = max_deploys();
    if max == 0 || token.is_empty() {
        // Uncapped, or no turn signal: persist a turn reset so counts don't bleed across turns if
        // the cap gets enabled later, but never block.
        if new_turn {
            save_state(&TurnState { token, deploys: 0 });
        }
        return Budget::Ok { remaining: None };
    }
    let (st, b) = decide(st, &token, max, consume);
    save_state(&st);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_turn_resets_the_count() {
        let st = TurnState {
            token: "turn-A".into(),
            deploys: 5,
        };
        let (st, b) = decide(st, "turn-B", 2, true);
        assert_eq!(st.token, "turn-B");
        assert_eq!(st.deploys, 1, "reset to 0, then this deploy counted");
        assert!(matches!(b, Budget::Ok { remaining: Some(1) }));
    }

    #[test]
    fn blocks_once_max_candidates_deployed() {
        let st = TurnState {
            token: "t".into(),
            deploys: 2,
        };
        let (st, b) = decide(st, "t", 2, true);
        assert_eq!(st.deploys, 2, "a blocked deploy does not count");
        assert!(matches!(b, Budget::Spent { .. }));
    }

    #[test]
    fn counts_up_to_the_cap_then_reports_remaining() {
        let st = TurnState {
            token: "t".into(),
            ..Default::default()
        };
        let (st, b) = decide(st, "t", 2, true);
        assert!(matches!(b, Budget::Ok { remaining: Some(1) }));
        let (_st, b) = decide(st, "t", 2, true);
        assert!(
            matches!(b, Budget::Ok { remaining: Some(0) }),
            "last candidate allowed, 0 left"
        );
    }

    #[test]
    fn peek_does_not_consume() {
        let st = TurnState {
            token: "t".into(),
            deploys: 1,
        };
        let (st, b) = decide(st, "t", 2, false);
        assert_eq!(st.deploys, 1, "build_epp peek leaves the count unchanged");
        assert!(matches!(b, Budget::Ok { remaining: Some(1) }));
    }

    /// The driver-written token file is the turn signal: trimmed content when present, empty (=>
    /// uncapped) when missing; never an error, never a stale default.
    #[test]
    fn turn_token_reads_the_driver_file_or_degrades_empty() {
        let dir = std::env::temp_dir().join(format!("turn-token-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("turn-token");

        assert_eq!(read_turn_token(&path), "", "missing file degrades to empty");
        std::fs::write(&path, "  12345-987654321\n").unwrap();
        assert_eq!(read_turn_token(&path), "12345-987654321", "trimmed content");
        std::fs::remove_dir_all(&dir).ok();
    }
}
