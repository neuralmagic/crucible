//! The loop's control states as data: one transition table the driver advances through and
//! the docs render from, so the diagram and the code cannot disagree.
//!
//! The driver keeps every piece of run state itself (ADR-0004: a plain state machine with an
//! explicit context). What lives here is only the control flow: which state the loop is in,
//! which event moves it, and how a run ends. A transition the table does not list is a bug in
//! the driver, reported as [`IllegalTransition`] rather than silently taken.

use crate::control::ControlState;
use crate::report::Reporter;
use crucible::diagram::{self, Cluster, Cursor, Digraph, Edge, IllegalTransition, Node, NodeKind};
use crucible_contract::LoopPhase;
use std::sync::Arc;

/// Where the loop is. Terminal is [`State::Done`]; everything else has a way out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum State {
    /// Before anything ran: the manifest is loaded, the world and judge built.
    Setup,
    /// The rung ladder against the unmodified tree.
    Preflight,
    /// Measuring the unmodified tree once, the score every candidate must beat.
    Baseline,
    /// N parallel candidates before the first turn, ranked; the winner seeds the deep loop.
    Wide,
    /// Between iterations: the gates (pause, approval, re-scope, distress, interrupt, budget)
    /// run here and decide whether the next turn starts.
    Head,
    /// One iteration: propose, apply, measure, decide.
    Turn,
    /// An operator pause; idle until resumed.
    Paused,
    /// Idle on a blocking approval the agent asked for.
    ParkedApproval,
    /// Idle on the broker's distress marker until the operator clears it.
    ParkedDistress,
    /// The loop has ended one way or another; what remains is reporting.
    Wrapup,
    /// The workflow's post-loop tasks over the kept best.
    Epilogue,
    /// Results, PRs, and the run record.
    Publish,
    Done,
}

/// What moves the loop. Events that end the loop carry the reason as [`Event::exit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Event {
    PreflightStarted,
    PreflightPassed,
    BaselineStarted,
    BaselineMeasured,
    /// A resumed run restores its baseline and best from the session log instead of measuring.
    Resumed,
    WideStarted,
    WideDone,
    TurnStarted,
    Kept,
    Discarded,
    /// The turn died on transport before the agent produced anything; the same iteration
    /// re-runs.
    TurnNeverStarted,
    /// The agent blocked on an approval with no fallback; the next head parks on it.
    Parked,
    Paused,
    Unpaused,
    ApprovalPending,
    /// The approval landed as a re-scope; the head re-baselines into the new regime.
    ApprovalGranted,
    ApprovalDenied,
    /// A re-scope arrived on the control bridge outside an approval wait.
    Rescoped,
    DistressMarked,
    DistressCleared,
    ParkTimedOut,
    Interrupted,
    OverBudget,
    IterationsExhausted,
    Solved,
    Stalled,
    Escalated,
    Stopped,
    EpilogueStarted,
    EpilogueDone,
    PublishStarted,
    Shutdown,
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum LoopExit {
    /// Every iteration ran without an early exit.
    Finished,
    /// A kept candidate satisfied the win condition.
    Solved,
    /// A cost or time cap was reached.
    Budget,
    /// Ctrl+C / a stop signal (at an interrupt checkpoint or while parked).
    Stopped,
    /// The agent declared the harness inadequate, or a `block` approval was denied with no
    /// fallback: halt for human review. The escalation itself is reported and the world rolled
    /// back eagerly at the break site (differently per site) so the variant only needs to
    /// mark the run as "needs human" for the exit code.
    Escalated,
    /// Consecutive turns died on transport before starting: the run is stalled on
    /// infrastructure, not out of iterations.
    Stalled,
}

impl LoopExit {
    /// The wire token + human-readable reason for `Reporter::shutdown`. `error` (a bail from
    /// inside the loop, never reaching this variant) is reported separately by `run_loop`.
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

    pub(crate) const ALL: [LoopExit; 6] = [
        LoopExit::Finished,
        LoopExit::Solved,
        LoopExit::Budget,
        LoopExit::Stopped,
        LoopExit::Escalated,
        LoopExit::Stalled,
    ];
}

