//! Run setup and command dispatch: the glue between the parsed CLI and the loop.
//!
//! [`dispatch`] routes the subcommands and otherwise hands off to [`run_from_manifest`], the one
//! run path: a `crucible.toml` builds the [`World`] + [`Judge`], anchors every path, picks a
//! front-end, and calls [`crate::runloop::driver::run_loop`].

use crate::agent::harness::HarnessRuntime;
use crate::args::{Args, Paths, Prepared, Ui};
use crate::cli::check;
use crate::cli::init;
use crate::cli::{Cli, Cmd, FlowArgs};
use crate::control;
use crate::control::recovery::{RecoveryPlan, ResumeRecovery, classify_session, plan_recovery};
use crate::deploy;
use crate::errors::FileError;
use crate::flow;
use crate::manifest;
use crate::process::STOP;
use crate::report;
use crate::report::console;
use crate::report::stream;
use crate::runloop::driver::{LoopRuntime, run_loop};
use crate::runloop::publish;
use crate::scope;
use anyhow::{Context, Result};
use crucible::crucible::{Judge, World};
use crucible_vcs::vcs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// The run layer's own failures: CLI-flag combinations the parser can't express, workspace
/// setup, and the manifest fields a run needs. Causes hang off `source()`; the top-level
/// dispatch turns these into anyhow errors so the CLI prints the whole chain.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RunError {
    #[error("watch-pr needs exactly one of --control-addr or --reseed")]
    WatchPrNoSink,
    #[error("watch-pr takes exactly one of --control-addr or --reseed, not both")]
    WatchPrTwoSinks,
    #[error(
        "a playbook needs a positive --max-cost; its source may not declare a limit its operator set"
    )]
    PlaybookNeedsBudget,
    #[error("manifest [agent] needs `goal` or `goal_file` (or pass --goal)")]
    NoGoal,
    #[error("--control-port requires --ui stream (or --resume)")]
    ControlPortNeedsStream,
    #[error("resume: {message}")]
    ResumeRefused { message: String },
    #[error(transparent)]
    File(#[from] FileError),
    #[error(transparent)]
    Workspace(#[from] crate::cli::workspace::WorkspaceError),
}

/// Route the parsed CLI: subcommands run standalone; everything else is a manifest run.
pub(crate) fn dispatch(cli: Cli) -> Result<()> {
    if cli.contract_version {
        println!("{}", crucible_contract::CONTRACT_VERSION);
        return Ok(());
    }

    if let Some(Cmd::Init { dir }) = &cli.command {
        let dir = dir.clone().unwrap_or_else(|| PathBuf::from("."));
        return init::run(&dir);
    }

    if let Some(Cmd::Check {
        manifest,
        parse_only,
        profile,
        clusters,
    }) = &cli.command
    {
        let mut outcome = if *parse_only {
            check::run_parse_only(manifest)?
        } else {
            check::run(manifest)?
        };
        if let Some(profile) = profile {
            // Static wiring always; the live sandbox-SA Secret probe only on a full check.
            let p = check::check_profile(profile, clusters.as_deref(), !*parse_only);
            outcome.findings.extend(p.findings);
            outcome.warnings.extend(p.warnings);
        }
        for line in &outcome.exposure {
            println!("[crucible check] {line}");
        }
        for w in &outcome.warnings {
            eprintln!("[crucible check] WARNING: {w}");
        }
        for f in &outcome.findings {
            eprintln!("[crucible check] FAIL: {f}");
        }
        if outcome.ok() {
            println!("[crucible check] OK: {}", manifest.display());
            return Ok(());
        }
        std::process::exit(1);
    }

    if let Some(Cmd::Scope(args)) = cli.command {
        // Constructing the engine runtime publishes the handle a `--propose` openshell turn
        // reaches; held for the duration of `scope::run`.
        let _engine = crate::agent::engine::EngineCtx::new()?;
        return scope::run(args);
    }

    // Watch a draft PR's review comments and steer a live run (publish-on-keep closed loop). No
    // workspace/loop, just poll the forge and write to the run's control bridge until interrupted.
    if let Some(Cmd::WatchPr {
        pr,
        control_addr,
        reseed,
        bot_user,
        allow_user,
        poll_secs,
        once,
    }) = cli.command
    {
        let sink = match (control_addr, reseed) {
            (Some(addr), None) => crate::control::pr_watch::Sink::Steer(addr),
            (None, Some(path)) => crate::control::pr_watch::Sink::Reseed(path),
            (None, None) => return Err(RunError::WatchPrNoSink.into()),
            (Some(_), Some(_)) => return Err(RunError::WatchPrTwoSinks.into()),
        };
        let opts = crate::control::pr_watch::WatchOpts {
            poll: std::time::Duration::from_secs(poll_secs),
            bot_user,
            authz: crate::control::pr_watch::Authz {
                allow_users: allow_user,
                ..Default::default()
            },
            once,
        };
        return crate::control::pr_watch::watch_and_steer(&pr, &sink, &opts);
    }

    if let Some(Cmd::Ps { namespace, json }) = cli.command {
        return crate::cli::ps::run(namespace.as_deref(), json);
    }

    // Dispatch a named `[build.<name>]` declared in the domain manifest (cluster or github-actions
    // backend) and print the pinned digest, or `--check` the github input mapping against the workflow.
    if let Some(Cmd::Build(args)) = cli.command {
        return crate::cli::build::run(args);
    }
    // Pure file-to-file fold: no engine runtime, no workspace.
    if let Some(Cmd::Flow(args)) = &cli.command {
        return flow_cmd(args);
    }
    if let Some(Cmd::Fetch { uri, out }) = &cli.command {
        // The engine runtime the S3 GetObject block_ons on (published for `publish::fetch_object`).
        let _engine = crate::agent::engine::EngineCtx::new()?;
        return publish::fetch_object(uri, out);
    }

    // One code-grounded ranking turn over an existing checkout. A cheap, checkout-backed agent turn
    // that gates scope spend by confirming an API-tier verdict. Prints verdict JSON. The controller
    // shells this from its escalation arm.
    if let Some(Cmd::RankGrounded(args)) = cli.command {
        // Constructing the engine runtime publishes the handle an openshell grounded turn reaches.
        let _engine = crate::agent::engine::EngineCtx::new()?;
        return crate::scope::rank_grounded::run(args);
    }

    if let Some(Cmd::Plan { action }) = &cli.command {
        return match action {
            crate::cli::PlanAction::CompileWorkflow {
                file,
                manifest,
                params,
            } => crate::plan::cli::compile_workflow(
                file,
                manifest.as_deref(),
                &crate::plan::cli::parse_params(params)?,
            ),
            crate::cli::PlanAction::Show {
                file,
                caps,
                mermaid,
                render,
            } => crate::plan::cli::show(file, &caps.iter().cloned().collect(), *mermaid, *render),
            crate::cli::PlanAction::DslReference { format } => {
                match format {
                    crate::cli::DslFormat::Markdown => {
                        print!("{}", crate::plan::starlark::reference::markdown())
                    }
                    crate::cli::DslFormat::Json => println!(
                        "{}",
                        serde_json::to_string_pretty(&crate::plan::starlark::reference::json())?
                    ),
                }
                Ok(())
            }
            crate::cli::PlanAction::Exposure { manifest, pr_repo } => {
                let m = manifest::Manifest::load_frozen(manifest)?;
                let exposure = crate::exposure::compute(&m, pr_repo.as_deref());
                println!("{}", serde_json::to_string_pretty(&exposure)?);
                Ok(())
            }
            crate::cli::PlanAction::Params { file } => {
                let source = std::fs::read_to_string(file)
                    .with_context(|| format!("reading {}", file.display()))?;
                let schema = crate::plan::starlark::declared_params(&source, file)?;
                println!("{}", serde_json::to_string_pretty(&schema)?);
                Ok(())
            }
            crate::cli::PlanAction::Run {
                file,
                params,
                max_cost,
                max_time,
                caps,
                agent_cmd,
                manifest,
                compute_driver,
                harness,
                model,
            } => {
                let _engine = crate::agent::engine::EngineCtx::new()?;
                crate::plan::cli::run(
                    file.as_deref(),
                    &crate::plan::cli::parse_params(params)?,
                    &caps.iter().cloned().collect(),
                    agent_cmd.clone(),
                    manifest.as_deref(),
                    crate::plan::cli::RunOpts {
                        ceilings: crate::plan::cli::Ceilings {
                            usd: *max_cost,
                            wall_clock: max_time
                                .as_deref()
                                .and_then(crate::duration::parse_duration),
                            wall_clock_raw: max_time.clone(),
                        },
                        compute_driver: *compute_driver,
                        agent: crate::args::AgentOverride {
                            harness: *harness,
                            model: model.clone(),
                        },
                    },
                )
            }
        };
    }

    if let Some(Cmd::Deploy { action }) = cli.command {
        // The WorkPod turn renderer has no manifest/controller shape, so it dispatches before the
        // render/apply split below.
        if let crate::cli::DeployAction::RenderTurn(a) = action {
            let kind = deploy::TurnKind::parse_cli(&a.turn_kind)?;
            let goal_text = a
                .goal_file
                .as_ref()
                .map(|f| {
                    std::fs::read_to_string(f)
                        .with_context(|| format!("reading --goal-file {}", f.display()))
                })
                .transpose()?;
            return deploy::render_turn_cmd(
                &a.profile,
                &deploy::TurnOpts {
                    kind,
                    name: a.name,
                    issue: a.issue,
                    goal_text,
                    repo_url: a.repo_url,
                    repo_ref: a.repo_ref,
                    sandbox_image: a.sandbox_image,
                    max_cost: a.max_cost,
                    digests: (!a.no_pin).then(|| {
                        Arc::new(deploy::RegistryDigests) as Arc<dyn deploy::DigestResolver>
                    }),
                    tier: a.tier,
                    gaming_refine_rounds: a.gaming_refine_rounds,
                    skip_gaming_review: a.skip_gaming_review,
                    authoritative: a.authoritative,
                    harness: a.harness,
                    model: a.model,
                    pack_path: a
                        .pack_path
                        .as_deref()
                        .map(deploy::PackPath::parse)
                        .transpose()?,
                },
            );
        }
        let (args, apply) = match action {
            crate::cli::DeployAction::Render(a) => (a, false),
            crate::cli::DeployAction::Apply(a) => (a, true),
            crate::cli::DeployAction::RenderTurn(_) => unreachable!("handled above"),
        };
        let pack = args.pack.then(|| {
            // Default the CM name from the pack dir basename; the controller passes a run-unique one.
            let configmap_name = args.pack_configmap_name.clone().unwrap_or_else(|| {
                let domain = args
                    .manifest
                    .as_deref()
                    .and_then(Path::parent)
                    .and_then(Path::file_name)
                    .and_then(|n| n.to_str())
                    .unwrap_or("pack");
                format!("{domain}-pack")
            });
            deploy::PackDelivery { configmap_name }
        });
        let playbook = playbook_launch(&args)?;
        let opts = deploy::RenderOpts {
            iterations: args.iterations,
            max_cost: args.max_cost,
            digests: (!args.no_pin)
                .then(|| Arc::new(deploy::RegistryDigests) as Arc<dyn deploy::DigestResolver>),
            pr_repo: args.pr_repo.clone(),
            pack,
            clusters_file: args.clusters.clone(),
            harness: args.harness,
            model: args.model.clone(),
            playbook,
        };
        if args.controller {
            // Deprecated in favor of the `crucible-controller` Helm chart (the one packaging path).
            // Warn on stderr so piped stdout (`| kubectl apply -f -`) stays clean.
            eprintln!(
                "[crucible deploy] WARNING: --controller is deprecated and will be removed; \
                 package the controller with the Helm chart at deploy/charts/crucible-controller/ instead."
            );
            return if apply {
                deploy::apply_controller_cmd(&args.profile, &opts)
            } else {
                deploy::render_controller_cmd(&args.profile, &opts)
            };
        }
        let manifest = args
            .manifest
            .context("--manifest is required unless --controller is set")?;
        return if apply {
            deploy::apply_cmd(&manifest, &args.profile, &opts)
        } else {
            deploy::render_cmd(&manifest, &args.profile, &opts)
        };
    }

    // One run path: a `crucible.toml` manifest builds the World + Judge and anchors every path.
    // The engine runtime is created here, once; constructing it publishes the handle every async call site reaches
    // (the openshell turns, the swept S3 publish calls). Held to the end of the process (the loop
    // exits via `process::exit`), so the runtime stays alive under the loop.
    let _engine = crate::agent::engine::EngineCtx::new()?;
    run_from_manifest(cli.run)
}

/// `crucible flow`: gather the inputs (the session log, the span export from a file or Datadog),
/// render, and write `--out`.
fn flow_cmd(args: &FlowArgs) -> Result<()> {
    let session_log = std::fs::read_to_string(&args.session)
        .with_context(|| format!("reading {}", args.session.display()))?;
    // clap rejects --spans + --dd-trace together, so at most one arm produces spans.
    let spans_json = match (&args.spans, &args.dd_trace) {
        (Some(p), _) => {
            Some(std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?)
        }
        (None, Some(trace_id)) => Some(crate::report::flow_dd::fetch_trace_spans(
            trace_id,
            &args.dd_window,
        )?),
        (None, None) => None,
    };
    let ext = args
        .out
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    let format = flow::FlowFormat::from_extension(ext)?;
    let rendered = flow::render(
        &flow::FlowInput {
            session_log,
            spans_json,
        },
        format,
    )?;
    std::fs::write(&args.out, rendered)
        .with_context(|| format!("writing {}", args.out.display()))?;
    println!("[crucible flow] wrote {}", args.out.display());
    Ok(())
}

/// Fold `--playbook`'s flags into the renderer's launch knobs. clap enforces the flag
/// combinations; `--max-cost` carries a loop default of 0, so the budget is checked here.
fn playbook_launch(args: &crate::cli::DeployArgs) -> Result<Option<deploy::PlaybookLaunch>> {
    if !args.playbook {
        return Ok(None);
    }
    let max_time = args.max_time.context("--playbook requires --max-time")?;
    if !args.max_cost.is_finite() || args.max_cost <= 0.0 {
        return Err(RunError::PlaybookNeedsBudget.into());
    }
    Ok(Some(deploy::PlaybookLaunch {
        max_time,
        max_cost: args.max_cost,
        params: crate::plan::cli::parse_params(&args.params)?,
    }))
}

/// Load a `crucible.toml`, build the World + Judge from it, and drive the loop. The one run
/// path: every domain flows through here. Front-ends: headless / jsonl / stream,
/// plus `--resume`.
fn run_from_manifest(args: Args) -> Result<()> {
    let manifest_path = args.manifest.clone().context(
        "crucible needs a manifest: pass --manifest <crucible.toml> (see docs/crucible-contract.md)",
    )?;
    // A composite domain has a top-level `[composite]` table and a different shape; it runs
    // multiple component workspaces under one base, so it takes the dedicated path.
    if manifest::is_composite(&manifest_path) {
        return run_composite(args, manifest_path);
    }
    let mut m = manifest::Manifest::load_frozen(&manifest_path)?;
    // `parent()` of a bare `crucible.toml` is `Some("")`, which is not a usable cwd, treat
    // an empty parent as the current directory.
    let manifest_dir = manifest_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    m.resolve_workflow(&manifest_dir)?;
    let workspace = manifest_dir.join(&m.workspace.dir);
    let state = args
        .state_dir
        .clone()
        .unwrap_or_else(|| manifest_dir.join("state"));
    let skills = m.agent.toolbox_dir.as_ref().map(|d| manifest_dir.join(d));
    let p = Paths::for_manifest(workspace.clone(), state, &manifest_dir, skills);

    if !workspace.exists() {
        crate::cli::workspace::manifest_setup(&m, &manifest_dir, &workspace)?;
        // Inject baked judge/fixture files into the fresh clone (frozen judges + one-time fixtures).
        // Frozen ones are also re-copied before each measure; the initial copy gives the agent a
        // present, compilable harness from turn one.
        for (src, dst, _frozen) in m.resolved_injects(&manifest_dir, &workspace)? {
            manifest::apply_inject(&src, &dst)
                .context("applying [workspace].inject after setup")?;
        }
    }
    vcs::ensure_repo(&workspace).context("ensuring workspace is a git repo")?;
    std::fs::create_dir_all(&p.state)
        .with_context(|| format!("creating state dir {}", p.state.display()))?;
    // The toolbox lands where the resolved harness discovers skills (CLI `--harness` wins,
    // matching `apply_agent_cfg`'s resolution below).
    let harness = args.harness.unwrap_or(m.agent.harness);
    crate::cli::workspace::install_toolbox(&p, &m.agent.toolbox_exclude, harness.skills_dir())?;

    // Fold the manifest's [agent] config onto Args (+ spawn the broker for openshell).
    let mut args = args;
    let frozen = crate::cli::setup::frozen_projection(
        &m,
        m.publish
            .as_ref()
            .and_then(|p| p.pr_repo.as_deref())
            .or(Some(args.pr_repo.as_str())),
        &std::collections::BTreeMap::new(),
        &p.session_log,
    )?;
    crate::cli::setup::apply_agent_cfg(&mut args, &m.agent, &m.secrets, &p.workspace, &frozen)?;
    // Single-repo publish target: a `[publish] pr_repo` in the manifest wins over any `--pr-repo` the
    // caller passed (the controller passes its per-repo default via the flag; a pack that names its
    // own fork overrides it). Absent → keep the flag value (empty by default, so no PR opens).
    if let Some(pr_repo) = m.publish.as_ref().and_then(|p| p.pr_repo.clone()) {
        args.pr_repo = pr_repo;
    }
    // Declared pipeline artifacts, for the publish layer (PR-body embed + S3 upload).
    args.artifacts = m.workspace.artifact.clone();

    let (goal, template) = resolve_goal_template(&args, &m.agent, &manifest_dir)?;
    // Cross-run memory: seed the prior run's tried-ideas ledger for this goal from S3 (best-effort),
    // so a fresh run (or a future harness version) inherits history instead of re-walking dead ends.
    let prior = publish::fetch_prior_results(&args.results_bucket, &goal).unwrap_or_default();
    if !prior.is_empty() {
        let n = prior.lines().count();
        eprintln!("seeded {n} prior tried-idea row(s) from S3 (cross-run memory)");
    }
    if m.is_task() {
        eprintln!("task mode: no [judge] — every completed turn is kept and published unscored");
    }
    // The world's comparability key, computed once the workspace has a HEAD
    // to pin against.
    let identity = crate::identity::for_manifest(&manifest_path, &manifest_dir, &workspace, &m)
        .context("computing run identity")?;
    let prep = Prepared {
        run_id: publish::run_id(&goal),
        prior,
        goal,
        template,
        identity,
        skip_baseline: m.is_task() || m.judge.as_ref().is_some_and(|j| j.skip_baseline),
        preflight: m.preflight.clone(),
        preflight_modes: m
            .measure
            .as_ref()
            .and_then(crate::manifest::MeasureCfg::build_modes)
            .unwrap_or_default(),
        seed_diff: read_seed_diff(&manifest_dir, m.agent.seed_diff.as_deref())?,
    };

    // Frozen injects (the gate's own files) go to the judge so it re-establishes them before each
    // scored measure, the agent can't edit the harness/test to game the gate. Resolve before the
    // workspace move.
    let frozen_injects: Vec<(PathBuf, PathBuf)> = m
        .resolved_injects(&manifest_dir, &workspace)?
        .into_iter()
        .filter(|(_, _, frozen)| *frozen)
        .map(|(src, dst, _)| (src, dst))
        .collect();
    args.search = m.search.clone();
    args.workflow = m.workflow.clone();
    args.workflow_frozen_injects = m.frozen_inject_pairs(&manifest_dir)?;
    args.workflow_toolbox_exclude = m.agent.toolbox_exclude.clone();
    let world = m.build_world(workspace.clone());
    let judge = m.build_judge(workspace, frozen_injects)?;

    drive_loop(args, p, prep, world, judge)
}

/// Read the `[agent].seed_diff` content for iteration 1's prompt. The identity build hashes the
/// same file; a declared seed that can't be read errors there first, this context is a backstop.
fn read_seed_diff(manifest_dir: &Path, seed_diff: Option<&str>) -> Result<Option<String>> {
    seed_diff
        .map(|rel| {
            let path = manifest_dir.join(rel);
            std::fs::read_to_string(&path)
                .with_context(|| format!("reading [agent].seed_diff {}", path.display()))
        })
        .transpose()
}

/// Run a composite domain: set up each component's checkout under one base workspace, build
/// the multi-workspace [`CompositeWorld`] + the combined gate, and drive the same loop. The components
/// co-locate under the base so the agent has one cwd / one sandbox upload tree spanning both repos.
fn run_composite(args: Args, manifest_path: PathBuf) -> Result<()> {
    let m = manifest::CompositeManifest::load_frozen(&manifest_path)?;
    let manifest_dir = manifest_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let base = m.base_dir(&manifest_dir);
    let components = m.resolve_components(&manifest_dir)?;
    eprintln!(
        "composite `{}`: {} components — {}",
        m.composite.name,
        components.len(),
        components
            .iter()
            .map(|c| format!("{} ({})", c.name, c.domain_dir.display()))
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Check out each component into <base>/<name>, then make each its own git repo (the per-component
    // overlay CompositeWorld commits). The combined external deployment setup is a follow-up.
    for c in &components {
        if !c.workspace.exists() {
            let repo = &c.manifest.repo;
            let src = repo
                .url
                .clone()
                .or_else(|| repo.path.clone())
                .with_context(|| format!("component `{}` [repo] needs url or path", c.name))?;
            crate::cli::workspace::clone_repo(&src, repo.git_ref.as_deref(), &c.workspace)
                .with_context(|| format!("cloning component `{}`", c.name))?;
        }
        vcs::ensure_repo(&c.workspace)
            .with_context(|| format!("ensuring component `{}` is a git repo", c.name))?;
    }

    // The world's comparability key: one component entry per checkout, all
    // pinned now that every workspace has a HEAD.
    let identity = crate::identity::for_composite(&manifest_path, &base, &components, &m)
        .context("computing run identity")?;

    let state = args
        .state_dir
        .clone()
        .unwrap_or_else(|| manifest_dir.join("state"));
    let skills = m.agent.toolbox_dir.as_ref().map(|d| manifest_dir.join(d));
    // The agent's cwd is the base (it sees every component checkout as a subdir).
    let p = Paths::for_manifest(base, state, &manifest_dir, skills);
    std::fs::create_dir_all(&p.state)
        .with_context(|| format!("creating state dir {}", p.state.display()))?;
    let harness = args.harness.unwrap_or(m.agent.harness);
    crate::cli::workspace::install_toolbox(&p, &m.agent.toolbox_exclude, harness.skills_dir())?;

    let mut args = args;
    // A composite has no single-repo [publish]; its forks are per component.
    let bounds = crate::cli::setup::run_bounds(
        &m.outputs,
        &m.build,
        Some(args.pr_repo.as_str()),
        &std::collections::BTreeMap::new(),
    );
    let frozen = crate::cli::setup::FrozenProjection {
        broker_env: crate::cli::setup::broker_bounds_env(&bounds, &p.session_log)?,
        disclosure: Some(crate::exposure::covered_from(
            crate::exposure::composite_capabilities(&m.agent, &m.capabilities),
        )),
        bounds: Some(bounds),
    };
    crate::cli::setup::apply_agent_cfg(&mut args, &m.agent, &m.secrets, &p.workspace, &frozen)?;
    // The per-component fork map for publish-on-keep, manifest-owned via [[component]].pr_repo.
    args.component_pr_repos = m.component_pr_repos();
    let (goal, template) = resolve_goal_template(&args, &m.agent, &manifest_dir)?;
    let prior = publish::fetch_prior_results(&args.results_bucket, &goal).unwrap_or_default();
    let prep = Prepared {
        run_id: publish::run_id(&goal),
        prior,
        goal,
        template,
        identity,
        skip_baseline: m.judge.skip_baseline,
        preflight: m.preflight.clone(),
        preflight_modes: m
            .measure
            .as_ref()
            .and_then(crate::manifest::MeasureCfg::build_modes)
            .unwrap_or_default(),
        seed_diff: read_seed_diff(&manifest_dir, m.agent.seed_diff.as_deref())?,
    };

    args.search = m.search.clone();
    args.workflow = m.workflow.clone();
    args.workflow_frozen_injects = Vec::new();
    args.workflow_toolbox_exclude = m.agent.toolbox_exclude.clone();
    let world = m.build_world(&manifest_dir)?;
    let judge = m.build_judge(&manifest_dir)?;
    drive_loop(args, p, prep, world, judge)
}

/// Resolve the run's goal + method-prompt template. `--goal`/`--goal-file` override the manifest (the
/// forge trigger injects a per-issue goal); otherwise the manifest's inline `goal` / `goal_file`.
fn resolve_goal_template(
    args: &Args,
    agent: &manifest::AgentCfg,
    manifest_dir: &Path,
) -> Result<(String, String), RunError> {
    let goal = if let Some(g) = &args.goal {
        g.clone()
    } else if let Some(f) = &args.goal_file {
        std::fs::read_to_string(f).map_err(FileError::at("reading --goal-file", f))?
    } else {
        match (&agent.goal, &agent.goal_file) {
            (Some(g), _) => g.clone(),
            (None, Some(f)) => {
                let path = manifest_dir.join(f);
                std::fs::read_to_string(&path).map_err(FileError::at("reading goal_file", &path))?
            }
            (None, None) => return Err(RunError::NoGoal),
        }
    };
    let template = match &agent.method_prompt {
        Some(mp) => {
            let path = manifest_dir.join(mp);
            std::fs::read_to_string(&path).map_err(FileError::at("reading method_prompt", &path))?
        }
        None => "{{GOAL}}\n\nStatus: {{STATUS}}\n{{STEER}}".to_string(),
    };
    Ok((goal, template))
}

/// The shared loop tail: install Ctrl+C, then pick the front-end (resume / jsonl / stream /
/// console) and drive [`run_loop`]. Single-domain and composite runs both end here.
fn drive_loop(
    args: Args,
    p: Paths,
    prep: Prepared,
    world: Arc<dyn World>,
    judge: Arc<dyn Judge>,
) -> Result<()> {
    let mut args = args;
    install_ctrlc()?;
    if args.control_port.is_some() && !args.resume && args.ui != Ui::Stream {
        return Err(RunError::ControlPortNeedsStream.into());
    }

    if workflow_implies_graph_loop(&args) {
        args.graph_loop = true;
    }

    // When the controller dispatched this loop pod, adopt its dispatch span as the run's trace
    // parent so Tempo shows controller → run → turn in one tree; the openshell turn spans nest under
    // this span because they're created on this same thread. `None` (a local run, or telemetry off)
    // leaves the turn spans rooting themselves independently. Held across the whole loop, then
    // dropped below so the span closes and the OTLP layer batches it before `flush`.
    let run_span = crate::agent::engine::run_span(&p.workspace.to_string_lossy(), &prep.run_id);
    // A signal would otherwise kill the process with this span still open and the batch unflushed,
    // so every rolled loop pod loses its run span. Installed here, where the span exists, rather
    // than behind a static.
    crate::agent::engine::abort_on_signal(run_span.clone());

    // The liveness beat runs for the whole loop, parented to the run span so its beats hang off the
    // run in Tempo. Declared AFTER `run_span` so the guard's Drop runs first and the beat is joined
    // before the span it holds goes away, on the error returns below as well as the success path.
    let heartbeat = crate::control::heartbeat::period_from_env()
        .map(|period| crate::control::heartbeat::start(period, run_span.clone()));
    let beat = heartbeat
        .as_ref()
        .map(crate::control::heartbeat::BeatGuard::beat);

    let outcome = {
        let _run_guard = run_span.as_ref().map(tracing::Span::enter);
        if args.resume {
            // Replay the parked log, then continue in append mode. A NoOp exits 0
            // WITHOUT re-running the finish path (replaying finish re-published the
            // kept candidate each crash-loop lap); Refuse keeps exit code 2's meaning.
            let recovered = classify_session(&p.session_log)?;
            match plan_recovery(&recovered, args.iterations, args.max_cost) {
                RecoveryPlan::NoOp { message } => {
                    eprintln!("resume: {message}");
                    return Ok(());
                }
                RecoveryPlan::Refuse { message } => {
                    return Err(RunError::ResumeRefused { message }.into());
                }
                RecoveryPlan::Continue {
                    repark,
                    pending_regime,
                } => {
                    let recovery = ResumeRecovery {
                        class: recovered.classification.class(),
                        iter: recovered.classification.iter(),
                        detail: recovered.classification.detail(),
                        repark,
                        pending_regime,
                    };
                    let meta = report::RunMeta::from_args(&args);
                    let r = stream::SessionReporter::resume(&p, meta)?;
                    // Fold the prior run's admissions before the bridge is up, so no
                    // inbound command can land on a half-built index.
                    let ledger = open_admission_ledger(&p, forge::ndjson::Open::Fold)?;
                    let control = start_control_bridge(&args, &p, &ledger)?;
                    let (_reporter, outcome) = run_loop(
                        &args,
                        &p,
                        &prep,
                        r,
                        &world,
                        &judge,
                        LoopRuntime {
                            control: control.clone(),
                            resume: Some(recovered.resume),
                            recovery: Some(recovery),
                            ledger: Some(ledger),
                            heartbeat: beat.clone(),
                        },
                    );
                    outcome?
                }
            }
        } else {
            let meta = report::RunMeta::from_args(&args);
            match args.ui {
                Ui::Jsonl => {
                    let r = stream::SessionReporter::stdout(meta);
                    let (_reporter, outcome) = run_loop(
                        &args,
                        &p,
                        &prep,
                        r,
                        &world,
                        &judge,
                        LoopRuntime {
                            heartbeat: beat.clone(),
                            ..LoopRuntime::default()
                        },
                    );
                    outcome?
                }
                Ui::Stream => {
                    let r = stream::SessionReporter::stream(&p, meta)?;
                    // A fresh run must not inherit the last run's un-drained inputs.
                    let ledger = open_admission_ledger(&p, forge::ndjson::Open::Truncate)?;
                    let control = start_control_bridge(&args, &p, &ledger)?;
                    let (_reporter, outcome) = run_loop(
                        &args,
                        &p,
                        &prep,
                        r,
                        &world,
                        &judge,
                        LoopRuntime {
                            control: control.clone(),
                            ledger: Some(ledger),
                            heartbeat: beat.clone(),
                            ..LoopRuntime::default()
                        },
                    );
                    outcome?
                }
                _ => {
                    let r = console::ConsoleReporter;
                    let (_reporter, outcome) = run_loop(
                        &args,
                        &p,
                        &prep,
                        r,
                        &world,
                        &judge,
                        LoopRuntime {
                            heartbeat: beat.clone(),
                            ..LoopRuntime::default()
                        },
                    );
                    outcome?
                }
            }
        }
    };
    crate::report::ingest_client::deliver_run_evidence(&p);
    // Explicit because this path ends in `process::exit`, which runs no destructors: the beat
    // thread holds a clone of the run span, so a live beat would keep it open past the flush below.
    drop(heartbeat);
    // Close the run span (drop it, now that its guard is gone) so the OTLP layer batches it, THEN
    // flush: the loop exits via process::exit, which skips EngineCtx::Drop.
    drop(run_span);
    crate::agent::engine::flush();
    std::process::exit(outcome.exit_code());
}

/// Ctrl+C stops cleanly at the next checkpoint.
fn install_ctrlc() -> Result<()> {
    ctrlc::set_handler(|| {
        STOP.store(true, Ordering::SeqCst);
        crate::process::pid_registry::kill_all();
        eprintln!("\n[crucible] interrupt received — wrapping up the current step…");
    })
    .context("installing Ctrl+C handler")
}

fn start_control_bridge(
    args: &Args,
    p: &Paths,
    ledger: &std::sync::Arc<crate::control::admission::AdmissionLedger>,
) -> Result<Option<std::sync::Arc<control::ControlState>>> {
    args.control_port
        .map(|port| control::spawn_bridge(port, p.clone(), ledger.clone()))
        .transpose()
}

/// Every external input is recorded here before it takes effect, so this must exist
/// before anything can deliver one.
fn open_admission_ledger(
    p: &Paths,
    mode: forge::ndjson::Open,
) -> Result<std::sync::Arc<crate::control::admission::AdmissionLedger>> {
    crate::control::admission::AdmissionLedger::open(&p.admissions, mode).map(std::sync::Arc::new)
}

/// An engine-task workflow only executes on the graph path; running it hand-sequenced would
/// silently ignore the authored graph (and its sessions). Authoring one is the opt-in, so
/// `--graph-loop` is implied. Legacy splice workflows run on both paths and imply nothing.
fn workflow_implies_graph_loop(args: &Args) -> bool {
    args.workflow
        .as_ref()
        .is_some_and(|w| !w.is_legacy_splice())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{args_from, workflow_from};
    use clap::Parser;

    // A minimal `command`-backend manifest (no broker, no Vertex) so `apply_agent_cfg` is
    // side-effect-free; `effort_line` optionally sets `[agent].reasoning_effort`.
    #[test]
    fn a_model_less_run_takes_the_resolved_harness_default() {
        let a = args_from(&["crucible", "--harness", "codex"]);
        assert_eq!(a.model(), crate::manifest::Harness::Codex.default_model());
    }

    /// Without a manifest there is no `[agent]` table to replace, so the flags are refused rather
    /// than accepted and ignored.
    #[test]
    fn plan_run_agent_flags_need_a_manifest() {
        for flags in [["--harness", "codex"], ["--model", "gpt-5.6-luna"]] {
            let argv = [
                "crucible",
                "plan",
                "run",
                "--file",
                "plan.toml",
                flags[0],
                flags[1],
            ];
            let Err(err) = <crate::cli::Cli as clap::Parser>::try_parse_from(argv) else {
                panic!("an agent flag without --manifest is refused: {argv:?}");
            };
            assert!(err.to_string().contains("--manifest"), "{err}");
        }
        let argv = [
            "crucible",
            "plan",
            "run",
            "--manifest",
            "crucible.toml",
            "--harness",
            "codex",
            "--model",
            "gpt-5.6-luna",
        ];
        assert!(
            <crate::cli::Cli as clap::Parser>::try_parse_from(argv).is_ok(),
            "both flags parse with a manifest"
        );
    }

    #[test]
    fn engine_workflow_implies_graph_loop() {
        let mut a = args_from(&["crucible"]);
        a.workflow = Some(workflow_from(
            r#"
            result = "decide"
            [[task]]
            name = "propose"
            kind = "engine"
            op = "propose"
            [[task]]
            name = "apply"
            kind = "engine"
            op = "apply"
            depends_on = ["propose"]
            [[task]]
            name = "measure"
            kind = "engine"
            op = "measure"
            depends_on = ["apply"]
            [[task]]
            name = "decide"
            kind = "engine"
            op = "decide"
            source = "measure"
            depends_on = ["measure"]
        "#,
        ));
        assert!(workflow_implies_graph_loop(&a));
    }

    #[test]
    fn legacy_splice_workflow_does_not_imply_graph_loop() {
        let mut a = args_from(&["crucible"]);
        a.workflow = Some(workflow_from(
            r#"
            [[task]]
            name = "lint"
            kind = "command"
            command = "true"
        "#,
        ));
        assert!(!workflow_implies_graph_loop(&a));
    }

    #[test]
    fn no_workflow_does_not_imply_graph_loop() {
        let a = args_from(&["crucible"]);
        assert!(!workflow_implies_graph_loop(&a));
    }

    fn deploy_args(extra: &[&str]) -> crate::cli::DeployArgs {
        let mut argv = vec![
            "crucible",
            "deploy",
            "render",
            "--manifest",
            "pack/crucible.toml",
            "--profile",
            "profile.toml",
        ];
        argv.extend_from_slice(extra);
        match Cli::try_parse_from(argv).expect("argv parses").command {
            Some(Cmd::Deploy {
                action: crate::cli::DeployAction::Render(a),
            }) => a,
            _ => panic!("deploy render"),
        }
    }

    #[test]
    fn playbook_render_refuses_a_zero_budget() {
        for budget in ["--max-cost=0", "--max-cost=0.0", "--max-cost=-1"] {
            let args = deploy_args(&["--playbook", "--max-time", "30m", budget]);
            let err = playbook_launch(&args).expect_err(budget);
            assert!(
                err.to_string().contains("positive --max-cost"),
                "{budget}: {err:#}"
            );
        }
        let args = deploy_args(&["--playbook", "--max-time", "30m", "--max-cost", "4.5"]);
        let launch = playbook_launch(&args)
            .expect("a positive budget")
            .expect("a playbook launch");
        assert_eq!(launch.max_cost, 4.5);
        assert_eq!(launch.max_time.to_string(), "1800s");
    }

    /// A malformed `--param` is rejected once, at the CLI, so the renderer holds a map that cannot
    /// be malformed.
    #[test]
    fn playbook_render_refuses_a_param_without_a_value() {
        let args = deploy_args(&[
            "--playbook",
            "--max-time",
            "30m",
            "--max-cost",
            "4.5",
            "--param",
            "topic",
        ]);
        assert!(playbook_launch(&args).is_err());
    }
}
