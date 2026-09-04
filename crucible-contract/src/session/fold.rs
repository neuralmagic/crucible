//! The session log folded into one state: every consumer of `state/session.jsonl` (the
//! engine's resume, its crash classification, the controller's ingest and rebuild) reads the
//! same [`LoopState`] by applying the same [`LoopState::apply`] to the same events.
//!
//! Only what the log can reproduce lives here. Process-local state (control bridge slots,
//! marker files, handles) is the engine's, reconstructed from the admission ledger.

use crate::event::AgentEvent;
use crate::identity::RunIdentity;
use crate::session::{PlanTaskWire, PrLinkWire, RecoveryClass, RowWire, SessionEvent};
use std::collections::BTreeMap;

/// `Shutdown.outcome` tokens. Unknown maps to `Other` so a newer writer never breaks an
/// older reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownOutcome {
    Finished,
    Solved,
    Budget,
    Stopped,
    Escalated,
    Stalled,
    Error,
    Suspended,
    Other(String),
}

impl ShutdownOutcome {
    pub fn parse(token: &str) -> Self {
        match token {
            "finished" => ShutdownOutcome::Finished,
            "solved" => ShutdownOutcome::Solved,
            "budget" => ShutdownOutcome::Budget,
            "stopped" => ShutdownOutcome::Stopped,
            "escalated" => ShutdownOutcome::Escalated,
            "stalled" => ShutdownOutcome::Stalled,
            "error" => ShutdownOutcome::Error,
            "suspended" => ShutdownOutcome::Suspended,
            other => ShutdownOutcome::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            ShutdownOutcome::Finished => "finished",
            ShutdownOutcome::Solved => "solved",
            ShutdownOutcome::Budget => "budget",
            ShutdownOutcome::Stopped => "stopped",
            ShutdownOutcome::Escalated => "escalated",
            ShutdownOutcome::Stalled => "stalled",
            ShutdownOutcome::Error => "error",
            ShutdownOutcome::Suspended => "suspended",
            ShutdownOutcome::Other(t) => t,
        }
    }
}

/// `ApprovalWait.mode` tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitMode {
    /// The run parks until the approval resolves.
    Block,
    /// The run keeps iterating; the grant lands whenever it arrives.
    Continue,
}

impl WaitMode {
    /// Unknown tokens degrade to `Continue`: a reader never re-parks on a mode it cannot honor.
    pub fn parse(token: &str) -> Self {
        if token == "block" {
            WaitMode::Block
        } else {
            WaitMode::Continue
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            WaitMode::Block => "block",
            WaitMode::Continue => "continue",
        }
    }
}

/// Evidence scraped from a dangling turn's agent events.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnEvidence {
    /// Events inside the dangling AgentStart bracket.
    pub agent_events: u32,
    /// Last per-turn cost seen (`Tokens.cost_usd` or `OtelSummary.cost_usd`).
    pub last_cost_usd: Option<f64>,
    /// Last error text (`Error.message` or an is_error `Result.error`), verbatim.
    pub last_error: Option<String>,
    /// From the preceding AgentSession line.
    pub session: Option<DanglingSession>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DanglingSession {
    pub name: String,
    /// 1-based, from the AgentSession line.
    pub turn: u32,
}

/// An approval the log left open (ApprovalWait with no ApprovalResolved after it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenApproval {
    pub handle: String,
    pub trace_id: String,
    pub mode: WaitMode,
    /// The plan task that opened the gate; `None` for a provisioning or distress wait.
    pub task: Option<String>,
    /// The resolution source, verbatim from the wire; `None` on logs written before it existed.
    pub source: Option<serde_json::Value>,
}

/// A PlanAdmitted whose iteration never accounted (no Row after it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPlan {
    pub plan_version: u32,
    /// Declared task names, in order.
    pub declared: Vec<String>,
    /// TaskResult names seen after this PlanAdmitted.
    pub resulted: Vec<String>,
}