impl Event {
    /// The exit an event ends the loop with, for the events that do.
    pub(crate) fn exit(self) -> Option<LoopExit> {
        Some(match self {
            Event::IterationsExhausted => LoopExit::Finished,
            Event::Solved => LoopExit::Solved,
            Event::OverBudget => LoopExit::Budget,
            Event::Interrupted | Event::Stopped | Event::ParkTimedOut => LoopExit::Stopped,
            Event::Escalated | Event::ApprovalDenied => LoopExit::Escalated,
            Event::Stalled => LoopExit::Stalled,
            _ => return None,
        })
    }

    /// The edge label in the rendered diagram: the variant name as words.
    fn label(self) -> String {
        diagram::words(&format!("{self:?}"))
    }
}

/// Every transition the driver may take. Order is the order the diagram lists them in.
pub(crate) const TRANSITIONS: &[(State, Event, State)] = {
    use Event as E;
    use State as S;
    &[
        (S::Setup, E::PreflightStarted, S::Preflight),
        (S::Setup, E::BaselineStarted, S::Baseline),
        (S::Setup, E::Resumed, S::Head),
        (S::Preflight, E::PreflightPassed, S::Baseline),
        (S::Preflight, E::Stopped, S::Done),
        (S::Baseline, E::BaselineMeasured, S::Head),
        (S::Baseline, E::Resumed, S::Head),
        (S::Head, E::WideStarted, S::Wide),
        (S::Wide, E::WideDone, S::Head),
        (S::Head, E::TurnStarted, S::Turn),
        (S::Head, E::Paused, S::Paused),
        (S::Head, E::ApprovalPending, S::ParkedApproval),
        (S::Head, E::Rescoped, S::Head),
        (S::Head, E::DistressMarked, S::ParkedDistress),
        (S::Head, E::Interrupted, S::Wrapup),
        (S::Head, E::OverBudget, S::Wrapup),
        (S::Head, E::Solved, S::Wrapup),
        (S::Head, E::IterationsExhausted, S::Wrapup),
        (S::Paused, E::Unpaused, S::Head),
        (S::ParkedApproval, E::ApprovalGranted, S::Head),
        (S::ParkedApproval, E::ApprovalDenied, S::Wrapup),
        (S::ParkedApproval, E::Stopped, S::Wrapup),
        (S::ParkedDistress, E::DistressCleared, S::Head),
        (S::ParkedDistress, E::ParkTimedOut, S::Wrapup),
        (S::ParkedDistress, E::Stopped, S::Wrapup),
        (S::Turn, E::Kept, S::Head),
        (S::Turn, E::Discarded, S::Head),
        (S::Turn, E::TurnNeverStarted, S::Head),
        (S::Turn, E::Parked, S::Head),
        (S::Turn, E::Stalled, S::Wrapup),
        (S::Turn, E::Escalated, S::Wrapup),
        (S::Turn, E::Stopped, S::Wrapup),
        (S::Wrapup, E::EpilogueStarted, S::Epilogue),
        (S::Wrapup, E::PublishStarted, S::Publish),
        (S::Epilogue, E::EpilogueDone, S::Wrapup),
        (S::Publish, E::Shutdown, S::Done),
    ]
};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("loop control bug: the loop left {state:?} without an exit event")]
pub(crate) struct NoExit {
    pub(crate) state: State,
}

impl State {
    /// The phase the control plane reports for this state. Wrapup keeps the exit's name when
    /// the run escalated, since that is the one an operator has to act on.
    pub(crate) fn phase(self, exit: Option<LoopExit>) -> LoopPhase {
        match self {
            State::Setup => LoopPhase::Starting,
            State::Preflight => LoopPhase::Preflight,
            State::Baseline => LoopPhase::Baseline,
            State::Wide => LoopPhase::Wide,
            State::Head | State::Turn => LoopPhase::Iteration,
            State::Paused => LoopPhase::Paused,
            State::ParkedApproval => LoopPhase::Parked,
            State::ParkedDistress => LoopPhase::Distressed,
            State::Wrapup if exit == Some(LoopExit::Escalated) => LoopPhase::Escalated,
            State::Wrapup | State::Publish | State::Done => LoopPhase::Finished,
            State::Epilogue => LoopPhase::Epilogue,
        }
    }
}

