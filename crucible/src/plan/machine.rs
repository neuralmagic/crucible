//! The plan executor's control flow as data: what one task can go through, what the plan as a
//! whole can go through, and why a task ends up blocked. The executor walks both tables at
//! every decision, so an edge missing here is a path the executor cannot take, and the
//! reference page is rendered from the same tables.

use crate::diagram::{self, Cluster, Cursor, Digraph, Edge, IllegalTransition, Node, NodeKind};
use crate::plan::ir::TaskName;
use crucible_contract::{BlockedReasonKind, TaskBlocked};

/// Where one task is. The six settled states are exactly the task statuses the session log
/// reports (`TaskState::status` in `plan::exec` maps them).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TaskState {
    /// Admitted but not yet dispatched: its dependencies are still settling, or the executor
    /// has not reached it.
    Pending,
    /// An attempt is in flight (a transport retry stays here).
    Running,
    /// A fan-out node whose instances are running; it settles when they fold.
    Fanout,
    Pass,
    Fail,
    Skipped,
    Transport,
    Blocked,
    Truncated,
}

impl TaskState {
    pub fn settled(self) -> bool {
        !matches!(
            self,
            TaskState::Pending | TaskState::Running | TaskState::Fanout
        )
    }
}

/// What moves one task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TaskEvent {
    Dispatched,
    /// A fan-out node's instances were named and dispatched.
    FannedOut,
    Passed,
    Failed,
    /// The task itself declared a skip in its result.
    SkippedByTask,
    /// A transport-class attempt failed and another attempt is allowed.
    TransportRetried,
    TransportExhausted,
    /// The budget ran out with retries still allowed.
    TransportCutByBudget,
    /// The substrate cannot run it (`needs` unmet).
    Unrunnable,
    DependencyDidNotPass,
    RequiredTaskFailed,
    BudgetCeiling,
    WallClockCeiling,
    /// The runner refused to stage its inputs.
    StagingRefused,
    /// A required task is unrunnable, so nothing in the plan runs.
    PlanTruncated,
    /// The fan-out node's `over` input was not a list of items.
    FanoutItemsInvalid,
    InstancesPassed,
    InstancesFailed,
}

pub const TASK_TRANSITIONS: &[(TaskState, TaskEvent, TaskState)] = {
    use TaskEvent as E;
    use TaskState as S;
    &[
        (S::Pending, E::Dispatched, S::Running),
        (S::Pending, E::FannedOut, S::Fanout),
        (S::Pending, E::Unrunnable, S::Skipped),
        (S::Pending, E::DependencyDidNotPass, S::Blocked),
        (S::Pending, E::RequiredTaskFailed, S::Blocked),
        (S::Pending, E::BudgetCeiling, S::Blocked),
        (S::Pending, E::WallClockCeiling, S::Blocked),
        (S::Pending, E::StagingRefused, S::Blocked),
        (S::Pending, E::PlanTruncated, S::Truncated),
        (S::Pending, E::FanoutItemsInvalid, S::Fail),
        (S::Running, E::Passed, S::Pass),
        (S::Running, E::Failed, S::Fail),
        (S::Running, E::SkippedByTask, S::Skipped),
        (S::Running, E::TransportRetried, S::Running),
        (S::Running, E::TransportExhausted, S::Transport),
        (S::Running, E::TransportCutByBudget, S::Transport),
        (S::Fanout, E::InstancesPassed, S::Pass),
        (S::Fanout, E::InstancesFailed, S::Fail),
    ]
};

/// Why a task was never dispatched. The `Display` form is the task's result note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockedReason {
    RequiredTaskFailed(TaskName),
    BudgetCeiling,
    WallClockCeiling,
    DependencyDidNotPass,
    StagingRefused(String),
}

impl BlockedReason {
    pub fn event(&self) -> TaskEvent {
        match self {
            BlockedReason::RequiredTaskFailed(_) => TaskEvent::RequiredTaskFailed,
            BlockedReason::BudgetCeiling => TaskEvent::BudgetCeiling,
            BlockedReason::WallClockCeiling => TaskEvent::WallClockCeiling,
            BlockedReason::DependencyDidNotPass => TaskEvent::DependencyDidNotPass,
            BlockedReason::StagingRefused(_) => TaskEvent::StagingRefused,
        }
    }
}

