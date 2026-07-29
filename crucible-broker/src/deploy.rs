//! Async composite deploy + live status polling. `deploy_composite` returns a `deploy_id`
//! immediately, runs the domain apply hook on a background thread, and the agent polls
//! `deploy_status(deploy_id)`, long-polling (`wait_seconds > 0`, condvar-backed) so a waiting
//! agent spends one call per phase transition, and the raw log tail is opt-in (`include_log`).
//!
//! ```text
//!   agent --deploy_composite--> broker: sync sandbox tree (verified; the agent's turn is blocked
//!                                       on this call, and the hash check catches its background
//!                                       processes) -> spawn run_hook(thread) -> {building, deploy_id}
//!   agent --deploy_status(id, wait_seconds)-> broker: {phase, done, result?, log?}  (until done)
//! ```
//!
//! The hook's exit code is the authoritative terminal verdict (3 = free compile/build error,
//! 0 = deployed/candidate-spent, else = a live-deploy failure); the stdout markers it prints only
//! drive the live `phase` hint.

use crate::build::{self, BuildReply};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// The one-line protocol between a domain apply hook and this generic broker: a stdout line
/// `crucible-phase: <label>` sets the deploy's live phase to `<label>`. The broker stays
/// domain-agnostic (it never knows what stages a domain has; the hook
/// names them). The only phases the broker itself owns are the lifecycle endpoints below, set from the
/// hook's exit code so `done` is authoritative regardless of what labels the hook emitted.
const PHASE_MARKER: &str = "crucible-phase:";
/// Before the hook emits its first marker.
const PHASE_STARTING: &str = "starting";
/// Terminal, set by the broker on exit 0.
const PHASE_DEPLOYED: &str = "deployed";
/// Terminal, set by the broker on any non-zero exit.
const PHASE_FAILED: &str = "failed";

/// Longest a single `deploy_status` long-poll may block. Keeps a blocked call well inside the MCP
/// client's tool-call timeout; the agent just calls again.
const MAX_WAIT: Duration = Duration::from_secs(120);

/// A deploy's live progress: the phase (a hook-defined label) and, once the hook exits, the terminal
/// reply. One mutex for both so the paired [`Condvar`] can watch every transition.
struct Progress {
    phase: String,
    /// `Some` once the hook has exited (the deploy is terminal); the BuildReply the agent gets.
    outcome: Option<BuildReply>,
}

/// One in-flight or completed deploy: the progress + its change signal, and the accumulated log
/// lines. Behind mutexes so the runner thread writes while a poll reads; `changed` is notified on
/// every phase transition and on finish, waking long-polling `deploy_status` calls.
struct DeployState {
    progress: Mutex<Progress>,
    changed: Condvar,
    log: Mutex<Vec<String>>,
}

impl DeployState {
    fn new() -> Self {
        Self {
            progress: Mutex::new(Progress {
                phase: PHASE_STARTING.to_string(),
                outcome: None,
            }),
            changed: Condvar::new(),
            log: Mutex::new(Vec::new()),
        }
    }
}

/// Tracks the run's composite deploys by id. Lives on the MCP server (no global state); the
/// `deploy_composite`/`deploy_status` tools share the one instance.
#[derive(Default)]
pub struct DeployRegistry {
    next: AtomicU64,
    inner: Mutex<HashMap<String, Arc<DeployState>>>,
}

/// Recover a poisoned lock's guard instead of panicking; a runner-thread panic must not take down
/// every later poll (and the broker forbids panics off the test path).
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl DeployRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The id of any non-terminal deploy (single-flight: one composite deploy at a time; concurrent
    /// rolls would fight over the same live deployment).
    fn in_flight(&self) -> Option<String> {
        lock(&self.inner)
            .iter()
            .find(|(_, s)| lock(&s.progress).outcome.is_none())
            .map(|(id, _)| id.clone())
    }

    fn register(&self) -> (String, Arc<DeployState>) {
        let n = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let id = format!("dep-{n}");
        let st = Arc::new(DeployState::new());
        lock(&self.inner).insert(id.clone(), st.clone());
        (id, st)
    }

    fn get(&self, id: &str) -> Option<Arc<DeployState>> {
        lock(&self.inner).get(id).cloned()
    }

    /// Test-only: stage an empty deploy and return its id (so a sibling module can assert the registry
    /// is shared without standing up the real sandbox-sync + hook path).
    #[cfg(test)]
    pub(crate) fn debug_register(&self) -> String {
        self.register().0
    }
}

