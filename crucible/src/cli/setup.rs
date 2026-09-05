//! Resolving a run from its manifest: the agent config folded onto the CLI args, the frozen
//! projection the broker sees, and a plan runner over the same workspace a loop run would use.

use crate::agent::harness::HarnessRuntime;
use crate::args::Args;
use crate::control::broker;
use crate::manifest;
use anyhow::{Context, Result};
use crucible_vcs::vcs;
use std::path::Path;

/// What the frozen manifest projects onto a run: the bounds the broker enforces, and the grants
/// the capability disclosure covers.
#[derive(Default)]
pub(crate) struct FrozenProjection {
    /// `BROKER_OUTPUTS` and friends, handed to the broker child only. The agent never sees them,
    /// so nothing inside the sandbox can alter a bound.
    pub(crate) broker_env: Vec<(String, String)>,
    pub(crate) disclosure: Option<crate::exposure::Covered>,
    /// The same resolved bounds the broker is handed, for the two kinds the engine writes itself.
    pub(crate) bounds: Option<crate::outputs::RunBounds>,
}

/// Resolve the frozen manifest's output bounds and capability disclosure for a run.
pub(crate) fn frozen_projection(
    m: &manifest::Manifest,
    pr_repo: Option<&str>,
    params: &std::collections::BTreeMap<String, String>,
    session_log: &Path,
) -> Result<FrozenProjection> {
    let bounds = run_bounds(&m.outputs, &m.build, pr_repo, params);
    Ok(FrozenProjection {
        broker_env: broker_bounds_env(&bounds, session_log)?,
        disclosure: Some(crate::exposure::covered(m)),
        bounds: Some(bounds),
    })
}

/// Fold the pack's `[outputs]` onto the engine default table.
pub(crate) fn run_bounds(
    outputs: &manifest::OutputsCfg,
    builds: &std::collections::BTreeMap<String, forge::spec::BuildSpec>,
    pr_repo: Option<&str>,
    params: &std::collections::BTreeMap<String, String>,
) -> crate::outputs::RunBounds {
    let defaults = manifest::outputs::default_targets(pr_repo, builds);
    crate::outputs::RunBounds::new(
        manifest::outputs::resolve(outputs, &defaults),
        params.clone(),
    )
}

/// Project the run's resolved output bounds into the environment the broker child reads.
pub(crate) fn broker_bounds_env(
    bounds: &crate::outputs::RunBounds,
    session_log: &Path,
) -> Result<Vec<(String, String)>> {
    Ok(vec![
        (
            crucible_contract::outputs::ENV_OUTPUTS.to_string(),
            serde_json::to_string(bounds.resolved())
                .context("serializing the resolved output bounds")?,
        ),
        (
            crucible_contract::outputs::ENV_OUTPUT_PARAMS.to_string(),
            serde_json::to_string(bounds.params())
                .context("serializing the run's bound parameters")?,
        ),
        (
            crucible_contract::outputs::ENV_SESSION_LOG.to_string(),
            session_log.display().to_string(),
        ),
    ])
}