impl BlockedReason {
    pub fn wire(&self) -> TaskBlocked {
        let (reason, task) = match self {
            BlockedReason::RequiredTaskFailed(task) => {
                (BlockedReasonKind::RequiredTaskFailed, Some(task.0.clone()))
            }
            BlockedReason::BudgetCeiling => (BlockedReasonKind::BudgetCeiling, None),
            BlockedReason::WallClockCeiling => (BlockedReasonKind::WallClockCeiling, None),
            BlockedReason::DependencyDidNotPass => (BlockedReasonKind::DependencyDidNotPass, None),
            BlockedReason::StagingRefused(_) => (BlockedReasonKind::StagingRefused, None),
        };
        TaskBlocked { reason, task }
    }
}

impl serde::Serialize for BlockedReason {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.wire().serialize(serializer)
    }
}

impl std::fmt::Display for BlockedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockedReason::RequiredTaskFailed(task) => write!(f, "required task {task} failed"),
            BlockedReason::BudgetCeiling => f.write_str("budget ceiling reached"),
            BlockedReason::WallClockCeiling => f.write_str("wall-clock ceiling reached"),
            BlockedReason::DependencyDidNotPass => f.write_str("dependency did not pass"),
            BlockedReason::StagingRefused(why) => f.write_str(why),
        }
    }
}

/// One task's cursor.
pub struct TaskMachine(Cursor<TaskState, TaskEvent>);

impl TaskMachine {
    pub fn new() -> Self {
        Self(Cursor::new("task", TASK_TRANSITIONS, TaskState::Pending))
    }

    pub fn state(&self) -> TaskState {
        self.0.state()
    }

    pub fn advance(&mut self, event: TaskEvent) -> Result<TaskState, IllegalTransition> {
        self.0.advance(event)
    }
}

impl Default for TaskMachine {
    fn default() -> Self {
        Self::new()
    }
}

/// Where the plan as a whole is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PlanState {
    Admitted,
    /// Ready tasks are being dispatched in topological order.
    Dispatching,
    /// The plan has halted; what remains settles as blocked, except epilogue tasks after a
    /// required task failed, which still run so the failure is reported.
    Draining,
    Completed,
    /// Halted, on the exit fixed by the event that halted it.
    Halted,
    /// A required task could not run on this substrate; nothing was dispatched.
    Truncated,
}

/// What moves the plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlanEvent {
    Started,
    RequiredTaskUnrunnable,
    RequiredTaskFailed,
    BudgetCeiling,
    WallClockCeiling,
    /// Every task has a result.
    Settled,
}

impl PlanEvent {
    /// The `shutdown` token the exit this event fixes reports.
    pub fn exit_token(self) -> Option<&'static str> {
        Some(match self {
            PlanEvent::RequiredTaskUnrunnable | PlanEvent::RequiredTaskFailed => "error",
            PlanEvent::BudgetCeiling | PlanEvent::WallClockCeiling => "budget",
            PlanEvent::Started | PlanEvent::Settled => return None,
        })
    }
}

pub const PLAN_TRANSITIONS: &[(PlanState, PlanEvent, PlanState)] = {
    use PlanEvent as E;
    use PlanState as S;
    &[
        (S::Admitted, E::RequiredTaskUnrunnable, S::Truncated),
        (S::Admitted, E::Started, S::Dispatching),
        (S::Dispatching, E::RequiredTaskFailed, S::Draining),
        (S::Dispatching, E::BudgetCeiling, S::Draining),
        (S::Dispatching, E::WallClockCeiling, S::Draining),
        (S::Dispatching, E::Settled, S::Completed),
        (S::Draining, E::Settled, S::Halted),
    ]
};

/// The plan's cursor.
pub struct PlanMachine(Cursor<PlanState, PlanEvent>);

impl PlanMachine {
    pub fn new() -> Self {
        Self(Cursor::new("plan", PLAN_TRANSITIONS, PlanState::Admitted))
    }

    pub fn state(&self) -> PlanState {
        self.0.state()
    }

    pub fn advance(&mut self, event: PlanEvent) -> Result<PlanState, IllegalTransition> {
        self.0.advance(event)
    }
}

impl Default for PlanMachine {
    fn default() -> Self {
        Self::new()
    }
}

fn task_kind(s: TaskState) -> NodeKind {
    match s {
        TaskState::Fanout => NodeKind::Nested,
        s if s.settled() => NodeKind::Outcome,
        _ => NodeKind::Plain,
    }
}

