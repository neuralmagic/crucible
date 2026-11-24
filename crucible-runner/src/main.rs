//! `crucible-runner`: a playbook's command, evaluate and report tasks and its output-decided
//! routes, run to a verdict. It carries no agent harness, calls no model, opens no decision desk
//! and mints no task tokens; a plan that needs one is refused before anything is dispatched.
//!
//! `plan run` takes the flags the full engine's does, so a pod wrapper rendered for one runs the
//! other.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Parser;
use crucible::host_env::HostEnv;
use crucible::manifest::Manifest;
use crucible::manifest::variant::VariantName;
use crucible::openshell::buildit::{self, RunSocket};
use crucible::paths::Paths;
use crucible::plan::attempt::Attempt;
use crucible::plan::exec::TaskRunner;
use crucible::plan::ir::{Isolation, Task, TaskKind, TaskName, ValidPlan};
use crucible::plan::refusal::{Refusal, task_refusals, workflow_refusals};
use crucible::plan::rerun::ParentRef;
use crucible::plan::run::{Ceilings, Engine, Launch, NoValidVerdict, Rerun, Source};
use crucible::plan::runner::ShellRunner;
use crucible::plan::setup::Start;
use crucible::plan::workspace_runner::{Dispatch, Job, WorkspaceRunner};
use crucible_contract::TransportCause;

#[derive(Parser)]
#[command(name = "crucible-runner", version, about)]
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    /// Print the controller/engine contract version and exit.
    #[arg(long, exclusive = true)]
    contract_version: bool,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Work with a pack's plan.
    Plan {
        #[command(subcommand)]
        action: PlanAction,
    },
}

#[derive(clap::Subcommand)]
enum PlanAction {
    /// Run the manifest's `[workflow]`, or the plan `--file` names, to a verdict. Exits nonzero
    /// when the plan does not reach a valid one.
    Run(RunArgs),
}

#[derive(clap::Args)]
struct RunArgs {
    /// The pack's manifest. Its workspace is set up as the full engine sets it up.
    #[arg(long)]
    manifest: PathBuf,
    /// A precompiled plan to run in the manifest's workspace, in place of the manifest's own
    /// `[workflow]`.
    #[arg(long)]
    file: Option<PathBuf>,
    /// A parameter value, `name=value`, repeatable.
    #[arg(long = "param", value_name = "NAME=VALUE")]
    params: Vec<String>,
    /// Total cost ceiling for the run, in USD. A playbook must be given one.
    #[arg(long)]
    max_cost: Option<f64>,
    /// Total wall-clock ceiling for the run (`90s`, `30m`, `2h`). A playbook must be given one.
    #[arg(long)]
    max_time: Option<String>,
    /// Substrate capabilities (repeatable). `any`-needs tasks always run.
    #[arg(long = "cap")]
    caps: Vec<String>,
    /// Where the full engine schedules an agent task's sandbox. Read and unused: this binary
    /// runs no agent task.
    #[arg(long, value_name = "DRIVER")]
    compute_driver: Option<String>,
    /// The full engine's agent harness flag. Refused: this binary runs no agent task.
    #[arg(long)]
    harness: Option<String>,
    /// The full engine's agent model flag. Refused: this binary runs no agent task.
    #[arg(long)]
    model: Option<String>,
    /// Continue the playbook run the manifest's state dir holds.
    #[arg(long)]
    resume: bool,
    /// The `[variant.<name>]` of the manifest to read; the base manifest without it.
    #[arg(long, value_name = "NAME")]
    variant: Option<VariantName>,
    /// Rerun a finished playbook run. PARENT is that run's state directory, or its evidence on
    /// the controller, fetched with the run token in CRUCIBLE_POD_TOKEN.
    #[arg(long, value_name = "PARENT")]
    rerun_of: Option<ParentRef>,
    /// A task the rerun dispatches again, with everything downstream of it. Repeatable.
    #[arg(long, value_name = "TASK", requires = "rerun_of")]
    from: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "--harness and --model choose what an agent task runs on, and crucible-runner runs no agent \
     task; launch this run on the full engine, or without them"
)]
struct AgentFlags;

