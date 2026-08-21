//! Session-event front-end: emit the loop's own events either to stdout (`--ui jsonl`)
//! or to `state/session.jsonl` for external tailers (`--ui stream`).
//!
//! Same loop and keep/discard logic as the console front-end, but it writes versioned
//! [`SessionEvent`] NDJSON that downstream consumers (the controller, the session
//! converter) replay + tail to rebuild the run.
//!
//! It forwards only `Some(event)` from the run_turn sink, so a replay of the log folds
//! into identical run state.

use crate::event::AgentEvent;
use crate::reporter::{AgentTurn, Phase, Reporter, Row, RunMeta, Stop, TurnBudget};
use crate::session::{self, RowWire, SessionEvent, SessionPhase};
use crate::{Args, Paths, STOP, agent};
use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::atomic::Ordering;
use std::time::Duration;

pub struct SessionReporter {
    sink: Sink,
    control: Option<std::path::PathBuf>,
    meta: RunMeta,
}

enum Sink {
    /// `--ui jsonl`: NDJSON on stdout only.
    Stdout(std::io::Stdout),
    /// `--ui stream`: the session-log file for external tailers AND stdout so the loop's decisions are visible
    /// in `kubectl logs` (mid-run). Without the stdout copy, the only thing on the pod's stdout/stderr is
    /// the broker's eprintln, so a healthy run looks dead from the outside (`0 decided rows`).
    Tee { out: std::io::Stdout, file: File },
}

impl Sink {
    fn write_event(&mut self, ev: &SessionEvent) {
        let line = session::encode(ev);
        match self {
            Sink::Stdout(out) => {
                let _ = writeln!(out, "{line}");
                let _ = out.flush();
            }
            Sink::Tee { out, file } => {
                let _ = writeln!(file, "{line}");
                let _ = file.flush();
                let _ = writeln!(out, "{line}");
                let _ = out.flush();
            }
        }
    }
}

impl SessionReporter {
    /// Emit session events as NDJSON on stdout.
    pub fn stdout(meta: RunMeta) -> Self {
        Self {
            sink: Sink::Stdout(std::io::stdout()),
            control: None,
            meta,
        }
    }

    /// Open the session log truncating, for a fresh stream-mode run.
    pub fn stream(p: &Paths, meta: RunMeta) -> Result<Self> {
        Self::open(p, meta, true)
    }

    /// Open the session log in append mode, for a resumed run (keeps the prior log so
    /// tailers' already-replayed state stays valid).
    pub fn resume(p: &Paths, meta: RunMeta) -> Result<Self> {
        Self::open(p, meta, false)
    }

    fn open(p: &Paths, meta: RunMeta, truncate: bool) -> Result<Self> {
        if let Some(dir) = p.session_log.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating state dir {}", dir.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(truncate)
            .append(!truncate)
            .open(&p.session_log)
            .with_context(|| format!("opening session log {}", p.session_log.display()))?;
        // A new run (fresh or resumed) must not inherit a stale stop signal.
        let _ = std::fs::remove_file(&p.control);
        Ok(Self {
            // Tee: the session-log file, plus stdout so the run is observable in `kubectl logs`.
            sink: Sink::Tee {
                out: std::io::stdout(),
                file,
            },
            control: Some(p.control.clone()),
            meta,
        })
    }

    fn emit(&mut self, ev: &SessionEvent) {
        self.sink.write_event(ev);
    }
}

impl Reporter for SessionReporter {
    fn start(&mut self, goal: &str, objective: &str) {
        self.emit(&SessionEvent::Start {
            goal: goal.trim().to_string(),
            gate: objective.to_string(),
            model: self.meta.model.clone(),
            namespace: self.meta.namespace.clone(),
            iters_total: self.meta.iters_total,
            max_cost: self.meta.max_cost,
            max_secs: self.meta.max_secs,
        });
    }

    fn phase(&mut self, phase: Phase) {
        self.emit(&SessionEvent::phase(phase));
    }

    fn note(&mut self, msg: &str) {
        self.emit(&SessionEvent::Note {
            msg: msg.to_string(),
        });
    }

    fn row(&mut self, row: &Row, solved: bool) {
        self.emit(&SessionEvent::Row {
            row: RowWire::from(row),
            solved,
        });
    }

