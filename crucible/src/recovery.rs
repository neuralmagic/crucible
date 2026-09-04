//! Recovery classification: the session log, folded once by the contract, names how a dead run
//! ended. Reads only the durable log (marker files are consume-on-read and gone by park time).
//! Facts come from [`crucible_contract::LoopState`]; policy lives in [`plan_recovery`].

use crate::loop_driver::ResumeState;
use crate::provisioning::{self, WaitMode};
use crate::session::RecoveryClass;
use anyhow::{Context, Result};
use crucible_contract::LoopState;
use crucible_contract::admission::AdmissionKey;
pub(crate) use crucible_contract::{Classification, OpenApproval, ShutdownOutcome};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
#[error("session log {} has no rows to resume from ({class})", .path.display())]
struct EmptySessionLog {
    path: std::path::PathBuf,
    class: String,
}

/// Everything `--resume` needs from the log, produced in one fold.
#[derive(Debug)]
pub(crate) struct SessionRecovery {
    pub resume: ResumeState,
    pub classification: Classification,
    /// Dangling approval regardless of class, for re-registration.
    pub pending_approval: Option<OpenApproval>,
    /// The plan tasks settled in the iteration the run died in, when it died inside a plan.
    pub prior_plan: Option<crate::loop_graph::PriorPlan>,
}

/// Replay the session log into resume counters and a tail classification. Torn final
/// lines are skipped; a rowless log is a refusal (`--resume` means continue, not restart).
pub(crate) fn classify_session(session_log: &Path) -> Result<SessionRecovery> {
    let body = std::fs::read_to_string(session_log).with_context(|| {
        format!(
            "reading session log {} to resume (run with --ui stream first?)",
            session_log.display()
        )
    })?;
    let state = LoopState::from_lines(body.lines());
    let classification = state.classify();
    if !state.has_rows() {
        return Err(EmptySessionLog {
            path: session_log.to_path_buf(),
            class: classification.class().to_string(),
        }
        .into());
    }
    let prior_plan = match (&classification, &state.plan) {
        (Classification::DiedInPlanTask { iter, .. }, Some(plan)) => {
            Some(crate::loop_graph::PriorPlan {
                iter: *iter,
                plan_version: plan.plan_version,
                declared: plan.tasks.iter().map(|t| t.name.clone()).collect(),
                results: state.plan_results.clone(),
            })
        }
        _ => None,
    };
    Ok(SessionRecovery {
        resume: ResumeState::from_view(state.resume_view()),
        pending_approval: state.open_approval().cloned(),
        classification,
        prior_plan,
    })
}

/// What `--resume` should do, derived from the classification.
pub(crate) enum RecoveryPlan {
    /// Exit 0 without entering the loop or re-running the finish path.
    NoOp { message: String },
    /// Nonzero exit: escalated runs need a human, not a re-run.
    Refuse { message: String },
    /// Enter the loop. `repark` re-arms the block-mode approval wait; `pending_regime`
    /// re-registers the approval key on the control bridge.
    Continue {
        repark: Option<provisioning::PendingProvisioning>,
        pending_regime: Option<String>,
    },
}

/// Classification hand-off from `run.rs` into the loop.
pub(crate) struct ResumeRecovery {
    pub class: RecoveryClass,
    pub iter: u32,
    pub detail: String,
    pub repark: Option<provisioning::PendingProvisioning>,
    pub pending_regime: Option<String>,
}

/// Map a classification to the resume action; all policy lives here.
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
            // Budget falls through: the operator may resume with a raised cap. Suspended
            // falls through with its approval still open, so the wait re-arms below.
            ShutdownOutcome::Budget
            | ShutdownOutcome::Stopped
            | ShutdownOutcome::Suspended
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
            .filter(|a| a.mode == crucible_contract::WaitMode::Block)
            .map(|a| provisioning::PendingProvisioning {
                mode: WaitMode::Block,
                trace_id: a.trace_id.clone(),
                handle: a.handle.clone(),
            }),
        pending_regime: s.pending_approval.as_ref().map(|a| a.trace_id.clone()),
    }
}

/// What a resume does about an approval the previous process left outstanding.
pub(crate) struct ResumeApproval {
    /// Re-arm the block-mode park, unless the grant is already on the ledger.
    pub repark: Option<provisioning::PendingProvisioning>,
    /// Re-register the approval key so an operator `approve` still resolves it.
    pub pending_regime: Option<String>,
    /// Session-log line when the ledger overrode the log's wait-state.
    pub note: Option<String>,
}

