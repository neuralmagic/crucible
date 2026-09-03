//! The boundary between the orchestration loop and how it talks to a human.
//!
//! The loop (`crate::run_loop`) is written once and calls a [`Reporter`]; the
//! implementations are [`crate::console::ConsoleReporter`] (headless: plain lines,
//! for CI / in-cluster pods / pipes) and [`crate::stream::SessionReporter`] (NDJSON session
//! events). All drive the identical keep/discard logic, so headless is a first-class
//! mode, not a degraded fallback.

use crate::{Args, Paths};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Cumulative-budget context for one agent turn. Cost is only authoritative at turn
/// end, so mid-turn the reporters price the streamed token samples (see
/// [`crate::event::provisional_cost`]) and emit provisional budget updates; the
/// loop's turn-end budget call reconciles them. Without this a run could spend its
/// whole cap inside one turn before any guard fires.
#[derive(Clone)]
pub struct TurnBudget {
    /// Run spend before this turn started (authoritative).
    pub spent_before: f64,
    /// When the run started, so provisional updates carry the run-relative clock.
    pub started: Instant,
    /// Effective cost cap resolved by the loop (live control override or the CLI
    /// arg); 0 means uncapped.
    pub max_cost: f64,
    /// Live liveness telemetry for provisional, mid-turn spend. The authoritative
    /// total still comes from the completed turn and overwrites this estimate.
    pub(crate) heartbeat: Option<Arc<crate::heartbeat::Heartbeat>>,
}

impl TurnBudget {
    /// Whether a provisional spend crosses the cap (same threshold as the loop's
    /// between-iteration guard).
    pub fn over_cap(&self, spent: f64) -> bool {
        self.max_cost > 0.0 && spent >= self.max_cost
    }

    /// Publish a provisional cumulative total while the agent is still running.
    pub fn record_provisional(&self, iter: u32, spent: f64) {
        if let Some(heartbeat) = &self.heartbeat {
            heartbeat.record(iter, spent);
        }
    }
}

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
    pub evidence: Vec<crate::session::EvidenceEntry>,
    /// The agent's whole CANDIDATE.md (`note` is its 120-char single-line fold). The PR
    /// body prints this; every table keeps using `note`. Empty when the agent wrote none.
    pub candidate_md: String,
}

