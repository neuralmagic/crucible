//! The loop's decisions, with no I/O in them.
//!
//! [`Machine`] holds the run's state and answers "what happens next" from plain inputs: the
//! head-of-iteration checks in the order the loop has always made them, the fold of a turn's
//! outcome into a row, the keep/discard bookkeeping, and the exit. The host in
//! [`crate::loop_driver`] performs every effect (agent turns, world snapshots, the reporter, the
//! control bridge, publish) and hands the results back. A test drives the machine with literal
//! inputs and asserts the decisions; nothing here can touch a file, a socket, or a clock.
//!
//! ```text
//!   host                                    machine
//!   ─────────────────────────────────────── ───────────────────────────────
//!   poll control/markers/clock ──inputs──▶  head()          ──Head──▶ act
//!   run turn / plan          ──IterStep──▶  settle()        ──Settle─▶ act
//!   snapshot / restore world ──result───▶   keep() / discard()
//!   poll budget              ──inputs──▶    over_budget()   ──exit───▶ shutdown
//! ```

use std::time::Duration;

use crate::provisioning::PendingProvisioning;
use crate::reporter::Row;

/// The loop's ceilings and switches, read once from `Args`.
#[derive(Debug, Clone)]
pub(crate) struct LoopCfg {
    pub iterations: u32,
    pub max_time: Option<Duration>,
    pub max_park: Option<Duration>,
    pub no_early_stop: bool,
}

/// The comparable segment: a baseline and everything measured against it. A re-scope swaps
/// the whole thing, so a goalpost can never be half-moved.
#[derive(Debug, Clone)]
pub(crate) struct Segment {
    pub regime: String,
    pub fingerprint: String,
    pub baseline_score: f64,
    pub best_score: f64,
    pub best_tiebreak: Option<f64>,
    pub baseline_total: u64,
    /// The world snapshot token of the kept best, the rollback target.
    pub best_snap: String,
}

/// The run's state across iterations. Everything the log can reproduce is here in the same
/// shape the contract fold restores it; the snapshot tokens and the pending park are the
/// process-local additions.
#[derive(Debug, Clone)]
pub(crate) struct RunState {
    pub rows: Vec<Row>,
    pub spent: f64,
    /// SHAs of commits this run kept, for the publish summary.
    pub kept_shas: Vec<String>,
    /// The pristine upstream SHA segment 0 was measured on; `None` on resume.
    pub base_sha: Option<String>,
    /// The pristine baseline snapshot token; `None` on resume.
    pub base_snap: Option<String>,
    pub solved_any: bool,
    /// Idle time spent parked on a human, excluded from the time cap. A resumed process
    /// measures its own wall clock, so this starts at zero there.
    pub parked_total: Duration,
    /// Never-started turns already spent on the current iteration. Restored on resume so the
    /// attempt bound counts the iteration's attempts, not one process's.
    pub dead_turns: u32,
    /// The block-mode approval the next head parks on.
    pub pending_block: Option<PendingProvisioning>,
    /// Branches prior publishes opened PRs from; publish skips them.
    pub published_branches: Vec<String>,
    pub segment: Segment,
}

/// How a run ended: the single enumeration of every way the loop exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopExit {
    /// The loop ran every iteration without an early exit.
    Finished,
    /// A kept candidate satisfied the win condition.
    Solved,
    /// A cost or time cap was reached.
    Budget,
    /// A stop signal, at a checkpoint or while parked.
    Stopped,
    /// The agent declared the harness inadequate, or a `block` approval was denied with no
    /// fallback: halt for human review.
    Escalated,
    /// [`MAX_DEAD_TURN_ATTEMPTS`] consecutive turns died before starting.
    Stalled,
}

