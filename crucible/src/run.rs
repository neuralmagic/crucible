//! Run setup and command dispatch: the glue between the parsed CLI and the loop.
//!
//! [`dispatch`] routes the subcommands and otherwise hands off to [`run_from_manifest`], the one
//! run path: a `crucible.toml` builds the [`World`] + [`Judge`], anchors every path, picks a
//! front-end, and calls [`crate::loop_driver::run_loop`].

use crate::crucible::{Judge, World};
use crate::loop_driver::{LoopRuntime, load_resume_state, run_loop};
use crate::{Args, Cli, Cmd, Paths, Prepared, STOP, Ui};
use crate::{
    agent, broker, check, console, control, deploy, init, manifest, publish, reporter, scope,
    stream,
};
use anyhow::{Context, Result};
use crucible_vcs::vcs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

/// Route the parsed CLI: subcommands run standalone; everything else is a manifest run.
pub(crate) fn dispatch(cli: Cli) -> Result<()> {
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
        let _engine = crate::engine::EngineCtx::new()?;
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
            (Some(addr), None) => crate::pr_watch::Sink::Steer(addr),
            (None, Some(path)) => crate::pr_watch::Sink::Reseed(path),
            (None, None) => {
                anyhow::bail!("watch-pr needs exactly one of --control-addr or --reseed")
            }
            (Some(_), Some(_)) => {
                anyhow::bail!("watch-pr takes exactly one of --control-addr or --reseed, not both")
            }
        };
        let opts = crate::pr_watch::WatchOpts {
            poll: std::time::Duration::from_secs(poll_secs),
            bot_user,
            authz: crate::pr_watch::Authz {
                allow_users: allow_user,
                ..Default::default()
            },
            once,
        };
        return crate::pr_watch::watch_and_steer(&pr, &sink, &opts);
    }

    if let Some(Cmd::Ps { namespace, json }) = cli.command {
        return crate::ps::run(namespace.as_deref(), json);
    }

    // Dispatch a named `[build.<name>]` declared in the domain manifest (cluster or github-actions
    // backend) and print the pinned digest, or `--check` the github input mapping against the workflow.
    if let Some(Cmd::Build(args)) = cli.command {
        return crate::build::run(args);
    }
    if let Some(Cmd::Fetch { uri, out }) = &cli.command {
        // The engine runtime the S3 GetObject block_ons on (published for `publish::fetch_object`).
        let _engine = crate::engine::EngineCtx::new()?;
        return publish::fetch_object(uri, out);
    }

    // One code-grounded ranking turn over an existing checkout. A cheap, checkout-backed agent turn
    // that gates scope spend by confirming an API-tier verdict. Prints verdict JSON. The controller
    // shells this from its escalation arm.
    if let Some(Cmd::RankGrounded(args)) = cli.command {
        // Constructing the engine runtime publishes the handle an openshell grounded turn reaches.
        let _engine = crate::engine::EngineCtx::new()?;
        return crate::rank_grounded::run(args);
    }

    if let Some(Cmd::Plan { action }) = &cli.command {
        return match action {
            crate::PlanAction::CompileWorkflow { file, manifest } => {
                crate::plan::cli::compile_workflow(file, manifest.as_deref())
            }
            crate::PlanAction::Show {
                file,
                caps,
                mermaid,
                render,
            } => crate::plan::cli::show(file, &caps.iter().cloned().collect(), *mermaid, *render),
            crate::PlanAction::Run {
                file,
                caps,
                agent_cmd,
                manifest,
            } => crate::plan::cli::run(
                file,
                &caps.iter().cloned().collect(),
                agent_cmd.clone(),
                manifest.as_deref(),
            ),
        };
    }

    if let Some(Cmd::Deploy { action }) = cli.command {
        // The WorkPod turn renderer has no manifest/controller shape, so it dispatches before the
        // render/apply split below.
        if let crate::DeployAction::RenderTurn(a) = action {
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
                    sandbox_image: a.sandbox_image,
                    max_cost: a.max_cost,
                    pin_digests: !a.no_pin,
                    tier: a.tier,
                    gaming_refine_rounds: a.gaming_refine_rounds,
                    skip_gaming_review: a.skip_gaming_review,
                    authoritative: a.authoritative,
                },
            );
        }
        let (args, apply) = match action {
            crate::DeployAction::Render(a) => (a, false),
            crate::DeployAction::Apply(a) => (a, true),
            crate::DeployAction::RenderTurn(_) => unreachable!("handled above"),
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
        let opts = deploy::RenderOpts {
            iterations: args.iterations,
            max_cost: args.max_cost,
            pin_digests: !args.no_pin,
            pr_repo: args.pr_repo.clone(),
            pack,
            clusters_file: args.clusters.clone(),
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
    let _engine = crate::engine::EngineCtx::new()?;
    run_from_manifest(cli.run)
}

fn parse_backend(s: &str) -> Result<agent::AgentBackend> {
    match s {
        "local" => Ok(agent::AgentBackend::Local),
        "openshell" => Ok(agent::AgentBackend::Openshell),
        "command" => Ok(agent::AgentBackend::Command),
        other => anyhow::bail!("[agent].backend must be local|openshell|command, got {other:?}"),
    }
}

/// Prepare the manifest's workspace: run `setup_cmd` (cwd = manifest dir, the one command that
/// runs there since it *creates* the workspace), else default to `git clone` of `[repo]`.
pub(crate) fn manifest_setup(
    m: &manifest::Manifest,
    manifest_dir: &Path,
    workspace: &Path,
) -> Result<()> {
    if let Some(cmd) = &m.workspace.setup_cmd {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(manifest_dir)
            .status()
            .with_context(|| format!("running setup_cmd: {cmd}"))?;
        if !status.success() {
            anyhow::bail!("setup_cmd failed ({status}): {cmd}");
        }
        return Ok(());
    }
    let src = m
        .repo
        .url
        .clone()
        .or_else(|| m.repo.path.clone())
        .context("[repo] needs url or path (or give [workspace].setup_cmd)")?;
    clone_repo(&src, m.repo.git_ref.as_deref(), workspace)
}

/// `git clone <src> <dest>` then optional `checkout <ref>`. The default-setup primitive, shared by the
/// single-domain `manifest_setup` and the per-component composite checkout.
pub(crate) fn clone_repo(src: &str, git_ref: Option<&str>, dest: &Path) -> Result<()> {
    let ok = std::process::Command::new("git")
        .args(["clone", src, &dest.to_string_lossy()])
        .status()
        .context("git clone")?
        .success();
    if !ok {
        anyhow::bail!("git clone {src} -> {} failed", dest.display());
    }
    if let Some(r) = git_ref {
        let _ = std::process::Command::new("git")
            .args(["-C", &dest.to_string_lossy(), "checkout", r])
            .status();
    }
    Ok(())
}

/// Build a plan runner over a manifest's agent config: the workspace is set up (or reused)
/// exactly as a loop run would, and `Agent` tasks run through the real harness path with the
/// manifest's `[agent]` defaults. Shares the loop's setup helpers so a plan run and a loop
/// run see the same world.
pub(crate) fn prep_plan_runner(
    manifest_path: &Path,
) -> Result<crate::plan::harness::HarnessRunner> {
    let m = manifest::Manifest::load_frozen(manifest_path)?;
    let manifest_dir = manifest_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let workspace = manifest_dir.join(&m.workspace.dir);
    let state = manifest_dir.join("state");
    let skills = m.agent.toolbox_dir.as_ref().map(|d| manifest_dir.join(d));
    let p = crate::Paths::for_manifest(workspace.clone(), state, &manifest_dir, skills);
    if !workspace.exists() {
        manifest_setup(&m, &manifest_dir, &workspace)?;
        for (src, dst, _frozen) in m.resolved_injects(&manifest_dir, &workspace) {
            manifest::apply_inject(&src, &dst)
                .context("applying [workspace].inject after setup")?;
        }
    }
    vcs::ensure_repo(&workspace).context("ensuring workspace is a git repo")?;
    std::fs::create_dir_all(&p.state)
        .with_context(|| format!("creating state dir {}", p.state.display()))?;
    install_toolbox(&p, &m.agent.toolbox_exclude, m.agent.harness.skills_dir())?;
    // Default Args (as if `crucible` ran flagless), then the manifest's [agent] folded on top —
    // the same resolution a loop run does.
    let mut args = <Cli as clap::Parser>::try_parse_from(["crucible"])
        .context("constructing default args")?
        .run;
    args.manifest = Some(manifest_path.to_path_buf());
    apply_agent_cfg(&mut args, &m.agent, &p.workspace)?;
    args.workflow_frozen_injects = m.frozen_inject_pairs(&manifest_dir);
    args.workflow_toolbox_exclude = m.agent.toolbox_exclude.clone();
    Ok(crate::plan::harness::HarnessRunner { args, paths: p })
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
    let m = manifest::Manifest::load_frozen(&manifest_path)?;
    // `parent()` of a bare `crucible.toml` is `Some("")`, which is not a usable cwd, treat
    // an empty parent as the current directory.
    let manifest_dir = manifest_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let workspace = manifest_dir.join(&m.workspace.dir);
    let state = args
        .state_dir
        .clone()
        .unwrap_or_else(|| manifest_dir.join("state"));
    let skills = m.agent.toolbox_dir.as_ref().map(|d| manifest_dir.join(d));
    let p = Paths::for_manifest(workspace.clone(), state, &manifest_dir, skills);

    if !workspace.exists() {
        manifest_setup(&m, &manifest_dir, &workspace)?;
        // Inject baked judge/fixture files into the fresh clone (frozen judges + one-time fixtures).
        // Frozen ones are also re-copied before each measure; the initial copy gives the agent a
        // present, compilable harness from turn one.
        for (src, dst, _frozen) in m.resolved_injects(&manifest_dir, &workspace) {
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
    install_toolbox(&p, &m.agent.toolbox_exclude, harness.skills_dir())?;

    // Fold the manifest's [agent] config onto Args (+ spawn the broker for openshell).
    let mut args = args;
    apply_agent_cfg(&mut args, &m.agent, &p.workspace)?;
    // Single-repo publish target: a `[publish] pr_repo` in the manifest wins over any `--pr-repo` the
    // caller passed (the controller passes its per-repo default via the flag; a pack that names its
    // own fork overrides it). Absent → keep the flag value (empty by default, so no PR opens).
    if let Some(pr_repo) = m.publish.as_ref().and_then(|p| p.pr_repo.clone()) {
        args.pr_repo = pr_repo;
    }

    let (goal, template) = resolve_goal_template(&args, &m.agent, &manifest_dir)?;
    // Cross-run memory: seed the prior run's tried-ideas ledger for this goal from S3 (best-effort),
    // so a fresh run (or a future harness version) inherits history instead of re-walking dead ends.
    let prior = publish::fetch_prior_results(&args.results_bucket, &goal).unwrap_or_default();
    if !prior.is_empty() {
        let n = prior.lines().count();
        eprintln!("seeded {n} prior tried-idea row(s) from S3 (cross-run memory)");
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
        skip_baseline: m.judge.skip_baseline,
    };

    // Frozen injects (the gate's own files) go to the judge so it re-establishes them before each
    // scored measure, the agent can't edit the harness/test to game the gate. Resolve before the
    // workspace move.
    let frozen_injects: Vec<(PathBuf, PathBuf)> = m
        .resolved_injects(&manifest_dir, &workspace)
        .into_iter()
        .filter(|(_, _, frozen)| *frozen)
        .map(|(src, dst, _)| (src, dst))
        .collect();
    args.search = m.search.clone();
    args.workflow = m.workflow.clone();
    args.workflow_frozen_injects = m.frozen_inject_pairs(&manifest_dir);
    args.workflow_toolbox_exclude = m.agent.toolbox_exclude.clone();
    let world = m.build_world(workspace.clone());
    let judge = m.build_judge(workspace, frozen_injects)?;

    drive_loop(args, p, prep, world, judge)
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
            clone_repo(&src, repo.git_ref.as_deref(), &c.workspace)
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
    install_toolbox(&p, &m.agent.toolbox_exclude, harness.skills_dir())?;

    let mut args = args;
    apply_agent_cfg(&mut args, &m.agent, &p.workspace)?;
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
    };

    args.search = m.search.clone();
    args.workflow = m.workflow.clone();
    args.workflow_frozen_injects = Vec::new();
    args.workflow_toolbox_exclude = m.agent.toolbox_exclude.clone();
    let world = m.build_world(&manifest_dir)?;
    let judge = m.build_judge(&manifest_dir)?;
    drive_loop(args, p, prep, world, judge)
}

/// Fold a manifest's `[agent]` config onto `Args` and, for the openshell backend, spawn the
/// provisioning broker. Shared by the single-domain and composite run paths.
fn apply_agent_cfg(args: &mut Args, agent: &manifest::AgentCfg, workspace: &Path) -> Result<()> {
    args.model = agent.model.clone();
    args.agent_cmd = agent.agent_cmd.clone();
    // Harness: CLI `--harness` wins, else the manifest's `[agent].harness` (default claude).
    if args.harness.is_none() {
        args.harness = Some(agent.harness);
    }
    args.hermes = agent.hermes.clone();
    // Reasoning effort: CLI `--effort` wins, else the manifest's `[agent].reasoning_effort`, else
    // `medium`, the loop IS the search, so heavy per-turn thinking mostly duplicates keep/discard.
    // A known-hard domain can still opt up to `high`/`max` in its manifest.
    if args.reasoning_effort.is_none() {
        args.reasoning_effort = agent.reasoning_effort;
    }
    if args.reasoning_effort.is_none() {
        args.reasoning_effort = Some(agent::ReasoningEffort::Medium);
    }
    // Backend: the manifest decides by default, but a CLI `--agent-backend openshell` overrides it
    // (the same manifest runs `local` on a laptop and `openshell` in the pod). Default
    // (Local) means "unset" → take the manifest's.
    if args.agent_backend == agent::AgentBackend::Local {
        args.agent_backend = parse_backend(&agent.backend)?;
    }
    // CLI `--sandbox-image` wins; else the manifest's.
    if args.sandbox_image.is_none() {
        args.sandbox_image = agent.sandbox_image.clone();
    }
    args.env = agent
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    // Openshell turns run claude inside the sandbox, whose env is built ONLY from `args.env`
    // (`openshell::run::env_script` / `vertex_config`, the sandbox never sees the pod's env).
    // A manifest without `[agent.env]` (a controller-drafted pack) would launch claude with no
    // Vertex config at all: it emits a synthetic "Not logged in" result claiming subtype=success,
    // turns=1, $0, a silent no-op run. Relay the standard Vertex keys from the pod env (the
    // deploy profile's `[env]`), exactly like the scope/rank turn paths; manifest values win.
    // Only for a Vertex-authenticated harness, hermes uses an env API key instead.
    if args.agent_backend == agent::AgentBackend::Openshell && args.harness().uses_vertex_provider()
    {
        crate::openshell::relay_vertex_env(&mut args.env);
    }
    args.relay = agent.relay.clone();
    args.openshell = agent.openshell.clone();
    args.broker = agent.broker.clone();

    // The provisioning broker: a run-lifetime child crucible spawns for the openshell
    // backend, reached by the sandboxed agent over streamable-http. Only that backend can reach it,
    // so don't spawn it for local/command.
    if args.broker.enabled && args.agent_backend == agent::AgentBackend::Openshell {
        // Hand the broker the deep loop's per-workspace sandbox name, its build sync must download
        // from the same sandbox the turns run in.
        let sandbox_name = crate::openshell::sandbox::name_for(workspace);
        args.broker_token =
            broker::ensure_running(&args.broker, &args.env, args.control_port, &sandbox_name)
                .context("starting the provisioning broker")?;
    }
    Ok(())
}

/// Resolve the run's goal + method-prompt template. `--goal`/`--goal-file` override the manifest (the
/// forge trigger injects a per-issue goal); otherwise the manifest's inline `goal` / `goal_file`.
fn resolve_goal_template(
    args: &Args,
    agent: &manifest::AgentCfg,
    manifest_dir: &Path,
) -> Result<(String, String)> {
    let goal = if let Some(g) = &args.goal {
        g.clone()
    } else if let Some(f) = &args.goal_file {
        std::fs::read_to_string(f)
            .with_context(|| format!("reading --goal-file {}", f.display()))?
    } else {
        match (&agent.goal, &agent.goal_file) {
            (Some(g), _) => g.clone(),
            (None, Some(f)) => std::fs::read_to_string(manifest_dir.join(f))
                .with_context(|| format!("reading goal_file {f}"))?,
            (None, None) => {
                anyhow::bail!("manifest [agent] needs `goal` or `goal_file` (or pass --goal)")
            }
        }
    };
    let template = match &agent.method_prompt {
        Some(mp) => std::fs::read_to_string(manifest_dir.join(mp))
            .with_context(|| format!("reading method_prompt {mp}"))?,
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
    world: Box<dyn World + Send>,
    judge: Box<dyn Judge + Send>,
) -> Result<()> {
    install_ctrlc()?;
    if args.control_port.is_some() && !args.resume && args.ui != Ui::Stream {
        anyhow::bail!("--control-port requires --ui stream (or --resume)");
    }

    // When the controller dispatched this loop pod, adopt its dispatch span as the run's trace
    // parent so Tempo shows controller → run → turn in one tree; the openshell turn spans nest under
    // this span because they're created on this same thread. `None` (a local run, or telemetry off)
    // leaves the turn spans rooting themselves independently. Held across the whole loop, then
    // dropped below so the span closes and the OTLP layer batches it before `flush`.
    let run_span = crate::engine::run_span(&p.workspace.to_string_lossy(), &prep.run_id);

    let outcome = {
        let _run_guard = run_span.as_ref().map(tracing::Span::enter);
        if args.resume {
            // Resume: replay the parked log, then continue in append (stream) mode.
            let resume = load_resume_state(&p.session_log)?;
            let meta = reporter::RunMeta::from_args(&args);
            let mut r = stream::SessionReporter::resume(&p, meta)?;
            let control = start_control_bridge(&args, &p)?;
            run_loop(
                &args,
                &p,
                &prep,
                &mut r,
                world.as_ref(),
                judge.as_ref(),
                LoopRuntime {
                    control: control.as_deref(),
                    resume: Some(resume),
                },
            )?
        } else {
            let meta = reporter::RunMeta::from_args(&args);
            match args.ui {
                Ui::Jsonl => {
                    let mut r = stream::SessionReporter::stdout(meta);
                    run_loop(
                        &args,
                        &p,
                        &prep,
                        &mut r,
                        world.as_ref(),
                        judge.as_ref(),
                        LoopRuntime::default(),
                    )?
                }
                Ui::Stream => {
                    let mut r = stream::SessionReporter::stream(&p, meta)?;
                    let control = start_control_bridge(&args, &p)?;
                    run_loop(
                        &args,
                        &p,
                        &prep,
                        &mut r,
                        world.as_ref(),
                        judge.as_ref(),
                        LoopRuntime {
                            control: control.as_deref(),
                            resume: None,
                        },
                    )?
                }
                _ => {
                    let mut r = console::ConsoleReporter;
                    run_loop(
                        &args,
                        &p,
                        &prep,
                        &mut r,
                        world.as_ref(),
                        judge.as_ref(),
                        LoopRuntime::default(),
                    )?
                }
            }
        }
    };
    deliver_run_evidence(&p);
    // Close the run span (drop it, now that its guard is gone) so the OTLP layer batches it, THEN
    // flush: the loop exits via process::exit, which skips EngineCtx::Drop.
    drop(run_span);
    crate::engine::flush();
    std::process::exit(outcome.exit_code());
}

/// Deliver the finished run's evidence to the controller's Tier 2 drop-box when the loop pod
/// carries the ingest env (`CRUCIBLE_INGEST_URL` + token path + pod name). Best-effort and never
/// fatal: a missing drop-box (a local run, or an old controller that injected nothing) is a silent
/// no-op and the wrapper's `SESSION` delimiter is the controller's fallback; a failed POST is already
/// logged loudly inside [`crate::ingest_client::post_artifact`]. The run-session is the loop's
/// authoritative record; the otel log rides along only when the collector produced one.
fn deliver_run_evidence(p: &Paths) {
    let Some(cfg) = crate::ingest_client::IngestConfig::from_env() else {
        return;
    };
    deliver_artifact(
        &cfg,
        crucible_contract::ArtifactKind::RunSession,
        &p.session_log,
    );
    // R5's in-process OTLP collector captures `otel.jsonl` in the run (state) dir; deliver it when it
    // exists. Absent until R5 lands, so this is inert plumbing today.
    let otel = p.state.join("otel.jsonl");
    if otel.exists() {
        deliver_artifact(&cfg, crucible_contract::ArtifactKind::OtelLog, &otel);
    }
}

/// Read `path`, gzip it, and POST it to the drop-box as `kind`. A read/gzip failure is logged and
/// skipped (never fatal to the run); the POST itself never fails the caller, a dead drop-box lands
/// `delivered: false` on the manifest, which the controller surfaces.
fn deliver_artifact(
    cfg: &crate::ingest_client::IngestConfig,
    kind: crucible_contract::ArtifactKind,
    path: &Path,
) {
    let raw = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "[crucible] Tier 2: reading {} for {kind} delivery failed: {e}",
                path.display()
            );
            return;
        }
    };
    let gz = match gzip(&raw) {
        Ok(g) => g,
        Err(e) => {
            eprintln!(
                "[crucible] Tier 2: gzipping {} for {kind} delivery failed: {e}",
                path.display()
            );
            return;
        }
    };
    let art = crate::ingest_client::post_artifact(cfg, kind, &gz);
    if art.delivered {
        eprintln!(
            "[crucible] Tier 2: delivered {kind} ({} gzipped bytes) to the drop-box",
            art.bytes
        );
    }
}

