//! Recovery classification: one streaming pass over a dead run's session log that names
//! HOW it died, so `--resume` acts on facts instead of pretending every interruption was
//! a clean stop.
//!
//! The classifier reads only the durable log, never marker files (those are
//! consume-on-read and gone by park time). It produces facts plus a class token; policy
//! lives in [`plan_recovery`], the one function that maps a classification to what
//! `--resume` does. Retry semantics, iteration accounting, and tree restoration belong
//! elsewhere: [`TurnEvidence`] is their input, not their implementation.

use crate::event::AgentEvent;
use crate::loop_driver::{ResumeFold, ResumeState};
use crate::provisioning::{self, WaitMode};
use crate::session::{self, RecoveryClass, SessionEvent};
use anyhow::{Context, Result};
use crucible_contract::admission::AdmissionKey;
use std::io::BufRead;
use std::path::Path;

/// Evidence scraped from a dangling turn's Agent events. Facts only: the retry/backoff
/// policy that consumes these belongs to iteration accounting, not to this module.
#[derive(Debug, Clone, Default)]
pub(crate) struct TurnEvidence {
    /// Agent events seen inside the dangling AgentStart..(missing AgentDone) bracket.
    pub agent_events: u32,
    /// Last per-turn cost seen (`Tokens.cost_usd` or `OtelSummary.cost_usd`).
    pub last_cost_usd: Option<f64>,
    /// Last error text (`Error.message` or an is_error `Result.error`), verbatim.
    pub last_error: Option<String>,
    /// The durable session the turn ran under, from the preceding AgentSession line.
    pub session: Option<DanglingSession>,
}

#[derive(Debug, Clone)]
pub(crate) struct DanglingSession {
    pub name: String,
    /// The 1-based turn number the AgentSession line announced.
    pub turn: u32,
}

/// An approval the dead run was still waiting on (dangling ApprovalWait).
#[derive(Debug, Clone)]
pub(crate) struct PendingApproval {
    pub handle: String,
    pub trace_id: String,
    pub mode: WaitMode,
}

/// A plan admitted but never accounted (PlanAdmitted with its iteration's Row absent).
/// The coarseness is inherent: the graph runner batches TaskResult emission until the
/// executor returns (`crate::loop_graph`), so a mid-plan death leaves no per-task rows;
/// `resulted` is non-empty only when the executor returned but the Row never landed.
#[derive(Debug, Clone)]
pub(crate) struct OpenPlan {
    pub plan_version: u32,
    /// PlanTaskWire names, in order.
    pub declared: Vec<String>,
    /// TaskResult names seen after this PlanAdmitted.
    pub resulted: Vec<String>,
    /// Admitted before the first iteration Phase (the wide tournament).
    pub wide: bool,
}

/// What the tail of the log says happened. Facts plus a class token; no policy.
#[derive(Debug, Clone)]
pub(crate) enum Classification {
    /// Shutdown line present: the previous process exited on purpose.
    CleanExit {
        outcome: ShutdownOutcome,
        reason: String,
    },
    /// Start (maybe Phase baseline) but no decided rows: nothing to resume from.
    DiedInBaseline,
    /// Died before the deep loop's first iteration, inside the wide tournament.
    DiedInWideRound {
        plan: Option<OpenPlan>,
        turn: Option<(u32, TurnEvidence)>,
    },
    /// AgentStart with no AgentDone: the turn was in flight.
    DiedMidTurn {
        iter: u32,
        evidence: TurnEvidence,
        approval: Option<PendingApproval>,
    },
    /// Turn finished (AgentDone) but no Row for that iter: died in measure/decide/keep.
    DiedDeciding { iter: u32, evidence: TurnEvidence },
    /// Graph loop: PlanAdmitted for an iteration with no Row and no dangling turn.
    DiedInPlanTask { iter: u32, plan: OpenPlan },
    /// Parked on a block-mode approval (dangling ApprovalWait, no turn in flight).
    DiedAwaitingApproval { approval: PendingApproval },
    /// No dangling turn/plan/approval; died between a Row and the next AgentStart.
    DiedBetweenIterations { last_iter: u32 },
}

/// The `Shutdown.outcome` tokens as a closed enum (`crate::loop_driver::LoopExit` owns the
/// writer side). Unknown tokens map to `Other` so a newer writer never breaks an older
/// resumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShutdownOutcome {
    Finished,
    Solved,
    Budget,
    Stopped,
    Escalated,
    Stalled,
    Error,
    Other(String),
}

impl ShutdownOutcome {
    pub(crate) fn parse(token: &str) -> Self {
        match token {
            "finished" => ShutdownOutcome::Finished,
            "solved" => ShutdownOutcome::Solved,
            "budget" => ShutdownOutcome::Budget,
            "stopped" => ShutdownOutcome::Stopped,
            "escalated" => ShutdownOutcome::Escalated,
            "stalled" => ShutdownOutcome::Stalled,
            "error" => ShutdownOutcome::Error,
            other => ShutdownOutcome::Other(other.to_string()),
        }
    }

