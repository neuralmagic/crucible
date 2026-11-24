//! Running a plan to its verdict: the manifest's workspace set up, resumed or rerun, the graph
//! executed, and the run's record written as it goes (session log, report, git memory, evidence).
//! What differs between binaries is an [`Engine`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use crucible_contract::session::{SessionEvent, TaskStatus};

use crate::paths::Paths;
use crate::plan::decide::DecisionDesk;
use crate::plan::exec::{Prior, Substrate, TaskRunner, required_tasks_held};
use crate::plan::history::SeriesHistory;
use crate::plan::ir::{Plan, Stage, TaskKind, TaskName, ValidPlan};
use crate::plan::record::declared_output;
use crate::plan::rerun::ParentRef;
use crate::plan::resume::RunStart;
use crate::plan::setup::Start;
use crate::plan::workflow::WorkflowType;
use crate::plan::workspace_runner::{Dispatch, WorkspaceRunner};

#[derive(Debug, thiserror::Error)]
#[error("plan did not reach a valid verdict ({exit})")]
pub struct NoValidVerdict {
    exit: String,
}

/// Read TOML (`.toml`) or JSON (anything else: the `PLAN.json` sentinel shape),
/// validate, and return the frozen plan.
pub fn load(path: &Path) -> Result<ValidPlan> {
    let src = std::fs::read_to_string(path)
        .with_context(|| format!("reading plan {}", path.display()))?;
    let plan = if path.extension().is_some_and(|e| e == "toml") {
        Plan::from_toml_str(&src)?
    } else {
        Plan::from_json_str(&src)?
    };
    plan.validate()
        .with_context(|| format!("validating plan {}", path.display()))
}

/// The `Shutdown.outcome` token a finished plan reports, in the vocabulary
/// the engine's recovery parses. Only a plan that ran its whole graph is
/// `finished`.
fn shutdown_outcome(out: &crate::plan::exec::PlanOutcome) -> &'static str {
    use crate::plan::exec::PlanExit;
    match out.exit {
        PlanExit::Completed if !out.valid => "error",
        ref exit => exit.shutdown_token(),
    }
}

/// Compile and execute a plan: real subprocesses, real outputs, the executor's real
/// semantics. With `--manifest`, agent tasks run through the real harness path; otherwise
/// the shell runner handles everything (`--agent-cmd` as the agent stand-in). Exits nonzero
/// when the plan is not valid.
/// What the launcher supplies. A playbook's source may not declare either, so a pack cannot
/// raise a limit its operator set; the engine refuses to dispatch one that arrives without both.
#[derive(Debug, Clone, Default)]
pub struct Ceilings {
    pub usd: Option<f64>,
    pub wall_clock: Option<std::time::Duration>,
    /// What the operator typed, so a rejection can quote it back rather than say "invalid".
    pub wall_clock_raw: Option<String>,
}

/// What a launch supplies beyond the graph and its parameters.
#[derive(Debug, Clone, Default)]
pub struct Launch {
    pub ceilings: Ceilings,
    /// Continue the playbook run the manifest's state dir holds instead of starting one.
    pub resume: bool,
    /// The manifest variant the run reads.
    pub variant: Option<crate::manifest::variant::VariantName>,
    /// Rerun a finished playbook run from some of its tasks instead of starting afresh.
    pub rerun: Option<Rerun>,
}

/// What a plan runs from.
pub enum Source<'a> {
    /// A manifest's workspace, running `plan` when one is given and the manifest's own graph
    /// otherwise.
    Manifest {
        manifest: &'a Path,
        plan: Option<&'a Path>,
    },
    /// A precompiled plan `runner` executes outside any manifest.
    Plan {
        plan: &'a Path,
        runner: Box<dyn TaskRunner>,
    },
}

/// What a binary brings to a plan run: how a manifest becomes a runner over its workspace, and
/// what its substrate can do.
pub trait Engine {
    type Dispatch: Dispatch + Clone + 'static;

