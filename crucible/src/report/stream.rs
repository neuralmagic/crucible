//! Session-event front-end: emit the loop's own events either to stdout (`--ui jsonl`)
//! or to `state/session.jsonl` for external tailers (`--ui stream`).
//!
//! Same loop and keep/discard logic as the console front-end, but it writes versioned
//! [`SessionEvent`] NDJSON that downstream consumers (the controller, the session
//! converter) replay + tail to rebuild the run.
//!
//! It forwards only `Some(event)` from the run_turn sink, so a replay of the log folds
//! into identical run state.

use crate::args::Paths;
use crate::process::STOP;
use crate::report::reporter::{Reporter, RunMeta, Stop};
use crate::report::session::{self, Row, RowWire, SessionEvent};
use anyhow::{Context, Result};
use crucible_contract::LoopPhase;
use crucible_contract::event::AgentEvent;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct SessionReporter {
    sink: Sink,
    control: Option<std::path::PathBuf>,
    meta: RunMeta,
}

/// Buffered bytes that force the session log's file flush ahead of the cadence.
const FILE_FLUSH_BYTES: usize = 8 * 1024;
/// Longest a session event waits in the file buffer; the control bridge tailer polls no less
/// often.
const FILE_FLUSH_EVERY: Duration = Duration::from_millis(100);

enum Sink {
    /// `--ui jsonl`: NDJSON on stdout only.
    Stdout(std::io::Stdout),
    /// `--ui stream`: the session-log file for external tailers AND stdout so the loop's
    /// decisions are visible in `kubectl logs` (mid-run). Without the stdout copy, the only
    /// thing on the pod's stdout/stderr is the broker's eprintln, so a healthy run looks dead
    /// from the outside (`0 decided rows`). Both sides buffer and flush together: a pod-log
    /// viewer polls at second granularity, so the 100 ms cadence is invisible. The buffers sit
    /// behind a mutex a flusher thread shares, so a quiet stretch still reaches the file and
    /// stdout within the cadence. `--ui jsonl` keeps its line-buffered stdout.
    Tee {
        buffers: Arc<Mutex<TeeBuffers>>,
        /// Kept alive for the run; dropping it stops the flusher after one last flush.
        _flusher: std::sync::mpsc::Sender<()>,
    },
}

/// The Tee's two buffered writers plus the cadence stamp, shared between the loop (writes) and
/// the flusher thread (time-based flushes).
struct TeeBuffers {
    out: std::io::BufWriter<std::io::Stdout>,
    file: std::io::BufWriter<File>,
    flushed: std::time::Instant,
}

fn flush_tee(buffers: &mut TeeBuffers) {
    let _ = buffers.file.flush();
    let _ = buffers.out.flush();
    buffers.flushed = std::time::Instant::now();
}

impl Sink {
    fn write_event(&mut self, ev: &SessionEvent) {
        self.write_line(&session::encode(ev));
    }

    fn write_agent(&mut self, ev: &AgentEvent) {
        self.write_line(&session::encode_agent(ev));
    }

    fn write_line(&mut self, line: &str) {
        match self {
            Sink::Stdout(out) => {
                let _ = writeln!(out, "{line}");
            }
            Sink::Tee { buffers, .. } => {
                let mut buffers = buffers.lock().unwrap_or_else(|p| p.into_inner());
                let _ = writeln!(buffers.file, "{line}");
                let _ = writeln!(buffers.out, "{line}");
                if buffers.file.buffer().len() >= FILE_FLUSH_BYTES
                    || buffers.out.buffer().len() >= FILE_FLUSH_BYTES
                    || buffers.flushed.elapsed() >= FILE_FLUSH_EVERY
                {
                    flush_tee(&mut buffers);
                }
            }
        }
    }

    /// Push the session log's buffered tail to disk and stdout; `run` can end in
    /// `std::process::exit`, which skips the drop flush.
    fn flush_file(&mut self) {
        if let Sink::Tee { buffers, .. } = self {
            flush_tee(&mut buffers.lock().unwrap_or_else(|p| p.into_inner()));
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
            sink: {
                let buffers = Arc::new(Mutex::new(TeeBuffers {
                    out: std::io::BufWriter::new(std::io::stdout()),
                    file: std::io::BufWriter::new(file),
                    flushed: std::time::Instant::now(),
                }));
                let (stop, stopped) = std::sync::mpsc::channel::<()>();
                let ticking = Arc::clone(&buffers);
                std::thread::spawn(move || {
                    loop {
                        match stopped.recv_timeout(FILE_FLUSH_EVERY) {
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                                flush_tee(&mut ticking.lock().unwrap_or_else(|p| p.into_inner()));
                            }
                            _ => {
                                // The reporter is gone: one last flush, then stop.
                                flush_tee(&mut ticking.lock().unwrap_or_else(|p| p.into_inner()));
                                break;
                            }
                        }
                    }
                });
                // The signal handlers cannot reach run context, so this hook is how the
                // buffered tail survives an exit-signal's `process::exit`.
                crate::process::flush_before_exit(Box::new({
                    let buffers = Arc::clone(&buffers);
                    move || flush_tee(&mut buffers.lock().unwrap_or_else(|p| p.into_inner()))
                }));
                Sink::Tee {
                    buffers,
                    _flusher: stop,
                }
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

    fn phase(&mut self, phase: LoopPhase, iter: u32) {
        self.emit(&SessionEvent::Phase { phase, iter });
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
            && crate::control::bridge::stop_file_says_stop(control)
        {
            crucible::deadline::terminate_live_children();
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

    fn escalation(&mut self, esc: &crate::control::escalation::Escalation) {
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

    fn agent_event(&mut self, ev: &AgentEvent) {
        self.sink.write_agent(ev);
    }

    fn recovery(&mut self, class: crate::report::session::RecoveryClass, iter: u32, detail: &str) {
        self.emit(&SessionEvent::Recovery {
            class,
            iter,
            detail: detail.to_string(),
        });
    }

    fn approval_wait(
        &mut self,
        handle: &str,
        trace_id: &str,
        mode: crate::control::provisioning::WaitMode,
    ) {
        self.emit(&SessionEvent::ApprovalWait {
            handle: handle.to_string(),
            trace_id: trace_id.to_string(),
            mode: mode.as_str().to_string(),
        });
    }

    fn approval_resolved(
        &mut self,
        outcome: crate::control::provisioning::ApprovalOutcome,
        reason: &str,
    ) {
        self.emit(&SessionEvent::ApprovalResolved {
            outcome: outcome.as_str().to_string(),
            reason: reason.to_string(),
        });
    }

    fn pr_links(&mut self, links: &[crate::report::session::PrLinkWire]) {
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
        self.sink.flush_file();
    }

    fn shutdown(&mut self, outcome: &str, reason: &str) {
        self.emit(&SessionEvent::Shutdown {
            outcome: outcome.to_string(),
            reason: reason.to_string(),
        });
        self.sink.flush_file();
    }
}