    fn as_str(&self) -> &str {
        match self {
            ShutdownOutcome::Finished => "finished",
            ShutdownOutcome::Solved => "solved",
            ShutdownOutcome::Budget => "budget",
            ShutdownOutcome::Stopped => "stopped",
            ShutdownOutcome::Escalated => "escalated",
            ShutdownOutcome::Stalled => "stalled",
            ShutdownOutcome::Error => "error",
            ShutdownOutcome::Other(t) => t,
        }
    }
}

/// Truncate to `max` chars on a char boundary, marking the cut.
fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

impl Classification {
    /// The wire token for the Recovery event.
    pub(crate) fn class(&self) -> RecoveryClass {
        match self {
            Classification::CleanExit { .. } => RecoveryClass::CleanExit,
            Classification::DiedInBaseline => RecoveryClass::DiedInBaseline,
            Classification::DiedInWideRound { .. } => RecoveryClass::DiedInWideRound,
            Classification::DiedMidTurn { .. } => RecoveryClass::DiedMidTurn,
            Classification::DiedDeciding { .. } => RecoveryClass::DiedDeciding,
            Classification::DiedInPlanTask { .. } => RecoveryClass::DiedInPlanTask,
            Classification::DiedAwaitingApproval { .. } => RecoveryClass::DiedAwaitingApproval,
            Classification::DiedBetweenIterations { .. } => RecoveryClass::DiedBetweenIterations,
        }
    }

    /// The iteration the interruption touched, for the Recovery event (0 if none).
    pub(crate) fn iter(&self) -> u32 {
        match self {
            Classification::DiedMidTurn { iter, .. }
            | Classification::DiedDeciding { iter, .. }
            | Classification::DiedInPlanTask { iter, .. } => *iter,
            Classification::DiedBetweenIterations { last_iter } => *last_iter,
            _ => 0,
        }
    }