/// `deploy_composite`: kick the domain apply hook on a background thread and return a `deploy_id`
/// IMMEDIATELY. The hook owns ALL the domain work (build the changed component(s), roll the deployment,
/// verify its invariant, and emit `crucible-phase:` markers) so crucible-broker stays domain-agnostic; we
/// only mediate: pull the tree, spawn the runner, account the candidate on completion.
pub(crate) fn deploy_composite(reg: &DeployRegistry) -> String {
    if !build::composite_enabled() {
        return build::json(&BuildReply::Disabled {
            reason: "composite deploy not enabled for this run (BROKER_COMPOSITE unset)".into(),
        });
    }
    // Budget peek (free): a build/compile error costs nothing; only a completed live deploy spends a
    // candidate, accounted by the runner thread on exit 0/other.
    if let crate::turn::Budget::Spent { reason } = crate::turn::check(false) {
        return build::json(&BuildReply::WrapUp { reason });
    }
    // Single-flight: if a deploy is already running, hand the agent its id to poll rather than start a
    // racing roll on the shared deployment.
    if let Some(id) = reg.in_flight() {
        return build::json(&BuildReply::Building { deploy_id: id });
    }
    // Pull the agent's current composite tree out of the live sandbox FIRST (synchronous; the
    // agent's turn is blocked on this call, and the verified sync catches its background writers);
    // doing it on the background thread would race the agent's next edits. Then hand the staged
    // tree to the runner.
    let ctx = build::composite_ctx_dir();
    let workdir = match build::sandbox_workdir() {
        Ok(w) => w,
        Err(e) => {
            return build::json(&BuildReply::Error {
                error: format!("{e:#}"),
            });
        }
    };
    if let Err(e) = build::sync_sandbox(&workdir, &ctx) {
        return build::json(&BuildReply::Error {
            error: format!("{e:#}"),
        });
    }
    let (id, st) = reg.register();
    let cmd = build::composite_apply_cmd();
    eprintln!("==> composite apply [{id}]: {cmd} (cwd {})", ctx.display());
    std::thread::spawn(move || run_hook(ctx, cmd, st));
    build::json(&BuildReply::Building { deploy_id: id })
}

/// What a `deploy_status` poll reports: the live `phase` and `done` always, the terminal `result`
/// once finished, and the new raw log lines since `since` only when `include_log` was set.
#[derive(Serialize)]
struct StatusReply {
    deploy_id: String,
    /// The hook's current stage label (or a broker lifecycle endpoint: `starting`/`deployed`/`failed`).
    phase: String,
    done: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<BuildReply>,
    /// Log lines emitted since the `since` offset (the scannable scrollback); opt-in.
    #[serde(skip_serializing_if = "Option::is_none")]
    log: Option<Vec<String>>,
    /// Pass this back as `since` next poll to get only the lines printed since this one.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_offset: Option<usize>,
}

