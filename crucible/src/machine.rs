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

/// One check the host performs at the head of an iteration, before any turn runs. The order is
/// the machine's, not the host's reading order: [`HEAD`] is the single place it is written down,
/// the host dispatches over it, and the published state chart is drawn from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeadCheck {
    /// Block while an operator holds the run paused.
    WaitIfPaused,
    /// Park on a `block` approval the last turn raised, budget-paused, until it resolves.
    ParkOnPendingBlock,
    /// Drain an approved re-scope: re-baseline into the granted regime, opening a segment.
    DrainRescope,
    /// Drain a denial that arrived while the run continued; the frozen regime stands.
    DrainDeny,
    /// Park on a distress marker the agent wrote, until the operator clears it.
    ParkOnDistress,
    /// End the run if a stop or interrupt landed.
    Interrupt,
    /// End the run if a cost or wall-clock cap is reached.
    Budget,
}

/// The head, in order. Every entry is dispatched by [`crate::loop_driver`]; nothing else may
/// happen before a turn.
pub(crate) const HEAD: &[HeadCheck] = &[
    HeadCheck::WaitIfPaused,
    HeadCheck::ParkOnPendingBlock,
    HeadCheck::DrainRescope,
    HeadCheck::DrainDeny,
    HeadCheck::ParkOnDistress,
    HeadCheck::Interrupt,
    HeadCheck::Budget,
];

/// What a head check tells the host to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeadFlow {
    /// Nothing happened; run the next check.
    Continue,
    /// Start the head over: state changed underneath the remaining checks.
    Restart,
    /// The run is over; [`Machine::exit`] carries why.
    Exit,
}

impl HeadCheck {
    /// The chart edges this check can take. Exhaustive: a new check names its edges before
    /// this compiles, so the head of the chart is the head the host walks.
    fn edges(self) -> &'static [Edge] {
        match self {
            HeadCheck::WaitIfPaused => &[],
            HeadCheck::ParkOnPendingBlock => &[
                Edge {
                    from: "Head",
                    to: "ApprovalPark",
                    label: "a block approval is pending",
                },
                Edge {
                    from: "ApprovalPark",
                    to: "Head",
                    label: "granted: the re-scope drain re-baselines",
                },
                Edge {
                    from: "ApprovalPark",
                    to: "escalated",
                    label: "denied with no fallback",
                },
                Edge {
                    from: "ApprovalPark",
                    to: "stopped",
                    label: "stop while parked",
                },
            ],
            HeadCheck::DrainRescope => &[Edge {
                from: "Head",
                to: "Head",
                label: "an approved re-scope re-baselines a new segment",
            }],
            HeadCheck::DrainDeny => &[Edge {
                from: "Head",
                to: "Head",
                label: "a denial noted; the frozen regime stands",
            }],
            HeadCheck::ParkOnDistress => &[
                Edge {
                    from: "Head",
                    to: "DistressPark",
                    label: "the agent raised distress(error)",
                },
                Edge {
                    from: "DistressPark",
                    to: "Head",
                    label: "the operator cleared the marker",
                },
                Edge {
                    from: "DistressPark",
                    to: "stopped",
                    label: "stop, or the park timed out",
                },
            ],
            HeadCheck::Interrupt => &[Edge {
                from: "Head",
                to: "stopped",
                label: "interrupt at the head",
            }],
            HeadCheck::Budget => &[Edge {
                from: "Head",
                to: "budget",
                label: "a cost or time cap was reached",
            }],
        }
    }
}

/// One transition in the rendered state chart. `label` is the condition that takes the loop
/// from `from` to `to`, in the order the host actually evaluates them.
struct Edge {
    from: &'static str,
    to: &'static str,
    label: &'static str,
}

