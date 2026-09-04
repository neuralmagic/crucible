//! What `plan run` does at an approval gate: park on the bridge and the controller, or
//! snapshot and exit; and what it knows on the way in: the results a previous process settled
//! and the resolutions already recorded.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crucible_contract::admission::{AdmissionKey, AdmissionOutcome, AdmittedInput};
use crucible_contract::{
    ApprovalWaits, ArtifactKind, GateDecision, GateResolution, GateWait, LoopState, ParkMode,
};

use crate::admission::AdmissionLedger;
use crate::control::ControlState;
use crate::ingest_client::{IngestConfig, fetch_pod_approvals, post_artifact};
use crate::plan::ParkPolicy;
use crate::plan::cli::GateOpts;
use crate::plan::exec::{Gate, TaskResult};
use crate::plan::ir::{TaskName, ValidPlan};
use crate::session::SessionEvent;
use crate::{Paths, STOP};

/// How often a parked run asks the controller for a resolution.
const CONTROLLER_POLL: Duration = Duration::from_secs(15);
/// How often a parked run checks the bridge, the stop flag, and the clock.
const PARK_TICK: Duration = Duration::from_millis(250);

/// How a wait at a gate ended.
pub(crate) enum Waited {
    Resolved(GateResolution),
    Suspend,
    Stopped,
}

/// How a park ended.
pub(crate) enum Parked {
    Resolved(GateResolution),
    Stopped,
    TimedOut,
}

pub(crate) struct Host {
    run_id: String,
    ledger: Option<Arc<AdmissionLedger>>,
    control: Option<Arc<ControlState>>,
    ingest: Option<IngestConfig>,
    resolutions: BTreeMap<String, GateResolution>,
    prior: BTreeMap<TaskName, TaskResult>,
}

impl Host {
    /// Open the run's gate state. With `paths` (a `--manifest` run) the admission ledger is
    /// opened, the control bridge started when a port was given, and on `--resume` the session
    /// log folded for what the previous process settled and which gate it left open.
    pub(crate) fn open(paths: Option<&Paths>, plan: &ValidPlan, gates: &GateOpts) -> Result<Host> {
        let run_id = crate::plan::gate::run_id_from_env();
        let mut resolutions: BTreeMap<String, GateResolution> =
            gates.approvals.iter().cloned().collect();
        let mut prior = BTreeMap::new();
        let mut ledger = None;
        let mut control = None;
        if let Some(paths) = paths {
            let mode = if gates.resume {
                forge::ndjson::Open::Fold
            } else {
                forge::ndjson::Open::Truncate
            };
            let opened = Arc::new(AdmissionLedger::open(&paths.admissions, mode)?);
            if gates.resume {
                let body = std::fs::read_to_string(&paths.session_log).with_context(|| {
                    format!(
                        "reading {} to resume (no session log: nothing to resume)",
                        paths.session_log.display()
                    )
                })?;
                let state = LoopState::from_lines(body.lines());
                prior = Self::prior_from(&state, plan);
                if let Some(open) = state.open_approval()
                    && !resolutions.contains_key(&open.trace_id)
                    && let Some(recorded) = opened.gate_resolution(&open.trace_id)
                {
                    resolutions.insert(open.trace_id.clone(), recorded);
                }
            }
            if let Some(port) = gates.control_port {
                control = Some(crate::control::spawn_bridge(
                    port,
                    paths.clone(),
                    opened.clone(),
                )?);
            }
            ledger = Some(opened);
        }
        Ok(Host {
            run_id,
            ledger,
            control,
            ingest: IngestConfig::from_env(),
            resolutions,
            prior,
        })
    }