/// Runs a task as a shell command in the root prepared for it.
#[derive(Clone)]
struct Commands {
    env: HostEnv,
    buildit_socket: RunSocket,
}

impl Dispatch for Commands {
    fn run_in(&self, paths: &Paths, task: &Task, job: Job<'_>) -> Attempt {
        if let Some(ask) = task.task.model_ask() {
            return Attempt::failed(
                0.0,
                Refusal::AsksModel {
                    task: task.name.clone(),
                    ask,
                }
                .to_string(),
            );
        }
        let buildit_socket = match task.task {
            TaskKind::Command { .. } | TaskKind::Evaluate { .. } => {
                match buildit::serve_tasks(&self.buildit_socket) {
                    Ok(socket) => socket,
                    Err(e) => return Attempt::transport(TransportCause::Other, format!("{e:#}")),
                }
            }
            _ => None,
        };
        let mut shell = ShellRunner {
            workdir: paths.workspace.clone(),
            agent_cmd: None,
            env: self.env.clone(),
            buildit_socket,
            github: false,
            task_servers: Vec::new(),
            model_route: None,
        };
        if task.isolation == Some(Isolation::Worktree) {
            shell.run_in_prepared_worktree(task, job.inputs, job.deadline, job.lane)
        } else {
            shell.run(task, job.attempt, job.inputs, job.lane)
        }
    }
}

struct Runner;

impl Engine for Runner {
    type Dispatch = Commands;

    fn prepare(
        &mut self,
        manifest: &Path,
        variant: Option<&VariantName>,
        params: &BTreeMap<String, String>,
        start: Start,
    ) -> Result<(WorkspaceRunner<Commands>, Manifest)> {
        let loaded = crucible::plan::setup::load(manifest, variant, params)?;
        if let Some(refusal) = loaded
            .manifest
            .workflow
            .as_ref()
            .and_then(|workflow| workflow_refusals(workflow).into_iter().next())
        {
            return Err(refusal.into());
        }
        loaded.checkout(start)?;
        let dispatch = Commands {
            env: loaded.host_env()?,
            buildit_socket: loaded.paths.buildit_socket(),
        };
        loaded.runner(dispatch)
    }

    fn admit(&self, plan: &ValidPlan) -> Result<()> {
        task_refusals(plan.tasks_topo())
            .next()
            .map_or(Ok(()), |refusal| Err(refusal.into()))
    }
}

fn run(args: RunArgs) -> Result<()> {
    if args.harness.is_some() || args.model.is_some() {
        return Err(AgentFlags.into());
    }
    crucible::plan::run::run(
        &mut Runner,
        Source::Manifest {
            manifest: &args.manifest,
            plan: args.file.as_deref(),
        },
        &crucible::plan::run::parse_params(&args.params)?,
        &args.caps.iter().cloned().collect(),
        Launch {
            ceilings: Ceilings {
                usd: args.max_cost,
                wall_clock: args
                    .max_time
                    .as_deref()
                    .and_then(crucible::duration::parse_duration),
                wall_clock_raw: args.max_time.clone(),
            },
            resume: args.resume,
            variant: args.variant.clone(),
            rerun: args.rerun_of.map(|parent| Rerun {
                parent,
                from: args.from.iter().cloned().map(TaskName).collect(),
            }),
        },
    )
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    if cli.contract_version {
        println!("{}", crucible_contract::CONTRACT_VERSION);
        return std::process::ExitCode::SUCCESS;
    }
    let Some(Cmd::Plan {
        action: PlanAction::Run(args),
    }) = cli.command
    else {
        use clap::CommandFactory;
        let _ = Cli::command().print_help();
        return std::process::ExitCode::from(2);
    };
    match run(args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:?}");
            std::process::ExitCode::from(if error.is::<NoValidVerdict>() {
                crucible::plan::INVALID_VERDICT_EXIT
            } else {
                1
            })
        }
    }
}
