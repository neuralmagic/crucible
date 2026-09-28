//! Command dispatch: the glue between the parsed CLI and each command. With no subcommand the
//! scored loop runs ([`crate::cli::scored::run_from_manifest`]), when it is built.

use crate::cli::check;
use crate::cli::init;
use crate::cli::{Cli, Cmd, FlowArgs};
use crate::deploy;
use crate::flow;
use crate::manifest;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Dispatch-level failures. The top-level dispatch turns these into anyhow errors so the CLI
/// prints the whole chain.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RunError {
    #[error(
        "a playbook needs a positive --max-cost; its source may not declare a limit its operator set"
    )]
    PlaybookNeedsBudget,
    #[cfg(not(feature = "autoresearch"))]
    #[error(
        "this crucible was built without the scored loop; rebuild with `--features autoresearch`, \
         or run a playbook with `crucible plan run`"
    )]
    ScoredLoopNotBuilt,
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

    #[cfg(feature = "autoresearch")]
    if let Some(Cmd::Scope(args)) = cli.command {
        // Constructing the engine runtime publishes the handle a `--propose` openshell turn
        // reaches; held for the duration of `scope::run`.
        let _engine = crate::agent::engine::EngineCtx::new()?;
        return crate::scope::run(args);
    }

    #[cfg(feature = "autoresearch")]
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
        return crate::cli::scored::watch_pr(
            &pr,
            control_addr,
            reseed,
            bot_user,
            allow_user,
            poll_secs,
            once,
        );
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
        // The engine runtime the S3 GetObject block_ons on (for `object_store::fetch_object`).
        let _engine = crate::agent::engine::EngineCtx::new()?;
        return crate::object_store::fetch_object(uri, out);
    }

    // One code-grounded ranking turn over an existing checkout. A cheap, checkout-backed agent turn
    // that gates scope spend by confirming an API-tier verdict. Prints verdict JSON. The controller
    // shells this from its escalation arm.
    #[cfg(feature = "autoresearch")]
    if let Some(Cmd::LoopStates { format }) = &cli.command {
        print!(
            "{}",
            match format {
                crate::cli::StatesFormat::Markdown => crate::runloop::machine::markdown(),
                crate::cli::StatesFormat::Mermaid => crate::runloop::machine::mermaid(),
                crate::cli::StatesFormat::Dot => crate::runloop::machine::dot(),
            }
        );
        return Ok(());
    }

    #[cfg(feature = "autoresearch")]
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
            crate::cli::PlanAction::States { format, graph } => {
                let digraph = match graph {
                    crate::cli::StatesGraph::Task => crate::plan::machine::task_digraph,
                    crate::cli::StatesGraph::Plan => crate::plan::machine::plan_digraph,
                };
                print!(
                    "{}",
                    match format {
                        crate::cli::StatesFormat::Markdown => crate::plan::machine::markdown(),
                        crate::cli::StatesFormat::Mermaid => digraph().mermaid(),
                        crate::cli::StatesFormat::Dot => digraph().dot(),
                    }
                );
                Ok(())
            }
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
                max_asks,
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
                            asks: Some(*max_asks),
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
    #[cfg(feature = "autoresearch")]
    {
        let _engine = crate::agent::engine::EngineCtx::new()?;
        crate::cli::scored::run_from_manifest(cli.run)
    }
    #[cfg(not(feature = "autoresearch"))]
    {
        let _ = cli.run;
        Err(RunError::ScoredLoopNotBuilt.into())
    }
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

#[cfg(test)]
mod tests {
    use crate::cli::run::*;
    use crate::testing::args_from;
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
