//! The loop's control states as data: one transition table the driver advances through and
//! the docs render from, so the diagram and the code cannot disagree.
//!
//! The driver keeps every piece of run state itself (ADR-0004: a plain state machine with an
//! explicit context). What lives here is only the control flow: which state the loop is in,
//! which event moves it, and how a run ends. A transition the table does not list is a bug in
//! the driver, reported as [`IllegalTransition`] rather than silently taken.

use crate::control::{ControlState, LoopPhase};
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
        let name = format!("{self:?}");
        let mut out = String::with_capacity(name.len() + 4);
        for (i, c) in name.chars().enumerate() {
            if c.is_uppercase() && i > 0 {
                out.push(' ');
            }
            out.extend(c.to_lowercase());
        }
        out
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
#[error("loop control bug: no transition from {from:?} on {event:?}")]
pub(crate) struct IllegalTransition {
    pub(crate) from: State,
    pub(crate) event: Event,
}

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

/// The driver's cursor over [`TRANSITIONS`]. With a control state attached, every advance
/// publishes the new phase, so the status an operator reads is the machine's and nothing else's.
pub(crate) struct Machine {
    state: State,
    exit: Option<LoopExit>,
    control: Option<Arc<ControlState>>,
}

impl Machine {
    pub(crate) fn new(control: Option<Arc<ControlState>>) -> Self {
        Self {
            state: State::Setup,
            exit: None,
            control,
        }
    }

    pub(crate) fn state(&self) -> State {
        self.state
    }

    /// Take `event` from the current state. The first exit-carrying event fixes how the run
    /// ended; later ones cannot change it.
    pub(crate) fn advance(&mut self, event: Event) -> Result<State, IllegalTransition> {
        let to = TRANSITIONS
            .iter()
            .find(|(from, ev, _)| *from == self.state && *ev == event)
            .map(|(_, _, to)| *to)
            .ok_or(IllegalTransition {
                from: self.state,
                event,
            })?;
        if let Some(exit) = event.exit() {
            self.exit.get_or_insert(exit);
        }
        self.state = to;
        if let Some(control) = &self.control {
            control.set_phase(to.phase(self.exit));
        }
        Ok(to)
    }

    /// How the run ended; an error while the loop is still going.
    pub(crate) fn exit(&self) -> Result<LoopExit, NoExit> {
        self.exit.ok_or(NoExit { state: self.state })
    }
}

/// The table as a mermaid `stateDiagram-v2`, one edge per transition, exits labeled.
pub(crate) fn mermaid() -> String {
    let mut out = String::from("stateDiagram-v2\n    [*] --> Setup\n");
    for (from, event, to) in TRANSITIONS {
        let label = match event.exit() {
            Some(exit) if exit.shutdown_reason().0 != event.label() => {
                format!("{} → {}", event.label(), exit.shutdown_reason().0)
            }
            _ => event.label(),
        };
        out.push_str(&format!("    {from:?} --> {to:?}: {label}\n"));
    }
    out.push_str("    Done --> [*]\n");
    out
}