/// `deploy_status`: scan a deploy's live progress. One poll = one frame; loop until `done`.
/// `wait_seconds > 0` long-polls: block (clamped to [`MAX_WAIT`]) until the phase changes or the
/// deploy finishes, so a waiting agent spends one call per transition instead of one per frame.
pub(crate) fn deploy_status(
    reg: &DeployRegistry,
    id: &str,
    include_log: bool,
    since: usize,
    wait_seconds: u64,
) -> String {
    let Some(st) = reg.get(id) else {
        return format!(
            r#"{{"status":"unknown","error":"no deploy {id} (never started, or from an earlier run)"}}"#
        );
    };
    let wait = Duration::from_secs(wait_seconds).min(MAX_WAIT);
    let (phase, outcome) = wait_for_change(&st, wait);
    let done = outcome.is_some();
    let (log, next_offset) = if include_log {
        let lines = lock(&st.log);
        let from = since.min(lines.len());
        (Some(lines[from..].to_vec()), Some(lines.len()))
    } else {
        (None, None)
    };
    let reply = StatusReply {
        deploy_id: id.to_string(),
        phase,
        done,
        result: outcome,
        log,
        next_offset,
    };
    serde_json::to_string(&reply)
        .unwrap_or_else(|e| format!(r#"{{"status":"error","error":"{e}"}}"#))
}

/// The long-poll: return the progress as soon as it's terminal, its phase differs from the phase at
/// entry, or `wait` elapses (immediately when `wait` is zero). Condvar-woken by [`set_phase`] /
/// [`finish`], so a blocked call costs nothing while nothing happens.
fn wait_for_change(st: &DeployState, wait: Duration) -> (String, Option<BuildReply>) {
    let deadline = Instant::now() + wait;
    let mut prog = lock(&st.progress);
    let entry_phase = prog.phase.clone();
    while prog.outcome.is_none() && prog.phase == entry_phase {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        // Same poison policy as `lock`: recover the guard, don't take down later polls.
        prog = match st.changed.wait_timeout(prog, remaining) {
            Ok((g, _)) => g,
            Err(e) => e.into_inner().0,
        };
    }
    (prog.phase.clone(), prog.outcome.clone())
}

/// Set the live phase (a hook-emitted label) and wake long-polling `deploy_status` calls.
fn set_phase(st: &DeployState, label: String) {
    lock(&st.progress).phase = label;
    st.changed.notify_all();
}

/// Run the apply hook to completion, streaming its merged stdout/stderr into the shared state line by
/// line (so a poll sees progress as it happens) and recording the terminal reply from the exit code.
fn run_hook(ctx: PathBuf, cmd: String, st: Arc<DeployState>) {
    // Merge stderr into stdout (`2>&1`) for ONE ordered live log; the hook prints FAIL/build errors to
    // stderr, and an interleaved transcript is what the agent wants to scan.
    let spawn = Command::new("bash")
        .arg("-c")
        .arg(format!("{cmd} 2>&1"))
        .current_dir(&ctx)
        .stdout(Stdio::piped())
        .spawn();
    let mut child = match spawn {
        Ok(c) => c,
        Err(e) => {
            finish(
                &st,
                PHASE_FAILED,
                BuildReply::Error {
                    error: format!("running composite apply `{cmd}`: {e}"),
                },
            );
            return;
        }
    };
    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if let Some(label) = parse_phase(&line) {
                set_phase(&st, label);
            }
            lock(&st.log).push(line);
        }
    }
    let code = child.wait().ok().and_then(|s| s.code());
    let log = lock(&st.log);
    let joined = log.join("\n");
    // Same exit-code contract the blocking version had. 3 = free compile/build error; 0 = deployed
    // (spend a candidate); anything else = a real live-deploy failure (spend a candidate, the
    // deployment moved).
    let (phase, reply) = match code {
        Some(3) => (
            PHASE_FAILED,
            BuildReply::CompileError {
                log: build::log_tail(joined.as_bytes(), b""),
            },
        ),
        Some(0) => {
            let _ = crate::turn::check(true);
            (
                PHASE_DEPLOYED,
                BuildReply::Deployed {
                    image_ref: build::last_line(joined.as_bytes()),
                },
            )
        }
        other => {
            let _ = crate::turn::check(true);
            (
                PHASE_FAILED,
                BuildReply::Error {
                    error: format!(
                        "composite apply failed (exit {other:?}): {}",
                        build::log_tail(joined.as_bytes(), b"")
                    ),
                },
            )
        }
    };
    drop(log);
    finish(&st, phase, reply);
}

/// Extract a hook-emitted phase label from a `crucible-phase: <label>` line, or `None` for any other
/// line. The broker doesn't know or care what the labels mean; the domain hook owns the stage names.
fn parse_phase(line: &str) -> Option<String> {
    line.trim_start()
        .strip_prefix(PHASE_MARKER)
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
}