/// The task table laid out for drawing.
pub fn task_digraph() -> Digraph {
    let cluster = |label, states: &[TaskState]| Cluster {
        label,
        nodes: states
            .iter()
            .map(|s| Node {
                name: format!("{s:?}"),
                kind: task_kind(*s),
            })
            .collect(),
    };
    Digraph {
        name: "task",
        start: "Pending".into(),
        clusters: vec![
            cluster(
                "open",
                &[TaskState::Pending, TaskState::Running, TaskState::Fanout],
            ),
            cluster(
                "settled: the task's status",
                &[
                    TaskState::Pass,
                    TaskState::Fail,
                    TaskState::Skipped,
                    TaskState::Transport,
                    TaskState::Blocked,
                    TaskState::Truncated,
                ],
            ),
        ],
        edges: TASK_TRANSITIONS
            .iter()
            .map(|(from, ev, to)| Edge {
                from: format!("{from:?}"),
                to: format!("{to:?}"),
                label: diagram::words(&format!("{ev:?}")),
                exit: to.settled() && *to != TaskState::Pass,
            })
            .collect(),
    }
}

/// The plan table laid out for drawing.
pub fn plan_digraph() -> Digraph {
    let node = |s: PlanState, kind| Node {
        name: format!("{s:?}"),
        kind,
    };
    Digraph {
        name: "plan",
        start: "Admitted".into(),
        clusters: vec![
            Cluster {
                label: "execution",
                nodes: vec![
                    node(PlanState::Admitted, NodeKind::Plain),
                    node(PlanState::Dispatching, NodeKind::Nested),
                    node(PlanState::Draining, NodeKind::Plain),
                ],
            },
            Cluster {
                label: "exit: the shutdown token",
                nodes: vec![
                    node(PlanState::Completed, NodeKind::Outcome),
                    node(PlanState::Halted, NodeKind::Outcome),
                    node(PlanState::Truncated, NodeKind::Outcome),
                ],
            },
        ],
        edges: PLAN_TRANSITIONS
            .iter()
            .map(|(from, ev, to)| {
                let event = diagram::words(&format!("{ev:?}"));
                let label = match ev.exit_token() {
                    Some(token) => diagram::exit_label(&event, token),
                    None if *to == PlanState::Completed => diagram::exit_label(&event, "finished"),
                    None => event,
                };
                Edge {
                    from: format!("{from:?}"),
                    to: format!("{to:?}"),
                    label,
                    exit: ev.exit_token().is_some(),
                }
            })
            .collect(),
    }
}