    /// One-line evidence summary for the Recovery event's `detail` and the resume note.
    pub(crate) fn detail(&self) -> String {
        match self {
            Classification::CleanExit { outcome, reason } => {
                format!("previous run exited {}: {reason}", outcome.as_str())
            }
            Classification::DiedInBaseline => "no decided rows in the log".to_string(),
            Classification::DiedInWideRound { plan, turn } => {
                let mut s = "died in the wide tournament".to_string();
                if let Some(p) = plan {
                    s.push_str(&format!(
                        ", plan v{} open ({}/{} tasks resulted)",
                        p.plan_version,
                        p.resulted.len(),
                        p.declared.len()
                    ));
                }
                if turn.is_some() {
                    s.push_str(", a turn was in flight");
                }
                s
            }
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
                "{}plan v{} at iter {iter} never accounted ({}/{} tasks resulted)",
                if plan.wide { "wide " } else { "" },
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

/// The per-event scanner. Fed every decoded line in order; `finish` names the tail.
#[derive(Default)]
struct TailScan {
    /// Only a TRAILING Shutdown counts: a resumed process appends past its
    /// predecessor's Shutdown, so any later event clears it.
    shutdown: Option<(String, String)>,
    saw_iteration_phase: bool,
    /// Any decided (non-wide, non-infra) row, matching the counter fold's filter.
    saw_any_row: bool,
    last_row_iter: u32,
    last_phase_iter: u32,
    /// AgentSession seen; the next AgentStart claims it.
    pending_session: Option<DanglingSession>,
    /// AgentStart .. missing AgentDone.
    open_turn: Option<(u32, TurnEvidence)>,
    /// AgentDone landed, the Row for that iter has not.
    done_unrowed: Option<(u32, TurnEvidence)>,
    /// PlanAdmitted .. missing its Row.
    open_plan: Option<OpenPlan>,
    /// ApprovalWait .. missing ApprovalResolved.
    open_approval: Option<PendingApproval>,
}

impl TailScan {
    fn feed(&mut self, ev: &SessionEvent) {
        if self.shutdown.is_some() && !matches!(ev, SessionEvent::Shutdown { .. }) {
            self.shutdown = None;
        }
        match ev {
            SessionEvent::Shutdown { outcome, reason } => {
                self.shutdown = Some((outcome.clone(), reason.clone()));
            }
            SessionEvent::Phase { phase, iter } => {
                if phase == "iteration" {
                    self.saw_iteration_phase = true;
                    self.last_phase_iter = *iter;
                    // The Row arm is the primary clear; this is the safety net for
                    // paths that account an iteration without a Row (park-continue).
                    self.done_unrowed = None;
                }
            }
            SessionEvent::AgentSession { session, turn, .. } => {
                self.pending_session = Some(DanglingSession {
                    name: session.clone(),
                    turn: *turn,
                });
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
            SessionEvent::Row { row, .. } => {
                if !matches!(row.phase.as_deref(), Some("wide") | Some("infra")) {
                    self.saw_any_row = true;
                    self.last_row_iter = row.iter;
                }
                if self
                    .done_unrowed
                    .as_ref()
                    .is_some_and(|(iter, _)| *iter == row.iter)
                {
                    self.done_unrowed = None;
                }
                // Any row after an admission accounts the plan's iteration: keep,
                // discard, discarded, infra-dead, or a wide result row.
                self.open_plan = None;
            }
            SessionEvent::PlanAdmitted {
                plan_version,
                tasks,
                ..
            } => {
                self.open_plan = Some(OpenPlan {
                    plan_version: *plan_version,
                    declared: tasks.iter().map(|t| t.name.clone()).collect(),
                    resulted: Vec::new(),
                    wide: !self.saw_iteration_phase,
                });
            }
            SessionEvent::TaskResult { task, .. } => {
                if let Some(plan) = &mut self.open_plan {
                    plan.resulted.push(task.clone());
                }
            }
            SessionEvent::ApprovalWait {
                handle,
                trace_id,
                mode,
            } => {
                self.open_approval = Some(PendingApproval {
                    handle: handle.clone(),
                    trace_id: trace_id.clone(),
                    // Unknown tokens degrade to Continue: never re-park on a mode a
                    // newer writer invented and this binary can't honor.
                    mode: if mode == "block" {
                        WaitMode::Block
                    } else {
                        WaitMode::Continue
                    },
                });
            }
            SessionEvent::ApprovalResolved { .. } => self.open_approval = None,
            // Counter-fold events and inert history (a Recovery from an earlier resume).
            _ => {}
        }
    }

    /// Name the tail. Precedence: each earlier case subsumes the later ones.
    fn finish(mut self) -> (Classification, Option<PendingApproval>) {
        let pending = self.open_approval.clone();
        let classification = if let Some((outcome, reason)) = self.shutdown {
            Classification::CleanExit {
                outcome: ShutdownOutcome::parse(&outcome),
                reason,
            }
        } else if !self.saw_any_row {
            Classification::DiedInBaseline
        } else if !self.saw_iteration_phase {
            Classification::DiedInWideRound {
                plan: self.open_plan,
                turn: self.open_turn,
            }
        } else if let Some((iter, evidence)) = self.open_turn {
            Classification::DiedMidTurn {
                iter,
                evidence,
                approval: self.open_approval.take(),
            }
        } else if let Some(plan) = self.open_plan {
            Classification::DiedInPlanTask {
                iter: self.last_phase_iter,
                plan,
            }
        } else if let Some(approval) = self.open_approval.take_if(|a| a.mode == WaitMode::Block) {
            Classification::DiedAwaitingApproval { approval }
        } else if let Some((iter, evidence)) = self.done_unrowed {
            Classification::DiedDeciding { iter, evidence }
        } else {
            Classification::DiedBetweenIterations {
                last_iter: self.last_row_iter,
            }
        };
        (classification, pending)
    }
}

/// Everything `--resume` needs from the log, produced in ONE pass: the counter fold
/// (rows/spent/best) and the tail classification.
#[derive(Debug)]
pub(crate) struct SessionRecovery {
    pub resume: ResumeState,
    pub classification: Classification,
    /// Dangling approval regardless of class (block or continue), for re-registration.
    pub pending_approval: Option<PendingApproval>,
}

/// Replay the session log into resume counters and a tail classification. Torn final
/// lines decode to `None` and are skipped, so a mid-write kill is tolerated by
/// construction. A rowless log is a refusal: `--resume` means continue, not restart.
pub(crate) fn classify_session(session_log: &Path) -> Result<SessionRecovery> {
    let file = std::fs::File::open(session_log).with_context(|| {
        format!(
            "reading session log {} to resume (run with --ui stream first?)",
            session_log.display()
        )
    })?;
    let mut fold = ResumeFold::default();
    let mut scan = TailScan::default();
    for line in std::io::BufReader::new(file).lines() {
        let line =
            line.with_context(|| format!("reading session log {}", session_log.display()))?;
        let Some(ev) = session::decode(&line) else {
            continue;
        };
        fold.feed(&ev);
        scan.feed(&ev);
    }
    let (classification, pending_approval) = scan.finish();
    if !fold.has_rows() {
        anyhow::bail!(
            "session log {} has no rows to resume from ({})",
            session_log.display(),
            classification.class()
        );
    }
    Ok(SessionRecovery {
        resume: fold.finish(),
        classification,
        pending_approval,
    })
}

/// What `--resume` should do, derived from the classification. The typed successor to
/// the arithmetic no-op guard in `run.rs`.
pub(crate) enum RecoveryPlan {
    /// Exit 0 without entering the loop (and without re-running the finish path).
    NoOp { message: String },
    /// Refuse with a nonzero explanation (escalated runs need a human, not a re-run).
    Refuse { message: String },
    /// Enter the loop. `repark` re-arms the block-mode approval wait; `pending_regime`
    /// re-registers the approval key on the control bridge.
    Continue {
        repark: Option<provisioning::PendingProvisioning>,
        pending_regime: Option<String>,
    },
}

/// The classification hand-off from `run.rs` into the loop, resolved by
/// [`plan_recovery`]. Carried on [`crate::loop_driver::LoopRuntime`].
pub(crate) struct ResumeRecovery {
    pub class: RecoveryClass,
    pub iter: u32,
    pub detail: String,
    pub repark: Option<provisioning::PendingProvisioning>,
    pub pending_regime: Option<String>,
}

/// Map a classification to the resume action. Policy lives here, in one auditable
/// function; the classifier only reports facts.
pub(crate) fn plan_recovery(s: &SessionRecovery, iterations: u32, max_cost: f64) -> RecoveryPlan {
    let finished = crate::loop_driver::resume_finished(&s.resume, iterations, max_cost);
    let nothing_to_do = || RecoveryPlan::NoOp {
        message: format!(
            "nothing to do ({} of {} iterations ran, ${:.2} of ${:.2} spent)",
            s.resume.next_iter.saturating_sub(1),
            iterations,
            s.resume.spent,
            max_cost
        ),
    };
    if let Classification::CleanExit { outcome, reason } = &s.classification {
        match outcome {
            ShutdownOutcome::Finished | ShutdownOutcome::Solved => {
                return RecoveryPlan::NoOp {
                    message: format!("run already {}: {reason}", outcome.as_str()),
                };
            }
            ShutdownOutcome::Escalated => {
                return RecoveryPlan::Refuse {
                    message: format!(
                        "run escalated for human review: {reason}; address the escalation, \
                         then start a fresh run or clear the log"
                    ),
                };
            }
            // Budget falls through to the arithmetic guard: the operator may resume
            // with a raised cap, in which case the run continues.
            ShutdownOutcome::Budget
            | ShutdownOutcome::Stopped
            | ShutdownOutcome::Stalled
            | ShutdownOutcome::Error
            | ShutdownOutcome::Other(_) => {}
        }
    }
    if finished {
        return nothing_to_do();
    }
    RecoveryPlan::Continue {
        repark: s
            .pending_approval
            .as_ref()
            .filter(|a| a.mode == WaitMode::Block)
            .map(|a| provisioning::PendingProvisioning {
                mode: WaitMode::Block,
                trace_id: a.trace_id.clone(),
                handle: a.handle.clone(),
            }),
        pending_regime: s.pending_approval.as_ref().map(|a| a.trace_id.clone()),
    }
}

/// What a resume does about an approval the previous process left outstanding, after both
/// sources of truth have been consulted.
pub(crate) struct ResumeApproval {
    /// Re-arm the block-mode park, unless the grant is already on the ledger.
    pub repark: Option<provisioning::PendingProvisioning>,
    /// Re-register the approval key so an operator `approve` still resolves it.
    pub pending_regime: Option<String>,
    /// One line for the session log when the ledger overrode the log's wait-state.
    pub note: Option<String>,
}

/// Precedence between the two sources of truth for an outstanding approval:
/// `admissions.jsonl` is authoritative for what an operator asked for, the session log for
/// what the loop was waiting on. So the ledger is consulted first, and a re-scope it holds
/// under the key derived from THIS ask ([`AdmissionKey::rescope_from`] over
/// [`AdmissionKey::approve`]) means the grant landed before the death: re-parking would
/// idle on an approval that already happened, so the park is dropped and the iteration-head
/// drain applies the re-scope. Any other state (no ledger, an unrelated re-scope, nothing
/// recorded) leaves the classifier's re-park and key registration alone.
pub(crate) fn resume_approval(
    rec: &ResumeRecovery,
    replay: Option<&crate::admission::ResumeReplay>,
) -> ResumeApproval {
    let granted = rec.pending_regime.as_deref().filter(|trace| {
        let derived = AdmissionKey::rescope_from(&AdmissionKey::approve(trace));
        replay.is_some_and(|r| {
            r.unsettled_rescope
                .as_ref()
                .is_some_and(|(key, _)| key == &derived)
        })
    });
    match granted {
        Some(trace) => ResumeApproval {
            repark: None,
            pending_regime: None,
            note: Some(format!(
                "resume: the approval for '{trace}' was already granted before the run died \
                 — applying the recorded re-scope instead of parking"
            )),
        },
        None => ResumeApproval {
            repark: rec.repark.clone(),
            pending_regime: rec.pending_regime.clone(),
            note: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{PlanTaskWire, RowWire, encode};

    fn write_log(name: &str, events: &[SessionEvent]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "crucible-recovery-{}-{name}.jsonl",
            std::process::id()
        ));
        let body = events.iter().map(encode).collect::<Vec<_>>().join("\n");
        std::fs::write(&path, body).unwrap();
        path
    }

    fn row(iter: u32, decision: &str, score: f64) -> SessionEvent {
        SessionEvent::Row {
            row: RowWire {
                iter,
                decision: decision.into(),
                note: String::new(),
                detail: String::new(),
                diff: String::new(),
                diffstat: String::new(),
                score: Some(score),
                total: None,
                phase: None,
                kept_snap: None,
                tiebreak: None,
                evidence: Vec::new(),
                candidate_md: String::new(),
            },
            solved: false,
        }
    }

    fn iteration_phase(iter: u32) -> SessionEvent {
        SessionEvent::Phase {
            phase: "iteration".into(),
            iter,
        }
    }

    /// Baseline row + first iteration head: the minimal healthy prefix.
    fn prefix() -> Vec<SessionEvent> {
        vec![row(0, "baseline", 240.0), iteration_phase(1)]
    }

    fn classify(name: &str, events: &[SessionEvent]) -> SessionRecovery {
        let path = write_log(name, events);
        let got = classify_session(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        got
    }

    #[test]
    fn trailing_shutdown_classifies_clean_exit_per_outcome() {
        for (token, want) in [
            ("finished", ShutdownOutcome::Finished),
            ("solved", ShutdownOutcome::Solved),
            ("budget", ShutdownOutcome::Budget),
            ("stopped", ShutdownOutcome::Stopped),
            ("escalated", ShutdownOutcome::Escalated),
            ("stalled", ShutdownOutcome::Stalled),
            ("error", ShutdownOutcome::Error),
            ("whatever", ShutdownOutcome::Other("whatever".into())),
        ] {
            let mut events = prefix();
            events.push(row(1, "keep", 210.0));
            events.push(SessionEvent::Shutdown {
                outcome: token.into(),
                reason: "r".into(),
            });
            let got = classify(&format!("clean-{token}"), &events);
            match &got.classification {
                Classification::CleanExit { outcome, reason } => {
                    assert_eq!(*outcome, want, "{token}");
                    assert_eq!(reason, "r");
                }
                other => panic!("{token}: expected CleanExit, got {other:?}"),
            }
        }
    }

    #[test]
    fn shutdown_from_a_prior_resume_is_not_a_clean_exit() {
        // Run 1 stopped cleanly; run 2 (a resume) appended more and died mid-turn.
        // Only a TRAILING Shutdown may classify as CleanExit.
        let mut events = prefix();
        events.push(row(1, "keep", 210.0));
        events.push(SessionEvent::Shutdown {
            outcome: "stopped".into(),
            reason: "ctrl-c".into(),
        });
        events.push(SessionEvent::Note {
            msg: "resumed".into(),
        });
        events.push(iteration_phase(2));
        events.push(SessionEvent::AgentStart { iter: 2 });
        let got = classify("resumed-then-died", &events);
        assert!(
            matches!(
                got.classification,
                Classification::DiedMidTurn { iter: 2, .. }
            ),
            "{:?}",
            got.classification
        );
    }

    #[test]
    fn dangling_agent_start_classifies_died_mid_turn_with_evidence() {
        let mut events = prefix();
        events.push(row(1, "discard", 250.0));
        events.push(iteration_phase(2));
        events.push(SessionEvent::AgentSession {
            session: "solver".into(),
            action: crate::session::SessionAction::Resumed,
            turn: 3,
        });
        events.push(SessionEvent::AgentStart { iter: 2 });
        events.push(SessionEvent::Agent {
            event: AgentEvent::Text {
                delta: "thinking".into(),
            },
        });
        events.push(SessionEvent::Agent {
            event: AgentEvent::Tokens(crate::event::Tokens {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                total: 15,
                rate: None,
                cost_usd: Some(1.83),
            }),
        });
        events.push(SessionEvent::Agent {
            event: AgentEvent::Error {
                error_type: "overloaded".into(),
                message: "server overloaded".into(),
            },
        });
        let got = classify("mid-turn", &events);
        match &got.classification {
            Classification::DiedMidTurn {
                iter,
                evidence,
                approval,
            } => {
                assert_eq!(*iter, 2);
                assert_eq!(evidence.agent_events, 3);
                assert_eq!(evidence.last_cost_usd, Some(1.83));
                assert_eq!(evidence.last_error.as_deref(), Some("server overloaded"));
                let sess = evidence.session.as_ref().expect("dangling session");
                assert_eq!(sess.name, "solver");
                assert_eq!(sess.turn, 3);
                assert!(approval.is_none());
            }
            other => panic!("expected DiedMidTurn, got {other:?}"),
        }
        assert_eq!(got.classification.iter(), 2);
        let detail = got.classification.detail();
        assert!(detail.contains("iter 2"), "{detail}");
        assert!(detail.contains("session solver turn 3"), "{detail}");
        // The counter fold ran in the same pass: the interrupted iter re-runs.
        assert_eq!(got.resume.next_iter, 2);
    }

    #[test]
    fn agent_done_without_row_classifies_died_deciding() {
        let mut events = prefix();
        events.push(row(1, "keep", 210.0));
        events.push(iteration_phase(2));
        events.push(SessionEvent::AgentStart { iter: 2 });
        events.push(SessionEvent::AgentDone);
        let got = classify("deciding", &events);
        assert!(
            matches!(
                got.classification,
                Classification::DiedDeciding { iter: 2, .. }
            ),
            "{:?}",
            got.classification
        );
    }

    #[test]
    fn plan_admitted_without_row_classifies_died_in_plan_task() {
        let mut events = prefix();
        events.push(row(1, "keep", 210.0));
        events.push(iteration_phase(2));
        events.push(SessionEvent::PlanAdmitted {
            plan_version: 7,
            reason: String::new(),
            budget_usd: 5.0,
            tasks: vec![
                PlanTaskWire {
                    name: "propose".into(),
                    kind: "agent".into(),
                    depends_on: vec![],
                    session: String::new(),
                    needs: String::new(),
                    required: true,
                },
                PlanTaskWire {
                    name: "measure".into(),
                    kind: "command".into(),
                    depends_on: vec!["propose".into()],
                    session: String::new(),
                    needs: String::new(),
                    required: true,
                },
            ],
        });
        let got = classify("plan-task", &events);
        match &got.classification {
            Classification::DiedInPlanTask { iter, plan } => {
                assert_eq!(*iter, 2);
                assert_eq!(plan.plan_version, 7);
                assert_eq!(plan.declared, vec!["propose", "measure"]);
                assert!(plan.resulted.is_empty());
                assert!(!plan.wide);
            }
            other => panic!("expected DiedInPlanTask, got {other:?}"),
        }
    }

    #[test]
    fn a_row_closes_the_open_plan() {
        let mut events = prefix();
        events.push(SessionEvent::PlanAdmitted {
            plan_version: 1,
            reason: String::new(),
            budget_usd: 5.0,
            tasks: vec![],
        });
        events.push(row(1, "discard", 260.0));
        let got = classify("plan-closed", &events);
        assert!(
            matches!(
                got.classification,
                Classification::DiedBetweenIterations { last_iter: 1 }
            ),
            "{:?}",
            got.classification
        );
    }

    #[test]
    fn pre_iteration_plan_classifies_died_in_wide_round() {
        // A wide tournament plan admitted before any iteration Phase.
        let events = vec![
            row(0, "baseline", 240.0),
            SessionEvent::PlanAdmitted {
                plan_version: 1,
                reason: String::new(),
                budget_usd: 5.0,
                tasks: vec![],
            },
        ];
        let got = classify("wide", &events);
        match &got.classification {
            Classification::DiedInWideRound { plan, turn } => {
                assert!(plan.as_ref().is_some_and(|p| p.wide));
                assert!(turn.is_none());
            }
            other => panic!("expected DiedInWideRound, got {other:?}"),
        }
    }

    #[test]
    fn dangling_block_approval_classifies_died_awaiting_approval() {
        let mut events = prefix();
        events.push(row(1, "discard", 250.0));
        events.push(SessionEvent::ApprovalWait {
            handle: "https://example.com/pr/7".into(),
            trace_id: "c=48".into(),
            mode: "block".into(),
        });
        let got = classify("awaiting", &events);
        match &got.classification {
            Classification::DiedAwaitingApproval { approval } => {
                assert_eq!(approval.handle, "https://example.com/pr/7");
                assert_eq!(approval.mode, WaitMode::Block);
            }
            other => panic!("expected DiedAwaitingApproval, got {other:?}"),
        }
        // The plan re-parks and re-registers the approval key.
        match plan_recovery(&got, 5, 0.0) {
            RecoveryPlan::Continue {
                repark,
                pending_regime,
            } => {
                let pp = repark.expect("block-mode wait re-parks");
                assert_eq!(pp.mode, WaitMode::Block);
                assert_eq!(pp.trace_id, "c=48");
                assert_eq!(pending_regime.as_deref(), Some("c=48"));
            }
            _ => panic!("expected Continue"),
        }
    }

    #[test]
    fn resolved_approval_does_not_dangle() {
        let mut events = prefix();
        events.push(row(1, "discard", 250.0));
        events.push(SessionEvent::ApprovalWait {
            handle: "h".into(),
            trace_id: "t".into(),
            mode: "block".into(),
        });
        events.push(SessionEvent::ApprovalResolved {
            outcome: "denied".into(),
            reason: "policy".into(),
        });
        let got = classify("resolved", &events);
        assert!(got.pending_approval.is_none());
        assert!(
            matches!(
                got.classification,
                Classification::DiedBetweenIterations { .. }
            ),
            "{:?}",
            got.classification
        );
    }

    #[test]
    fn continue_mode_dangling_wait_rides_as_pending_approval() {
        let mut events = prefix();
        events.push(SessionEvent::ApprovalWait {
            handle: "h".into(),
            trace_id: "regime-x".into(),
            mode: "continue".into(),
        });
        events.push(row(1, "discard", 250.0));
        let got = classify("continue-wait", &events);
        assert!(
            matches!(
                got.classification,
                Classification::DiedBetweenIterations { .. }
            ),
            "{:?}",
            got.classification
        );
        let pending = got.pending_approval.as_ref().expect("wait still open");
        assert_eq!(pending.mode, WaitMode::Continue);
        match plan_recovery(&got, 5, 0.0) {
            RecoveryPlan::Continue {
                repark,
                pending_regime,
            } => {
                assert!(repark.is_none(), "continue-mode never re-parks");
                assert_eq!(pending_regime.as_deref(), Some("regime-x"));
            }
            _ => panic!("expected Continue"),
        }
    }

    #[test]
    fn stop_while_parked_keeps_the_approval_for_a_repark() {
        // A stop doesn't resolve the ask: Shutdown(stopped) lands with the wait open.
        let mut events = prefix();
        events.push(row(1, "discard", 250.0));
        events.push(SessionEvent::ApprovalWait {
            handle: "h".into(),
            trace_id: "t".into(),
            mode: "block".into(),
        });
        events.push(SessionEvent::Shutdown {
            outcome: "stopped".into(),
            reason: "stop signal received".into(),
        });
        let got = classify("stop-parked", &events);
        assert!(matches!(
            got.classification,
            Classification::CleanExit {
                outcome: ShutdownOutcome::Stopped,
                ..
            }
        ));
        match plan_recovery(&got, 5, 0.0) {
            RecoveryPlan::Continue { repark, .. } => {
                assert!(
                    repark.is_some(),
                    "clean stop-while-parked re-parks on resume"
                );
            }
            _ => panic!("expected Continue"),
        }
    }

    #[test]
    fn open_turn_beats_open_plan_and_carries_the_approval() {
        // Graph loop: plan open, turn in flight inside it, continue-mode wait open.
        let mut events = prefix();
        events.push(row(1, "keep", 210.0));
        events.push(iteration_phase(2));
        events.push(SessionEvent::PlanAdmitted {
            plan_version: 3,
            reason: String::new(),
            budget_usd: 5.0,
            tasks: vec![],
        });
        events.push(SessionEvent::ApprovalWait {
            handle: "h".into(),
            trace_id: "t".into(),
            mode: "continue".into(),
        });
        events.push(SessionEvent::AgentStart { iter: 2 });
        let got = classify("precedence", &events);
        match &got.classification {
            Classification::DiedMidTurn { iter, approval, .. } => {
                assert_eq!(*iter, 2);
                assert!(approval.is_some(), "the outstanding wait rides as evidence");
            }
            other => panic!("expected DiedMidTurn, got {other:?}"),
        }
        assert!(got.pending_approval.is_some());
    }

    #[test]
    fn torn_final_line_is_tolerated() {
        let mut events = prefix();
        events.push(row(1, "keep", 210.0));
        let path = write_log("torn", &events);
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            write!(f, "\n{{\"v\":1,\"kind\":\"age").unwrap();
        }
        let got = classify_session(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(matches!(
            got.classification,
            Classification::DiedBetweenIterations { last_iter: 1 }
        ));
        assert_eq!(got.resume.next_iter, 2);
    }

    #[test]
    fn rowless_log_is_a_refusal_naming_the_class() {
        let path = write_log(
            "rowless",
            &[SessionEvent::Note {
                msg: "starting".into(),
            }],
        );
        let err = classify_session(&path).unwrap_err();
        let _ = std::fs::remove_file(&path);
        let msg = format!("{err:#}");
        assert!(msg.contains("no rows to resume from"), "{msg}");
        assert!(msg.contains("died_in_baseline"), "{msg}");
    }

    #[test]
    fn infra_dead_turn_tail_classifies_between_iterations() {
        // A dead turn's bracket closes (AgentDone) and its infra row lands; the
        // iteration was not consumed and nothing dangles.
        let mut events = prefix();
        events.push(SessionEvent::AgentStart { iter: 1 });
        events.push(SessionEvent::AgentDone);
        events.push(SessionEvent::Row {
            row: RowWire {
                iter: 1,
                decision: "infra-dead".into(),
                note: "connection refused".into(),
                detail: String::new(),
                diff: String::new(),
                diffstat: String::new(),
                score: None,
                total: None,
                phase: Some("infra".into()),
                kept_snap: None,
                tiebreak: None,
                evidence: Vec::new(),
                candidate_md: String::new(),
            },
            solved: false,
        });
        let got = classify("infra", &events);
        assert!(
            matches!(
                got.classification,
                Classification::DiedBetweenIterations { last_iter: 0 }
            ),
            "{:?}",
            got.classification
        );
        assert_eq!(got.resume.next_iter, 1, "the dead iter re-runs");
    }

    // --- plan_recovery policy ---

    fn clean_exit(outcome: &str, next_iter: u32, spent: f64) -> SessionRecovery {
        SessionRecovery {
            resume: ResumeState {
                rows: Vec::new(),
                best_score: 210.0,
                baseline_score: 240.0,
                baseline_total: 0,
                spent,
                next_iter,
                solved_any: false,
                identity: None,
                best_tiebreak: None,
                published_branches: Vec::new(),
            },
            classification: Classification::CleanExit {
                outcome: ShutdownOutcome::parse(outcome),
                reason: "r".into(),
            },
            pending_approval: None,
        }
    }

    #[test]
    fn finished_and_solved_runs_are_noops_even_with_iterations_left() {
        for outcome in ["finished", "solved"] {
            match plan_recovery(&clean_exit(outcome, 3, 1.0), 10, 5.0) {
                RecoveryPlan::NoOp { message } => {
                    assert!(message.contains(outcome), "{message}");
                }
                _ => panic!("{outcome}: expected NoOp"),
            }
        }
    }

    #[test]
    fn escalated_run_refuses_to_resume() {
        match plan_recovery(&clean_exit("escalated", 3, 1.0), 10, 5.0) {
            RecoveryPlan::Refuse { message } => {
                assert!(message.contains("escalated for human review"), "{message}");
            }
            _ => panic!("expected Refuse"),
        }
    }

    #[test]
    fn budget_exit_honors_a_raised_cap() {
        // Still over the cap: no-op with the preserved guard message.
        match plan_recovery(&clean_exit("budget", 3, 5.0), 10, 5.0) {
            RecoveryPlan::NoOp { message } => {
                assert_eq!(
                    message,
                    "nothing to do (2 of 10 iterations ran, $5.00 of $5.00 spent)"
                );
            }
            _ => panic!("expected NoOp"),
        }
        // The operator raised --max-cost: the run continues.
        assert!(matches!(
            plan_recovery(&clean_exit("budget", 3, 5.0), 10, 20.0),
            RecoveryPlan::Continue { .. }
        ));
    }

    #[test]
    fn stopped_and_error_exits_continue_within_caps() {
        for outcome in ["stopped", "error", "stalled"] {
            assert!(
                matches!(
                    plan_recovery(&clean_exit(outcome, 3, 1.0), 10, 5.0),
                    RecoveryPlan::Continue { .. }
                ),
                "{outcome}"
            );
        }
    }

    #[test]
    fn arithmetic_guard_still_noops_a_torn_tail() {
        // No Shutdown (torn tail), but every iteration ran: the old guard survives.
        let s = SessionRecovery {
            resume: ResumeState {
                rows: Vec::new(),
                best_score: 210.0,
                baseline_score: 240.0,
                baseline_total: 0,
                spent: 3.5,
                next_iter: 6,
                solved_any: false,
                identity: None,
                best_tiebreak: None,
                published_branches: Vec::new(),
            },
            classification: Classification::DiedBetweenIterations { last_iter: 5 },
            pending_approval: None,
        };
        match plan_recovery(&s, 5, 10.0) {
            RecoveryPlan::NoOp { message } => {
                assert_eq!(
                    message,
                    "nothing to do (5 of 5 iterations ran, $3.50 of $10.00 spent)"
                );
            }
            _ => panic!("expected NoOp"),
        }
    }

    fn awaiting(trace: &str) -> ResumeRecovery {
        ResumeRecovery {
            class: RecoveryClass::DiedAwaitingApproval,
            iter: 3,
            detail: String::new(),
            repark: Some(provisioning::PendingProvisioning {
                mode: WaitMode::Block,
                trace_id: trace.into(),
                handle: "https://github.com/o/r/pull/7".into(),
            }),
            pending_regime: Some(trace.into()),
        }
    }

    fn replay_holding(rescope: Option<(AdmissionKey, &str)>) -> crate::admission::ResumeReplay {
        crate::admission::ResumeReplay {
            unsettled_rescope: rescope.map(|(key, regime)| (key, regime.to_string())),
            ..Default::default()
        }
    }

    #[test]
    fn a_ledger_recorded_grant_for_this_ask_suppresses_the_repark() {
        let rec = awaiting("model=Q;c=48");
        let derived = AdmissionKey::rescope_from(&AdmissionKey::approve("model=Q;c=48"));
        let action = resume_approval(&rec, Some(&replay_holding(Some((derived, "model=Q;c=48")))));
        assert!(
            action.repark.is_none(),
            "the grant landed before the death — parking would wait for nothing"
        );
        assert!(
            action.pending_regime.is_none(),
            "and the ask is closed, so a fresh approve can't grant it twice"
        );
        assert!(action.note.is_some_and(|n| n.contains("already granted")));
    }

    #[test]
    fn an_unrelated_pending_rescope_leaves_the_repark_alone() {
        let rec = awaiting("model=Q;c=48");
        let other = AdmissionKey::rescope_from(&AdmissionKey::approve("some-other-ask"));
        let action = resume_approval(&rec, Some(&replay_holding(Some((other, "c=8")))));
        assert!(action.repark.is_some());
        assert_eq!(action.pending_regime.as_deref(), Some("model=Q;c=48"));
        assert!(action.note.is_none());
    }

    #[test]
    fn with_no_ledger_or_no_grant_the_classifier_decides_alone() {
        let rec = awaiting("t");
        for replay in [None, Some(replay_holding(None))] {
            let action = resume_approval(&rec, replay.as_ref());
            assert!(action.repark.is_some());
            assert_eq!(action.pending_regime.as_deref(), Some("t"));
        }
    }
}