/// What the tail of the log says happened.
#[derive(Debug, Clone, PartialEq)]
pub enum Classification {
    /// A trailing Shutdown line: the previous process exited on purpose.
    CleanExit {
        outcome: ShutdownOutcome,
        reason: String,
    },
    /// No decided rows: nothing to resume from.
    DiedInBaseline,
    /// AgentStart with no AgentDone: the turn was in flight.
    DiedMidTurn {
        iter: u32,
        evidence: TurnEvidence,
        approval: Option<OpenApproval>,
    },
    /// Turn finished but no Row for that iteration: died in measure/decide/keep.
    DiedDeciding { iter: u32, evidence: TurnEvidence },
    /// PlanAdmitted for an iteration with no Row and no dangling turn.
    DiedInPlanTask { iter: u32, plan: OpenPlan },
    /// Parked on a block-mode approval with no turn in flight.
    DiedAwaitingApproval { approval: OpenApproval },
    /// No dangling turn, plan, or approval; died between a Row and the next AgentStart.
    DiedBetweenIterations { last_iter: u32 },
}

impl Classification {
    pub fn class(&self) -> RecoveryClass {
        match self {
            Classification::CleanExit { .. } => RecoveryClass::CleanExit,
            Classification::DiedInBaseline => RecoveryClass::DiedInBaseline,
            Classification::DiedMidTurn { .. } => RecoveryClass::DiedMidTurn,
            Classification::DiedDeciding { .. } => RecoveryClass::DiedDeciding,
            Classification::DiedInPlanTask { .. } => RecoveryClass::DiedInPlanTask,
            Classification::DiedAwaitingApproval { .. } => RecoveryClass::DiedAwaitingApproval,
            Classification::DiedBetweenIterations { .. } => RecoveryClass::DiedBetweenIterations,
        }
    }

    /// The iteration the interruption touched (0 if none).
    pub fn iter(&self) -> u32 {
        match self {
            Classification::DiedMidTurn { iter, .. }
            | Classification::DiedDeciding { iter, .. }
            | Classification::DiedInPlanTask { iter, .. } => *iter,
            Classification::DiedBetweenIterations { last_iter } => *last_iter,
            _ => 0,
        }
    }

    /// One-line evidence summary for the Recovery event and the resume note.
    pub fn detail(&self) -> String {
        match self {
            Classification::CleanExit { outcome, reason } => {
                format!("previous run exited {}: {reason}", outcome.as_str())
            }
            Classification::DiedInBaseline => "no decided rows in the log".to_string(),
            Classification::DiedMidTurn {
                iter,
                evidence,
                approval,
            } => {
                let mut s = format!(
                    "turn in flight at iter {iter}, {} agent events",
                    evidence.agent_events
                );
                if let Some(c) = evidence.last_cost_usd {
                    s.push_str(&format!(", last cost ${c:.2}"));
                }
                if let Some(sess) = &evidence.session {
                    s.push_str(&format!(", session {} turn {}", sess.name, sess.turn));
                }
                if let Some(e) = &evidence.last_error {
                    s.push_str(&format!(", last error: {}", trunc(e, 200)));
                }
                if let Some(a) = approval {
                    s.push_str(&format!(", approval {} outstanding", a.handle));
                }
                s
            }
            Classification::DiedDeciding { iter, evidence } => {
                let mut s = format!("turn at iter {iter} completed but was never decided");
                if let Some(sess) = &evidence.session {
                    s.push_str(&format!(
                        " (session {} turn {} cursor advanced ungraded)",
                        sess.name, sess.turn
                    ));
                }
                s
            }
            Classification::DiedInPlanTask { iter, plan } => format!(
                "plan v{} at iter {iter} never accounted ({}/{} tasks resulted)",
                plan.plan_version,
                plan.resulted.len(),
                plan.declared.len()
            ),
            Classification::DiedAwaitingApproval { approval } => {
                format!(
                    "parked on approval {} ({})",
                    approval.handle, approval.trace_id
                )
            }
            Classification::DiedBetweenIterations { last_iter } => {
                format!("died between iterations, last decided iter {last_iter}")
            }
        }
    }
}

fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

/// One settled plan task, keyed by name (fan-out instances carry `node[key]`).
#[derive(Debug, Clone, PartialEq)]
pub struct TaskResultWire {
    pub status: String,
    pub task_kind: String,
    pub iter: u32,
    pub attempts: u32,
    pub cost_usd: f64,
    pub secs: f64,
    pub note: String,
    pub output: Option<serde_json::Value>,
}

/// The latest admitted work graph.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanWire {
    pub plan_version: u32,
    pub budget_usd: f64,
    pub tasks: Vec<PlanTaskWire>,
}

/// The counters `--resume` restores, as the log carries them.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumeView {
    /// Decided deep-loop rows (no `wide`, no `infra`), in log order.
    pub rows: Vec<RowWire>,
    pub best_score: f64,
    pub best_tiebreak: Option<f64>,
    pub baseline_score: f64,
    pub baseline_total: u64,
    pub spent: f64,
    /// First iteration to run (last logged iteration + 1).
    pub next_iter: u32,
    /// Never-started turns already spent on `next_iter`, so the attempt bound survives a resume.
    pub dead_turns: u32,
    pub solved_any: bool,
    pub identity: Option<RunIdentity>,
    /// Head branches of every draft PR prior segments already opened.
    pub published_branches: Vec<String>,
}

/// The log folded into one state.
#[derive(Debug, Clone, Default)]
pub struct LoopState {
    /// Every Row verbatim, in log order.
    pub rows: Vec<RowWire>,
    pub solved_any: bool,
    /// Last Budget line.
    pub spent: f64,
    pub elapsed_secs: Option<u64>,
    /// `Summary.best_score`, when the run got that far.
    pub summary_best: Option<f64>,
    /// Last Identity line: a run resumed more than once re-emits one each time.
    pub identity: Option<RunIdentity>,
    pub pr_links: Vec<PrLinkWire>,
    pub escalation: Option<(String, String, String)>,
    /// The latest admitted graph and the results settled under it. A PlanAdmitted with a
    /// different `plan_version` clears the results.
    pub plan: Option<PlanWire>,
    pub plan_results: BTreeMap<String, TaskResultWire>,
    /// Every Recovery line, in order.
    pub recoveries: Vec<(RecoveryClass, u32, String)>,
    /// The trailing Shutdown. A resumed process appends past its predecessor's Shutdown, so any
    /// later event clears it.
    pub terminal: Option<(ShutdownOutcome, String)>,
    /// Lines whose `kind` this reader does not know.
    pub unknown: u32,
    // Open brackets, exactly the facts the tail classifier needs.
    saw_any_row: bool,
    last_row_iter: u32,
    last_phase_iter: u32,
    pending_session: Option<DanglingSession>,
    open_turn: Option<(u32, TurnEvidence)>,
    done_unrowed: Option<(u32, TurnEvidence)>,
    open_plan: Option<OpenPlan>,
    open_approval: Option<OpenApproval>,
}

impl LoopState {
    /// Fold every decodable line of a session log. Blank and torn lines are skipped, exactly
    /// as [`crate::session::decode`] skips them.
    pub fn from_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> Self {
        let mut state = LoopState::default();
        for line in lines {
            if let Some(ev) = crate::session::decode(line) {
                state.apply(&ev);
            }
        }
        state
    }