    /// The runner over `manifest`'s workspace, set up or taken over as `start` says, and the
    /// manifest it was built from.
    fn prepare(
        &mut self,
        manifest: &Path,
        variant: Option<&crate::manifest::variant::VariantName>,
        params: &BTreeMap<String, String>,
        start: Start,
    ) -> Result<(WorkspaceRunner<Self::Dispatch>, crate::manifest::Manifest)>;

    /// Refuse a plan this engine cannot run, before anything is dispatched.
    fn admit(&self, _plan: &ValidPlan) -> Result<()> {
        Ok(())
    }

    /// Where human-decided routes open their requests; none leaves the `human` capability off.
    fn desk<'a>(&self, _log: Option<&'a std::fs::File>) -> Option<Box<dyn DecisionDesk + 'a>> {
        None
    }

    /// What the run's substrate can do, given the capabilities the launch named.
    fn substrate(&self, caps: BTreeSet<String>) -> Result<Substrate> {
        Ok(Substrate { caps })
    }

    /// The trace and span a task result is recorded under.
    fn trace_context(&self) -> Option<(String, String)> {
        None
    }
}

/// A rerun's parent and the tasks it dispatches again.
#[derive(Debug, Clone)]
pub struct Rerun {
    /// The finished run: its state directory, or its evidence on the controller.
    pub parent: ParentRef,
    pub from: Vec<TaskName>,
}

#[derive(Debug, thiserror::Error)]
#[error("--rerun-of reruns a playbook run; {manifest} does not declare a playbook")]
struct RerunNeedsPlaybook {
    manifest: String,
}

#[derive(Debug, thiserror::Error)]
#[error("--rerun-of {} is this run's own state directory; copy the parent's state elsewhere first", .0.display())]
struct RerunIntoParent(std::path::PathBuf);

#[derive(Debug, thiserror::Error)]
#[error("--resume continues a playbook run; {manifest} does not declare a playbook")]
struct ResumeNeedsPlaybook {
    manifest: String,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "a playbook needs {missing} from whoever launches it. Its source may not declare a limit its \
     operator set, so the engine will not dispatch a task without one."
)]
struct MissingCeiling {
    missing: String,
}

#[derive(Debug, thiserror::Error)]
#[error("--max-time {raw:?} is not a duration (try `90s`, `30m`, `2h`)")]
struct BadDuration {
    raw: String,
}

#[derive(Debug, thiserror::Error)]
#[error("{manifest} declares no [workflow]; pass --file, or give the manifest a graph to compile")]
struct NoGraph {
    manifest: String,
}

#[derive(Debug, thiserror::Error)]
#[error("--param {got:?} is not name=value")]
struct BadParam {
    got: String,
}

/// Split `name=value` pairs. The value may contain `=`; the name may not, so the first one wins.
pub fn parse_params(pairs: &[String]) -> Result<BTreeMap<String, String>> {
    pairs
        .iter()
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) if !name.is_empty() => Ok((name.to_string(), value.to_string())),
            _ => Err(BadParam { got: pair.clone() }.into()),
        })
        .collect()
}

/// Keep the run's task commits beside its other evidence, so a rerun can replay them.
fn record_git_memory(paths: &Paths) -> Result<()> {
    let start = RunStart::read(&paths.state)?;
    let commits = crucible_vcs::git_memory::task_commits(&paths.workspace, &start.base)?;
    let fetched = crate::workspace::fetched_blobs(&paths.workspace)?;
    crate::plan::rerun::write_git_memory(&paths.state, &start, &commits, &fetched)?;
    Ok(())
}

/// Where a rerun fetches a parent named by URL, inside its own state directory.
const FETCHED_PARENT_DIR: &str = "rerun-parent";