/// Record the terminal phase + reply and wake long-polling `deploy_status` calls.
fn finish(st: &DeployState, phase: &str, outcome: BuildReply) {
    {
        let mut prog = lock(&st.progress);
        prog.phase = phase.to_string();
        prog.outcome = Some(outcome);
    }
    st.changed.notify_all();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composite_deploy_is_disabled_without_its_own_gate() {
        // BROKER_COMPOSITE is unset in the test process => the tool short-circuits to `disabled` and
        // never touches the sandbox or spawns a runner.
        let reg = DeployRegistry::new();
        assert!(deploy_composite(&reg).contains(r#""status":"disabled""#));
    }

    #[test]
    fn status_of_an_unknown_deploy_is_reported_not_panicked() {
        let reg = DeployRegistry::new();
        let out = deploy_status(&reg, "dep-404", false, 0, 0);
        assert!(out.contains(r#""status":"unknown""#));
    }

    /// The broker reads ONLY the generic `crucible-phase:` sentinel; the label is whatever the (domain)
    /// hook emitted, opaque to the broker. Ordinary log chatter never moves the phase.
    #[test]
    fn parse_phase_reads_the_generic_sentinel_only() {
        assert_eq!(
            parse_phase("crucible-phase: building-epp").as_deref(),
            Some("building-epp")
        );
        assert_eq!(
            parse_phase("  crucible-phase:   rolling  ").as_deref(),
            Some("rolling")
        );
        // A domain we know nothing about: the label passes through verbatim.
        assert_eq!(
            parse_phase("crucible-phase: warming-tpu-cache").as_deref(),
            Some("warming-tpu-cache")
        );
        // Human stage echoes are NOT the protocol; they only land in the scannable log.
        assert_eq!(parse_phase("apply: building EPP via forge"), None);
        assert_eq!(parse_phase("crucible-phase:"), None);
        assert_eq!(parse_phase("some unrelated chatter"), None);
    }

    /// A completed deploy reports done + its terminal result, and an opt-in log tail honors `since`.
    #[test]
    fn status_surfaces_phase_done_result_and_optin_log() {
        let reg = DeployRegistry::new();
        let (id, st) = reg.register();
        {
            let mut log = lock(&st.log);
            log.push("crucible-phase: building-epp".into());
            log.push("DEPLOYED epp=x vllm=y".into());
        }
        finish(
            &st,
            PHASE_DEPLOYED,
            BuildReply::Deployed {
                image_ref: "DEPLOYED epp=x vllm=y".into(),
            },
        );

        // Cheap poll: phase + done, no log.
        let bare = deploy_status(&reg, &id, false, 0, 0);
        assert!(bare.contains(r#""phase":"deployed""#));
        assert!(bare.contains(r#""done":true"#));
        assert!(bare.contains(r#""status":"deployed""#));
        assert!(!bare.contains(r#""log""#));

        // Opt-in log from offset 1 yields only the second line + a next_offset of 2.
        let tail = deploy_status(&reg, &id, true, 1, 0);
        assert!(tail.contains("DEPLOYED epp=x vllm=y"));
        assert!(!tail.contains("building-epp"));
        assert!(tail.contains(r#""next_offset":2"#));
    }

    /// A long-poll on a finished deploy returns immediately; there is nothing left to wait for.
    #[test]
    fn long_poll_on_a_done_deploy_returns_immediately() {
        let reg = DeployRegistry::new();
        let (id, st) = reg.register();
        finish(
            &st,
            PHASE_DEPLOYED,
            BuildReply::Deployed {
                image_ref: "img".into(),
            },
        );
        let start = Instant::now();
        let out = deploy_status(&reg, &id, false, 0, 60);
        assert!(out.contains(r#""done":true"#));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "must not sit out the wait: {:?}",
            start.elapsed()
        );
    }

    /// A long-poll blocked on a live deploy wakes on the next phase transition (a real cross-thread
    /// condvar wake, no mock): the runner thread flips the phase 50ms in, the poll returns with it.
    #[test]
    fn long_poll_wakes_on_a_phase_transition() {
        let reg = DeployRegistry::new();
        let (id, st) = reg.register();
        let runner = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            set_phase(&st, "rolling".into());
        });
        let start = Instant::now();
        let out = deploy_status(&reg, &id, false, 0, 60);
        assert!(out.contains(r#""phase":"rolling""#), "{out}");
        assert!(out.contains(r#""done":false"#));
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "woke on the transition, not the timeout: {:?}",
            start.elapsed()
        );
        runner.join().unwrap();
    }

    /// A long-poll with no transition returns at its deadline with the unchanged frame.
    #[test]
    fn long_poll_times_out_with_the_current_frame() {
        let reg = DeployRegistry::new();
        let (id, _st) = reg.register();
        let start = Instant::now();
        let out = deploy_status(&reg, &id, false, 0, 1);
        assert!(out.contains(r#""phase":"starting""#));
        assert!(out.contains(r#""done":false"#));
        assert!(start.elapsed() >= Duration::from_secs(1));
    }
}
