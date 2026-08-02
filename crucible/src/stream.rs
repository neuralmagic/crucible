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
use crate::reporter::{AgentTurn, Phase, Reporter, Row, RunMeta, Stop};
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
        let cost = agent::run_turn_with_session(
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
                    sink.write_event(&SessionEvent::Agent { event: ev.clone() });
                }
            },
        );
        if let Some(note) =
            crate::agent_session::commit_if_ok(&p.state, prepared.as_ref(), !is_error)
        {
            is_error = true;
            error = Some(note);
        }
        self.emit(&SessionEvent::AgentDone);
        AgentTurn {
            cost,
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
            baseline_score,
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
            best_score,
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