    /// Apply one event. Exhaustive: adding a variant to [`SessionEvent`] is a compile error
    /// here until the fold says what it means.
    pub fn apply(&mut self, ev: &SessionEvent) {
        if self.terminal.is_some() && !matches!(ev, SessionEvent::Shutdown { .. }) {
            self.terminal = None;
        }
        match ev {
            SessionEvent::Start { .. } => {}
            SessionEvent::Phase { phase, iter } => {
                if phase == "iteration" {
                    self.last_phase_iter = *iter;
                    self.done_unrowed = None;
                }
            }
            SessionEvent::Note { .. } => {}
            SessionEvent::Row { row, solved } => {
                if !matches!(row.phase.as_deref(), Some("wide") | Some("infra")) {
                    self.saw_any_row = true;
                    self.last_row_iter = row.iter;
                    self.solved_any |= *solved;
                }
                if self
                    .done_unrowed
                    .as_ref()
                    .is_some_and(|(iter, _)| *iter == row.iter)
                {
                    self.done_unrowed = None;
                }
                self.open_plan = None;
                self.rows.push(row.clone());
            }
            SessionEvent::AgentStart { iter } => {
                self.open_turn = Some((
                    *iter,
                    TurnEvidence {
                        session: self.pending_session.take(),
                        ..TurnEvidence::default()
                    },
                ));
            }
            SessionEvent::AgentSession { session, turn, .. } => {
                self.pending_session = Some(DanglingSession {
                    name: session.clone(),
                    turn: *turn,
                });
            }
            SessionEvent::Agent { event } => {
                if let Some((_, evidence)) = &mut self.open_turn {
                    evidence.agent_events += 1;
                    match event {
                        AgentEvent::Tokens(t) => {
                            if let Some(c) = t.cost_usd {
                                evidence.last_cost_usd = Some(c);
                            }
                        }
                        AgentEvent::OtelSummary { cost_usd, .. } => {
                            evidence.last_cost_usd = Some(*cost_usd);
                        }
                        AgentEvent::Error { message, .. } => {
                            evidence.last_error = Some(message.clone());
                        }
                        AgentEvent::Result {
                            is_error: true,
                            error: Some(e),
                            ..
                        } => {
                            evidence.last_error = Some(e.clone());
                        }
                        _ => {}
                    }
                }
            }
            SessionEvent::AgentDone => {
                self.done_unrowed = self.open_turn.take();
            }
            SessionEvent::Budget {
                spent,
                elapsed_secs,
            } => {
                self.spent = *spent;
                self.elapsed_secs = Some(*elapsed_secs);
            }
            SessionEvent::Summary { best_score, .. } => self.summary_best = *best_score,
            SessionEvent::Escalation {
                category,
                reason,
                evidence,
            } => {
                self.escalation = Some((category.clone(), reason.clone(), evidence.clone()));
            }
            SessionEvent::Segment { .. } => {}
            SessionEvent::Identity { identity } => self.identity = Some(identity.clone()),
            SessionEvent::PrLinks { links } => self.pr_links.extend(links.iter().cloned()),
            SessionEvent::Finished => {}
            SessionEvent::PlanAdmitted {
                plan_version,
                budget_usd,
                tasks,
                ..
            } => {
                if self.plan.as_ref().map(|p| p.plan_version) != Some(*plan_version) {
                    self.plan_results.clear();
                }
                self.plan = Some(PlanWire {
                    plan_version: *plan_version,
                    budget_usd: *budget_usd,
                    tasks: tasks.clone(),
                });
                self.open_plan = Some(OpenPlan {
                    plan_version: *plan_version,
                    declared: tasks.iter().map(|t| t.name.clone()).collect(),
                    resulted: Vec::new(),
                });
            }
            SessionEvent::AsksEmitted { .. } => {}
            SessionEvent::TaskResult {
                task,
                status,
                task_kind,
                iter,
                attempts,
                cost_usd,
                output,
                note,
                secs,
                ..
            } => {
                if let Some(plan) = &mut self.open_plan {
                    plan.resulted.push(task.clone());
                }
                self.plan_results.insert(
                    task.clone(),
                    TaskResultWire {
                        status: status.clone(),
                        task_kind: task_kind.clone(),
                        iter: *iter,
                        attempts: *attempts,
                        cost_usd: *cost_usd,
                        secs: *secs,
                        note: note.clone(),
                        output: output.clone(),
                    },
                );
            }
            SessionEvent::Shutdown { outcome, reason } => {
                self.terminal = Some((ShutdownOutcome::parse(outcome), reason.clone()));
            }
            SessionEvent::ApprovalWait {
                handle,
                trace_id,
                mode,
                task,
                source,
                ..
            } => {
                self.open_approval = Some(OpenApproval {
                    handle: handle.clone(),
                    trace_id: trace_id.clone(),
                    mode: WaitMode::parse(mode),
                    task: task.clone(),
                    source: source.clone(),
                });
            }
            SessionEvent::ApprovalResolved { .. } => self.open_approval = None,
            SessionEvent::OutputRefused { .. } => {}
            SessionEvent::Recovery {
                class,
                iter,
                detail,
            } => self.recoveries.push((*class, *iter, detail.clone())),
            SessionEvent::Unknown => self.unknown += 1,
        }
    }