/// Fold a manifest's `[agent]` config onto `Args` and, for the openshell backend, spawn the
/// provisioning broker. Shared by the single-domain and composite run paths.
pub(crate) fn apply_agent_cfg(
    args: &mut Args,
    agent: &manifest::AgentCfg,
    secrets: &[manifest::SecretDecl],
    workspace: &Path,
    frozen: &FrozenProjection,
) -> Result<()> {
    // Model and harness: the CLI flag wins, else the manifest's `[agent]` value.
    if args.model.is_none() {
        args.model = Some(agent.model.clone());
    }
    args.agent_cmd = agent.agent_cmd.clone();
    if args.harness.is_none() {
        args.harness = Some(agent.harness);
    }
    args.hermes = agent.hermes.clone();
    args.codex = agent.codex.clone();
    args.disallowed_tools = agent.disallowed_tools.clone();
    // Reasoning effort: CLI `--effort` wins, else the manifest's `[agent].reasoning_effort`, else
    // `medium`, the loop IS the search, so heavy per-turn thinking mostly duplicates keep/discard.
    // A known-hard domain can still opt up to `high`/`max` in its manifest.
    if args.reasoning_effort.is_none() {
        args.reasoning_effort = agent.reasoning_effort;
    }
    if args.reasoning_effort.is_none() {
        args.reasoning_effort = Some(crate::manifest::ReasoningEffort::Medium);
    }
    // Backend: the manifest decides by default, but a CLI `--agent-backend openshell` overrides it
    // (the same manifest runs `local` on a laptop and `openshell` in the pod). Default
    // (Local) means "unset" → take the manifest's.
    if args.agent_backend == manifest::AgentBackend::Local {
        args.agent_backend = agent.backend;
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
    // Only for a Vertex-authenticated harness; codex authenticates against the ChatGPT backend.
    if args.agent_backend == manifest::AgentBackend::Openshell
        && crate::agent::harness::resolve_auth(
            args.harness(),
            &crate::agent::inference::InferenceEnv::from_process_env()?,
        ) == crate::agent::harness::AuthProvider::Vertex
    {
        crate::openshell::run::relay_vertex_env(&mut args.env);
    }
    // The pack's declared secrets, for the ones the registry says this agent may hold. The kubelet
    // put them in this process's environment; without this the sandbox never sees them.
    crate::openshell::run::relay_agent_visible_secrets(secrets, &mut args.env);
    // Who the agent's commits are attributed to. Same reason: the sandbox never sees the pod's env,
    // so the identity the controller named for this run has to be relayed like everything else.
    crate::openshell::run::relay_identity_env(&mut args.env);
    args.relay = agent.relay.clone();
    args.disclosure = frozen.disclosure.clone();
    args.output_bounds = frozen.bounds.clone();
    args.openshell = agent.openshell.clone();
    args.broker = agent.broker.clone();

    // The provisioning broker: a run-lifetime child crucible spawns for the openshell
    // backend, reached by the sandboxed agent over streamable-http. Only that backend can reach it,
    // so don't spawn it for local/command.
    if args.broker.enabled && args.agent_backend == manifest::AgentBackend::Openshell {
        // Hand the broker the deep loop's per-workspace sandbox name, its build sync must download
        // from the same sandbox the turns run in.
        let sandbox_name = crate::openshell::sandbox::name_for(workspace);
        args.broker_token = broker::ensure_running(
            &args.broker,
            &args.env,
            &frozen.broker_env,
            args.control_port,
            &sandbox_name,
        )
        .context("starting the provisioning broker")?;
    }
    Ok(())
}

/// Build a plan runner over a manifest's agent config: the workspace is set up (or reused)
/// exactly as a loop run would, and `Agent` tasks run through the real harness path with the
/// manifest's `[agent]` defaults. Shares the loop's setup helpers so a plan run and a loop
/// run see the same world.
/// The runner for a manifest, and the manifest it was built from. Both, because the caller needs
/// the graph and the lane off the manifest and compiling it twice would compile the pack twice.
/// Prepare with no supplied parameters: the shape the tests want, gated so an uncalled function
/// stays out of the binary.
#[cfg(test)]
pub(crate) fn prep_plan_runner(
    manifest_path: &Path,
) -> Result<(crate::plan::harness::HarnessRunner, manifest::Manifest)> {
    prep_plan_runner_with_params(
        manifest_path,
        &std::collections::BTreeMap::new(),
        crate::openshell::gateway::ComputeDriver::Podman,
        crate::args::AgentOverride::default(),
    )
}

pub(crate) fn prep_plan_runner_with_params(
    manifest_path: &Path,
    params: &std::collections::BTreeMap<String, String>,
    compute_driver: crate::openshell::gateway::ComputeDriver,
    agent: crate::args::AgentOverride,
) -> Result<(crate::plan::harness::HarnessRunner, manifest::Manifest)> {
    let mut m = manifest::Manifest::load_frozen(manifest_path)?;
    let manifest_dir = manifest::manifest_dir(manifest_path);
    m.resolve_workflow_with(&manifest_dir, params)?;
    let workspace = manifest_dir.join(&m.workspace.dir);
    let state = manifest_dir.join("state");
    let skills = m.agent.toolbox_dir.as_ref().map(|d| manifest_dir.join(d));
    let p = crate::args::Paths::for_manifest(workspace.clone(), state, &manifest_dir, skills);
    if !workspace.exists() {
        crate::cli::workspace::manifest_setup(&m, &manifest_dir, &workspace)?;
        for (src, dst, _frozen) in m.resolved_injects(&manifest_dir, &workspace)? {
            manifest::apply_inject(&src, &dst)
                .context("applying [workspace].inject after setup")?;
        }
    }
    vcs::ensure_repo(&workspace).context("ensuring workspace is a git repo")?;
    // The harness's own files, plus where a task's staged inputs land. Without this a playbook's
    // per-task commit sweeps in the toolbox, the results log, and every artifact an ancestor
    // handed this task, and each task's commit reads as though it produced all of them.
    crucible_vcs::git_memory::install_harness_excludes(
        &workspace,
        &[format!("{}/", crate::plan::STAGED_INPUTS)],
    );
    std::fs::create_dir_all(&p.state)
        .with_context(|| format!("creating state dir {}", p.state.display()))?;
    let harness = agent.harness.unwrap_or(m.agent.harness);
    crate::cli::workspace::install_toolbox(&p, &m.agent.toolbox_exclude, harness.skills_dir())?;
    // Default Args (as if `crucible` ran flagless) carrying the launch's own agent flags, then
    // the manifest's [agent] folded on top — the same resolution a loop run does.
    let mut args = Args::defaults().context("constructing default args")?;
    args.manifest = Some(manifest_path.to_path_buf());
    args.compute_driver = compute_driver;
    args.harness = agent.harness;
    args.model = agent.model;
    let frozen = frozen_projection(
        &m,
        m.publish.as_ref().and_then(|p| p.pr_repo.as_deref()),
        params,
        &p.session_log,
    )?;
    apply_agent_cfg(&mut args, &m.agent, &m.secrets, &p.workspace, &frozen)?;
    args.workflow_frozen_injects = m.frozen_inject_pairs(&manifest_dir)?;
    args.workflow_toolbox_exclude = m.agent.toolbox_exclude.clone();
    // A playbook's git memory is per task; the scored loop owns the same repository for
    // keep/discard of whole candidates and must not find per-task commits inside an iteration.
    let commit_per_task = m
        .workflow
        .as_ref()
        .is_some_and(|w| w.workflow_type == crate::plan::workflow::WorkflowType::Playbook);
    Ok((
        crate::plan::harness::HarnessRunner {
            args,
            paths: p,
            commit_per_task,
            captured_bytes: std::sync::atomic::AtomicU64::new(0),
            staged: Default::default(),
        },
        m,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{args_from, manifest_toml, tempdir};
    use std::fs;

    #[test]
    fn effort_defaults_to_medium_when_unset() {
        let m: manifest::Manifest = toml::from_str(&manifest_toml("")).unwrap();
        let mut a = args_from(&["crucible"]);
        apply_agent_cfg(
            &mut a,
            &m.agent,
            &m.secrets,
            Path::new("ws"),
            &FrozenProjection::default(),
        )
        .unwrap();
        assert_eq!(
            a.reasoning_effort,
            Some(crate::manifest::ReasoningEffort::Medium)
        );
    }

    #[test]
    fn manifest_effort_beats_the_default() {
        let m: manifest::Manifest =
            toml::from_str(&manifest_toml("reasoning_effort = \"max\"")).unwrap();
        let mut a = args_from(&["crucible"]);
        apply_agent_cfg(
            &mut a,
            &m.agent,
            &m.secrets,
            Path::new("ws"),
            &FrozenProjection::default(),
        )
        .unwrap();
        assert_eq!(
            a.reasoning_effort,
            Some(crate::manifest::ReasoningEffort::Max)
        );
    }

    #[test]
    fn harness_defaults_to_claude_and_manifest_sets_it() {
        let m: manifest::Manifest = toml::from_str(&manifest_toml("")).unwrap();
        let mut a = args_from(&["crucible"]);
        apply_agent_cfg(
            &mut a,
            &m.agent,
            &m.secrets,
            Path::new("ws"),
            &FrozenProjection::default(),
        )
        .unwrap();
        assert_eq!(a.harness(), crate::manifest::Harness::Claude);

        let m: manifest::Manifest = toml::from_str(&manifest_toml("harness = \"hermes\"")).unwrap();
        let mut a = args_from(&["crucible"]);
        apply_agent_cfg(
            &mut a,
            &m.agent,
            &m.secrets,
            Path::new("ws"),
            &FrozenProjection::default(),
        )
        .unwrap();
        assert_eq!(a.harness(), crate::manifest::Harness::Hermes);
    }

    #[test]
    fn cli_harness_beats_the_manifest() {
        let m: manifest::Manifest = toml::from_str(&manifest_toml("harness = \"hermes\"")).unwrap();
        let mut a = args_from(&["crucible", "--harness", "claude"]);
        apply_agent_cfg(
            &mut a,
            &m.agent,
            &m.secrets,
            Path::new("ws"),
            &FrozenProjection::default(),
        )
        .unwrap();
        assert_eq!(a.harness(), crate::manifest::Harness::Claude);
    }

    #[test]
    fn the_manifest_model_applies_when_the_cli_names_none() {
        let m: manifest::Manifest =
            toml::from_str(&manifest_toml("model = \"claude-haiku-4-5\"")).unwrap();
        let mut a = args_from(&["crucible"]);
        assert_eq!(a.model(), crate::manifest::Harness::Claude.default_model());
        apply_agent_cfg(
            &mut a,
            &m.agent,
            &m.secrets,
            Path::new("ws"),
            &FrozenProjection::default(),
        )
        .unwrap();
        assert_eq!(a.model(), "claude-haiku-4-5");
    }

    /// The loop wrapper renders `--model=<m>` from the controller's provider resolution, so the
    /// manifest's `[agent].model` must not overwrite it.
    #[test]
    fn cli_model_beats_the_manifest() {
        let m: manifest::Manifest =
            toml::from_str(&manifest_toml("model = \"claude-haiku-4-5\"")).unwrap();
        let mut a = args_from(&["crucible", "--model", "gpt-5.6-luna"]);
        apply_agent_cfg(
            &mut a,
            &m.agent,
            &m.secrets,
            Path::new("ws"),
            &FrozenProjection::default(),
        )
        .unwrap();
        assert_eq!(a.model(), "gpt-5.6-luna");
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
        apply_agent_cfg(
            &mut a,
            &m.agent,
            &m.secrets,
            Path::new("ws"),
            &FrozenProjection::default(),
        )
        .unwrap();
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

    /// `[agent.codex]` is the codex twin of the block above: the shared `[agent].model` names a
    /// Claude model, so a codex domain overrides it here.
    #[test]
    fn codex_subtable_parses_and_denies_unknown_fields() {
        let m: manifest::Manifest = toml::from_str(&format!(
            "{}\n[agent.codex]\nmodel = \"gpt-5.6-sol\"\nauth = \"api\"\napi_key = \"WORK\"\n",
            manifest_toml("harness = \"codex\"")
        ))
        .unwrap();
        let mut a = args_from(&["crucible"]);
        apply_agent_cfg(
            &mut a,
            &m.agent,
            &m.secrets,
            Path::new("ws"),
            &FrozenProjection::default(),
        )
        .unwrap();
        assert_eq!(a.harness(), crate::manifest::Harness::Codex);
        assert_eq!(a.codex.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(a.codex.auth, manifest::CodexAuthMode::Api);
        assert_eq!(a.codex.api_key.as_deref(), Some("WORK"));

        let default: manifest::Manifest =
            toml::from_str(&manifest_toml("harness = \"codex\"")).unwrap();
        assert_eq!(default.agent.codex.auth, manifest::CodexAuthMode::Auto);

        let err = toml::from_str::<manifest::Manifest>(&format!(
            "{}\n[agent.codex]\nmodle = \"typo\"\n",
            manifest_toml("")
        ));
        assert!(err.is_err(), "unknown [agent.codex] key must be rejected");

        let err = toml::from_str::<manifest::Manifest>(&format!(
            "{}\n[agent.codex]\nauth = \"oauth\"\n",
            manifest_toml("")
        ));
        assert!(err.is_err(), "unknown codex auth mode must be rejected");
    }

    #[test]
    fn cli_effort_beats_manifest_and_default() {
        let m: manifest::Manifest =
            toml::from_str(&manifest_toml("reasoning_effort = \"high\"")).unwrap();
        let mut a = args_from(&["crucible", "--effort", "low"]);
        apply_agent_cfg(
            &mut a,
            &m.agent,
            &m.secrets,
            Path::new("ws"),
            &FrozenProjection::default(),
        )
        .unwrap();
        assert_eq!(
            a.reasoning_effort,
            Some(crate::manifest::ReasoningEffort::Low)
        );
    }

    /// A manifest without `[agent.env]` (a controller-drafted pack) still gets the pod's Vertex
    /// config into the sandbox env for the openshell backend, otherwise claude launches with no
    /// credentials and no-ops the whole run (synthetic "Not logged in" result, subtype=success, $0).
    #[test]
    fn openshell_backend_relays_pod_vertex_env_when_manifest_has_none() {
        let _guard = crucible::test_support::env_lock();
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
        let result = apply_agent_cfg(
            &mut a,
            &m.agent,
            &m.secrets,
            Path::new("ws"),
            &FrozenProjection::default(),
        );
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
        let _guard = crucible::test_support::env_lock();
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
        let result = apply_agent_cfg(
            &mut a,
            &m.agent,
            &m.secrets,
            Path::new("ws"),
            &FrozenProjection::default(),
        );
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
        let _guard = crucible::test_support::env_lock();
        unsafe {
            std::env::set_var("CLAUDE_CODE_USE_VERTEX", "1");
        }

        let m: manifest::Manifest = toml::from_str(&manifest_toml("")).unwrap();
        let mut a = args_from(&["crucible"]);
        let result = apply_agent_cfg(
            &mut a,
            &m.agent,
            &m.secrets,
            Path::new("ws"),
            &FrozenProjection::default(),
        );
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

    #[test]
    fn the_broker_projection_carries_the_declared_bounds_and_the_runs_params() {
        let m: manifest::Manifest = toml::from_str(
            r#"
            [repo]
            path = "."
            [agent]
            backend = "openshell"
            goal = "g"
            [judge]
            measure_cmd = "m"
            direction = "higher"
            [outputs.tracker-comment]
            count = 3
            target = { open = { scope = "PROJ-*", param = "issue_key" } }
        "#,
        )
        .expect("manifest parses");
        let params =
            std::collections::BTreeMap::from([("issue_key".to_string(), "PROJ-9".to_string())]);
        let bounds = run_bounds(&m.outputs, &m.build, None, &params);
        let env =
            broker_bounds_env(&bounds, Path::new("/run/state/session.jsonl")).expect("projects");
        let get = |k: &str| {
            env.iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| v.as_str())
                .unwrap_or_else(|| panic!("{k} missing from the projection"))
        };
        let resolved: crucible_contract::outputs::ResolvedOutputs =
            serde_json::from_str(get(crucible_contract::outputs::ENV_OUTPUTS))
                .expect("the broker deserializes what the engine wrote");
        let tracker = resolved
            .get(crucible_contract::outputs::OutputKind::TrackerComment)
            .expect("declared kind");
        assert_eq!(tracker.count, 3);
        assert!(
            resolved
                .get(crucible_contract::outputs::OutputKind::GpuCapture)
                .is_some_and(|b| b.count > 0)
        );
        let bound: std::collections::BTreeMap<String, String> =
            serde_json::from_str(get(crucible_contract::outputs::ENV_OUTPUT_PARAMS))
                .expect("params json");
        assert_eq!(bound.get("issue_key").map(String::as_str), Some("PROJ-9"));
        assert_eq!(
            get(crucible_contract::outputs::ENV_SESSION_LOG),
            "/run/state/session.jsonl"
        );
    }

    /// A playbook with no upstream repository: the engine creates the workspace, lands the injects,
    /// and commits them as the baseline, so the task sees the script from its first dispatch.
    #[test]
    fn a_repo_less_playbook_gets_a_workspace_seeded_from_its_injects() {
        let dir = tempdir("repo-less-playbook");
        fs::write(dir.join("tool.py"), "print('ok')\n").unwrap();
        fs::write(
            dir.join("workflow.star"),
            "t = command(name = \"t\", run = \"python3 tool.py\")\nworkflow(type = \"playbook\", tasks = [t])\n",
        )
        .unwrap();
        fs::write(
            dir.join("crucible.toml"),
            r#"
            [workspace]
            inject = ["tool.py"]
            [agent]
            backend = "command"
            agent_cmd = "true"
            goal = "g"
            [workflow]
            type = "playbook"
            file = "workflow.star"
            "#,
        )
        .unwrap();
        prep_plan_runner(&dir.join("crucible.toml")).expect("prep");
        let ws = dir.join("workspace");
        assert!(ws.join("tool.py").is_file());
        let tracked = std::process::Command::new("git")
            .args([
                "-C",
                &ws.display().to_string(),
                "ls-tree",
                "--name-only",
                "HEAD",
            ])
            .output()
            .expect("git ls-tree");
        let tracked = String::from_utf8_lossy(&tracked.stdout);
        assert!(
            tracked.lines().any(|l| l == "tool.py"),
            "baseline commit lacks the inject: {tracked:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
