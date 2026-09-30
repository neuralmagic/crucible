//! Resuming a playbook from its session log: what the interrupted run settled, what it spent, and
//! how long it had been running (RFC-0002:C-PLAYBOOK-RESUME).

use std::path::{Path, PathBuf};
use std::time::Duration;

use crucible_contract::session::SessionEvent;
use serde::{Deserialize, Serialize};

use crate::plan::exec::{
    FanoutSummary, Prior, TaskResult, TaskStatus, UnknownTaskStatus, row_task,
};
use crate::plan::ir::{TaskName, ValidPlan};
use crate::plan::machine::BlockedReason;

const RUN_START_FILE: &str = "playbook-run.json";

/// What a playbook run records before it dispatches anything, so a resumed process knows which run
/// it is continuing, when that run began, which commit its pristine workspace is, and which graph
/// it was running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunStart {
    pub run: String,
    pub started_at: jiff::Timestamp,
    pub base: String,
    pub plan: String,
}

/// The compiled graph's content digest. Parameter values and the cost ceiling are bound into
/// it, so a relaunch with different ones digests differently.
pub fn plan_digest(plan: &ValidPlan) -> String {
    crucible_contract::artifact::content_digest(
        serde_json::to_string(plan.plan())
            .unwrap_or_default()
            .as_bytes(),
    )
}

#[derive(Debug, thiserror::Error)]
pub enum ResumeError {
    #[error(
        "nothing to resume: {} is missing, so this run never started (run without --resume)",
        .0.display()
    )]
    NeverStarted(PathBuf),
    #[error("reading {}: {source}", .path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("writing {}: {source}", .path.display())]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{} is not a run start record: {source}", .path.display())]
    BadRunStart {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error(
        "this state belongs to run {then:?}, not {now:?}; two runs were given the same state \
         directory"
    )]
    OtherRun { then: String, now: String },
    #[error(
        "the graph compiled for this resume ({now}) is not the one the interrupted run started \
         ({then}); the pack, its parameters, or the cost ceiling changed, so this is a different run"
    )]
    PlanChanged { then: String, now: String },
    #[error("the session log settles {task:?} before admitting a graph")]
    NotAdmitted { task: String },
    #[error("the session log settles {task:?}, which this plan cannot produce")]
    UnknownTask { task: String },
    #[error("the session log settles {task:?} with an unknown status: {source}")]
    BadStatus {
        task: String,
        source: UnknownTaskStatus,
    },
}

impl RunStart {
    pub fn begin(run: &str, base: String, plan: &ValidPlan) -> Self {
        RunStart {
            run: run.to_string(),
            started_at: jiff::Timestamp::now(),
            base,
            plan: plan_digest(plan),
        }
    }

    /// Whether this process is the same run, running the same graph, as the one that wrote the
    /// record.
    pub fn continues(&self, run: &str, plan: &ValidPlan) -> Result<(), ResumeError> {
        if self.run != run {
            return Err(ResumeError::OtherRun {
                then: self.run.clone(),
                now: run.to_string(),
            });
        }
        let digest = plan_digest(plan);
        if digest != self.plan {
            return Err(ResumeError::PlanChanged {
                then: self.plan.clone(),
                now: digest,
            });
        }
        Ok(())
    }

    /// How long the run has been going at `now`, counting any time it spent down.
    pub fn elapsed(&self, now: jiff::Timestamp) -> Duration {
        now.duration_since(self.started_at)
            .try_into()
            .unwrap_or(Duration::ZERO)
    }

    pub fn path(state: &Path) -> PathBuf {
        state.join(RUN_START_FILE)
    }

    pub fn read(state: &Path) -> Result<Self, ResumeError> {
        let path = Self::path(state);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ResumeError::NeverStarted(path));
            }
            Err(source) => return Err(ResumeError::Read { path, source }),
        };
        serde_json::from_str(&text).map_err(|source| ResumeError::BadRunStart { path, source })
    }

    /// Written through a rename, so a crash leaves the previous record or this one, never half.
    pub fn write(&self, state: &Path) -> Result<(), ResumeError> {
        let path = Self::path(state);
        let staging = state.join(format!("{RUN_START_FILE}.tmp"));
        let bytes = serde_json::to_vec(self).map_err(|source| ResumeError::BadRunStart {
            path: path.clone(),
            source,
        })?;
        std::fs::write(&staging, bytes)
            .and_then(|()| std::fs::rename(&staging, &path))
            .map_err(|source| ResumeError::Write { path, source })
    }
}