/// The driver's cursor over [`TRANSITIONS`]. Every move publishes the phase: to the control
/// state when one is attached, and to the reporter whenever the `(phase, iter)` pair changes,
/// so the status an operator reads and the session log's `phase` lines are the machine's and
/// nothing else's.
pub(crate) struct Machine {
    cursor: Cursor<State, Event>,
    exit: Option<LoopExit>,
    control: Option<Arc<ControlState>>,
    iter: u32,
    reported: Option<(LoopPhase, u32)>,
}

impl Machine {
    pub(crate) fn new(control: Option<Arc<ControlState>>) -> Self {
        Self {
            cursor: Cursor::new("loop", TRANSITIONS, State::Setup),
            exit: None,
            control,
            iter: 0,
            reported: None,
        }
    }

    pub(crate) fn state(&self) -> State {
        self.cursor.state()
    }

    /// Take `event` from the current state. The first exit-carrying event fixes how the run
    /// ended; later ones cannot change it.
    pub(crate) fn advance<R: Reporter>(
        &mut self,
        event: Event,
        r: &mut R,
    ) -> Result<State, IllegalTransition> {
        let to = self.cursor.advance(event)?;
        if let Some(exit) = event.exit() {
            self.exit.get_or_insert(exit);
        }
        self.publish(r);
        Ok(to)
    }

    /// The iteration the loop is on, for the phase it reports.
    pub(crate) fn set_iter<R: Reporter>(&mut self, iter: u32, r: &mut R) {
        self.iter = iter;
        self.publish(r);
    }

    fn publish<R: Reporter>(&mut self, r: &mut R) {
        let phase = self.state().phase(self.exit);
        if let Some(control) = &self.control {
            control.set_phase(phase);
        }
        if self.reported != Some((phase, self.iter)) {
            self.reported = Some((phase, self.iter));
            r.phase(phase, self.iter);
        }
    }

    /// How the run ended; an error while the loop is still going.
    pub(crate) fn exit(&self) -> Result<LoopExit, NoExit> {
        self.exit.ok_or(NoExit {
            state: self.state(),
        })
    }
}

/// The table laid out for drawing: states grouped by when they happen, idle states dashed,
/// exit edges in the exit color and labeled with the shutdown token.
pub(crate) fn digraph() -> Digraph {
    let kind = |s: State| match s {
        State::Paused | State::ParkedApproval | State::ParkedDistress => NodeKind::Idle,
        State::Turn => NodeKind::Nested,
        State::Done => NodeKind::Terminal,
        _ => NodeKind::Plain,
    };
    let cluster = |label, states: &[State]| Cluster {
        label,
        nodes: states
            .iter()
            .map(|s| Node {
                name: format!("{s:?}"),
                kind: kind(*s),
            })
            .collect(),
    };
    Digraph {
        name: "loop",
        start: "Setup".into(),
        clusters: vec![
            cluster(
                "before the loop",
                &[State::Setup, State::Preflight, State::Baseline, State::Wide],
            ),
            cluster(
                "the loop",
                &[
                    State::Head,
                    State::Turn,
                    State::Paused,
                    State::ParkedApproval,
                    State::ParkedDistress,
                ],
            ),
            cluster(
                "after the loop",
                &[State::Wrapup, State::Epilogue, State::Publish, State::Done],
            ),
        ],
        edges: TRANSITIONS
            .iter()
            .map(|(from, event, to)| Edge {
                from: format!("{from:?}"),
                to: format!("{to:?}"),
                label: match event.exit() {
                    Some(exit) => diagram::exit_label(&event.label(), exit.shutdown_reason().0),
                    None => event.label(),
                },
                exit: event.exit().is_some(),
            })
            .collect(),
    }
}

pub(crate) fn mermaid() -> String {
    digraph().mermaid()
}

pub(crate) fn dot() -> String {
    digraph().dot()
}