/// The generated reference page.
pub fn markdown() -> String {
    let mut out = String::from(
        "# Plan execution states\n\n\
         Generated from `crucible/src/plan/machine.rs` by `crucible plan states`; \
         `scripts/state-docs.sh --check` keeps it current. The executor walks both tables at \
         every decision, so an edge missing here is a path it cannot take. The graph itself, \
         what the tasks are and how they depend on each other, is described in \
         [Work graphs](./work-graphs.md); this page is how the executor walks one.\n\n\
         ## One task\n\n\
         A task is pending until its dependencies settle, runs (retrying transport-class \
         failures up to the configured count), and settles on one of the six statuses the \
         session log reports. Everything that reaches a settled state without an attempt is a \
         blocked task, and the edge names why.\n\n\
         ![One task's states](img/plan-task-states.svg)\n\n\
         ## The plan\n\n\
         The plan dispatches ready tasks in topological order until every task has settled or \
         a required task fails or a ceiling is reached. After a halt the remaining tasks settle \
         as blocked; epilogue tasks still run after a required task fails so the failure is \
         reported.\n\n\
         ![The plan's states](img/plan-states.svg)\n\n\
         The sources are `docs/img/plan-task-states.dot` and `docs/img/plan-states.dot` \
         (`crucible plan states --format dot`).\n\n\
         ## Why a task is blocked\n\n\
         The note on a blocked task's result is one of these.\n\n\
         | Note | Meaning |\n|---|---|\n",
    );
    for (reason, meaning) in [
        (
            BlockedReason::RequiredTaskFailed(TaskName("<task>".into())),
            "A required task failed and the plan short-circuited before this one ran.",
        ),
        (
            BlockedReason::BudgetCeiling,
            "The plan's spend reached its budget.",
        ),
        (
            BlockedReason::WallClockCeiling,
            "The plan's wall-clock limit passed.",
        ),
        (
            BlockedReason::DependencyDidNotPass,
            "A dependency settled without passing and the task's join needs it to.",
        ),
        (
            BlockedReason::StagingRefused("<the runner's reason>".into()),
            "The runner could not stage the task's declared inputs.",
        ),
    ] {
        out.push_str(&format!("| `{reason}` | {meaning} |\n"));
    }
    out.push_str("\n## How a plan ends\n\n| State | Token | Meaning |\n|---|---|---|\n");
    for (state, token, meaning) in [
        (
            "Completed",
            "finished",
            "Every task settled; the plan is valid when every required task passed.",
        ),
        (
            "Halted",
            "error or budget",
            "A required task failed (`error`), or a budget or wall-clock ceiling was reached (`budget`); the rest drained as blocked.",
        ),
        (
            "Truncated",
            "error",
            "A required task cannot run on this substrate; nothing was dispatched.",
        ),
    ] {
        out.push_str(&format!("| {state} | `{token}` | {meaning} |\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagram::table_problems;

    #[test]
    fn both_tables_are_well_formed() {
        assert_eq!(
            table_problems(TASK_TRANSITIONS, TaskState::Pending, TaskState::settled),
            Vec::<String>::new()
        );
        assert_eq!(
            table_problems(PLAN_TRANSITIONS, PlanState::Admitted, |s| matches!(
                s,
                PlanState::Completed | PlanState::Halted | PlanState::Truncated
            )),
            Vec::<String>::new()
        );
    }

    #[test]
    fn every_blocked_reason_names_an_edge_into_blocked_and_keeps_its_note() {
        for reason in [
            BlockedReason::RequiredTaskFailed(TaskName("gate".into())),
            BlockedReason::BudgetCeiling,
            BlockedReason::WallClockCeiling,
            BlockedReason::DependencyDidNotPass,
            BlockedReason::StagingRefused("no room".into()),
        ] {
            let ev = reason.event();
            assert!(TASK_TRANSITIONS.contains(&(TaskState::Pending, ev, TaskState::Blocked)));
        }
        assert_eq!(
            BlockedReason::RequiredTaskFailed(TaskName("gate".into())).to_string(),
            "required task gate failed"
        );
        assert_eq!(
            BlockedReason::StagingRefused("no room".into()).to_string(),
            "no room"
        );
        assert_eq!(
            BlockedReason::RequiredTaskFailed(TaskName("gate".into())).wire(),
            TaskBlocked {
                reason: BlockedReasonKind::RequiredTaskFailed,
                task: Some("gate".into()),
            }
        );
        assert_eq!(
            BlockedReason::StagingRefused("no room".into()).wire(),
            TaskBlocked {
                reason: BlockedReasonKind::StagingRefused,
                task: None,
            }
        );
    }

    #[test]
    fn a_transport_retry_stays_running_and_a_settled_task_moves_no_more() {
        let mut m = TaskMachine::new();
        m.advance(TaskEvent::Dispatched).unwrap();
        m.advance(TaskEvent::TransportRetried).unwrap();
        m.advance(TaskEvent::TransportExhausted).unwrap();
        assert_eq!(m.state(), TaskState::Transport);
        assert!(m.advance(TaskEvent::Passed).is_err());
        let mut p = PlanMachine::new();
        p.advance(PlanEvent::Started).unwrap();
        p.advance(PlanEvent::BudgetCeiling).unwrap();
        assert!(p.advance(PlanEvent::WallClockCeiling).is_err());
        assert_eq!(p.advance(PlanEvent::Settled), Ok(PlanState::Halted));
    }

    #[test]
    fn the_diagrams_carry_every_edge() {
        let task = task_digraph().dot();
        for (from, _, to) in TASK_TRANSITIONS {
            assert!(task.contains(&format!("    {from:?} -> {to:?} [label=")));
        }
        let plan = plan_digraph().dot();
        assert!(
            plan.contains(
                "Dispatching -> Draining [label=\"required task failed\\n→ error\", color="
            )
        );
        assert!(plan.contains("Dispatching -> Completed [label=\"settled\\n→ finished\"]"));
        assert!(markdown().contains("| `required task <task> failed` |"));
    }
}