    /// Every resolution the run holds on the way in.
    pub(crate) fn resolutions(&self) -> Vec<(String, GateResolution)> {
        self.resolutions
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// The results a previous process settled under this plan; empty on a fresh run.
    pub(crate) fn take_prior(&mut self) -> BTreeMap<TaskName, TaskResult> {
        std::mem::take(&mut self.prior)
    }

    fn prior_from(state: &LoopState, plan: &ValidPlan) -> BTreeMap<TaskName, TaskResult> {
        let Some(admitted) = &state.plan else {
            return BTreeMap::new();
        };
        crate::loop_graph::PriorPlan {
            iter: 0,
            plan_version: admitted.plan_version,
            declared: admitted.tasks.iter().map(|t| t.name.clone()).collect(),
            results: state.plan_results.clone(),
        }
        .seed(plan)
    }

    /// Wait at `open` under the run's park policy.
    pub(crate) fn wait(
        &mut self,
        open: &Gate,
        gates: &GateOpts,
        paths: Option<&Paths>,
    ) -> Result<Waited> {
        // Every policy idles first; what differs is what the end of the idle means.
        self.announce(open, ParkMode::Park);
        let timeout = match (gates.max_park, open.timeout_secs) {
            (Some(cap), Some(secs)) => Some(cap.min(Duration::from_secs(secs))),
            (Some(cap), None) => Some(cap),
            (None, Some(secs)) => Some(Duration::from_secs(secs)),
            (None, None) => None,
        };
        Ok(match self.park(open, timeout, paths)? {
            Parked::Resolved(r) => Waited::Resolved(r),
            Parked::Stopped if gates.policy == ParkPolicy::ParkThenSuspend => Waited::Suspend,
            Parked::Stopped => Waited::Stopped,
            Parked::TimedOut if gates.policy == ParkPolicy::ParkThenSuspend => Waited::Suspend,
            Parked::TimedOut => Waited::Resolved(GateResolution::timeout()),
        })
    }

    /// Idle until the bridge, the controller, a stop, or the clock ends the wait. A resolution
    /// from the controller is recorded on the ledger so a later resume finds it; the bridge
    /// records its own.
    pub(crate) fn park(
        &mut self,
        open: &Gate,
        timeout: Option<Duration>,
        paths: Option<&Paths>,
    ) -> Result<Parked> {
        if let Some(control) = &self.control {
            control.arm_gate(open.trace_id.clone());
        }
        eprintln!(
            "[crucible] parked on gate {} ({}): idle, awaiting approval{}",
            open.task,
            open.trace_id,
            timeout
                .map(|t| format!(" for up to {}s", t.as_secs()))
                .unwrap_or_default()
        );
        let start = Instant::now();
        let mut last_poll: Option<Instant> = None;
        loop {
            if let Some(resolution) = self.control.as_ref().and_then(|c| c.take_gate_resolution()) {
                return Ok(Parked::Resolved(resolution));
            }
            // SIGTERM is not an interrupt here (`ctrlc` handles SIGINT only), so a pod deletion
            // reaches a parked run as the stop file its preStop hook writes.
            if STOP.load(Ordering::SeqCst)
                || paths.is_some_and(|p| crate::control::stop_file_says_stop(&p.control))
            {
                return Ok(Parked::Stopped);
            }
            if timeout.is_some_and(|cap| start.elapsed() >= cap) {
                return Ok(Parked::TimedOut);
            }
            let due = last_poll.is_none_or(|t| t.elapsed() >= CONTROLLER_POLL);
            if due && let Some(cfg) = &self.ingest {
                last_poll = Some(Instant::now());
                if let Some(found) = fetch_pod_approvals(cfg)
                    .into_iter()
                    .find(|a| a.trace_id == open.trace_id)
                {
                    self.record(&open.trace_id, &found.resolution);
                    return Ok(Parked::Resolved(found.resolution));
                }
            }
            std::thread::sleep(PARK_TICK);
        }
    }

    /// Tell the controller which gates this run is parked on. Best effort: a run with no
    /// drop-box (a laptop) has nobody to tell.
    fn announce(&self, open: &Gate, mode: ParkMode) {
        let Some(cfg) = &self.ingest else {
            return;
        };
        let waits = ApprovalWaits {
            v: ApprovalWaits::VERSION,
            run_id: self.run_id.clone(),
            control_port: None,
            waits: vec![GateWait {
                trace_id: open.trace_id.clone(),
                handle: open.handle.clone(),
                task: open.task.0.clone(),
                source: open.source.clone(),
                summary: open.summary.clone(),
                mode,
                requested_at: crate::suspend::now_secs(),
            }],
        };
        let Ok(json) = serde_json::to_vec(&waits) else {
            return;
        };
        let Ok(gz) = Self::gzip(&json) else {
            return;
        };
        post_artifact(cfg, ArtifactKind::ApprovalWaits, &gz);
    }

    fn gzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
        use std::io::Write as _;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(bytes)?;
        enc.finish()
    }

    /// Record a resolution that arrived from outside the bridge, under the gate's key.
    fn record(&self, trace_id: &str, resolution: &GateResolution) {
        let Some(ledger) = &self.ledger else {
            return;
        };
        let input = match resolution.decision {
            GateDecision::Granted => AdmittedInput::Approve,
            GateDecision::Denied => AdmittedInput::Deny {
                reason: resolution.reason_text(),
            },
        };
        let key = AdmissionKey::approve(trace_id);
        if let Ok(crate::admission::Admitted::Fresh(key)) = ledger.admit(Some(key), input) {
            let _ = ledger.settle(
                &key,
                AdmissionOutcome::Applied,
                &format!(
                    "gate {} via {}",
                    resolution.decision.as_str(),
                    resolution.source.as_deref().unwrap_or("controller")
                ),
            );
        }
    }
}

impl Host {
    /// Snapshot the run at an open gate and deliver it for resumption.
    pub(crate) fn suspend(&self, paths: &Paths, open: &Gate) -> Result<()> {
        let record = crate::suspend::ResumeRecord {
            v: crate::suspend::ResumeRecord::VERSION,
            run_id: crate::plan::gate::run_id_from_env(),
            gate: open.trace_id.clone(),
            head: crate::suspend::head_of(&paths.workspace),
            suspended_at: crate::suspend::now_secs(),
        };
        let gz = crate::suspend::snapshot(&paths.state, &paths.workspace, &record)?;
        let delivered = crate::suspend::deliver(&paths.session_log, &gz)?;
        eprintln!(
            "[crucible] suspended at gate {} ({}){}",
            open.task,
            open.trace_id,
            if delivered {
                ": snapshot delivered to the drop-box"
            } else {
                ": snapshot kept in the state dir"
            }
        );
        Ok(())
    }
}

impl Host {
    pub(crate) fn wait_event(&self, open: &Gate) -> SessionEvent {
        SessionEvent::ApprovalWait {
            handle: open.handle.clone(),
            trace_id: open.trace_id.clone(),
            mode: "block".to_string(),
            task: Some(open.task.0.clone()),
            source: serde_json::to_value(&open.source).ok(),
            park: Some(ParkMode::Park.as_str().to_string()),
        }
    }

    pub(crate) fn resolved_event(&self, open: &Gate, resolution: &GateResolution) -> SessionEvent {
        SessionEvent::ApprovalResolved {
            outcome: match resolution.source.as_deref() {
                Some("timeout") => "timeout".to_string(),
                _ => resolution.decision.as_str().to_string(),
            },
            reason: resolution.reason_text(),
            trace_id: open.trace_id.clone(),
            by: resolution.by.as_ref().map(|by| by.as_str().to_string()),
            source: resolution.source.clone(),
        }
    }
}