/// Gzip `bytes` at the default compression level (the same discipline the scope transcript uses).
fn gzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::Write as _;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(bytes)?;
    enc.finish()
}

/// Ctrl+C stops cleanly at the next checkpoint.
fn install_ctrlc() -> Result<()> {
    ctrlc::set_handler(|| {
        STOP.store(true, Ordering::SeqCst);
        crate::pid_registry::kill_all();
        eprintln!("\n[crucible] interrupt received — wrapping up the current step…");
    })
    .context("installing Ctrl+C handler")
}

fn start_control_bridge(
    args: &Args,
    p: &Paths,
) -> Result<Option<std::sync::Arc<control::ControlState>>> {
    args.control_port
        .map(|port| control::spawn_bridge(port, p.clone()))
        .transpose()
}

/// Copy every non-excluded skill under `p.skills` into the workspace's `skills_dir` (the
/// harness's discovery path, see [`crate::harness::Harness::skills_dir`]).
/// `exclude` names setup-only skills (deployment config, workload capture) that the loop agent must
/// never see, see [`manifest::AgentCfg::toolbox_exclude`]. A name in `exclude` that doesn't
/// exist under the toolbox dir is a manifest bug (the exclusion is silently doing nothing), so
/// it's an error rather than a warning.
pub(crate) fn install_toolbox(p: &Paths, exclude: &[String], skills_dir: &str) -> Result<()> {
    let Some(src) = &p.skills else {
        return Ok(());
    };
    for name in exclude {
        if !src.join(name).is_dir() {
            anyhow::bail!(
                "[agent].toolbox_exclude names `{name}`, but no such skill dir exists under {}",
                src.display()
            );
        }
    }
    let dst = p.workspace.join(skills_dir);
    std::fs::create_dir_all(&dst)?;
    for entry in std::fs::read_dir(src)? {
        let skill = entry?.path();
        if skill.is_dir() {
            let Some(name) = skill.file_name() else {
                continue;
            };
            if exclude.iter().any(|e| e.as_str() == name) {
                continue;
            }
            let _ = std::fs::remove_dir_all(dst.join(name));
            let opts = fs_extra::dir::CopyOptions::new().overwrite(true);
            fs_extra::dir::copy(&skill, &dst, &opts)
                .with_context(|| format!("failed to copy skill {}", skill.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::fs;

    // A minimal `command`-backend manifest (no broker, no Vertex) so `apply_agent_cfg` is
    // side-effect-free; `effort_line` optionally sets `[agent].reasoning_effort`.
    fn manifest_toml(effort_line: &str) -> String {
        format!(
            r#"
            [repo]
            path = "."
            [judge]
            measure_cmd = "m"
            direction = "higher"
            objective = "v"
            [agent]
            backend = "command"
            agent_cmd = "a"
            goal = "g"
            {effort_line}
        "#
        )
    }

    fn args_from(argv: &[&str]) -> Args {
        crate::Cli::parse_from(argv).run
    }

    #[test]
    fn effort_defaults_to_medium_when_unset() {
        let m: manifest::Manifest = toml::from_str(&manifest_toml("")).unwrap();
        let mut a = args_from(&["crucible"]);
        apply_agent_cfg(&mut a, &m.agent, Path::new("ws")).unwrap();
        assert_eq!(a.reasoning_effort, Some(agent::ReasoningEffort::Medium));
    }

    #[test]
    fn manifest_effort_beats_the_default() {
        let m: manifest::Manifest =
            toml::from_str(&manifest_toml("reasoning_effort = \"max\"")).unwrap();
        let mut a = args_from(&["crucible"]);
        apply_agent_cfg(&mut a, &m.agent, Path::new("ws")).unwrap();
        assert_eq!(a.reasoning_effort, Some(agent::ReasoningEffort::Max));
    }

    #[test]
    fn harness_defaults_to_claude_and_manifest_sets_it() {
        let m: manifest::Manifest = toml::from_str(&manifest_toml("")).unwrap();
        let mut a = args_from(&["crucible"]);
        apply_agent_cfg(&mut a, &m.agent, Path::new("ws")).unwrap();
        assert_eq!(a.harness(), crate::harness::Harness::Claude);

        let m: manifest::Manifest = toml::from_str(&manifest_toml("harness = \"hermes\"")).unwrap();
        let mut a = args_from(&["crucible"]);
        apply_agent_cfg(&mut a, &m.agent, Path::new("ws")).unwrap();
        assert_eq!(a.harness(), crate::harness::Harness::Hermes);
    }

    #[test]
    fn cli_harness_beats_the_manifest() {
        let m: manifest::Manifest = toml::from_str(&manifest_toml("harness = \"hermes\"")).unwrap();
        let mut a = args_from(&["crucible", "--harness", "claude"]);
        apply_agent_cfg(&mut a, &m.agent, Path::new("ws")).unwrap();
        assert_eq!(a.harness(), crate::harness::Harness::Claude);
    }

    /// `[agent.hermes]` parses (and rides onto Args), and an unknown key inside it is a manifest
    /// error, not silently ignored config.
    #[test]
    fn hermes_subtable_parses_and_denies_unknown_fields() {
        let m: manifest::Manifest = toml::from_str(&format!(
            "{}\n[agent.hermes]\nmodel = \"anthropic/claude-haiku-4-5\"\n",
            manifest_toml("harness = \"hermes\"")
        ))
        .unwrap();
        let mut a = args_from(&["crucible"]);
        apply_agent_cfg(&mut a, &m.agent, Path::new("ws")).unwrap();
        assert_eq!(
            a.hermes.model.as_deref(),
            Some("anthropic/claude-haiku-4-5")
        );

        let err = toml::from_str::<manifest::Manifest>(&format!(
            "{}\n[agent.hermes]\nmodle = \"typo\"\n",
            manifest_toml("")
        ));
        assert!(err.is_err(), "unknown [agent.hermes] key must be rejected");
    }

    #[test]
    fn cli_effort_beats_manifest_and_default() {
        let m: manifest::Manifest =
            toml::from_str(&manifest_toml("reasoning_effort = \"high\"")).unwrap();
        let mut a = args_from(&["crucible", "--effort", "low"]);
        apply_agent_cfg(&mut a, &m.agent, Path::new("ws")).unwrap();
        assert_eq!(a.reasoning_effort, Some(agent::ReasoningEffort::Low));
    }

    /// A manifest without `[agent.env]` (a controller-drafted pack) still gets the pod's Vertex
    /// config into the sandbox env for the openshell backend, otherwise claude launches with no
    /// credentials and no-ops the whole run (synthetic "Not logged in" result, subtype=success, $0).
    #[test]
    fn openshell_backend_relays_pod_vertex_env_when_manifest_has_none() {
        let _guard = crate::test_env_lock();
        unsafe {
            std::env::set_var("CLAUDE_CODE_USE_VERTEX", "1");
        }
        unsafe {
            std::env::set_var("ANTHROPIC_VERTEX_PROJECT_ID", "proj-from-pod");
        }

        let m: manifest::Manifest = toml::from_str(
            r#"
            [repo]
            path = "."
            [judge]
            measure_cmd = "m"
            direction = "higher"
            objective = "v"
            [agent]
            backend = "openshell"
            goal = "g"
        "#,
        )
        .unwrap();
        let mut a = args_from(&["crucible"]);
        let result = apply_agent_cfg(&mut a, &m.agent, Path::new("ws"));
        unsafe {
            std::env::remove_var("CLAUDE_CODE_USE_VERTEX");
        }
        unsafe {
            std::env::remove_var("ANTHROPIC_VERTEX_PROJECT_ID");
        }
        result.unwrap();

        let get = |k: &str| a.env.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.clone());
        assert_eq!(get("CLAUDE_CODE_USE_VERTEX").as_deref(), Some("1"));
        assert_eq!(
            get("ANTHROPIC_VERTEX_PROJECT_ID").as_deref(),
            Some("proj-from-pod")
        );
    }

    /// The manifest's `[agent.env]` stays authoritative, the relay only fills missing keys.
    #[test]
    fn manifest_agent_env_wins_over_the_pod_env() {
        let _guard = crate::test_env_lock();
        unsafe {
            std::env::set_var("ANTHROPIC_VERTEX_PROJECT_ID", "proj-from-pod");
        }

        let m: manifest::Manifest = toml::from_str(
            r#"
            [repo]
            path = "."
            [judge]
            measure_cmd = "m"
            direction = "higher"
            objective = "v"
            [agent]
            backend = "openshell"
            goal = "g"
            [agent.env]
            ANTHROPIC_VERTEX_PROJECT_ID = "proj-from-manifest"
        "#,
        )
        .unwrap();
        let mut a = args_from(&["crucible"]);
        let result = apply_agent_cfg(&mut a, &m.agent, Path::new("ws"));
        unsafe {
            std::env::remove_var("ANTHROPIC_VERTEX_PROJECT_ID");
        }
        result.unwrap();

        let vals: Vec<&str> = a
            .env
            .iter()
            .filter(|(k, _)| k == "ANTHROPIC_VERTEX_PROJECT_ID")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(vals, ["proj-from-manifest"]);
    }

    /// The relay is an openshell-only bridge: a command/local turn inherits the process env
    /// natively, so `args.env` stays exactly the manifest's.
    #[test]
    fn command_backend_does_not_relay_the_pod_env() {
        let _guard = crate::test_env_lock();
        unsafe {
            std::env::set_var("CLAUDE_CODE_USE_VERTEX", "1");
        }

        let m: manifest::Manifest = toml::from_str(&manifest_toml("")).unwrap();
        let mut a = args_from(&["crucible"]);
        let result = apply_agent_cfg(&mut a, &m.agent, Path::new("ws"));
        unsafe {
            std::env::remove_var("CLAUDE_CODE_USE_VERTEX");
        }
        result.unwrap();

        assert!(
            a.env.is_empty(),
            "no relay for the command backend: {:?}",
            a.env
        );
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crucible-run-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).expect("mkdir tmp");
        dir
    }

    /// The run-session/otel gzip helper round-trips: what the controller decompresses out of the
    /// drop-box is byte-for-byte the session NDJSON the loop delivered.
    #[test]
    fn gzip_roundtrips_the_session_ndjson() {
        use std::io::Read as _;
        let ndjson = "{\"v\":1,\"kind\":\"row\"}\n{\"v\":1,\"kind\":\"shutdown\"}\n";
        let gz = gzip(ndjson.as_bytes()).expect("gzip");
        assert!(
            gz != ndjson.as_bytes(),
            "the bytes were actually compressed"
        );
        let mut out = String::new();
        flate2::read::GzDecoder::new(gz.as_slice())
            .read_to_string(&mut out)
            .expect("gunzip");
        assert_eq!(out, ndjson);
    }

    /// A toolbox source dir with two skills; `install_toolbox` must copy only the one not named in
    /// `toolbox_exclude`, the setup-only skill never reaches the workspace `.claude/skills`.
    #[test]
    fn install_toolbox_skips_excluded_skills() {
        let dir = tempdir("toolbox-exclude");
        let skills = dir.join("skills");
        fs::create_dir_all(skills.join("rig-config")).unwrap();
        fs::write(skills.join("rig-config/SKILL.md"), "setup-only").unwrap();
        fs::create_dir_all(skills.join("bench")).unwrap();
        fs::write(skills.join("bench/SKILL.md"), "measure").unwrap();
        let workspace = dir.join("workspace");
        fs::create_dir_all(&workspace).unwrap();

        let p = Paths::for_manifest(workspace.clone(), dir.join("state"), &dir, Some(skills));
        install_toolbox(&p, &["rig-config".to_string()], ".claude/skills")
            .expect("install_toolbox");

        assert!(
            !workspace.join(".claude/skills/rig-config").exists(),
            "excluded skill must not reach the workspace"
        );
        assert!(
            workspace.join(".claude/skills/bench/SKILL.md").exists(),
            "the non-excluded skill must still be copied"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// A `toolbox_exclude` name that doesn't exist under the toolbox dir is a manifest bug (the
    /// exclusion is silently doing nothing), `install_toolbox` must refuse to proceed rather than
    /// quietly no-op.
    #[test]
    fn install_toolbox_errors_on_an_exclude_name_that_does_not_exist() {
        let dir = tempdir("toolbox-exclude-typo");
        let skills = dir.join("skills");
        fs::create_dir_all(skills.join("bench")).unwrap();
        let workspace = dir.join("workspace");
        fs::create_dir_all(&workspace).unwrap();

        let p = Paths::for_manifest(workspace.clone(), dir.join("state"), &dir, Some(skills));
        let err =
            install_toolbox(&p, &["rig-cofnig-typo".to_string()], ".claude/skills").unwrap_err();
        assert!(err.to_string().contains("rig-cofnig-typo"));

        let _ = fs::remove_dir_all(&dir);
    }
}
