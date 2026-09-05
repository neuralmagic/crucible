//! One iteration's steps in driver vocabulary: measure a candidate, rule keep/discard, read the
//! turn's markers. Shared by the hand-sequenced loop and the graph runner.

use crate::args::{Args, Paths};
use crate::control;
use crate::control::escalation;
use crate::control::provisioning;
use crate::report::session::Row;
use crate::report::{AgentTurn, Reporter, Stop};
use anyhow::Result;
use crucible::crucible::{Judge, World};

/// The secondary numeric a [`crucible::crucible::Reading`] may carry in its detail JSON (a test
/// gate's total test count). The engine threads it as `baseline_total` into the judge; a
/// domain without it just reports `None`.
pub(crate) fn reading_total(r: &crucible::crucible::Reading) -> Option<u64> {
    r.detail.get("total").and_then(|v| v.as_u64())
}

/// The judge measured the live candidate. Holds everything the decision and the results row need,
/// so a kept row carries its reading by construction.
pub(crate) struct Measured {
    pub(crate) reading: crucible::crucible::Reading,
    pub(crate) note: String,
    pub(crate) diff: String,
    pub(crate) diffstat: String,
    /// The grade step's declared-vs-ran evidence record; empty everywhere but the graph
    /// runner's grade task (the plain measure path has no declared evidence set).
    pub(crate) evidence: Vec<crate::report::session::EvidenceEntry>,
    /// The agent's whole CANDIDATE.md (see [`candidate_note`]); rides the row to publish.
    pub(crate) candidate_md: String,
}

/// The outcome of [`decide_row`]: the results row, the keep/discard verdict, and the reading
/// the keep path commits (its score becomes `best_score`, its note labels the snapshot).
pub(crate) struct Decided {
    pub(crate) row: Row,
    pub(crate) verdict: crucible::crucible::Decision,
    pub(crate) reading: crucible::crucible::Reading,
}

/// One iteration's outcome in driver vocabulary, produced by either path (the typestate
/// chain or the graph template) and folded by the shared keep/discard tail in
/// [`run_loop_body`].
pub(crate) enum IterStep {
    Decided(Box<Decided>),
    /// Discard and move on (failed turn, failed apply, gate rejection). The reason lands in the
    /// iteration's Row, so the run summary counts every iteration honestly — a run that lost all
    /// its iterations must not read as a clean "finished" with an empty scoreboard.
    Discarded {
        reason: String,
    },
    /// The turn never started: a transport-class death (sandbox setup, auth, connection)
    /// before the agent produced anything. There is no candidate to discard, so the driver
    /// re-runs the SAME iteration instead of consuming it, bounded by
    /// [`MAX_DEAD_TURN_ATTEMPTS`] consecutive attempts.
    NeverStarted {
        reason: String,
    },
    /// Halt for human review (the escalation is already reported).
    Escalated,
    /// Park at the next iteration head on a blocking approval.
    Parked(provisioning::PendingProvisioning),
    /// Stop signal at the post-turn checkpoint.
    Stopped,
}

/// What the post-turn sentinel drains decided, in the exact order the loop checks them.
/// Notes and the structured escalation event are emitted in here; the caller owns the
/// world rollback and the loop control that follows. Shared by the typestate path and the
/// graph runner so both react to a turn identically.
pub(crate) enum TurnVerdict {
    Proceed,
    /// The turn failed (`is_error`): discard the iteration.
    Discard,
    /// The turn died on a transport-class error (auth expiry, rate limit, connection): worth
    /// re-running the turn — the session survives, so a retry resumes rather than restarts.
    /// Carries the surfaced error so the retry/stall records name the actual failure.
    Retry(String),
    /// The agent escalated: halt for human review.
    Escalate,
    /// The agent blocked on a pending approval with no fallback.
    Park(provisioning::PendingProvisioning),
    Stop,
}

/// Whether an agent-turn error is transport-class: the infrastructure between us and the model
/// failed, not the turn's content. Matched on the surfaced error text because the CLI backends
/// flatten their HTTP/auth failures to strings.
fn is_transport_turn_error(why: &str) -> bool {
    let why = why.to_ascii_lowercase();
    // HTTP status codes match only as standalone tokens: a bare `contains("502")` would
    // misclassify any error mentioning "5023 rows" or a path like `/tmp/x502`.
    let codes = ["401", "403", "429", "500", "502", "503", "529"];
    let code_hit = why
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|tok| codes.contains(&tok));
    code_hit
        || [
            "unauthenticated",
            "invalid authentication",
            "rate limit",
            "overloaded",
            "connection reset",
            "connection refused",
            "failed to connect",
            "timed out",
            "timeout",
        ]
        .iter()
        .any(|sig| why.contains(sig))
}