/// The ledger wins over the log: a re-scope recorded under this ask's derived key means
/// the grant landed before the death, so drop the park and let the drain apply it.
pub(crate) fn resume_approval(
    rec: &ResumeRecovery,
    replay: Option<&crate::admission::ResumeReplay>,
) -> ResumeApproval {
    // A distress suspend borrows the approval bracket, but nothing will ever send a rescope for
    // it: the grant is the operator clearing the marker (resume in place) or re-rolling the pod
    // (the marker lives on the forge-storage emptyDir, so the new pod starts clean). Re-parking
    // here would wait on a signal with no sender.
    if rec
        .repark
        .as_ref()
        .is_some_and(|pp| pp.handle == crate::distress::HANDLE)
    {
        return ResumeApproval {
            repark: None,
            pending_regime: None,
            note: Some(
                "resumed after a distress suspend: the re-roll is the grant, not re-parking"
                    .to_string(),
            ),
        };
    }
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
    use crate::event::AgentEvent;
    use crate::session::{PlanTaskWire, RowWire, SessionEvent, encode};
    use crucible_contract::WaitMode as WireMode;

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
                    join: String::new(),
                    stage: String::new(),
                    over: String::new(),
                    max_fanout: 0,
                },
                PlanTaskWire {
                    name: "measure".into(),
                    kind: "command".into(),
                    depends_on: vec!["propose".into()],
                    session: String::new(),
                    needs: String::new(),
                    required: true,
                    join: String::new(),
                    stage: String::new(),
                    over: String::new(),
                    max_fanout: 0,
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
            }
            other => panic!("expected DiedInPlanTask, got {other:?}"),
        }
    }

    /// A death inside a plan hands the resume the tasks that settled under it, so the resumed
    /// iteration can start from them.
    #[test]
    fn a_death_inside_a_plan_carries_the_settled_tasks_for_the_resume() {
        let mut events = prefix();
        events.push(SessionEvent::PlanAdmitted {
            plan_version: 1,
            reason: String::new(),
            budget_usd: 1.0,
            tasks: ["review", "lint"]
                .iter()
                .map(|n| PlanTaskWire {
                    name: (*n).into(),
                    kind: "command".into(),
                    depends_on: vec![],
                    session: String::new(),
                    needs: "any".into(),
                    required: true,
                    join: "all".into(),
                    stage: "iteration".into(),
                    over: String::new(),
                    max_fanout: 0,
                })
                .collect(),
        });
        events.push(SessionEvent::TaskResult {
            task: "review".into(),
            status: "pass".into(),
            plan_version: 1,
            task_kind: "command".into(),
            iter: 1,
            digest: String::new(),
            job: String::new(),
            attempts: 1,
            cost_usd: 0.3,
            metric: None,
            output: Some(serde_json::json!({"n": 1})),
            note: String::new(),
            secs: 1.0,
            trace_id: String::new(),
            span_id: String::new(),
        });
        let got = classify("plan-prior", &events);
        assert!(matches!(
            got.classification,
            Classification::DiedInPlanTask { iter: 1, .. }
        ));
        let prior = got
            .prior_plan
            .expect("a plan death carries its settled tasks");
        assert_eq!(prior.iter, 1);
        assert_eq!(prior.plan_version, 1);
        assert_eq!(
            prior.declared,
            vec!["review".to_string(), "lint".to_string()]
        );
        assert_eq!(prior.results.len(), 1);
        assert_eq!(prior.results["review"].status, "pass");

        let mut events = prefix();
        events.push(row(1, "keep", 210.0));
        let got = classify("no-plan", &events);
        assert!(got.prior_plan.is_none(), "no plan death, nothing to carry");
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

    /// A plan admitted before any iteration phase (a log from before the wide tournament was
    /// removed) is an open plan like any other.
    #[test]
    fn pre_iteration_plan_classifies_died_in_plan_task() {
        let mut events = vec![row(0, "baseline", 240.0)];
        events.push(SessionEvent::PlanAdmitted {
            plan_version: 1,
            reason: String::new(),
            budget_usd: 1.0,
            tasks: vec![PlanTaskWire {
                name: "propose-0".into(),
                kind: "agent".into(),
                depends_on: vec![],
                session: String::new(),
                needs: "any".into(),
                required: true,
                join: "all".into(),
                stage: "iteration".into(),
                over: String::new(),
                max_fanout: 0,
            }],
        });
        let got = classify("pre-iteration-plan", &events);
        match &got.classification {
            Classification::DiedInPlanTask { iter, plan } => {
                assert_eq!(*iter, 0, "no iteration phase was seen");
                assert_eq!(plan.declared, vec!["propose-0".to_string()]);
            }
            other => panic!("expected DiedInPlanTask, got {other:?}"),
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
            task: None,
            source: None,
            park: None,
        });
        let got = classify("awaiting", &events);
        match &got.classification {
            Classification::DiedAwaitingApproval { approval } => {
                assert_eq!(approval.handle, "https://example.com/pr/7");
                assert_eq!(approval.mode, WireMode::Block);
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
            task: None,
            source: None,
            park: None,
        });
        events.push(SessionEvent::ApprovalResolved {
            outcome: "denied".into(),
            reason: "policy".into(),
            trace_id: String::new(),
            by: None,
            source: None,
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
            task: None,
            source: None,
            park: None,
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
        assert_eq!(pending.mode, WireMode::Continue);
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
            task: None,
            source: None,
            park: None,
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
            task: None,
            source: None,
            park: None,
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
                dead_turns: 0,
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
            prior_plan: None,
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
                dead_turns: 0,
                solved_any: false,
                identity: None,
                best_tiebreak: None,
                published_branches: Vec::new(),
            },
            classification: Classification::DiedBetweenIterations { last_iter: 5 },
            pending_approval: None,
            prior_plan: None,
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
    fn a_distress_wait_never_reparks() {
        // The distress bracket has no sender: no rescope is coming, and the marker died with the
        // old pod's emptyDir. Re-parking would idle the fresh pod until --max-park.
        let rec = ResumeRecovery {
            class: RecoveryClass::DiedAwaitingApproval,
            iter: 4,
            detail: String::new(),
            repark: Some(provisioning::PendingProvisioning {
                mode: WaitMode::Block,
                trace_id: crate::distress::HANDLE.into(),
                handle: crate::distress::HANDLE.into(),
            }),
            pending_regime: Some(crate::distress::HANDLE.into()),
        };
        let action = resume_approval(&rec, None);
        assert!(action.repark.is_none(), "no signal will ever arrive");
        assert!(
            action.pending_regime.is_none(),
            "distress is not a regime ask"
        );
        assert!(
            action
                .note
                .is_some_and(|n| n.contains("re-roll is the grant")),
            "the log says why it did not park"
        );
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