    /// Whether any deep-loop row was decided. A log without one is not resumable.
    pub fn has_rows(&self) -> bool {
        self.saw_any_row
    }

    /// The approval the log left open, whatever the classification.
    pub fn open_approval(&self) -> Option<&OpenApproval> {
        self.open_approval.as_ref()
    }

    /// Declared plan tasks with no result under the current plan.
    pub fn plan_open(&self) -> Vec<String> {
        let Some(plan) = &self.plan else {
            return Vec::new();
        };
        plan.tasks
            .iter()
            .map(|t| t.name.clone())
            .filter(|name| !self.plan_results.contains_key(name))
            .collect()
    }

    /// What the tail says happened. Each earlier case subsumes the later ones.
    pub fn classify(&self) -> Classification {
        if let Some((outcome, reason)) = &self.terminal {
            return Classification::CleanExit {
                outcome: outcome.clone(),
                reason: reason.clone(),
            };
        }
        if !self.saw_any_row {
            return Classification::DiedInBaseline;
        }
        if let Some((iter, evidence)) = &self.open_turn {
            return Classification::DiedMidTurn {
                iter: *iter,
                evidence: evidence.clone(),
                approval: self.open_approval.clone(),
            };
        }
        if let Some(plan) = &self.open_plan {
            return Classification::DiedInPlanTask {
                iter: self.last_phase_iter,
                plan: plan.clone(),
            };
        }
        if let Some(approval) = self
            .open_approval
            .as_ref()
            .filter(|a| a.mode == WaitMode::Block)
        {
            return Classification::DiedAwaitingApproval {
                approval: approval.clone(),
            };
        }
        if let Some((iter, evidence)) = &self.done_unrowed {
            return Classification::DiedDeciding {
                iter: *iter,
                evidence: evidence.clone(),
            };
        }
        Classification::DiedBetweenIterations {
            last_iter: self.last_row_iter,
        }
    }

    /// Decided deep-loop rows: not a never-started turn record, and not a `wide` lane row from a
    /// log written before the wide tournament was removed.
    pub fn deep_rows(&self) -> impl DoubleEndedIterator<Item = &RowWire> {
        self.rows
            .iter()
            .filter(|r| !matches!(r.phase.as_deref(), Some("wide") | Some("infra")))
    }

    /// Never-started turns at the tail of the log: the streak an in-flight iteration has
    /// already spent. Any row from a turn that started ends the streak; a `distressed` row
    /// annotates the turn before it rather than reporting one, so it does not.
    pub fn dead_turns(&self) -> u32 {
        self.rows
            .iter()
            .rev()
            .filter(|r| r.decision != "distressed")
            .take_while(|r| r.phase.as_deref() == Some("infra"))
            .count() as u32
    }