/// The table as a Graphviz digraph: states grouped by when they happen, idle states dashed,
/// exit edges in the exit color and labeled with the shutdown token.
pub(crate) fn dot() -> String {
    let group = |s: State| match s {
        State::Setup | State::Preflight | State::Baseline | State::Wide => 0,
        State::Head
        | State::Turn
        | State::Paused
        | State::ParkedApproval
        | State::ParkedDistress => 1,
        State::Wrapup | State::Epilogue | State::Publish | State::Done => 2,
    };
    let mut out = String::from(
        "digraph loop {\n\
         \x20   graph [rankdir=TB, fontname=\"Helvetica\", fontsize=11, fontcolor=\"#55606a\", \
         pad=0.4, nodesep=0.5, ranksep=0.7, splines=true, newrank=true];\n\
         \x20   node [shape=box, style=\"rounded,filled\", fillcolor=\"#e4ebe7\", color=\"#2b6f62\", \
         fontname=\"Helvetica\", fontsize=12, fontcolor=\"#1d2329\", margin=\"0.18,0.1\"];\n\
         \x20   edge [fontname=\"Helvetica\", fontsize=10, color=\"#55606a\", fontcolor=\"#55606a\", \
         arrowsize=0.8];\n\
         \x20   start [shape=point, width=0.14, color=\"#1d2329\"];\n",
    );
    let mut states: Vec<State> = TRANSITIONS.iter().flat_map(|(a, _, b)| [*a, *b]).collect();
    states.sort();
    states.dedup();
    for (label, g) in [
        ("before the loop", 0),
        ("the loop", 1),
        ("after the loop", 2),
    ] {
        out.push_str(&format!(
            "    subgraph cluster_{g} {{\n        label=\"{label}\"; style=dashed; color=\"#c8d0cc\"; \
             labeljust=l;\n"
        ));
        for s in states.iter().filter(|s| group(**s) == g) {
            let attrs = match s {
                State::Paused | State::ParkedApproval | State::ParkedDistress => {
                    " [style=\"rounded,filled,dashed\"]"
                }
                State::Turn => " [peripheries=2]",
                State::Done => {
                    " [shape=doublecircle, label=\"\", width=0.22, fillcolor=\"#1d2329\", color=\"#1d2329\"]"
                }
                _ => "",
            };
            out.push_str(&format!("        {s:?}{attrs};\n"));
        }
        out.push_str("    }\n");
    }
    out.push_str("    start -> Setup;\n");
    for (from, event, to) in TRANSITIONS {
        let (label, style) = match event.exit() {
            Some(exit) if exit.shutdown_reason().0 != event.label() => (
                format!("{}\\n→ {}", event.label(), exit.shutdown_reason().0),
                ", color=\"#8a4b1c\", fontcolor=\"#8a4b1c\"",
            ),
            Some(_) => (event.label(), ", color=\"#8a4b1c\", fontcolor=\"#8a4b1c\""),
            None => (event.label(), ""),
        };
        out.push_str(&format!(
            "    {from:?} -> {to:?} [label=\"{label}\"{style}];\n"
        ));
    }
    out.push_str("}\n");
    out
}

/// The generated reference page: the diagram and the exit vocabulary it labels edges with.
pub(crate) fn markdown() -> String {
    let mut out = String::from(
        "# Loop control states\n\n\
         Generated from `crucible/src/runloop/machine.rs` by `crucible loop-states`; \
         `scripts/loop-docs.sh --check` keeps it current. The driver advances through this \
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
    fn every_state_is_reachable_from_setup() {
        let mut seen = HashSet::from([State::Setup]);
        let mut frontier = vec![State::Setup];
        while let Some(s) = frontier.pop() {
            for (from, _, to) in TRANSITIONS {
                if *from == s && seen.insert(*to) {
                    frontier.push(*to);
                }
            }
        }
        for s in states() {
            assert!(seen.contains(&s), "{s:?} is unreachable");
        }
    }

    #[test]
    fn every_state_but_done_leads_somewhere_and_done_leads_nowhere() {
        for s in states() {
            let out = TRANSITIONS.iter().filter(|(from, _, _)| *from == s).count();
            if s == State::Done {
                assert_eq!(out, 0, "Done must be terminal");
            } else {
                assert!(out > 0, "{s:?} is a dead end");
            }
        }
    }

    #[test]
    fn a_state_and_event_name_one_transition() {
        let mut seen = HashSet::new();
        for (from, ev, _) in TRANSITIONS {
            assert!(seen.insert((*from, *ev)), "{from:?} on {ev:?} listed twice");
        }
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

    #[test]
    fn the_machine_keeps_its_first_exit_and_refuses_unlisted_moves() {
        let mut m = Machine::new(None);
        assert_eq!(
            m.exit(),
            Err(NoExit {
                state: State::Setup
            })
        );
        m.advance(Event::BaselineStarted).unwrap();
        m.advance(Event::BaselineMeasured).unwrap();
        assert_eq!(
            m.advance(Event::Kept),
            Err(IllegalTransition {
                from: State::Head,
                event: Event::Kept
            })
        );
        m.advance(Event::TurnStarted).unwrap();
        m.advance(Event::Escalated).unwrap();
        m.advance(Event::PublishStarted).unwrap();
        m.advance(Event::Shutdown).unwrap();
        assert_eq!(m.state(), State::Done);
        assert_eq!(m.exit(), Ok(LoopExit::Escalated));
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
        m.advance(Event::BaselineStarted).unwrap();
        assert_eq!(control.phase(), LoopPhase::Baseline);
        m.advance(Event::BaselineMeasured).unwrap();
        m.advance(Event::TurnStarted).unwrap();
        m.advance(Event::Escalated).unwrap();
        assert_eq!(control.phase(), LoopPhase::Escalated);
        m.advance(Event::PublishStarted).unwrap();
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