/// Seed a rerun: rebuild the workspace from the parent's base, the blobs its partial clone held
/// and the reused tasks' commits, restore their captured files, and record where this run started.
fn start_rerun(
    manifest: &Path,
    loaded: &crate::manifest::Manifest,
    paths: &Paths,
    run: &str,
    plan: &ValidPlan,
    rerun: &Rerun,
) -> Result<Prior> {
    let dir = match &rerun.parent {
        ParentRef::Dir(dir) => {
            let same =
                |a: &Path, b: &Path| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
                    (Ok(a), Ok(b)) => a == b,
                    _ => false,
                };
            if same(dir, &paths.state) {
                return Err(RerunIntoParent(dir.clone()).into());
            }
            dir.clone()
        }
        ParentRef::Url(url) => {
            let dir = paths.state.join(FETCHED_PARENT_DIR);
            crate::evidence::fetch_parent(url, &dir)?;
            dir
        }
    };
    let parent = crate::plan::rerun::ParentDir::read(&dir)?;
    let seeded = crate::plan::rerun::plan(plan, &parent.parent, &rerun.from)?;
    let commits = seeded.evidence(plan, &parent)?;
    let fetched = crate::plan::rerun::read_fetched_blobs(&parent.dir)?;
    let base =
        crate::plan::setup::restore_for_rerun(manifest, loaded, &parent.start, &commits, &fetched)?;
    seeded
        .restore_captured(&parent, &paths.state)
        .context("restoring the parent's captured files")?;
    let tree = crucible_vcs::git_memory::tree_of(&paths.workspace, &base)?.unwrap_or_default();
    RunStart::begin(run, base, tree, plan, loaded.variant.clone()).write(&paths.state)?;
    Ok(seeded.prior)
}