    /// The counters a resume restores. Decided rows carry `score`/`total`, so baseline and
    /// best restore exactly; keeps are monotone within a segment, so the last kept row is the
    /// best and its tiebreak travels with the best score.
    pub fn resume_view(&self) -> ResumeView {
        let rows: Vec<RowWire> = self.deep_rows().cloned().collect();
        let baseline_score = rows.first().and_then(|r| r.score).unwrap_or(f64::INFINITY);
        let baseline_total = rows.first().and_then(|r| r.total).unwrap_or(0);
        let best_score = self.summary_best.unwrap_or_else(|| {
            rows.iter()
                .filter(|r| r.decision == "keep")
                .filter_map(|r| r.score)
                .fold(baseline_score, f64::min)
        });
        let best_tiebreak = rows
            .iter()
            .rev()
            .find(|r| r.decision == "keep")
            .or_else(|| rows.first())
            .and_then(|r| r.tiebreak);
        let next_iter = rows.iter().map(|r| r.iter).max().unwrap_or(0) + 1;
        ResumeView {
            rows,
            dead_turns: self.dead_turns(),
            best_score,
            best_tiebreak,
            baseline_score,
            baseline_total,
            spent: self.spent,
            next_iter,
            solved_any: self.solved_any,
            identity: self.identity.clone(),
            published_branches: self.pr_links.iter().map(|l| l.branch.clone()).collect(),
        }
    }

    /// The run's cost when no Budget line landed: the sum of settled plan-task costs.
    pub fn cost_usd(&self) -> f64 {
        if self.spent > 0.0 {
            return self.spent;
        }
        self.plan_results.values().map(|r| r.cost_usd).sum()
    }