    fn run_agent(
        &mut self,
        args: &Args,
        p: &Paths,
        it: u32,
        prompt: &str,
        resume_prompt: Option<&str>,
        session: Option<&str>,
        budget: TurnBudget,
    ) -> AgentTurn {
        let prepared = match crate::agent_session::prepare_named(&p.state, session) {
            Ok(prepared) => prepared,
            Err(error) => {
                return AgentTurn {
                    cost: 0.0,
                    is_error: true,
                    error: Some(error),
                };
            }
        };
        if let Some(turn) = &prepared {
            self.emit(&SessionEvent::AgentSession {
                session: turn.logical_name.clone(),
                action: turn.action(),
                turn: turn.completed_turns + 1,
            });
        }
        self.emit(&SessionEvent::AgentStart { iter: it });
        // Borrow the sink directly so the closure can append without re-borrowing self.
        let sink = &mut self.sink;
        // The result event's is_error rides through into session.jsonl (and the
        // controller relay) via the emitted Agent event; capture it here too so the
        // loop can discard a failed turn.
        let mut is_error = false;
        let mut error = None;
        // Stop the agent at most once per turn when provisional spend crosses the cap.
        let mut over_cap_stopped = false;
        let turn = agent::run_turn_with_session(
            args,
            p,
            crate::agent_session::effective_prompt(prepared.as_ref(), prompt, resume_prompt),
            true,
            prepared.as_ref(),
            |_raw, _stream, ev| {
                if let Some(ev) = ev {
                    if let AgentEvent::Result {
                        is_error: e,
                        error: text,
                        ..
                    } = ev
                    {
                        is_error = *e;
                        error = text.clone();
                    }
                    if let AgentEvent::Error { message, .. } = ev {
                        is_error = true;
                        error = Some(message.clone());
                    }
                    if let AgentEvent::Tokens(t) = ev {
                        // Provisional mid-turn budget line; the loop's turn-end
                        // budget call reconciles it with the authoritative cost.
                        let spent =
                            budget.spent_before + crate::event::provisional_cost(&args.model, t);
                        sink.write_event(&SessionEvent::Budget {
                            spent,
                            elapsed_secs: budget.started.elapsed().as_secs(),
                        });
                        if budget.over_cap(spent) && !over_cap_stopped {
                            over_cap_stopped = true;
                            sink.write_event(&SessionEvent::Note {
                                msg: format!(
                                    "budget: provisional spend ${spent:.4} reached cap ${:.2} mid-turn — stopping the agent",
                                    budget.max_cost
                                ),
                            });
                            // Ends a local agent child; a sandboxed turn has no
                            // local pid, so there the loop's post-turn guard stops
                            // the run instead.
                            crate::pid_registry::kill_all();
                        }
                    }
                    sink.write_event(&SessionEvent::Agent { event: ev.clone() });
                }
            },
        );
        // The turn's own failure channel: a transport that never produced a turn is an error
        // even when no event carried one.
        if let Some(failure) = turn.failure() {
            is_error = true;
            error = Some(failure.to_string());
        }
        if let Some(note) =
            crate::agent_session::commit_if_ok(&p.state, prepared.as_ref(), !is_error)
        {
            is_error = true;
            error = Some(note);
        }
        self.emit(&SessionEvent::AgentDone);
        AgentTurn {
            cost: turn.cost_usd,
            is_error,
            error,
        }
    }

    fn budget(&mut self, spent: f64, elapsed: Duration) {
        self.emit(&SessionEvent::Budget {
            spent,
            elapsed_secs: elapsed.as_secs(),
        });
    }

    fn check_interrupt(&mut self, _p: &Paths, _rows: &[Row]) -> Stop {
        // Stop on either the loop's own Ctrl+C (STOP) or a cross-process stop
        // written to control.json. The loop owns the
        // agent child, so kill it on a cross-process stop in case one is somehow live.
        if STOP.load(Ordering::SeqCst) {
            return Stop::Quit;
        }
        if let Some(control) = &self.control
            && crate::control::stop_file_says_stop(control)
        {
            crate::pid_registry::kill_all();
            return Stop::Quit;
        }
        Stop::Continue
    }

    fn segment(&mut self, fingerprint: &str, baseline_score: f64, regime: &str) {
        self.emit(&SessionEvent::Segment {
            fingerprint: fingerprint.to_string(),
            baseline_score: Some(baseline_score).filter(|s| s.is_finite()),
            regime: regime.to_string(),
        });
    }

    fn escalation(&mut self, esc: &crate::escalation::Escalation) {
        self.emit(&SessionEvent::Escalation {
            category: esc.category.clone(),
            reason: esc.reason.clone(),
            evidence: esc.evidence.clone(),
        });
    }

    fn identity(&mut self, identity: &crate::identity::RunIdentity) {
        self.emit(&SessionEvent::Identity {
            identity: identity.clone(),
        });
    }

    fn plan_event(&mut self, ev: &SessionEvent) {
        self.emit(ev);
    }

    fn recovery(&mut self, class: crate::session::RecoveryClass, iter: u32, detail: &str) {
        self.emit(&SessionEvent::Recovery {
            class,
            iter,
            detail: detail.to_string(),
        });
    }

    fn approval_wait(&mut self, handle: &str, trace_id: &str, mode: crate::provisioning::WaitMode) {
        self.emit(&SessionEvent::ApprovalWait {
            handle: handle.to_string(),
            trace_id: trace_id.to_string(),
            mode: mode.as_str().to_string(),
        });
    }

    fn approval_resolved(&mut self, outcome: &str, reason: &str) {
        self.emit(&SessionEvent::ApprovalResolved {
            outcome: outcome.to_string(),
            reason: reason.to_string(),
        });
    }

    fn pr_links(&mut self, links: &[crate::session::PrLinkWire]) {
        if links.is_empty() {
            return;
        }
        self.emit(&SessionEvent::PrLinks {
            links: links.to_vec(),
        });
    }

    fn summary(&mut self, rows: &[Row], objective: &str, best_score: f64) {
        self.emit(&SessionEvent::Summary {
            rows: rows.iter().map(RowWire::from).collect(),
            gate: objective.to_string(),
            best_score: Some(best_score).filter(|s| s.is_finite()),
        });
        self.emit(&SessionEvent::Finished);
    }

    fn shutdown(&mut self, outcome: &str, reason: &str) {
        self.emit(&SessionEvent::Shutdown {
            outcome: outcome.to_string(),
            reason: reason.to_string(),
        });
    }
}