/// What happens once the head lets an iteration run. The head's own transitions are not here:
/// they come from [`HEAD`], which is the order the host dispatches, so that half of the chart
/// cannot disagree with the code that walks it.
const EDGES: &[Edge] = &[
    Edge {
        from: "Head",
        to: "finished",
        label: "no iterations left",
    },
    Edge {
        from: "Head",
        to: "Iteration",
        label: "otherwise: run the turn",
    },
    Edge {
        from: "Iteration",
        to: "Decide",
        label: "decided: a measured candidate",
    },
    Edge {
        from: "Decide",
        to: "Head",
        label: "kept or discarded",
    },
    Edge {
        from: "Decide",
        to: "solved",
        label: "kept and solved, early stop on",
    },
    Edge {
        from: "Decide",
        to: "budget",
        label: "a cap was reached deciding",
    },
    Edge {
        from: "Iteration",
        to: "Head",
        label: "discarded, or parked for the next head",
    },
    Edge {
        from: "Iteration",
        to: "Iteration",
        label: "never-started: re-run, bounded",
    },
    Edge {
        from: "Iteration",
        to: "stalled",
        label: "consecutive dead turns hit the bound",
    },
    Edge {
        from: "Iteration",
        to: "escalated",
        label: "escalated by the agent",
    },
    Edge {
        from: "Iteration",
        to: "stopped",
        label: "stop at the post-turn checkpoint",
    },
];

/// The loop's state machine as a mermaid `stateDiagram-v2`. Exits are the shutdown tokens the
/// wire carries, so a reader can match a diagram node to a run's shutdown line.
pub(crate) fn mermaid() -> String {
    let mut out = String::from("stateDiagram-v2\n    [*] --> Head\n");
    for edge in HEAD.iter().flat_map(|c| c.edges()).chain(EDGES) {
        out.push_str(&format!(
            "    {} --> {}: {}\n",
            edge.from, edge.to, edge.label
        ));
    }
    for exit in EXITS {
        out.push_str(&format!("    {} --> [*]\n", exit.shutdown_reason().0));
    }
    out
}

/// Every way the loop can end. Held complete by `every_exit_is_drawn`.
const EXITS: &[LoopExit] = &[
    LoopExit::Finished,
    LoopExit::Solved,
    LoopExit::Budget,
    LoopExit::Stopped,
    LoopExit::Escalated,
    LoopExit::Stalled,
];