    /// The run's best score: the Summary's, else the last kept deep row's.
    pub fn best_score(&self) -> Option<f64> {
        self.summary_best.or_else(|| {
            self.deep_rows()
                .rev()
                .find(|r| r.decision == "keep")
                .and_then(|r| r.score)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::encode;

    fn row(iter: u32, decision: &str, score: f64, phase: Option<&str>) -> SessionEvent {
        SessionEvent::Row {
            row: RowWire {
                iter,
                decision: decision.into(),
                note: String::new(),
                detail: String::new(),
                diff: String::new(),
                diffstat: String::new(),
                score: Some(score),
                tiebreak: None,
                total: None,
                phase: phase.map(str::to_string),
                kept_snap: None,
                evidence: Vec::new(),
                candidate_md: String::new(),
            },
            solved: false,
        }
    }

    fn iteration(iter: u32) -> SessionEvent {
        SessionEvent::Phase {
            phase: "iteration".into(),
            iter,
        }
    }

    fn task(name: &str, cost: f64) -> SessionEvent {
        SessionEvent::TaskResult {
            task: name.into(),
            status: "pass".into(),
            plan_version: 1,
            task_kind: "command".into(),
            iter: 1,
            digest: String::new(),
            job: String::new(),
            attempts: 1,
            cost_usd: cost,
            metric: None,
            output: Some(serde_json::json!({"n": 1})),
            note: String::new(),
            secs: 0.5,
            trace_id: String::new(),
            span_id: String::new(),
        }
    }

    fn plan(version: u32, names: &[&str]) -> SessionEvent {
        SessionEvent::PlanAdmitted {
            plan_version: version,
            reason: String::new(),
            budget_usd: 1.0,
            tasks: names
                .iter()
                .map(|n| PlanTaskWire {
                    name: (*n).into(),
                    kind: "command".into(),
                    depends_on: Vec::new(),
                    session: String::new(),
                    needs: String::new(),
                    required: true,
                    join: String::new(),
                    stage: String::new(),
                    over: String::new(),
                    max_fanout: 0,
                })
                .collect(),
        }
    }

    fn fold(events: &[SessionEvent]) -> LoopState {
        let mut s = LoopState::default();
        for ev in events {
            s.apply(ev);
        }
        s
    }

    #[test]
    fn from_lines_skips_blank_and_torn_lines_and_counts_unknown_kinds() {
        let body = [
            encode(&row(0, "baseline", 240.0, None)),
            String::new(),
            r#"{"v":1,"kind":"teleport"}"#.to_string(),
            r#"{"v":1,"kind":"row","row":{"iter":1,"deci"#.to_string(),
            encode(&iteration(1)),
        ]
        .join("\n");
        let s = LoopState::from_lines(body.lines());
        assert_eq!(s.rows.len(), 1);
        assert_eq!(s.unknown, 1);
        assert!(s.has_rows());
    }

    #[test]
    fn a_trailing_shutdown_is_terminal_and_a_later_event_clears_it() {
        let mut events = vec![
            row(0, "baseline", 240.0, None),
            iteration(1),
            SessionEvent::Shutdown {
                outcome: "suspended".into(),
                reason: "approve:r:gate".into(),
            },
        ];
        let s = fold(&events);
        assert_eq!(
            s.classify(),
            Classification::CleanExit {
                outcome: ShutdownOutcome::Suspended,
                reason: "approve:r:gate".into()
            }
        );
        events.push(SessionEvent::Note {
            msg: "resumed".into(),
        });
        let s = fold(&events);
        assert!(s.terminal.is_none());
        assert_eq!(
            s.classify(),
            Classification::DiedBetweenIterations { last_iter: 0 }
        );
    }

    #[test]
    fn shutdown_tokens_round_trip_and_unknown_is_other() {
        for token in [
            "finished",
            "solved",
            "budget",
            "stopped",
            "escalated",
            "stalled",
            "error",
            "suspended",
        ] {
            assert_eq!(ShutdownOutcome::parse(token).as_str(), token);
        }
        assert_eq!(
            ShutdownOutcome::parse("whatever"),
            ShutdownOutcome::Other("whatever".into())
        );
    }

    #[test]
    fn plan_results_key_by_task_and_reset_when_the_plan_version_changes() {
        let s = fold(&[
            row(0, "baseline", 240.0, None),
            iteration(1),
            plan(1, &["a", "b[x]", "b[y]"]),
            task("a", 0.2),
            task("b[x]", 0.3),
        ]);
        assert_eq!(s.plan_open(), vec!["b[y]".to_string()]);
        assert_eq!(s.cost_usd(), 0.5, "no budget line: task costs sum");
        assert!(matches!(
            s.classify(),
            Classification::DiedInPlanTask { iter: 1, .. }
        ));

        let mut events = vec![
            row(0, "baseline", 240.0, None),
            iteration(1),
            plan(1, &["a"]),
            task("a", 0.2),
            row(1, "keep", 200.0, None),
            iteration(2),
            plan(2, &["a", "c"]),
        ];
        let s = fold(&events);
        assert!(s.plan_results.is_empty(), "a new plan version starts empty");
        assert_eq!(s.plan_open(), vec!["a".to_string(), "c".to_string()]);
        events.push(task("a", 0.1));
        let s = fold(&events);
        assert_eq!(s.plan_open(), vec!["c".to_string()]);
    }

    #[test]
    fn a_budget_line_wins_over_the_task_cost_sum() {
        let s = fold(&[
            row(0, "baseline", 240.0, None),
            plan(1, &["a"]),
            task("a", 0.2),
            SessionEvent::Budget {
                spent: 1.5,
                elapsed_secs: 10,
            },
        ]);
        assert_eq!(s.cost_usd(), 1.5);
        assert_eq!(s.elapsed_secs, Some(10));
    }

    #[test]
    fn best_score_prefers_the_summary_then_the_last_kept_deep_row() {
        let base = vec![
            row(0, "baseline", 240.0, None),
            iteration(1),
            row(1, "keep", 220.0, None),
            row(2, "wide-keep-0", 100.0, Some("wide")),
            row(3, "discard", 260.0, None),
        ];
        let s = fold(&base);
        assert_eq!(s.best_score(), Some(220.0));
        assert_eq!(s.deep_rows().count(), 3, "the wide row is not a deep row");
        let mut with_summary = base.clone();
        with_summary.push(SessionEvent::Summary {
            rows: Vec::new(),
            gate: "bench".into(),
            best_score: Some(210.0),
        });
        assert_eq!(fold(&with_summary).best_score(), Some(210.0));
    }

    #[test]
    fn the_resume_view_restores_baseline_best_and_next_iter_from_deep_rows() {
        let s = fold(&[
            row(0, "baseline", 240.0, None),
            iteration(1),
            row(1, "keep", 220.0, None),
            row(1, "infra", 0.0, Some("infra")),
            row(2, "discard", 260.0, None),
            SessionEvent::Budget {
                spent: 2.5,
                elapsed_secs: 40,
            },
        ]);
        let v = s.resume_view();
        assert_eq!(v.rows.len(), 3);
        assert_eq!(v.baseline_score, 240.0);
        assert_eq!(v.best_score, 220.0);
        assert_eq!(v.next_iter, 3);
        assert_eq!(v.spent, 2.5);
        assert_eq!(
            v.dead_turns, 0,
            "a decided row after the dead turn ends the streak"
        );
    }

    #[test]
    fn the_resume_view_carries_the_dead_turn_streak_the_log_ends_on() {
        // A pod that died mid-streak must not hand its successor a fresh attempt budget.
        let s = fold(&[
            row(0, "baseline", 240.0, None),
            iteration(1),
            row(1, "keep", 220.0, None),
            iteration(2),
            row(2, "infra-dead", 0.0, Some("infra")),
            row(2, "infra-dead", 0.0, Some("infra")),
        ]);
        let v = s.resume_view();
        assert_eq!(v.next_iter, 2, "the never-started iteration re-runs");
        assert_eq!(v.dead_turns, 2);

        // A distress row annotates the turn before it; it does not report one, so it neither
        // ends the streak nor counts toward it.
        let mut with_distress = vec![
            row(0, "baseline", 240.0, None),
            iteration(2),
            row(2, "infra-dead", 0.0, Some("infra")),
        ];
        with_distress.push(row(1, "distressed", 0.0, None));
        assert_eq!(fold(&with_distress).resume_view().dead_turns, 1);

        // No dead turn at the tail, no streak.
        assert_eq!(
            fold(&[row(0, "baseline", 240.0, None)])
                .resume_view()
                .dead_turns,
            0
        );
    }

    #[test]
    fn an_open_gate_carries_its_task_and_source_and_a_resolution_closes_it() {
        let source =
            serde_json::json!({"kind": "jira", "key": "PROJ-1", "until": {"status": "Ready"}});
        let mut events = vec![
            row(0, "baseline", 240.0, None),
            iteration(1),
            SessionEvent::ApprovalWait {
                handle: "PROJ-1".into(),
                trace_id: "approve:r:gate".into(),
                mode: "block".into(),
                task: Some("gate".into()),
                source: Some(source.clone()),
                park: Some("suspend".into()),
            },
        ];
        let s = fold(&events);
        let open = s.open_approval().expect("gate is open");
        assert_eq!(open.task.as_deref(), Some("gate"));
        assert_eq!(open.source.as_ref(), Some(&source));
        assert_eq!(open.mode, WaitMode::Block);
        assert!(matches!(
            s.classify(),
            Classification::DiedAwaitingApproval { .. }
        ));
        events.push(SessionEvent::ApprovalResolved {
            outcome: "granted".into(),
            reason: String::new(),
            trace_id: "approve:r:gate".into(),
            by: Some("alice".into()),
            source: Some("jira".into()),
        });
        assert!(fold(&events).open_approval().is_none());
    }

    #[test]
    fn an_unknown_wait_mode_never_reparks() {
        assert_eq!(WaitMode::parse("block"), WaitMode::Block);
        assert_eq!(WaitMode::parse("continue"), WaitMode::Continue);
        assert_eq!(WaitMode::parse("sideways"), WaitMode::Continue);
        let s = fold(&[
            row(0, "baseline", 240.0, None),
            iteration(1),
            SessionEvent::ApprovalWait {
                handle: "h".into(),
                trace_id: "t".into(),
                mode: "sideways".into(),
                task: None,
                source: None,
                park: None,
            },
        ]);
        assert_eq!(
            s.classify(),
            Classification::DiedBetweenIterations { last_iter: 0 }
        );
    }
}
