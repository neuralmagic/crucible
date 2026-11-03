//! Resuming a playbook from its session log: what the interrupted run settled, what it spent, and
//! how long it had been running (RFC-0002:C-PLAYBOOK-RESUME).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crucible_contract::session::SessionEvent;
use serde::{Deserialize, Serialize};

use crate::manifest::variant::VariantName;
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
    /// The tree `base` points at, so a rerun can start from the same content in a checkout
    /// that does not have the commit itself.
    #[serde(default)]
    pub base_tree: String,
    pub plan: String,
    /// The manifest variant the run reads; absent for the base manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<VariantName>,
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
    #[error(
        "this resume reads {}, but the interrupted run read {}; a different variant is a \
         different run",
        variant_label(.now.as_ref()),
        variant_label(.then.as_ref())
    )]
    VariantChanged {
        then: Option<VariantName>,
        now: Option<VariantName>,
    },
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

fn variant_label(variant: Option<&VariantName>) -> String {
    variant.map_or("the base manifest".to_owned(), |v| format!("[variant.{v}]"))
}

impl RunStart {
    pub fn begin(
        run: &str,
        base: String,
        base_tree: String,
        plan: &ValidPlan,
        variant: Option<VariantName>,
    ) -> Self {
        RunStart {
            run: run.to_string(),
            started_at: jiff::Timestamp::now(),
            base,
            base_tree,
            plan: plan_digest(plan),
            variant,
        }
    }