/// Fold a session log into what the run settled by the time it had been going for `elapsed`.
/// Torn lines are skipped: a process killed mid-write leaves one.
pub fn fold(log: &str, plan: &ValidPlan, elapsed: Duration) -> Result<Prior, ResumeError> {
    let mut prior = Prior {
        elapsed,
        ..Prior::default()
    };
    for event in log.lines().filter_map(crucible_contract::session::decode) {
        prior.shut_down = false;
        match event {
            SessionEvent::PlanAdmitted { .. } => prior.admitted = true,
            SessionEvent::TaskResult {
                task,
                status,
                attempts,
                cost_usd,
                output,
                note,
                blocked,
                transport,
                ..
            } => {
                if !prior.admitted {
                    return Err(ResumeError::NotAdmitted { task });
                }
                let name = TaskName(task.clone());
                let row = row_task(plan, &name)
                    .ok_or_else(|| ResumeError::UnknownTask { task: task.clone() })?;
                let status =
                    status
                        .parse::<TaskStatus>()
                        .map_err(|source| ResumeError::BadStatus {
                            task: task.clone(),
                            source,
                        })?;
                let fanout = row
                    .over
                    .as_ref()
                    .and(output.clone())
                    .and_then(|output| serde_json::from_value::<FanoutSummary>(output).ok());
                let blocked = blocked.map(|b| BlockedReason::from_wire(b, &note));
                let result = TaskResult {
                    status,
                    attempts,
                    cost_usd,
                    output,
                    note: (!note.is_empty()).then_some(note),
                    fanout,
                    blocked,
                    transport,
                };
                if prior.results.insert(name.clone(), result).is_none() {
                    prior.order.push(name);
                }
            }
            SessionEvent::HistoryEntry { .. } => prior.recorded_history = true,
            SessionEvent::Shutdown { .. } => prior.shut_down = true,
            _ => {}
        }
    }
    Ok(prior)
}

/// Read and fold `state/session.jsonl`. A run killed before its first line has an empty log.
pub fn fold_log(
    session_log: &Path,
    plan: &ValidPlan,
    elapsed: Duration,
) -> Result<Prior, ResumeError> {
    let log = match std::fs::read_to_string(session_log) {
        Ok(log) => log,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(ResumeError::Read {
                path: session_log.to_path_buf(),
                source,
            });
        }
    };
    fold(&log, plan, elapsed)
}

#[cfg(test)]
mod tests {
    use crate::plan::exec::Prior;
    use crate::plan::exec::TaskStatus;
    use crate::plan::ir::{Plan, TaskName, ValidPlan};
    use crate::plan::machine::BlockedReason;
    use crate::plan::resume::{ResumeError, RunStart, fold, plan_digest};
    use std::time::Duration;

    const ADMITTED: &str =
        r#"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":[]}"#;

    fn graph(budget: f64) -> ValidPlan {
        Plan::from_toml_str(&format!(
            r#"
            version = 1
            [budget]
            usd = {budget}
            [[task]]
            name = "discover"
            kind = "command"
            command = "true"
            [[task]]
            name = "audit"
            kind = "command"
            command = "true"
            depends_on = ["discover"]
            over = {{ task = "discover", field = "targets" }}
            max_fanout = 4
            [[task]]
            name = "wrap"
            kind = "command"
            command = "true"
            depends_on = ["audit"]
            "#
        ))
        .unwrap()
        .validate()
        .unwrap()
    }

    fn row(task: &str, status: &str, cost: f64) -> String {
        format!(
            r#"{{"v":1,"kind":"task_result","task":"{task}","status":"{status}","attempts":1,"cost_usd":{cost}}}"#
        )
    }

    fn log(lines: &[&str]) -> String {
        lines.join("\n")
    }

    fn start(run: &str, plan: &ValidPlan) -> RunStart {
        RunStart {
            run: run.into(),
            started_at: "2026-09-29T12:00:00Z".parse().unwrap(),
            base: "abc".into(),
            plan: plan_digest(plan),
        }
    }

    #[test]
    fn a_run_continues_only_itself_running_the_same_graph() {
        let plan = graph(1.0);
        let started = start("crucible-run-a", &plan);
        assert!(started.continues("crucible-run-a", &plan).is_ok());
        assert!(matches!(
            started.continues("crucible-run-b", &plan),
            Err(ResumeError::OtherRun { then, now }) if then == "crucible-run-a" && now == "crucible-run-b"
        ));
        assert!(
            matches!(
                started.continues("crucible-run-a", &graph(2.0)),
                Err(ResumeError::PlanChanged { .. })
            ),
            "a raised cost ceiling is a different run"
        );
    }

    #[test]
    fn the_same_source_digests_the_same_in_every_process() {
        assert_eq!(plan_digest(&graph(1.0)), plan_digest(&graph(1.0)));
        assert_ne!(plan_digest(&graph(1.0)), plan_digest(&graph(1.5)));
    }

    #[test]
    fn elapsed_counts_downtime_and_never_goes_negative() {
        let started = start("r", &graph(1.0));
        assert_eq!(
            started.elapsed("2026-09-29T12:05:00Z".parse().unwrap()),
            Duration::from_secs(300)
        );
        assert_eq!(
            started.elapsed("2026-09-29T11:00:00Z".parse().unwrap()),
            Duration::ZERO,
            "a clock that stepped back grants no extra time"
        );
    }