impl LoopExit {
    /// The wire token and reason for the shutdown line.
    pub(crate) fn shutdown_reason(self) -> (&'static str, &'static str) {
        match self {
            LoopExit::Finished => ("finished", "all iterations completed"),
            LoopExit::Solved => ("solved", "a kept candidate satisfied the win condition"),
            LoopExit::Budget => ("budget", "a cost or time cap was reached"),
            LoopExit::Stopped => ("stopped", "stop signal received"),
            LoopExit::Escalated => (
                "escalated",
                "the agent declared the harness inadequate — halted for human review",
            ),
            LoopExit::Stalled => (
                "stalled",
                "the run stalled on consecutive transport failures — no turn could start",
            ),
        }
    }

    /// Whether the run concluded on its own terms, so the epilogue and the final publish run.
    pub(crate) fn concluded(self) -> bool {
        matches!(
            self,
            LoopExit::Finished | LoopExit::Budget | LoopExit::Solved
        )
    }
}

/// Consecutive never-started turns after which the run halts as [`LoopExit::Stalled`]. Such a
/// turn re-runs its iteration instead of consuming it, so without this bound one dead node could
/// spin the run forever.
pub(crate) const MAX_DEAD_TURN_ATTEMPTS: u32 = 3;

/// Why a park ended, as the host observed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParkOutcome {
    /// A grant landed as a re-scope; the head's rescope drain re-baselines.
    Resumed,
    /// A denial or a park timeout.
    Denied(String),
    /// A stop while parked.
    Stopped,
}

/// Why a distress park ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DistressOutcome {
    /// The operator removed the marker: that is the grant.
    Cleared,
    Stopped,
    TimedOut,
}

/// A cap the head found reached.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum BudgetHit {
    Cost { spent: f64, cap: f64 },
    Time,
}

/// One iteration's outcome, produced by either loop path and folded by [`Machine::settle`].
pub(crate) enum IterStep {
    Decided(Box<crate::loop_driver::Decided>),
    /// Discard and move on; the reason lands in the row.
    Discarded {
        reason: String,
    },
    /// The turn never started: re-run the same iteration, bounded.
    NeverStarted {
        reason: String,
    },
    /// Halt for human review (the escalation is already reported).
    Escalated,
    /// Park at the next head on a blocking approval.
    Parked(PendingProvisioning),
    /// Stop signal at the post-turn checkpoint.
    Stopped,
}

/// What the host does with a settled iteration.
pub(crate) enum Settle {
    /// Record `row`, restore the best tree, advance.
    Discard {
        row: Row,
    },
    /// Record `row`, restore the best tree, re-run the same iteration; `stalled` ends the run.
    Rerun {
        row: Row,
        attempt: u32,
        stalled: bool,
    },
    /// Restore the best tree and end the run for a human.
    Escalate,
    /// The park is armed for the next head; advance.
    Park,
    Stop,
    /// A measured candidate: keep or discard per `verdict`.
    Decide(Box<crate::loop_driver::Decided>),
}

/// The loop's decisions over its state.
pub(crate) struct Machine {
    pub cfg: LoopCfg,
    pub run: RunState,
    /// The iteration about to run. Advances only when a turn actually started.
    pub it: u32,
    /// The distress marker this run already parked on, by its `ts_ms`.
    parked_distress_ts: Option<u64>,
    exit: Option<LoopExit>,
}

impl Machine {
    pub(crate) fn new(cfg: LoopCfg, run: RunState, start_iter: u32) -> Self {
        Machine {
            cfg,
            run,
            it: start_iter,
            parked_distress_ts: None,
            exit: None,
        }
    }

    /// The exit, once one is set. The host's loop condition.
    pub(crate) fn exit(&self) -> Option<LoopExit> {
        self.exit
    }

    /// Whether another iteration may run.
    pub(crate) fn has_iterations(&self) -> bool {
        self.it <= self.cfg.iterations
    }

    pub(crate) fn end(&mut self, exit: LoopExit) {
        self.exit = Some(exit);
    }

    /// The block-mode approval the head parks on, taken so it parks once.
    pub(crate) fn take_pending_block(&mut self) -> Option<PendingProvisioning> {
        self.run.pending_block.take()
    }