    /// Whether this process is the same run, running the same graph, as the one that wrote the
    /// record.
    pub fn continues(
        &self,
        run: &str,
        plan: &ValidPlan,
        variant: Option<&VariantName>,
    ) -> Result<(), ResumeError> {
        if self.run != run {
            return Err(ResumeError::OtherRun {
                then: self.run.clone(),
                now: run.to_string(),
            });
        }
        if self.variant.as_ref() != variant {
            return Err(ResumeError::VariantChanged {
                then: self.variant.clone(),
                now: variant.cloned(),
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
///
/// A `task_started` holds its estimate until a `task_result` for the same task settles it. A
/// `recovery` line is written by the process that resumed, so whatever was still held there
/// belonged to a process that died, and is charged; so is whatever is held at the end of the log.
pub fn fold(log: &str, plan: &ValidPlan, elapsed: Duration) -> Result<Prior, ResumeError> {
    let mut prior = Prior {
        elapsed,
        ..Prior::default()
    };
    let mut held: BTreeMap<String, f64> = BTreeMap::new();
    for event in log.lines().filter_map(crucible_contract::session::decode) {
        prior.shut_down = false;
        match event {
            SessionEvent::PlanAdmitted { .. } => prior.admitted = true,
            SessionEvent::TaskStarted {
                task, estimate_usd, ..
            } => {
                if estimate_usd.is_finite() && estimate_usd > 0.0 {
                    *held.entry(task).or_default() += estimate_usd;
                }
            }
            SessionEvent::Recovery { .. } => {
                prior.reserved_usd += held.values().sum::<f64>();
                held.clear();
            }
            SessionEvent::TaskResult { ref task, .. } => {
                if !prior.admitted {
                    return Err(ResumeError::NotAdmitted { task: task.clone() });
                }
                held.remove(task);
                if let Some((node, _)) = TaskName(task.clone()).instance_of() {
                    held.remove(node);
                }
                let Some(row) = settled_row(plan, event)? else {
                    continue;
                };
                if let Some(from) = row.reused_from {
                    prior.reused.insert(row.name.clone(), from);
                }
                if prior.results.insert(row.name.clone(), row.result).is_none() {
                    prior.order.push(row.name);
                }
            }
            SessionEvent::HistoryEntry { .. } => prior.recorded_history = true,
            SessionEvent::Shutdown { .. } => prior.shut_down = true,
            _ => {}
        }
    }
    prior.reserved_usd += held.values().sum::<f64>();
    Ok(prior)
}

/// One settled row of a session log as the executor holds it.
#[derive(Debug, Clone)]
pub struct SettledRow {
    pub name: TaskName,
    pub result: TaskResult,
    /// The run that settled it, when the run that logged it reused it.
    pub reused_from: Option<String>,
}

/// The row a `task_result` event logs, read against `plan`; `None` for any other event.
pub fn settled_row(
    plan: &ValidPlan,
    event: SessionEvent,
) -> Result<Option<SettledRow>, ResumeError> {
    let SessionEvent::TaskResult {
        task,
        status,
        attempts,
        cost_usd,
        output,
        note,
        blocked,
        transport,
        repairs,
        reused_from,
        ..
    } = event
    else {
        return Ok(None);
    };
    let name = TaskName(task.clone());
    let row =
        row_task(plan, &name).ok_or_else(|| ResumeError::UnknownTask { task: task.clone() })?;
    let status = status
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
    Ok(Some(SettledRow {
        name,
        result: TaskResult {
            status,
            attempts,
            cost_usd,
            output,
            note: (!note.is_empty()).then_some(note),
            fanout,
            blocked,
            transport,
            repairs,
        },
        reused_from,
    }))
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
    use crate::manifest::variant::VariantName;
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
            base_tree: String::new(),
            plan: plan_digest(plan),
            variant: None,
        }
    }

    #[test]
    fn a_run_continues_only_itself_running_the_same_graph() {
        let plan = graph(1.0);
        let started = start("crucible-run-a", &plan);
        assert!(started.continues("crucible-run-a", &plan, None).is_ok());
        assert!(matches!(
            started.continues("crucible-run-b", &plan, None),
            Err(ResumeError::OtherRun { then, now }) if then == "crucible-run-a" && now == "crucible-run-b"
        ));
        assert!(
            matches!(
                started.continues("crucible-run-a", &graph(2.0), None),
                Err(ResumeError::PlanChanged { .. })
            ),
            "a raised cost ceiling is a different run"
        );
    }

    #[test]
    fn a_resume_under_another_variant_is_a_different_run() {
        let plan = graph(1.0);
        let offline = VariantName::parse("offline").unwrap();
        let started = RunStart {
            variant: Some(offline.clone()),
            ..start("crucible-run-a", &plan)
        };
        assert!(
            started
                .continues("crucible-run-a", &plan, Some(&offline))
                .is_ok()
        );
        let error = started
            .continues("crucible-run-a", &plan, None)
            .unwrap_err();
        assert!(
            matches!(&error, ResumeError::VariantChanged { then: Some(v), now: None } if v == &offline),
            "{error}"
        );
        assert!(
            error.to_string().contains(
                "reads the base manifest, but the interrupted run read [variant.offline]"
            ),
            "{error}"
        );
        let base = start("crucible-run-a", &plan);
        assert!(matches!(
            base.continues("crucible-run-a", &plan, Some(&offline)),
            Err(ResumeError::VariantChanged {
                then: None,
                now: Some(_)
            })
        ));

        let record = serde_json::to_value(&base).unwrap();
        assert!(record.get("variant").is_none(), "{record}");
        let reread: RunStart = serde_json::from_value(record).unwrap();
        assert_eq!(reread.variant, None);
        let reread: RunStart =
            serde_json::from_value(serde_json::to_value(&started).unwrap()).unwrap();
        assert_eq!(reread.variant, Some(offline));
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

    fn started(task: &str, attempt: u32, estimate: f64) -> String {
        format!(
            r#"{{"v":1,"kind":"task_started","task":"{task}","attempt":{attempt},"estimate_usd":{estimate}}}"#
        )
    }

    const RECOVERY: &str =
        r#"{"v":1,"kind":"recovery","class":"died_in_plan_task","iter":0,"detail":""}"#;

    /// An attempt's cost reaches the log only on its task's row, so an attempt whose process died
    /// first is charged what it reserved when it started. The row settles the reservations of its
    /// own process; one left by a process that died before a resume stays charged even after
    /// the task later settles.
    #[test]
    fn an_attempt_that_never_settled_is_charged_its_estimate() {
        let plan = graph(1.0);
        let spent = |lines: &[&str]| {
            fold(&log(lines), &plan, Duration::ZERO)
                .unwrap()
                .spent_usd()
        };
        let cases: [(&str, Vec<String>, f64); 6] = [
            ("died mid-attempt", vec![started("discover", 1, 0.25)], 0.25),
            (
                "settled by its row",
                vec![started("discover", 1, 0.25), row("discover", "pass", 0.1)],
                0.1,
            ),
            (
                "a retried attempt reserves again until the row",
                vec![
                    started("audit[a]", 1, 0.25),
                    started("audit[a]", 2, 0.25),
                    started("audit[b]", 1, 0.25),
                    row("audit[a]", "pass", 0.3),
                ],
                0.55,
            ),
            (
                "died before a resume that later settled it",
                vec![
                    started("discover", 1, 0.25),
                    RECOVERY.to_owned(),
                    started("discover", 1, 0.25),
                    row("discover", "pass", 0.1),
                ],
                0.35,
            ),
            (
                "a row named under its task settles what that task started",
                vec![started("audit", 1, 0.25), row("audit[a]", "pass", 0.1)],
                0.1,
            ),
            (
                "died twice",
                vec![
                    started("discover", 1, 0.25),
                    RECOVERY.to_owned(),
                    started("discover", 1, 0.25),
                ],
                0.5,
            ),
        ];
        for (case, lines, want) in cases {
            let lines: Vec<&str> = std::iter::once(ADMITTED)
                .chain(lines.iter().map(String::as_str))
                .collect();
            let got = spent(&lines);
            assert!((got - want).abs() < 1e-9, "{case}: {got} != {want}");
        }
    }

    /// A task that takes its process down on every attempt never writes a row, so without a
    /// durable start each resume would dispatch it again for free and the cost ceiling would
    /// never see what it spent. Each crash here is the session log as it stood while the attempt
    /// ran; the third resume finds the ceiling spent and blocks the task instead of running it.
    #[test]
    fn a_task_that_kills_its_process_every_attempt_is_stopped_by_the_cost_ceiling() {
        use crate::plan::exec::{
            Attempt, AttemptOutcome, ExecCfg, PlanExit, Substrate, TaskRunner, execute,
        };
        use crate::plan::ir::Task;
        use crate::plan::machine::BlockedReason;
        use std::collections::BTreeMap;
        use std::num::NonZeroUsize;

        struct Crashing<'a> {
            log: &'a std::path::Path,
            crashed_at: Option<String>,
            dispatched: Vec<String>,
        }
        impl TaskRunner for Crashing<'_> {
            fn run(
                &mut self,
                task: &Task,
                _attempt: u32,
                _inputs: &BTreeMap<TaskName, serde_json::Value>,
                _deadline: Option<crate::deadline::Deadline>,
            ) -> Attempt {
                self.dispatched.push(task.name.0.clone());
                if task.name.0 == "write" && self.crashed_at.is_none() {
                    self.crashed_at = Some(std::fs::read_to_string(self.log).unwrap());
                }
                Attempt {
                    outcome: AttemptOutcome::Pass(serde_json::json!({})),
                    cost_usd: 0.3,
                    repairs: Vec::new(),
                }
            }
        }

        let plan = Plan::from_toml_str(
            r#"
            version = 1
            [budget]
            usd = 1.0
            [[task]]
            name = "draft"
            kind = "agent"
            prompt = "draft it"
            [[task]]
            name = "write"
            kind = "agent"
            prompt = "write it"
            depends_on = ["draft"]
            "#,
        )
        .unwrap()
        .validate()
        .unwrap();
        let dir = std::env::temp_dir().join(format!("crucible-crash-loop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        std::fs::write(&path, format!("{ADMITTED}\n")).unwrap();

        let mut prior: Option<Prior> = None;
        let mut spent_at_resume = Vec::new();
        let outcome = loop {
            if prior.is_some() {
                let mut text = std::fs::read_to_string(&path).unwrap();
                text.push_str(RECOVERY);
                text.push('\n');
                std::fs::write(&path, text).unwrap();
            }
            let journal = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            let mut runner = Crashing {
                log: &path,
                crashed_at: None,
                dispatched: Vec::new(),
            };
            let outcome = execute(
                &plan,
                &Substrate::default(),
                ExecCfg {
                    prior: prior.as_ref(),
                    width: NonZeroUsize::new(4).unwrap(),
                    journal: Some(&journal),
                    ..ExecCfg::default()
                },
                &mut runner,
                |task, result| {
                    let line = row(&task.name.0, result.status.as_str(), result.cost_usd);
                    let mut file = &journal;
                    std::io::Write::write_all(&mut file, format!("{line}\n").as_bytes()).unwrap();
                },
            )
            .unwrap();
            match runner.crashed_at {
                Some(left) => {
                    std::fs::write(&path, &left).unwrap();
                    let folded = fold(&left, &plan, Duration::ZERO).unwrap();
                    spent_at_resume.push(folded.spent_usd());
                    prior = Some(folded);
                    assert!(spent_at_resume.len() < 8, "the crash loop never stopped");
                }
                None => {
                    assert!(
                        !runner.dispatched.contains(&"write".to_owned()),
                        "{:?}",
                        runner.dispatched
                    );
                    break outcome;
                }
            }
        };

        let want = [0.55, 0.8, 1.05];
        assert_eq!(spent_at_resume.len(), want.len(), "{spent_at_resume:?}");
        for (got, want) in spent_at_resume.iter().zip(want) {
            assert!((got - want).abs() < 1e-9, "{spent_at_resume:?}");
        }
        assert!(matches!(outcome.exit, PlanExit::BudgetExceeded));
        assert!(!outcome.valid);
        assert_eq!(
            outcome.results[&TaskName::from("write")].blocked,
            Some(BlockedReason::BudgetCeiling)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