    #[test]
    fn the_start_record_round_trips_and_its_absence_is_named() {
        let dir = std::env::temp_dir().join(format!("crucible-run-start-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(matches!(
            RunStart::read(&dir),
            Err(ResumeError::NeverStarted(path)) if path == RunStart::path(&dir)
        ));
        let started = start("r", &graph(1.0));
        started.write(&dir).unwrap();
        assert_eq!(RunStart::read(&dir).unwrap(), started);
        std::fs::write(RunStart::path(&dir), r#"{"run":"r","extra":1}"#).unwrap();
        assert!(matches!(
            RunStart::read(&dir),
            Err(ResumeError::BadRunStart { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_log_folds_into_its_rows_in_order_skipping_a_torn_line() {
        let text = log(&[
            ADMITTED,
            &row("discover", "pass", 0.5),
            &row("audit[a]", "pass", 0.25),
            r#"{"v":1,"kind":"task_result","task":"audit[b]","sta"#,
        ]);
        let prior = fold(&text, &graph(1.0), Duration::from_secs(7)).unwrap();
        assert_eq!(
            prior.order,
            [TaskName::from("discover"), TaskName::from("audit[a]")]
        );
        assert_eq!(prior.results[&"discover".into()].status, TaskStatus::Pass);
        assert_eq!(prior.elapsed, Duration::from_secs(7));
        assert!(prior.admitted && !prior.shut_down && !prior.recorded_history);
    }

    #[test]
    fn only_a_trailing_shutdown_closes_the_log() {
        let shutdown = r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":"done"}"#;
        let history =
            r#"{"v":1,"kind":"history_entry","entry":{"task":"","status":null,"output":null}}"#;
        let closed = fold(
            &log(&[ADMITTED, &row("discover", "pass", 0.0), history, shutdown]),
            &graph(1.0),
            Duration::ZERO,
        )
        .unwrap();
        assert!(closed.shut_down && closed.recorded_history);
        let reopened = fold(
            &log(&[ADMITTED, shutdown, &row("discover", "pass", 0.0)]),
            &graph(1.0),
            Duration::ZERO,
        )
        .unwrap();
        assert!(!reopened.shut_down);
    }

    #[test]
    fn a_log_the_plan_could_not_have_written_is_refused() {
        let plan = graph(1.0);
        assert!(matches!(
            fold(&row("discover", "pass", 0.0), &plan, Duration::ZERO),
            Err(ResumeError::NotAdmitted { task }) if task == "discover"
        ));
        assert!(matches!(
            fold(&log(&[ADMITTED, &row("ghost", "pass", 0.0)]), &plan, Duration::ZERO),
            Err(ResumeError::UnknownTask { task }) if task == "ghost"
        ));
        assert!(matches!(
            fold(&log(&[ADMITTED, &row("discover", "maybe", 0.0)]), &plan, Duration::ZERO),
            Err(ResumeError::BadStatus { task, .. }) if task == "discover"
        ));
    }

    #[test]
    fn a_blocked_row_keeps_its_reason_and_a_refused_staging_keeps_its_why() {
        let text = log(&[
            ADMITTED,
            r#"{"v":1,"kind":"task_result","task":"wrap","status":"blocked","note":"required task discover failed","blocked":{"reason":"required_task_failed","task":"discover"}}"#,
            r#"{"v":1,"kind":"task_result","task":"audit","status":"blocked","note":"inputs/x is a symlink","blocked":{"reason":"staging_refused"}}"#,
        ]);
        let prior = fold(&text, &graph(1.0), Duration::ZERO).unwrap();
        assert_eq!(
            prior.results[&"wrap".into()].blocked,
            Some(BlockedReason::RequiredTaskFailed("discover".into()))
        );
        assert_eq!(
            prior.results[&"audit".into()].blocked,
            Some(BlockedReason::StagingRefused(
                "inputs/x is a symlink".into()
            ))
        );
    }

    /// A node's row totals its items, so counting both would charge the items twice; a node cut
    /// off before its row still charges the items it ran.
    #[test]
    fn spend_counts_each_dispatch_once() {
        let whole = fold(
            &log(&[
                ADMITTED,
                &row("discover", "pass", 0.5),
                &row("audit[a]", "pass", 0.25),
                &row("audit[b]", "pass", 0.25),
                &row("audit", "pass", 0.5),
            ]),
            &graph(1.0),
            Duration::ZERO,
        )
        .unwrap();
        assert!(
            (whole.spent_usd() - 1.0).abs() < 1e-9,
            "{}",
            whole.spent_usd()
        );
        let cut = fold(
            &log(&[
                ADMITTED,
                &row("discover", "pass", 0.5),
                &row("audit[a]", "pass", 0.25),
            ]),
            &graph(1.0),
            Duration::ZERO,
        )
        .unwrap();
        assert!((cut.spent_usd() - 0.75).abs() < 1e-9, "{}", cut.spent_usd());
        assert_eq!(Prior::default().spent_usd(), 0.0);
    }
}