/// The generated reference page: the diagram and the exit vocabulary it labels edges with.
pub(crate) fn markdown() -> String {
    let mut out = String::from(
        "# Loop control states\n\n\
         Generated from `crucible/src/runloop/machine.rs` by `crucible loop-states`; \
         `scripts/state-docs.sh --check` keeps it current. The driver advances through this \
         table at every gate, so an edge missing here is a transition the loop cannot take.\n\n\
         Each **Turn** is one iteration's work graph (propose → apply → measure → decide), \
         rendered in [Work graphs](./work-graphs.md). Everything else is the control shell \
         around it: the gates at the **Head**, the parks, and how a run ends. Dashed states are \
         idle; the colored edges are the ways out.\n\n\
         ![The loop's control states](img/loop-states.svg)\n\n\
         The source is `docs/img/loop-states.dot` (`crucible loop-states --format dot`).\n",
    );
    out.push_str("\n## How a run ends\n\nThe edge label after the arrow is the `shutdown` token on the session log.\n\n| Token | Meaning |\n|---|---|\n");
    for exit in LoopExit::ALL {
        let (token, reason) = exit.shutdown_reason();
        out.push_str(&format!("| `{token}` | {reason} |\n"));
    }
    out.push_str("\nAn error inside the loop reports `error` and takes none of these edges.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashSet};

    fn states() -> BTreeSet<State> {
        TRANSITIONS.iter().flat_map(|(a, _, b)| [*a, *b]).collect()
    }

    #[test]
    fn the_table_is_well_formed() {
        assert_eq!(
            crucible::diagram::table_problems(TRANSITIONS, State::Setup, |s| s == State::Done),
            Vec::<String>::new()
        );
    }

    #[test]
    fn exit_events_leave_the_loop_and_nothing_else_does() {
        for (from, ev, to) in TRANSITIONS {
            let leaves = matches!(to, State::Wrapup | State::Done)
                && !matches!(from, State::Wrapup | State::Epilogue | State::Publish);
            assert_eq!(
                ev.exit().is_some(),
                leaves,
                "{from:?} --{ev:?}--> {to:?}: exit events and loop-leaving edges must coincide"
            );
        }
    }

    #[test]
    fn every_exit_reason_has_an_edge() {
        let reachable: HashSet<LoopExit> = TRANSITIONS
            .iter()
            .filter_map(|(_, ev, _)| ev.exit())
            .collect();
        for exit in LoopExit::ALL {
            assert!(reachable.contains(&exit), "{exit:?} has no edge");
        }
    }

    /// Records the phases the machine reports; every other `Reporter` call is inert.
    #[derive(Default)]
    struct Phases(Vec<(LoopPhase, u32)>);

    impl Reporter for Phases {
        fn start(&mut self, _: &str, _: &str) {}
        fn phase(&mut self, phase: LoopPhase, iter: u32) {
            self.0.push((phase, iter));
        }
        fn note(&mut self, _: &str) {}
        fn row(&mut self, _: &crate::report::session::Row, _: bool) {}
        fn run_agent(
            &mut self,
            _: &crate::args::Args,
            _: &crate::args::Paths,
            _: u32,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: crate::report::TurnBudget,
        ) -> crate::report::AgentTurn {
            crate::report::AgentTurn::default()
        }
        fn check_interrupt(
            &mut self,
            _: &crate::args::Paths,
            _: &[crate::report::session::Row],
        ) -> crate::report::Stop {
            crate::report::Stop::Continue
        }
        fn summary(&mut self, _: &[crate::report::session::Row], _: &str, _: f64) {}
    }

    #[test]
    fn the_machine_keeps_its_first_exit_and_refuses_unlisted_moves() {
        let mut m = Machine::new(None);
        let r = &mut Phases::default();
        assert_eq!(
            m.exit(),
            Err(NoExit {
                state: State::Setup
            })
        );
        m.advance(Event::BaselineStarted, r).unwrap();
        m.advance(Event::BaselineMeasured, r).unwrap();
        let err = m.advance(Event::Kept, r).unwrap_err();
        assert_eq!(err.machine, "loop");
        assert_eq!((err.from.as_str(), err.event.as_str()), ("Head", "Kept"));
        m.advance(Event::TurnStarted, r).unwrap();
        m.advance(Event::Escalated, r).unwrap();
        m.advance(Event::PublishStarted, r).unwrap();
        m.advance(Event::Shutdown, r).unwrap();
        assert_eq!(m.state(), State::Done);
        assert_eq!(m.exit(), Ok(LoopExit::Escalated));
    }

    #[test]
    fn the_reporter_hears_each_phase_once_with_the_iteration_it_belongs_to() {
        let mut m = Machine::new(None);
        let r = &mut Phases::default();
        m.advance(Event::PreflightStarted, r).unwrap();
        m.advance(Event::PreflightPassed, r).unwrap();
        m.advance(Event::BaselineMeasured, r).unwrap();
        m.set_iter(1, r);
        m.advance(Event::TurnStarted, r).unwrap();
        m.advance(Event::Discarded, r).unwrap();
        m.set_iter(2, r);
        m.advance(Event::DistressMarked, r).unwrap();
        m.advance(Event::DistressCleared, r).unwrap();
        m.advance(Event::TurnStarted, r).unwrap();
        m.advance(Event::Parked, r).unwrap();
        m.set_iter(3, r);
        m.advance(Event::ApprovalPending, r).unwrap();
        m.advance(Event::ApprovalDenied, r).unwrap();
        m.advance(Event::PublishStarted, r).unwrap();
        m.advance(Event::Shutdown, r).unwrap();
        assert_eq!(
            r.0,
            vec![
                (LoopPhase::Preflight, 0),
                (LoopPhase::Baseline, 0),
                (LoopPhase::Iteration, 0),
                (LoopPhase::Iteration, 1),
                (LoopPhase::Iteration, 2),
                (LoopPhase::Distressed, 2),
                (LoopPhase::Iteration, 2),
                (LoopPhase::Iteration, 3),
                (LoopPhase::Parked, 3),
                (LoopPhase::Escalated, 3),
                (LoopPhase::Finished, 3),
            ]
        );
    }

    #[test]
    fn the_diagram_lists_every_transition_once() {
        let diagram = mermaid();
        assert!(diagram.starts_with("stateDiagram-v2\n"));
        for (from, ev, to) in TRANSITIONS {
            let edge = format!("    {from:?} --> {to:?}: {}", ev.label());
            assert_eq!(
                diagram.matches(&edge).count(),
                1,
                "{edge} should appear exactly once"
            );
        }
        assert!(diagram.contains("over budget → budget"));
        assert!(dot().contains("Head -> Wrapup [label=\"over budget\\n→ budget\", color="));
    }

    #[test]
    fn every_state_reports_a_phase_and_wrapup_keeps_an_escalation() {
        for s in states() {
            let _ = s.phase(None);
        }
        assert_eq!(State::Wrapup.phase(None), LoopPhase::Finished);
        assert_eq!(
            State::Wrapup.phase(Some(LoopExit::Escalated)),
            LoopPhase::Escalated
        );
        assert_eq!(State::Turn.phase(None).as_str(), "iteration");
    }

    #[test]
    fn an_attached_control_state_sees_every_phase_the_machine_enters() {
        let control = Arc::new(ControlState::default());
        let mut m = Machine::new(Some(Arc::clone(&control)));
        let r = &mut Phases::default();
        m.advance(Event::BaselineStarted, r).unwrap();
        assert_eq!(control.phase(), LoopPhase::Baseline);
        m.advance(Event::BaselineMeasured, r).unwrap();
        m.advance(Event::TurnStarted, r).unwrap();
        m.advance(Event::Escalated, r).unwrap();
        assert_eq!(control.phase(), LoopPhase::Escalated);
        m.advance(Event::PublishStarted, r).unwrap();
        assert_eq!(control.phase(), LoopPhase::Finished);
    }

    #[test]
    fn the_dot_graph_lists_every_state_and_transition_once() {
        let graph = dot();
        assert!(graph.starts_with("digraph loop {\n"));
        for s in states() {
            let node = format!("        {s:?}");
            assert_eq!(graph.matches(&node).count(), 1, "{s:?} declared once");
        }
        for (from, _, to) in TRANSITIONS {
            assert!(graph.contains(&format!("    {from:?} -> {to:?} [label=")));
        }
        assert!(graph.contains("over budget\\n→ budget"));
        assert!(graph.contains("Done [shape=doublecircle"));
    }
}