pub(crate) fn drain_turn_markers<R: Reporter>(
    r: &mut R,
    p: &Paths,
    control: Option<&control::ControlState>,
    it: u32,
    turn: &AgentTurn,
    rows: &[Row],
) -> TurnVerdict {
    // A failed agent turn (the CLI reported is_error, e.g. a credential-less
    // "Not logged in" no-op with subtype "success", cost 0) left the workspace
    // untouched. Discard the iteration with the reason instead of measuring an
    // unchanged workspace and logging the no-op as a keep/discard "success", the
    // same restore-and-continue an unscoreable (apply-failed) candidate takes.
    if turn.is_error {
        let why = turn.error.as_deref().unwrap_or("agent reported an error");
        // A transport-class death (expired token, rate limit, dropped connection) says nothing
        // about the candidate; the session survives, so a retried turn resumes where it died
        // instead of the iteration being discarded (a 5h turn once died ungraded on one 401).
        if is_transport_turn_error(why) {
            r.note(&format!(
                "agent turn hit a transport error (retrying): {why}"
            ));
            return TurnVerdict::Retry(why.to_string());
        }
        r.note(&format!("agent turn failed (discarding iter {it}): {why}"));
        return TurnVerdict::Discard;
    }

    // The agent's sanctioned move against a frozen judge: if it declared the harness
    // inadequate this turn, restore the world and halt for human review, never measure
    // or keep on top of an escalation. A malformed marker is surfaced but ignored, so
    // a stray/garbled file can't wedge a paid run.
    match escalation::take(&p.escalation) {
        Some(Ok(esc)) => {
            r.escalation(&esc);
            return TurnVerdict::Escalate;
        }
        Some(Err(msg)) => r.note(&format!("ignoring malformed ESCALATION.json: {msg}")),
        None => {}
    }

    // The agent opened a mediated-provisioning approval and recorded how to wait. `block`
    // means it has no frozen-regime fallback, so skip the (empty) measure and park at the
    // next iteration head; `continue` means it left a fallback candidate, so fall through
    // and measure it while the re-scope lands asynchronously.
    match provisioning::take(&p.provisioning) {
        Some(Ok(pp)) => {
            // Record the regime the approval would grant so an operator `approve` over the
            // control bridge can resolve it (the attended path). The forge path sends its
            // own `rescope`; both converge on the iteration-head drain.
            if let Some(control) = control.as_ref() {
                control.set_pending_regime(pp.trace_id.clone());
            }
            // Open the approval bracket on the wire: a dangling wait (no resolve before
            // the log ends) is how a resume knows an approval was still outstanding.
            r.approval_wait(&pp.handle, &pp.trace_id, pp.mode);
            match pp.mode {
                provisioning::WaitMode::Block => {
                    r.note(&format!(
                        "parked: blocked on approval {} — nothing else to try, awaiting the re-scope",
                        pp.handle
                    ));
                    return TurnVerdict::Park(pp);
                }
                provisioning::WaitMode::Continue => r.note(&format!(
                    "awaiting approval {} — continuing in the frozen regime",
                    pp.handle
                )),
            }
        }
        Some(Err(msg)) => r.note(&format!(
            "ignoring malformed PROVISIONING_PENDING.json: {msg}"
        )),
        None => {}
    }

    if matches!(r.check_interrupt(p, rows), Stop::Quit) {
        return TurnVerdict::Stop;
    }
    TurnVerdict::Proceed
}

/// Measure the live candidate: the judge's reading, the note (the agent's CANDIDATE.md
/// summary when it wrote one, else the measure's note), and the staged diff captured before
/// keep/discard commits or resets it. Shared by the typestate path and the graph runner so
/// both measure identically.
pub(crate) fn measure_candidate(
    judge: &dyn Judge,
    ctx: &crucible::crucible::MeasureCtx,
    p: &Paths,
    world: &dyn World,
) -> Result<Measured> {
    let reading = judge.measure(ctx)?;
    Ok(measured_from_reading(reading, p, world))
}

