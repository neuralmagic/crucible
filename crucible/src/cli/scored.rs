//! The scored loop's run path: a `crucible.toml` builds the [`World`] + [`Judge`], anchors every
//! path, picks a front-end, and calls [`crate::runloop::driver::run_loop`].

use crate::agent::harness::HarnessRuntime;
use crate::args::{Args, Paths, Prepared, Ui};
use crate::control;
use crate::control::recovery::{RecoveryPlan, ResumeRecovery, classify_session, plan_recovery};
use crate::errors::FileError;
use crate::manifest;
use crate::process::STOP;
use crate::report;
use crate::report::console;
use crate::report::stream;
use crate::runloop::driver::{LoopRuntime, run_loop};
use crate::runloop::publish;
use anyhow::{Context, Result};
use crucible::crucible::{Judge, World};
use crucible_vcs::vcs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// The scored run's own failures: CLI-flag combinations the parser can't express, workspace
/// setup, and the manifest fields a run needs.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ScoredError {
    #[error("watch-pr needs exactly one of --control-addr or --reseed")]
    WatchPrNoSink,
    #[error("watch-pr takes exactly one of --control-addr or --reseed, not both")]
    WatchPrTwoSinks,
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

/// `crucible watch-pr`: poll draft PRs' review comments and steer a live run or reseed the next.
pub(crate) fn watch_pr(
    pr: &[String],
    control_addr: Option<String>,
    reseed: Option<PathBuf>,
    bot_user: String,
    allow_user: Vec<String>,
    poll_secs: u64,
    once: bool,
) -> Result<()> {
    let sink = match (control_addr, reseed) {
        (Some(addr), None) => crate::control::pr_watch::Sink::Steer(addr),
        (None, Some(path)) => crate::control::pr_watch::Sink::Reseed(path),
        (None, None) => return Err(ScoredError::WatchPrNoSink.into()),
        (Some(_), Some(_)) => return Err(ScoredError::WatchPrTwoSinks.into()),
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
    crate::control::pr_watch::watch_and_steer(pr, &sink, &opts)
}

/// Load a `crucible.toml`, build the World + Judge from it, and drive the loop. The one run
/// path: every domain flows through here. Front-ends: headless / jsonl / stream,
/// plus `--resume`.
pub(crate) fn run_from_manifest(mut args: Args) -> Result<()> {
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
    // The toolbox lands where the resolved harness discovers skills.
    let harness = crate::cli::setup::pin_agent(&mut args, &m.agent)?;
    crate::cli::workspace::install_toolbox(
        &p,
        &m.agent.toolbox_exclude,
        harness.spec().skills_dir,
    )?;

    // Fold the manifest's [agent] config onto Args (+ spawn the broker for openshell).
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
fn run_composite(mut args: Args, manifest_path: PathBuf) -> Result<()> {
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
    let harness = crate::cli::setup::pin_agent(&mut args, &m.agent)?;
    crate::cli::workspace::install_toolbox(
        &p,
        &m.agent.toolbox_exclude,
        harness.spec().skills_dir,
    )?;

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
) -> Result<(String, String), ScoredError> {
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
            (None, None) => return Err(ScoredError::NoGoal),
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
    install_ctrlc()?;
    if args.control_port.is_some() && !args.resume && args.ui != Ui::Stream {
        return Err(ScoredError::ControlPortNeedsStream.into());
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
                    return Err(ScoredError::ResumeRefused { message }.into());
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
                    let meta = report::reporter::RunMeta::from_args(&args);
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
            let meta = report::reporter::RunMeta::from_args(&args);
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
) -> Result<Option<std::sync::Arc<control::bridge::ControlState>>> {
    args.control_port
        .map(|port| control::bridge::spawn_bridge(port, p.clone(), ledger.clone()))
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