    /// The park is over. A denial on a `block` wait leaves nothing to try, so it ends the run
    /// for a human; a stop ends it as stopped; a grant lets the head's rescope drain proceed.
    pub(crate) fn on_park(&mut self, parked: Duration, outcome: &ParkOutcome) -> Option<LoopExit> {
        self.run.parked_total += parked;
        let exit = match outcome {
            ParkOutcome::Resumed => return None,
            ParkOutcome::Denied(_) => LoopExit::Escalated,
            ParkOutcome::Stopped => LoopExit::Stopped,
        };
        self.exit = Some(exit);
        Some(exit)
    }

    /// An approved re-scope re-baselined: the new segment replaces the old one whole.
    pub(crate) fn rescope(&mut self, segment: Segment) {
        self.run.segment = segment;
    }

    /// A distress marker was read at the head. `Some(row)` when this marker has not parked the
    /// run before; the row annotates the turn that raised it, and is omitted when an identical
    /// row is already on the record (an in-place restart re-reads a marker the operator never
    /// cleared).
    pub(crate) fn on_distress(&mut self, ts_ms: u64, reason: &str) -> Option<Option<Row>> {
        if self.parked_distress_ts == Some(ts_ms) {
            return None;
        }
        self.parked_distress_ts = Some(ts_ms);
        let row = Row {
            iter: self.it.saturating_sub(1),
            decision: "distressed".to_string(),
            note: reason.to_string(),
            ..Default::default()
        };
        let seen = self.run.rows.iter().any(|prior| {
            prior.decision == row.decision && prior.iter == row.iter && prior.note == row.note
        });
        Some((!seen).then_some(row))
    }

    /// The distress park is over.
    pub(crate) fn on_distress_park(
        &mut self,
        parked: Duration,
        outcome: DistressOutcome,
    ) -> Option<LoopExit> {
        self.run.parked_total += parked;
        match outcome {
            DistressOutcome::Cleared => None,
            DistressOutcome::Stopped | DistressOutcome::TimedOut => {
                self.exit = Some(LoopExit::Stopped);
                self.exit
            }
        }
    }

    /// A cap reached, if any. `elapsed` is the run's wall clock; parked time is excluded.
    pub(crate) fn over_budget(&self, live_max_cost: f64, elapsed: Duration) -> Option<BudgetHit> {
        if live_max_cost > 0.0 && self.run.spent >= live_max_cost {
            return Some(BudgetHit::Cost {
                spent: self.run.spent,
                cap: live_max_cost,
            });
        }
        if let Some(cap) = self.cfg.max_time
            && elapsed.saturating_sub(self.run.parked_total) >= cap
        {
            return Some(BudgetHit::Time);
        }
        None
    }

    /// The pack seed diff goes to iteration 1 only.
    pub(crate) fn wants_seed(&self) -> bool {
        self.it == 1
    }

    pub(crate) fn add_cost(&mut self, usd: f64) {
        self.run.spent += usd;
    }

    /// Fold one iteration's outcome. Any step other than `NeverStarted` proves a turn started
    /// and resets the stall streak; a never-started turn keeps the iteration and counts toward
    /// [`MAX_DEAD_TURN_ATTEMPTS`].
    pub(crate) fn settle(&mut self, step: IterStep) -> Settle {
        if !matches!(step, IterStep::NeverStarted { .. }) {
            self.run.dead_turns = 0;
        }
        let it = self.it;
        match step {
            IterStep::Decided(d) => Settle::Decide(d),
            IterStep::Discarded { reason } => {
                let row = Row {
                    iter: it,
                    decision: "discarded".to_string(),
                    note: reason,
                    ..Default::default()
                };
                Settle::Discard { row }
            }
            IterStep::NeverStarted { reason } => {
                self.run.dead_turns += 1;
                let row = Row {
                    iter: it,
                    decision: "infra-dead".to_string(),
                    note: reason,
                    phase: Some("infra".to_string()),
                    ..Default::default()
                };
                let stalled = self.run.dead_turns >= MAX_DEAD_TURN_ATTEMPTS;
                if stalled {
                    self.exit = Some(LoopExit::Stalled);
                }
                Settle::Rerun {
                    row,
                    attempt: self.run.dead_turns,
                    stalled,
                }
            }
            IterStep::Escalated => {
                self.exit = Some(LoopExit::Escalated);
                Settle::Escalate
            }
            IterStep::Parked(pp) => {
                self.run.pending_block = Some(pp);
                Settle::Park
            }
            IterStep::Stopped => {
                self.exit = Some(LoopExit::Stopped);
                Settle::Stop
            }
        }
    }