/// Attach the candidate note and diff to an authored reading.
pub(crate) fn measured_from_reading(
    reading: crucible::crucible::Reading,
    p: &Paths,
    world: &dyn World,
) -> Measured {
    let (candidate_note, candidate_md) = candidate_note(p);
    let note = if candidate_note.is_empty() {
        reading.note.clone()
    } else {
        candidate_note
    };
    let (diff, diffstat) = capture_diff(world);
    Measured {
        reading,
        note,
        diff,
        diffstat,
        evidence: Vec::new(),
        candidate_md,
    }
}

/// Rule keep/discard on a measured candidate and build its results row: the one
/// decision+row constructor, shared by the typestate path and the graph runner. The
/// reading the row reports and the keep path commits is the one that was actually
/// measured (`Measured` is consumed).
pub(crate) fn decide_row(
    judge: &dyn Judge,
    best_score: f64,
    best_tiebreak: Option<f64>,
    it: u32,
    m: Measured,
) -> Decided {
    let Measured {
        reading,
        note,
        diff,
        diffstat,
        evidence,
        candidate_md,
    } = m;
    let verdict = judge.decide(&reading, best_score, best_tiebreak);
    let row = Row {
        iter: it,
        decision: if verdict.keep { "keep" } else { "discard" }.into(),
        note,
        detail: judge.detail(&reading),
        diff,
        diffstat,
        score: reading.score,
        tiebreak: reading.tiebreak,
        total: reading_total(&reading),
        phase: None,
        kept_snap: None,
        evidence,
        candidate_md,
    };
    Decided {
        row,
        verdict,
        reading,
    }
}

/// True when a cost/time cap is set and reached; notes it on `r`. `parked_total` is idle time
/// spent waiting on a human approval, excluded from the wall-clock the time cap measures.
/// The effective cost cap: a live control override wins over the CLI arg.
pub(crate) fn live_max_cost(args: &Args, control: Option<&control::ControlState>) -> f64 {
    control
        .and_then(control::ControlState::live_max_cost)
        .unwrap_or(args.max_cost)
}

/// Capture the agent's staged change for this iteration before keep/discard
/// commits or resets it. Returns (full diff, one-line shortstat).
pub(crate) fn capture_diff(world: &dyn World) -> (String, String) {
    // The world stages + diffs its own repo(s): a single workspace for GitWorld/CommandWorld, EVERY
    // component for CompositeWorld (the composite base dir isn't a repo, so a top-level diff is empty).
    let (diff, stat) = world.staged_diff();
    // Cap a runaway diff so it never blows up the UI / results log. Slice on a char boundary so a
    // multi-byte char straddling the limit can't panic.
    let diff = if diff.len() > 200_000 {
        let end = (0..=200_000)
            .rev()
            .find(|&i| diff.is_char_boundary(i))
            .unwrap_or(0);
        format!("{}\n… (diff truncated at 200 KB)", &diff[..end])
    } else {
        diff
    };
    (diff, stat.trim().to_string())
}

/// Read (and consume) the agent's CANDIDATE.md, returning `(note, full)`: the 120-char
/// single-line fold for tables, plus the whole text so the PR body prints the actual
/// writeup (DeepGEMM#5 shipped only the fold, truncated mid-word). Consumed because the
/// file is harness furniture excluded from git; without the delete, a discard's clean no
/// longer removes it and a stale note would bleed into later iterations' rows.
pub(crate) fn candidate_note(p: &Paths) -> (String, String) {
    let path = p.workspace.join("CANDIDATE.md");
    let full: String = std::fs::read_to_string(&path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    let note = full.replace('\n', " ").chars().take(120).collect();
    (note, full)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 401 that killed a 5h turn, plus the other transport signatures, classify as retryable;
    /// content-level failures (an escalation-worthy error string, a plain crash) do not.
    #[test]
    fn transport_turn_errors_classify_retryable() {
        for why in [
            "Failed to authenticate. API Error: 401 [{\"error\":{\"code\":401,\"status\":\"UNAUTHENTICATED\"}}]",
            "API Error: 429 rate limit exceeded",
            "server overloaded, try again later",
            "connection reset by peer",
            "request timed out",
        ] {
            assert!(is_transport_turn_error(why), "{why}");
        }
        for why in [
            "agent reported an error",
            "the model refused the task",
            "candidate deleted the reference oracle",
        ] {
            assert!(!is_transport_turn_error(why), "{why}");
        }
    }
}