/// One-line evidence rendering for the human-readable outputs (console rows, RESULTS.md,
/// PR bodies): `refcheck ✓ calc-diff ✓ tensor-pipe SKIPPED (reason)`.
pub(crate) fn evidence_line(evidence: &[crate::session::EvidenceEntry]) -> String {
    evidence
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run-wide context every front-end needs to render the start banner. Built once
/// from [`Args`] so the NDJSON reporters don't each re-derive it.
#[derive(Clone)]
pub struct RunMeta {
    pub namespace: String,
    pub model: String,
    pub iters_total: u32,
    pub max_cost: f64,
    pub max_secs: u64,
}

impl RunMeta {
    pub fn from_args(args: &Args) -> Self {
        Self {
            namespace: args.namespace.clone(),
            model: args.model().to_string(),
            iters_total: args.iterations,
            max_cost: args.max_cost,
            max_secs: args.max_time().map(|d| d.as_secs()).unwrap_or(0),
        }
    }
}

/// What the run achieved, for the process exit code.
#[derive(Debug, Clone, Copy, Default)]
pub struct Outcome {
    pub improved: bool,
    pub solved: bool,
    /// The agent declared the harness inadequate (harness-inadequate escalation): the run stopped for
    /// human review, distinct from both success and a plain no-improvement.
    pub escalated: bool,
}

impl Outcome {
    /// CI exit code. Escalation gets its own code (2) so the outer pod / CI can route a
    /// "needs human" stop separately from success (0) and no-improvement (1).
    pub fn exit_code(&self) -> i32 {
        if self.escalated {
            2
        } else if self.improved || self.solved {
            0
        } else {
            1
        }
    }
}

/// Which stage of the loop is running, for the progress display.
#[derive(Clone, Copy)]
pub enum Phase {
    /// The zero-agent-cost rung ladder against the unmodified tree, before any baseline.
    Preflight,
    Baseline,
    Iteration(u32),
}

/// What the operator decided at an interrupt checkpoint.
pub enum Stop {
    Continue,
    Quit,
}

/// The outcome of one agent turn the loop needs beyond the streamed events: the
/// cost to budget on, and whether the CLI reported the turn as an error so the loop
/// can discard a no-op instead of measuring an unchanged workspace as a success.
#[derive(Debug, Clone, Default)]
pub struct AgentTurn {
    /// The turn's cost (USD).
    pub cost: f64,
    /// The `result` event's `is_error` flag, a failed turn (e.g. a credential-less
    /// "Not logged in" no-op) even when its subtype is "success".
    pub is_error: bool,
    /// The failure text the CLI reported (the synthetic message), for the discard note.
    pub error: Option<String>,
}

/// Everything the loop needs from its front-end. The loop never prints directly.
pub trait Reporter {
    /// The run is starting with this goal and objective label.
    fn start(&mut self, goal: &str, objective: &str);
    /// A new stage began.
    fn phase(&mut self, phase: Phase);
    /// A free-form progress note (setup steps, steer injection, ...).
    fn note(&mut self, msg: &str);
    /// A decided row (baseline / keep / discard); `solved` flags a winning fix.
    fn row(&mut self, row: &Row, solved: bool);
    /// Run the agent subprocess for `it`, streaming its output to the front-end.
    /// Returns the turn's cost and its error verdict ([`AgentTurn`]) so the loop can
    /// budget on it and discard a failed no-op turn. `budget` carries the run spend
    /// so far and the cost cap: reporters stream provisional budget updates from
    /// mid-turn token samples and may stop a local agent once the provisional spend
    /// crosses the cap.
    #[allow(clippy::too_many_arguments)]
    fn run_agent(
        &mut self,
        args: &Args,
        p: &Paths,
        it: u32,
        prompt: &str,
        resume_prompt: Option<&str>,
        session: Option<&str>,
        budget: TurnBudget,
    ) -> AgentTurn;
    /// Cumulative budget after a turn; lets the front-end draw a gauge / warn.
    fn budget(&mut self, _spent: f64, _elapsed: Duration) {}
    /// Interrupt checkpoint: decide whether to stop.
    fn check_interrupt(&mut self, p: &Paths, rows: &[Row]) -> Stop;
    /// A (re-)scope boundary (a re-scope boundary): a fresh baseline + `fingerprint` begin a new comparable
    /// segment (`regime` labels the new evaluation regime). The default renders a note; the
    /// session reporter overrides it to emit a structured [`crate::session::SessionEvent::Segment`].
    fn segment(&mut self, fingerprint: &str, baseline_score: f64, regime: &str) {
        self.note(&format!(
            "segment [{regime}] fingerprint={fingerprint} baseline={baseline_score}"
        ));
    }
    /// The run's comparability key, reported once at run start (and again on
    /// `--resume`, the freshly recomputed value). The default renders a note; the session
    /// reporter overrides it to emit a structured [`crate::session::SessionEvent::Identity`].
    fn identity(&mut self, identity: &crate::identity::RunIdentity) {
        self.note(&format!(
            "run identity {} ({} component(s), measure_cmd={})",
            identity.digest,
            identity.components.len(),
            identity.measure_cmd
        ));
    }
    /// The agent escalated (declared the harness inadequate), halting the run for human review.
    /// The default renders it as a note; the session reporter overrides this to emit a structured
    /// [`crate::session::SessionEvent::Escalation`].
    fn escalation(&mut self, esc: &crate::escalation::Escalation) {
        let evidence = if esc.evidence.trim().is_empty() {
            String::new()
        } else {
            format!(" — evidence: {}", esc.evidence.trim())
        };
        self.note(&format!(
            "ESCALATION [{}]: {}{evidence}",
            esc.category, esc.reason
        ));
    }
    /// An additive work-graph wire line (`PlanAdmitted` / `TaskResult`) from a graph-loop
    /// iteration. Default no-op: only the session reporter persists them; the console
    /// front-end has no plan rendering, and the legacy sequencing path never emits one.
    fn plan_event(&mut self, _ev: &crate::session::SessionEvent) {}
    /// How this resume classified the previous shutdown; the session reporter emits a
    /// structured [`crate::session::SessionEvent::Recovery`].
    fn recovery(&mut self, class: crate::session::RecoveryClass, iter: u32, detail: &str) {
        self.note(&format!("recovery: {class} (iter {iter}): {detail}"));
    }
    /// Opens the approval bracket a resume's classifier reads: a dangling wait means the
    /// run died with the approval open.
    fn approval_wait(&mut self, handle: &str, trace_id: &str, mode: crate::provisioning::WaitMode) {
        self.note(&format!(
            "approval wait [{}] {handle} ({trace_id})",
            mode.as_str()
        ));
    }
    /// The wait reached a terminal outcome (`granted`/`denied`/`timeout`), closing the
    /// bracket. Deliberately NOT called on a stop-while-parked: a stop doesn't resolve
    /// the ask, and the still-open bracket makes a resume re-park on it.
    fn approval_resolved(&mut self, outcome: &str, reason: &str) {
        self.note(&format!("approval {outcome}: {reason}"));
    }
    /// The draft PR(s) publish-on-keep opened, reported once after publish so the durable session
    /// log carries them (the controller's pull-ingest folds them onto the kept candidates' `pr_url`).
    /// The default is a no-op, the console already prints each URL via `note`; only the session
    /// reporter persists a structured [`crate::session::SessionEvent::PrLinks`] line. Best-effort:
    /// an append failure must never fail the run.
    fn pr_links(&mut self, _links: &[crate::session::PrLinkWire]) {}
    /// The run finished; render the final summary.
    fn summary(&mut self, rows: &[Row], objective: &str, best_score: f64);
    /// The loop is exiting, called exactly once as the very last thing `run_loop` does before
    /// returning (every exit path, including an early-return error). `outcome` is one of
    /// `finished`/`solved`/`budget`/`stopped`/`escalated`/`error`; `reason` is a short
    /// human-readable detail. The default renders a note; the session reporter overrides it to
    /// emit a structured [`crate::session::SessionEvent::Shutdown`] as the log's final line, so a
    /// consumer tailing it can tell "the run ended" from "the stream just went quiet".
    fn shutdown(&mut self, outcome: &str, reason: &str) {
        self.note(&format!("run ended: {outcome} ({reason})"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn over_cap_matches_the_loop_guard_threshold() {
        let b = TurnBudget {
            spent_before: 0.0,
            started: Instant::now(),
            max_cost: 0.0,
            heartbeat: None,
        };
        assert!(!b.over_cap(1e9), "0 means uncapped");
        let b = TurnBudget { max_cost: 5.0, ..b };
        assert!(!b.over_cap(4.99));
        assert!(b.over_cap(5.0), ">= like over_budget");
    }

    #[test]
    fn provisional_spend_reaches_the_heartbeat() {
        let heartbeat = Arc::new(crate::heartbeat::Heartbeat::new());
        let budget = TurnBudget {
            spent_before: 1.0,
            started: Instant::now(),
            max_cost: 5.0,
            heartbeat: Some(Arc::clone(&heartbeat)),
        };

        budget.record_provisional(2, 1.2345);

        assert!((heartbeat.spent_usd() - 1.235).abs() < 1e-9);
    }
}