    /// A settled row goes on the record.
    pub(crate) fn record(&mut self, row: Row) {
        self.run.rows.push(row);
    }

    /// The candidate was kept: the segment's best moves to its reading, and the snapshot the
    /// host took becomes the rollback target. `snapshot` is `Err(why)` when the world could not
    /// snapshot; the change stays live and the best keeps its old rollback target.
    pub(crate) fn keep(
        &mut self,
        row: &mut Row,
        reading: &crucible::crucible::Reading,
        solved: bool,
        snapshot: Result<(String, Option<String>), String>,
    ) {
        if let Some(s) = reading.score {
            self.run.segment.best_score = s;
            // The kept candidate defines both axes: a stale tiebreak would compare the next tie
            // against a scalar the current best never earned.
            self.run.segment.best_tiebreak = reading.tiebreak;
        }
        if let Ok((snap, sha)) = snapshot {
            if let Some(sha) = sha {
                self.run.kept_shas.push(sha);
            }
            row.kept_snap = Some(snap.clone());
            self.run.segment.best_snap = snap;
        }
        self.run.solved_any |= solved;
    }

    /// After a decided iteration: a cap or an early solve ends the run, else the next
    /// iteration is up.
    pub(crate) fn after_decide(
        &mut self,
        verdict: &crucible::crucible::Decision,
        live_max_cost: f64,
        elapsed: Duration,
    ) -> Option<BudgetHit> {
        if let Some(hit) = self.over_budget(live_max_cost, elapsed) {
            self.exit = Some(LoopExit::Budget);
            return Some(hit);
        }
        if verdict.keep && verdict.solved && !self.cfg.no_early_stop {
            self.exit = Some(LoopExit::Solved);
            return None;
        }
        self.it += 1;
        None
    }

    /// The iteration was consumed without a decision (a discard, a park).
    pub(crate) fn advance(&mut self) {
        self.it += 1;
    }