/// The published page: the chart plus what each terminal state means on the wire.
pub(crate) fn doc_page() -> String {
    let mut out = String::from(
        "# The loop state machine\n\n\
         <!-- Generated by `crucible loop-reference`; edit crucible/src/machine.rs instead. -->\n\n\
         The scored loop's decisions live in one I/O-free machine: the host performs effects and \
         hands the results back, and the machine answers what happens next. Every edge below is a \
         variant the compiler knows about, so this page cannot drift from the binary that drew it.\n\n\
         ```mermaid\n",
    );
    out.push_str(&mermaid());
    out.push_str("```\n\n## How a run ends\n\nThe terminal states are the shutdown tokens the session log carries.\n\n| Token | Meaning |\n| --- | --- |\n");
    for exit in EXITS {
        let (token, reason) = exit.shutdown_reason();
        out.push_str(&format!("| `{token}` | {reason} |\n"));
    }
    out
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

    /// Adding a `LoopExit` breaks `shutdown_reason` first; this holds the new one to reaching
    /// the published chart rather than existing only in the type.
    #[test]
    fn every_exit_is_drawn_and_terminal() {
        let chart = mermaid();
        for exit in EXITS {
            let token = exit.shutdown_reason().0;
            assert!(
                chart.contains(&format!("{token} --> [*]")),
                "{token}: {chart}"
            );
        }
        assert_eq!(
            EXITS.len(),
            6,
            "a new exit needs an EXITS entry and an edge"
        );
    }

    /// The label an iteration's outcome is drawn under. Exhaustive on purpose: a new
    /// `IterStep` has to be named here before this compiles, and the test below then holds it
    /// to appearing on the chart.
    fn step_label(step: &IterStep) -> &'static str {
        match step {
            IterStep::Decided(_) => "decided",
            IterStep::Discarded { .. } => "discarded",
            IterStep::NeverStarted { .. } => "never-started",
            IterStep::Escalated => "escalated",
            IterStep::Parked(_) => "parked",
            IterStep::Stopped => "stop at the post-turn checkpoint",
        }
    }

    /// The same for an iteration's outcomes: `step_label` is exhaustive, so a new `IterStep`
    /// must be named, and naming it without drawing it fails here.
    #[test]
    fn every_iteration_outcome_is_drawn() {
        let chart = mermaid();
        let steps = [
            IterStep::Discarded { reason: "r".into() },
            IterStep::NeverStarted { reason: "r".into() },
            IterStep::Escalated,
            IterStep::Stopped,
        ];
        for step in &steps {
            let label = step_label(step);
            assert!(
                chart.contains(label),
                "{label} is not on the chart: {chart}"
            );
        }
        // Decided and Parked carry payloads a test cannot cheaply build; their labels are
        // asserted directly against the same exhaustive source.
        assert!(chart.contains("decided"), "{chart}");
        assert!(chart.contains("parked"), "{chart}");
    }

    /// The head is dispatched from `HEAD`, so a check that is not listed never runs. `edges`
    /// is exhaustive, which makes a new variant compile only once it is described; this holds
    /// it to also being dispatched, exactly once.
    #[test]
    fn head_dispatches_every_check_exactly_once() {
        let all = [
            HeadCheck::WaitIfPaused,
            HeadCheck::ParkOnPendingBlock,
            HeadCheck::DrainRescope,
            HeadCheck::DrainDeny,
            HeadCheck::ParkOnDistress,
            HeadCheck::Interrupt,
            HeadCheck::Budget,
        ];
        for check in all {
            assert_eq!(
                HEAD.iter().filter(|h| **h == check).count(),
                1,
                "{check:?} is not dispatched exactly once"
            );
        }
        assert_eq!(
            HEAD.len(),
            all.len(),
            "HEAD has a check this test does not know"
        );
    }

    /// The order is the contract the chart is drawn from: a pending approval parks before a
    /// re-scope drains it, and both settle before the run can be ended by a cap.
    #[test]
    fn the_head_parks_before_it_drains_and_ends() {
        let at = |c: HeadCheck| HEAD.iter().position(|h| *h == c).expect("dispatched");
        assert!(at(HeadCheck::ParkOnPendingBlock) < at(HeadCheck::DrainRescope));
        assert!(at(HeadCheck::DrainRescope) < at(HeadCheck::DrainDeny));
        assert!(at(HeadCheck::ParkOnDistress) < at(HeadCheck::Interrupt));
        assert!(at(HeadCheck::Interrupt) < at(HeadCheck::Budget));
    }

    /// Every edge must start somewhere the chart can be entered from, so the diagram is one
    /// connected machine rather than a pile of arrows.
    #[test]
    fn the_chart_has_no_unreachable_state() {
        let mut reachable = vec!["Head"];
        let mut grew = true;
        while grew {
            grew = false;
            for edge in EDGES {
                if reachable.contains(&edge.from) && !reachable.contains(&edge.to) {
                    reachable.push(edge.to);
                    grew = true;
                }
            }
        }
        for edge in EDGES {
            assert!(
                reachable.contains(&edge.from),
                "{} is drawn but never reached",
                edge.from
            );
        }
    }

    /// The page carries the chart and the wire vocabulary a reader matches a run against.
    #[test]
    fn the_doc_page_carries_the_chart_and_the_shutdown_table() {
        let page = doc_page();
        assert!(page.contains("```mermaid"), "{page}");
        assert!(page.contains("stateDiagram-v2"), "{page}");
        for exit in EXITS {
            let (token, reason) = exit.shutdown_reason();
            assert!(page.contains(&format!("| `{token}` |")), "{page}");
            assert!(page.contains(reason), "{page}");
        }
    }
}
