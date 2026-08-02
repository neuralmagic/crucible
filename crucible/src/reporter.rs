//! The boundary between the orchestration loop and how it talks to a human.
//!
//! The loop (`crate::run_loop`) is written once and calls a [`Reporter`]; the
//! implementations are [`crate::console::ConsoleReporter`] (headless: plain lines,
//! for CI / in-cluster pods / pipes) and [`crate::stream::SessionReporter`] (NDJSON session
//! events). All drive the identical keep/discard logic, so headless is a first-class
//! mode, not a degraded fallback.

use crate::{Args, Paths};
use std::time::Duration;

/// One row in the results log / final summary.
#[derive(Clone, Default)]
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
    /// Total test count for this row (test gate), for the same reason.
    pub total: Option<u64>,
    /// `Some("wide")` for wide-round rows; `None` for the deep (default) loop.
    pub phase: Option<String>,
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
            model: args.model.clone(),
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
    /// budget on it and discard a failed no-op turn.
    fn run_agent(
        &mut self,
        args: &Args,
        p: &Paths,
        it: u32,
        prompt: &str,
        resume_prompt: Option<&str>,
        session: Option<&str>,
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