    /// The kept context the epilogue runs against, from the last kept row.
    pub(crate) fn kept_context(&self) -> Option<crate::loop_graph::KeptContext> {
        self.run
            .rows
            .iter()
            .rev()
            .find(|row| row.decision == "keep")
            .map(|row| crate::loop_graph::KeptContext {
                iter: row.iter,
                score: row.score,
                tiebreak: row.tiebreak,
                sha: self.run.kept_shas.last().cloned(),
                snapshot: row.kept_snap.clone(),
                note: row.note.clone(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible::crucible::{Decision, Reading};

    fn cfg() -> LoopCfg {
        LoopCfg {
            iterations: 3,
            max_time: Some(Duration::from_secs(100)),
            max_park: None,
            no_early_stop: false,
        }
    }

    fn run() -> RunState {
        RunState {
            rows: vec![Row {
                iter: 0,
                decision: "baseline".into(),
                score: Some(240.0),
                ..Default::default()
            }],
            spent: 0.0,
            dead_turns: 0,
            kept_shas: Vec::new(),
            base_sha: None,
            base_snap: None,
            solved_any: false,
            parked_total: Duration::ZERO,
            pending_block: None,
            published_branches: Vec::new(),
            segment: Segment {
                regime: "default".into(),
                fingerprint: "f".into(),
                baseline_score: 240.0,
                best_score: 240.0,
                best_tiebreak: None,
                baseline_total: 0,
                best_snap: "base".into(),
            },
        }
    }

    fn reading(score: f64) -> Reading {
        Reading {
            valid: true,
            score: Some(score),
            tiebreak: None,
            solved: false,
            note: String::new(),
            detail: serde_json::Value::Null,
        }
    }

    fn decided(it: u32, keep: bool, solved: bool, score: f64) -> IterStep {
        IterStep::Decided(Box::new(crate::loop_driver::Decided {
            row: Row {
                iter: it,
                decision: if keep { "keep" } else { "discard" }.into(),
                score: Some(score),
                ..Default::default()
            },
            verdict: Decision { keep, solved },
            reading: reading(score),
        }))
    }

    #[test]
    fn a_kept_candidate_moves_the_best_and_the_rollback_target() {
        let mut m = Machine::new(cfg(), run(), 1);
        let Settle::Decide(d) = m.settle(decided(1, true, false, 220.0)) else {
            panic!("a decided step settles as Decide");
        };
        let mut row = d.row;
        m.keep(
            &mut row,
            &d.reading,
            false,
            Ok(("snap-1".into(), Some("sha-1".into()))),
        );
        assert_eq!(m.run.segment.best_score, 220.0);
        assert_eq!(m.run.segment.best_snap, "snap-1");
        assert_eq!(row.kept_snap.as_deref(), Some("snap-1"));
        assert_eq!(m.run.kept_shas, vec!["sha-1".to_string()]);
        assert!(!m.run.solved_any);
        assert!(
            m.after_decide(&d.verdict, 5.0, Duration::ZERO).is_none(),
            "no cap hit"
        );
        assert_eq!(m.it, 2, "a decided iteration advances");
        assert!(m.exit().is_none());
    }

    #[test]
    fn a_failed_snapshot_keeps_the_score_but_not_the_rollback_target() {
        let mut m = Machine::new(cfg(), run(), 1);
        let mut row = Row::default();
        m.keep(&mut row, &reading(200.0), false, Err("git said no".into()));
        assert_eq!(m.run.segment.best_score, 200.0);
        assert_eq!(m.run.segment.best_snap, "base");
        assert!(row.kept_snap.is_none());
        assert!(m.run.kept_shas.is_empty());
    }

    #[test]
    fn a_solved_keep_ends_the_run_unless_early_stop_is_off() {
        let mut m = Machine::new(cfg(), run(), 1);
        let verdict = Decision {
            keep: true,
            solved: true,
        };
        assert!(m.after_decide(&verdict, 5.0, Duration::ZERO).is_none());
        assert_eq!(m.exit(), Some(LoopExit::Solved));

        let mut c = cfg();
        c.no_early_stop = true;
        let mut m = Machine::new(c, run(), 1);
        assert!(m.after_decide(&verdict, 5.0, Duration::ZERO).is_none());
        assert_eq!(m.exit(), None);
        assert_eq!(m.it, 2);
    }

    #[test]
    fn caps_read_the_live_override_and_exclude_parked_time() {
        let mut m = Machine::new(cfg(), run(), 1);
        m.add_cost(4.0);
        assert!(m.over_budget(5.0, Duration::ZERO).is_none());
        assert_eq!(
            m.over_budget(3.0, Duration::ZERO),
            Some(BudgetHit::Cost {
                spent: 4.0,
                cap: 3.0
            }),
            "a live cap below the spend hits"
        );
        assert_eq!(
            m.over_budget(0.0, Duration::from_secs(150)),
            Some(BudgetHit::Time)
        );
        m.run.parked_total = Duration::from_secs(60);
        assert!(
            m.over_budget(0.0, Duration::from_secs(150)).is_none(),
            "parked time is not active time"
        );
        assert!(
            m.after_decide(
                &Decision {
                    keep: false,
                    solved: false
                },
                3.0,
                Duration::ZERO
            )
            .is_some()
        );
        assert_eq!(m.exit(), Some(LoopExit::Budget));
    }

    #[test]
    fn never_started_turns_rerun_the_same_iteration_then_stall() {
        let mut m = Machine::new(cfg(), run(), 2);
        for attempt in 1..MAX_DEAD_TURN_ATTEMPTS {
            let Settle::Rerun {
                row,
                attempt: n,
                stalled,
            } = m.settle(IterStep::NeverStarted {
                reason: "401".into(),
            })
            else {
                panic!("a never-started turn reruns");
            };
            assert_eq!(row.iter, 2);
            assert_eq!(row.phase.as_deref(), Some("infra"));
            assert_eq!(n, attempt);
            assert!(!stalled);
            assert_eq!(m.it, 2, "the iteration is not consumed");
        }
        let Settle::Rerun { stalled, .. } = m.settle(IterStep::NeverStarted {
            reason: "401".into(),
        }) else {
            panic!("rerun");
        };
        assert!(stalled);
        assert_eq!(m.exit(), Some(LoopExit::Stalled));
    }

    #[test]
    fn a_resumed_run_keeps_the_streak_the_log_already_spent() {
        // The bound is per iteration, not per process: a pod that died after two dead turns
        // gets one more attempt on resume, not three.
        let mut resumed = run();
        resumed.dead_turns = MAX_DEAD_TURN_ATTEMPTS - 1;
        let mut m = Machine::new(cfg(), resumed, 2);
        let Settle::Rerun {
            attempt, stalled, ..
        } = m.settle(IterStep::NeverStarted {
            reason: "401".into(),
        })
        else {
            panic!("rerun");
        };
        assert_eq!(attempt, MAX_DEAD_TURN_ATTEMPTS);
        assert!(stalled, "the attempt the resume inherited is the last one");
        assert_eq!(m.exit(), Some(LoopExit::Stalled));
    }

    #[test]
    fn a_started_turn_resets_the_stall_streak() {
        let mut m = Machine::new(cfg(), run(), 1);
        let _ = m.settle(IterStep::NeverStarted { reason: "x".into() });
        let _ = m.settle(IterStep::NeverStarted { reason: "x".into() });
        let Settle::Discard { row } = m.settle(IterStep::Discarded {
            reason: "turn failed".into(),
        }) else {
            panic!("discard");
        };
        assert_eq!(row.decision, "discarded");
        m.advance();
        let Settle::Rerun { attempt, .. } = m.settle(IterStep::NeverStarted { reason: "x".into() })
        else {
            panic!("rerun");
        };
        assert_eq!(attempt, 1, "the streak restarted after a started turn");
    }

    #[test]
    fn escalate_park_and_stop_set_the_exit_or_arm_the_park() {
        let mut m = Machine::new(cfg(), run(), 1);
        assert!(matches!(m.settle(IterStep::Escalated), Settle::Escalate));
        assert_eq!(m.exit(), Some(LoopExit::Escalated));

        let mut m = Machine::new(cfg(), run(), 1);
        let pp = PendingProvisioning {
            mode: crate::provisioning::WaitMode::Block,
            trace_id: "t".into(),
            handle: "h".into(),
        };
        assert!(matches!(m.settle(IterStep::Parked(pp)), Settle::Park));
        assert!(m.exit().is_none());
        assert_eq!(
            m.take_pending_block().map(|p| p.trace_id),
            Some("t".to_string())
        );
        assert!(m.take_pending_block().is_none(), "taken once");

        let mut m = Machine::new(cfg(), run(), 1);
        assert!(matches!(m.settle(IterStep::Stopped), Settle::Stop));
        assert_eq!(m.exit(), Some(LoopExit::Stopped));
    }

    #[test]
    fn a_park_denial_on_a_block_wait_escalates_and_the_wait_is_not_active_time() {
        let mut m = Machine::new(cfg(), run(), 1);
        assert_eq!(
            m.on_park(Duration::from_secs(30), &ParkOutcome::Resumed),
            None
        );
        assert_eq!(m.run.parked_total, Duration::from_secs(30));
        assert_eq!(
            m.on_park(Duration::from_secs(5), &ParkOutcome::Denied("no".into())),
            Some(LoopExit::Escalated)
        );
        assert_eq!(m.run.parked_total, Duration::from_secs(35));
        let mut m = Machine::new(cfg(), run(), 1);
        assert_eq!(
            m.on_park(Duration::ZERO, &ParkOutcome::Stopped),
            Some(LoopExit::Stopped)
        );
    }

    #[test]
    fn a_distress_marker_parks_once_per_timestamp_and_never_duplicates_its_row() {
        let mut m = Machine::new(cfg(), run(), 3);
        let first = m.on_distress(7, "torch skew").expect("a new marker parks");
        let row = first.expect("first sighting writes a row");
        assert_eq!(row.iter, 2, "annotates the turn that raised it");
        assert_eq!(row.decision, "distressed");
        m.record(row);
        assert!(
            m.on_distress(7, "torch skew").is_none(),
            "the same marker never re-parks"
        );
        let again = m
            .on_distress(8, "torch skew")
            .expect("a fresh marker parks");
        assert!(again.is_none(), "an identical row is already on the record");
        assert_eq!(
            m.on_distress_park(Duration::from_secs(9), DistressOutcome::Cleared),
            None
        );
        assert_eq!(m.run.parked_total, Duration::from_secs(9));
        assert_eq!(
            m.on_distress_park(Duration::ZERO, DistressOutcome::TimedOut),
            Some(LoopExit::Stopped)
        );
    }

    #[test]
    fn a_rescope_swaps_the_whole_segment() {
        let mut m = Machine::new(cfg(), run(), 1);
        m.rescope(Segment {
            regime: "c=48".into(),
            fingerprint: "g".into(),
            baseline_score: 300.0,
            best_score: 300.0,
            best_tiebreak: Some(1.0),
            baseline_total: 9,
            best_snap: "s2".into(),
        });
        assert_eq!(m.run.segment.regime, "c=48");
        assert_eq!(m.run.segment.best_snap, "s2");
        assert_eq!(m.run.segment.baseline_total, 9);
    }

    #[test]
    fn the_seed_rides_iteration_one_only_and_iterations_are_bounded() {
        let mut m = Machine::new(cfg(), run(), 1);
        assert!(m.wants_seed());
        m.advance();
        assert!(!m.wants_seed());
        m.advance();
        assert!(m.has_iterations());
        m.advance();
        assert!(!m.has_iterations());
    }

    #[test]
    fn the_kept_context_is_the_last_kept_row() {
        let mut m = Machine::new(cfg(), run(), 1);
        assert!(m.kept_context().is_none());
        m.record(Row {
            iter: 1,
            decision: "keep".into(),
            score: Some(220.0),
            kept_snap: Some("s1".into()),
            note: "first".into(),
            ..Default::default()
        });
        m.run.kept_shas.push("sha1".into());
        m.record(Row {
            iter: 2,
            decision: "discard".into(),
            ..Default::default()
        });
        let kept = m.kept_context().expect("a keep exists");
        assert_eq!(kept.iter, 1);
        assert_eq!(kept.snapshot.as_deref(), Some("s1"));
        assert_eq!(kept.sha.as_deref(), Some("sha1"));
    }

    #[test]
    fn shutdown_tokens_match_the_wire_vocabulary() {
        for (exit, token) in [
            (LoopExit::Finished, "finished"),
            (LoopExit::Solved, "solved"),
            (LoopExit::Budget, "budget"),
            (LoopExit::Stopped, "stopped"),
            (LoopExit::Escalated, "escalated"),
            (LoopExit::Stalled, "stalled"),
        ] {
            assert_eq!(exit.shutdown_reason().0, token);
            assert_eq!(
                crucible_contract::ShutdownOutcome::parse(token).as_str(),
                token
            );
        }
        assert!(LoopExit::Finished.concluded());
        assert!(LoopExit::Budget.concluded());
        assert!(LoopExit::Solved.concluded());
        assert!(!LoopExit::Stopped.concluded());
        assert!(!LoopExit::Escalated.concluded());
        assert!(!LoopExit::Stalled.concluded());
    }
}
