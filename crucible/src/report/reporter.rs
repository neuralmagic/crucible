//! The [`Reporter`] a scored loop talks to, and the turn, run and outcome context it passes.

use crate::args::{Args, Paths};
use crate::report::session::Row;
use crucible_contract::LoopPhase;
use std::time::Duration;

/// One-line evidence rendering for the human-readable outputs (console rows, RESULTS.md,
/// PR bodies): `refcheck ✓ calc-diff ✓ tensor-pipe SKIPPED (reason)`.
pub(crate) fn evidence_line(evidence: &[crate::report::session::EvidenceEntry]) -> String {
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

/// What the operator decided at an interrupt checkpoint.
pub enum Stop {
    Continue,
    Quit,
}

/// Everything the loop needs from its front-end. The loop never prints directly.
pub trait Reporter {
    /// The run is starting with this goal and objective label.
    fn start(&mut self, goal: &str, objective: &str);
    /// The loop moved to `phase` at iteration `iter` (0 before the first).
    fn phase(&mut self, phase: LoopPhase, iter: u32);
    /// A free-form progress note (setup steps, steer injection, ...).
    fn note(&mut self, msg: &str);
    /// A decided row (baseline / keep / discard); `solved` flags a winning fix.
    fn row(&mut self, row: &Row, solved: bool);
    /// The candidate turn of `iter` is starting on `model`, in `session` when it has one.
    fn agent_starting(
        &mut self,
        _iter: u32,
        _model: &str,
        _session: Option<&crate::agent::agent_session::SessionTurn>,
    ) {
    }
    /// One line of the running agent's output.
    fn agent_line(&mut self, _line: &str, _stream: crate::agent::event::RawStream) {}
    /// Cumulative budget after a turn; lets the front-end draw a gauge / warn.
    fn budget(&mut self, _spent: f64, _elapsed: Duration) {}
    /// Interrupt checkpoint: decide whether to stop.
    fn check_interrupt(&mut self, p: &Paths, rows: &[Row]) -> Stop;
    /// A (re-)scope boundary (a re-scope boundary): a fresh baseline + `fingerprint` begin a new comparable
    /// segment (`regime` labels the new evaluation regime). The default renders a note; the
    /// session reporter overrides it to emit a structured [`crate::report::session::SessionEvent::Segment`].
    fn segment(&mut self, fingerprint: &str, baseline_score: f64, regime: &str) {
        self.note(&format!(
            "segment [{regime}] fingerprint={fingerprint} baseline={baseline_score}"
        ));
    }
    /// The run's comparability key, reported once at run start (and again on
    /// `--resume`, the freshly recomputed value). The default renders a note; the session
    /// reporter overrides it to emit a structured [`crate::report::session::SessionEvent::Identity`].
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
    /// [`crate::report::session::SessionEvent::Escalation`].
    fn escalation(&mut self, esc: &crate::control::escalation::Escalation) {
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
    /// An additive work-graph wire line (`PlanAdmitted` / `TaskResult`) from a loop iteration's plan
    /// iteration. Default no-op: only the session reporter persists them; the console
    /// front-end has no plan rendering, and the legacy sequencing path never emits one.
    fn plan_event(&mut self, _ev: &crate::report::session::SessionEvent) {}
    /// How this resume classified the previous shutdown; the session reporter emits a
    /// structured [`crate::report::session::SessionEvent::Recovery`].
    fn recovery(&mut self, class: crate::report::session::RecoveryClass, iter: u32, detail: &str) {
        self.note(&format!("recovery: {class} (iter {iter}): {detail}"));
    }
    /// Opens the approval bracket a resume's classifier reads: a dangling wait means the
    /// run died with the approval open.
    fn approval_wait(
        &mut self,
        handle: &str,
        trace_id: &str,
        mode: crate::control::provisioning::WaitMode,
    ) {
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
    /// reporter persists a structured [`crate::report::session::SessionEvent::PrLinks`] line. Best-effort:
    /// an append failure must never fail the run.
    fn pr_links(&mut self, _links: &[crate::report::session::PrLinkWire]) {}
    /// The run finished; render the final summary.
    fn summary(&mut self, rows: &[Row], objective: &str, best_score: f64);
    /// The loop is exiting, called exactly once as the very last thing `run_loop` does before
    /// returning (every exit path, including an early-return error). `outcome` is one of
    /// `finished`/`solved`/`budget`/`stopped`/`escalated`/`error`; `reason` is a short
    /// human-readable detail. The default renders a note; the session reporter overrides it to
    /// emit a structured [`crate::report::session::SessionEvent::Shutdown`] as the log's final line, so a
    /// consumer tailing it can tell "the run ended" from "the stream just went quiet".
    fn shutdown(&mut self, outcome: &str, reason: &str) {
        self.note(&format!("run ended: {outcome} ({reason})"));
    }
}