pub fn run<E: Engine>(
    engine: &mut E,
    source: Source<'_>,
    params: &BTreeMap<String, String>,
    caps: &BTreeSet<String>,
    launch: Launch,
) -> Result<()> {
    use crate::plan::exec::{ExecCfg, PlanExit, execute};
    let Launch {
        ceilings,
        resume,
        variant,
        rerun,
    } = launch;

    if let (None, Some(raw)) = (ceilings.wall_clock, ceilings.wall_clock_raw.as_ref()) {
        return Err(BadDuration { raw: raw.clone() }.into());
    }
    let history = SeriesHistory::from_env()?;
    let run_name = std::env::var(crucible_contract::ENV_RUN_NAME).ok();
    let run = run_name.as_deref().unwrap_or("local");

    // Either the plan was handed to us, or the manifest names the graph and we compile it.
    let mut evidence: Option<Paths> = None;
    // A playbook run records an entry for its launch series: under the record task its workflow
    // names, or an empty one for a precompiled plan, which names none.
    let mut series_entry: Option<Option<TaskName>> = None;
    let mut playbook = false;
    let mut prior: Option<Prior> = None;
    let mut resuming = false;
    // What reports each task's agent; None for a precompiled plan, which has no manifest.
    let mut reporter: Option<E::Dispatch> = None;
    let (plan, mut runner, events): (ValidPlan, Box<dyn TaskRunner>, Option<std::fs::File>) =
        match source {
            Source::Manifest {
                manifest: m,
                plan: path,
            } => {
                let state = crate::plan::setup::state_dir(m);
                let resumed = resume.then(|| RunStart::read(&state)).transpose()?;
                let start = if resume || rerun.is_some() {
                    Start::Continues
                } else {
                    Start::Fresh
                };
                let (mut prepared, loaded) = engine.prepare(m, variant.as_ref(), params, start)?;
                let session_log = prepared.paths.session_log.clone();
                reporter = Some(prepared.dispatch.clone());
                evidence = Some(prepared.paths.clone());
                playbook = loaded
                    .workflow
                    .as_ref()
                    .is_some_and(|w| w.workflow_type == WorkflowType::Playbook);
                if resume && !playbook {
                    return Err(ResumeNeedsPlaybook {
                        manifest: m.display().to_string(),
                    }
                    .into());
                }
                if rerun.is_some() && !playbook {
                    return Err(RerunNeedsPlaybook {
                        manifest: m.display().to_string(),
                    }
                    .into());
                }
                if playbook {
                    series_entry = Some(None);
                    let missing = match (ceilings.usd, ceilings.wall_clock) {
                        (None, None) => Some("--max-cost and --max-time"),
                        (None, Some(_)) => Some("--max-cost"),
                        (Some(_), None) => Some("--max-time"),
                        (Some(_), Some(_)) => None,
                    };
                    if let Some(missing) = missing {
                        return Err(MissingCeiling {
                            missing: missing.to_string(),
                        }
                        .into());
                    }
                }
                let mut plan = match path {
                    Some(p) => load(p)?,
                    None => {
                        let workflow = loaded.workflow.as_ref().ok_or_else(|| NoGraph {
                            manifest: m.display().to_string(),
                        })?;
                        if workflow.workflow_type == WorkflowType::Playbook {
                            series_entry = Some(workflow.history_record.clone());
                            workflow.playbook_plan()?
                        } else {
                            let caps = crate::plan::workflow::WorkflowCaps::for_lane(
                                workflow.workflow_type,
                            )
                            .with_persistent_sessions();
                            crate::plan::template::iteration_template(Some(workflow), &caps)?
                        }
                    }
                };
                if let Some(usd) = ceilings.usd {
                    plan = plan.with_budget(usd)?;
                }
                match (resumed, &rerun) {
                    (Some(resumed), _) => {
                        resumed.continues(run, &plan, variant.as_ref())?;
                        crate::plan::setup::restore_pristine(m, &loaded, &resumed.base)?;
                        prior = Some(crate::plan::resume::fold_log(
                            &session_log,
                            &plan,
                            resumed.elapsed(jiff::Timestamp::now()),
                        )?);
                        resuming = true;
                        prepared.take_over();
                    }
                    (None, Some(rerun)) => {
                        prior = Some(start_rerun(m, &loaded, &prepared.paths, run, &plan, rerun)?);
                        prepared.take_over();
                    }
                    (None, None) if playbook => {
                        let workspace = &prepared.paths.workspace;
                        let base = crucible_vcs::vcs::head_sha(workspace)
                            .context("reading the workspace's starting commit")?;
                        let tree = crucible_vcs::git_memory::tree_of(workspace, &base)?
                            .unwrap_or_default();
                        RunStart::begin(run, base, tree, &plan, variant.clone()).write(&state)?;
                    }
                    (None, None) => {}
                }
                let f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&session_log)
                    .with_context(|| format!("opening {}", session_log.display()))?;
                (plan, Box::new(prepared), Some(f))
            }
            Source::Plan { plan: p, runner } => {
                let mut plan = load(p)?;
                if let Some(usd) = ceilings.usd {
                    plan = plan.with_budget(usd)?;
                }
                (plan, runner, None)
            }
        };
    engine.admit(&plan)?;
    if let Some(ceiling) = ceilings.wall_clock {
        plan.check_timeouts(ceiling)?;
    }
    // A task under a deadline runs in a process group of its own, where the terminal's SIGINT
    // does not reach it.
    let _ = ctrlc::set_handler(|| {
        crate::deadline::terminate_live_children();
        std::process::exit(130);
    });
    // Manifest runs append plan wire events to the run's session log so tailers (and the
    // controller's ingest) see the graph and its live progress; shell runs have no state dir.
    let append = |f: &std::fs::File, ev: &SessionEvent| {
        use std::io::Write;
        let mut w = f;
        let _ = writeln!(w, "{}", crucible_contract::session::encode(ev));
    };
    // A log that already ends in a shutdown is closed: the run finished, and nothing may follow
    // its last line.
    let events = events.filter(|_| !prior.as_ref().is_some_and(|p| p.shut_down));
    // A run with a decision desk can open decision requests, which is the `human` capability.
    let desk = engine.desk(events.as_ref());
    let mut caps = caps.clone();
    if desk.is_some() {
        caps.insert(crate::plan::ir::NEEDS_HUMAN.to_owned());
    }
    let substrate = engine.substrate(caps)?;
    if let Some(f) = &events {
        if let Some(prior) = prior.as_ref().filter(|_| resuming) {
            append(
                f,
                &SessionEvent::Recovery {
                    class: crucible_contract::session::RecoveryClass::DiedInPlanTask,
                    iter: 0,
                    detail: format!(
                        "{} of {} tasks settled, ${:.4} spent over {} before the interruption",
                        plan.tasks_topo()
                            .filter(|t| prior.results.contains_key(&t.name))
                            .count(),
                        plan.tasks_topo().count(),
                        prior.spent_usd(),
                        crate::duration::Shown(prior.elapsed),
                    ),
                },
            );
        }
        if !prior.as_ref().is_some_and(|p| p.admitted) {
            append(
                f,
                &crate::plan::session_events::plan_admitted(
                    &plan,
                    series_entry.as_ref().and_then(Option::as_ref),
                ),
            );
            if let Some(prior) = &prior {
                for (task, result) in prior.rows(&plan) {
                    if let Some(from) = prior.reused.get(&task.name) {
                        append(
                            f,
                            &crate::plan::session_events::reused_result(
                                plan.plan().version,
                                &task,
                                result,
                                from,
                            ),
                        );
                    }
                }
            }
        }
    }
    let selected_results: std::collections::BTreeSet<_> = plan
        .tasks_topo()
        .filter_map(|task| match &task.task {
            TaskKind::Report {
                result: Some(result),
                ..
            } => Some(result.0.clone()),
            _ => None,
        })
        .collect();
    let mut report = crucible_contract::RunReport {
        run: std::env::var(crucible_contract::ENV_RUN_DISPLAY_NAME)
            .unwrap_or_else(|_| run.to_string()),
        run_url: match (
            std::env::var(crucible_contract::ENV_UI_BASE_URL).ok(),
            &run_name,
        ) {
            (Some(base), Some(run)) => Some(format!("{}/runs/{run}", base.trim_end_matches('/'))),
            _ => None,
        },
        tasks: Vec::new(),
        verdict: crucible_contract::RunVerdict::Pending,
        results: selected_results
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    crucible_contract::ReportResult {
                        status: "pending".to_string(),
                        output: None,
                    },
                )
            })
            .collect(),
    };
    let report_path = forge::storage_root().join(crucible_contract::REPORT_FILE);
    let write_report = |report: &crucible_contract::RunReport| {
        if let Some(parent) = report_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec(report) {
            let _ = std::fs::write(&report_path, bytes);
        }
    };
    write_report(&report);
    let mut statuses: BTreeMap<TaskName, TaskStatus> = BTreeMap::new();
    let mut completed_early = false;
    let mut observe = |task: &crate::plan::ir::Task, result: &crate::plan::exec::TaskResult| {
        report.tasks.push(crucible_contract::TaskReport {
            name: task.name.0.clone(),
            status: result.status,
            cost_usd: result.cost_usd,
            blocked: result
                .blocked
                .as_ref()
                .map(crate::plan::machine::BlockedReason::wire),
            transport: result.transport,
        });
        if let Some(selected) = report.results.get_mut(&task.name.0) {
            selected.status = result.status.as_str().to_string();
            selected.output = declared_output(task, result);
        }
        statuses.insert(task.name.clone(), result.status);
        completed_early |= playbook
            && task.stage == Stage::Iteration
            && crate::plan::exec::declared_completion(task, result).is_some();
        if completed_early
            || plan
                .tasks_topo()
                .filter(|t| t.stage == Stage::Iteration)
                .all(|t| statuses.contains_key(&t.name))
        {
            report.verdict = crucible_contract::RunVerdict::of(required_tasks_held(
                &plan,
                |name| match statuses.get(name) {
                    Some(status) => Some(*status),
                    None => completed_early.then_some(TaskStatus::NotTaken),
                },
            ));
        }
        write_report(&report);
    };
    if let Some(prior) = &prior {
        for (task, result) in prior.rows(&plan) {
            observe(&task, result);
        }
    }
    let files = evidence
        .as_ref()
        .map(|p| crate::plan::files::CapturedFiles::new(&p.state));
    let clock = crate::plan::admission::TaskClock::default();
    let mut took = BTreeMap::new();
    let out = execute(
        &plan,
        &substrate,
        ExecCfg {
            wall_clock: ceilings.wall_clock,
            history: Some(&history),
            early_completion: playbook,
            prior: prior.as_ref(),
            journal: events.as_ref(),
            clock: Some(&clock),
            files: files.as_ref(),
            decisions: desk.as_deref(),
            ..ExecCfg::default()
        },
        runner.as_mut(),
        |task, result| {
            observe(task, result);
            let started = clock.ran(&task.name);
            if let Some(secs) = started {
                took.insert(task.name.clone(), secs);
            }
            let secs = started.unwrap_or(0.0);
            if let Some(f) = &events {
                append(
                    f,
                    &crate::plan::session_events::task_result(
                        plan.plan().version,
                        0,
                        task,
                        result,
                        reporter.as_ref().and_then(|d| d.task_agent(task)),
                        engine.trace_context(),
                        secs,
                    ),
                );
            }
        },
    )?;
    report.verdict = crucible_contract::RunVerdict::of(out.valid);
    write_report(&report);
    for t in plan.tasks_topo() {
        if let Some(r) = out.results.get(&t.name) {
            println!(
                "  {:<20} {:<10} attempts={} cost=${:.4}{}{}{}",
                t.name.0,
                r.status.as_str(),
                r.attempts,
                r.cost_usd,
                took.get(&t.name)
                    .map(|secs| format!(" elapsed={}", elapsed(*secs)))
                    .unwrap_or_default(),
                r.output
                    .as_ref()
                    .map(|v| format!("  out={v}"))
                    .unwrap_or_default(),
                r.note
                    .as_ref()
                    .map(|n| format!("  ({n})"))
                    .unwrap_or_default(),
            );
        }
    }
    let exit = match &out.exit {
        PlanExit::Completed => "completed".to_string(),
        PlanExit::Truncated { task } => format!("truncated at {task}"),
        PlanExit::ShortCircuit { task } => format!("short-circuited at {task}"),
        PlanExit::BudgetExceeded => "budget exceeded".to_string(),
        PlanExit::TimeExceeded => "wall-clock ceiling reached".to_string(),
        PlanExit::CompletedEarly { task, reason } => match reason {
            Some(reason) => format!("completed early by {task}: {reason}"),
            None => format!("completed early by {task}"),
        },
    };
    if let Some(p) = evidence.as_ref().filter(|_| playbook)
        && let Err(error) = record_git_memory(p)
    {
        eprintln!("git memory could not be kept for a rerun: {error:#}");
    }
    if let (Some(f), Some(record)) = (&events, &series_entry)
        && !prior.as_ref().is_some_and(|p| p.recorded_history)
    {
        append(
            f,
            &SessionEvent::HistoryEntry {
                entry: crate::plan::record::run_entry(&plan, record.as_ref(), &out.results),
            },
        );
    }
    if let Some(f) = &events {
        append(
            f,
            &SessionEvent::Shutdown {
                outcome: shutdown_outcome(&out).to_string(),
                reason: exit.clone(),
            },
        );
    }
    if let Some(p) = &evidence {
        crate::evidence::deliver_run_evidence(p);
    }
    println!(
        "plan v{}: {} — spent ${:.4} of ${}",
        plan.plan().version,
        exit,
        out.spent_usd,
        plan.plan().budget.usd
    );
    if !out.valid {
        return Err(NoValidVerdict {
            exit: exit.to_string(),
        }
        .into());
    }
    println!("verdict: valid");
    Ok(())
}

/// `secs` as the run summary prints a task's wall time: `4.2s`, `12m03s`, `1h05m09s`.
fn elapsed(secs: f64) -> String {
    if secs < 60.0 {
        return format!("{secs:.1}s");
    }
    let whole = std::time::Duration::try_from_secs_f64(secs)
        .unwrap_or_default()
        .as_secs();
    let (h, m, s) = (whole / 3600, whole / 60 % 60, whole % 60);
    match h {
        0 => format!("{m}m{s:02}s"),
        _ => format!("{h}h{m:02}m{s:02}s"),
    }
}

#[cfg(test)]
mod tests {
    use crate::plan::run::elapsed;

    #[test]
    fn a_tasks_wall_time_reads_in_the_largest_unit_it_reaches() {
        assert_eq!(elapsed(4.24), "4.2s");
        assert_eq!(elapsed(723.9), "12m03s");
        assert_eq!(elapsed(3909.0), "1h05m09s");
    }
}
