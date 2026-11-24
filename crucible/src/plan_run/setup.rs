//! A plan runner over a manifest: the same workspace and agent config a loop run would use.

use crate::args::Args;
use crate::manifest;
use crate::manifest::launch::Setting;
use crate::setup::{
    apply_agent_cfg, end_run_at_setup, frozen_projection, pin_agent, start_mcp, warn_inherited_env,
};
use anyhow::{Context, Result};
use crucible_vcs::vcs;
use std::path::Path;

/// A launch that sets one setting for every agent task through both its agent flags and a
/// `*.<setting>` value.
#[derive(Debug, thiserror::Error)]
#[error(
    "the launch sets every agent task's {setting} twice, as a flag and as *.{setting}; pass one"
)]
pub(crate) struct LaunchSettingTwice {
    setting: Setting,
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
) -> Result<(crate::plan_run::harness::HarnessRunner, manifest::Manifest)> {
    prep_plan_runner_with_params(
        manifest_path,
        None,
        &std::collections::BTreeMap::new(),
        crate::openshell::gateway::ComputeDriver::Podman,
        crate::args::AgentOverride::default(),
        None,
        Start::Fresh,
    )
}

/// Whether a run checks out its own workspace or continues one an earlier run started (a resume
/// or a rerun), whose recorded commit decides the checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Start {
    Fresh,
    Continues,
}

/// A launch ref on a workspace that already exists, which a fresh run would not check out.
#[derive(Debug, thiserror::Error)]
#[error(
    "repo.ref {git_ref} needs a fresh checkout, but {} already exists; remove it or launch \
     without repo.ref",
    .workspace.display()
)]
pub(crate) struct LaunchRefNeedsFreshWorkspace {
    git_ref: manifest::GitRef,
    workspace: std::path::PathBuf,
}

/// A launched commit that no branch or tag the pack admits contains.
#[derive(Debug, thiserror::Error)]
#[error(
    "repo.ref {commit} is on no branch or tag [repo.overrides].refs admits; the branches and tags \
     holding it are [{}]",
    .on.join(", ")
)]
pub(crate) struct PinOutsideAllowedRefs {
    commit: manifest::GitRef,
    on: Vec<String>,
}

/// Keep a freshly cloned commit only when a branch or tag the pack admits holds it; otherwise
/// remove the clone, so no later run reuses it.
fn check_pinned_commit(
    workspace: &Path,
    commit: &manifest::GitRef,
    gate: &manifest::RepoOverridesCfg,
) -> Result<()> {
    let refusal: anyhow::Error = match crate::workspace::refs_containing(workspace, commit) {
        Ok(on) if on.iter().any(|name| gate.allows(name)) => return Ok(()),
        Ok(on) => PinOutsideAllowedRefs {
            commit: commit.clone(),
            on,
        }
        .into(),
        Err(e) => e.into(),
    };
    if let Err(e) = std::fs::remove_dir_all(workspace) {
        tracing::warn!(workspace = %workspace.display(), error = %e, "removing a refused clone");
    }
    Err(refusal)
}

/// Put a resumed run's workspace back to the commit it started from, with the manifest's injects
/// laid over it as a fresh checkout would have them.
pub(crate) fn restore_pristine(
    manifest_path: &Path,
    m: &manifest::Manifest,
    base: &str,
) -> Result<()> {
    let manifest_dir = manifest::manifest_dir(manifest_path);
    let workspace = manifest_dir.join(&m.workspace.dir);
    crucible_vcs::git_memory::restore(&workspace, base, &[]).with_context(|| {
        format!("restoring the workspace to the commit the run started from ({base})")
    })?;
    for (src, dst, _frozen) in m.resolved_injects(&manifest_dir, &workspace)? {
        crate::inject::apply(&src, &dst)
            .context("applying [workspace].inject to the restored workspace")?;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error(
    "the parent run started from {base}, which this workspace neither has nor matches; a rerun \
     starts where its parent did"
)]
struct RerunBaseUnavailable {
    base: String,
}

/// Rebuild a rerun's workspace: the blobs `fetched` names that a partial clone lacks, the commit
/// its parent started from, or this checkout's `HEAD` when that holds the same tree or the
/// workspace has no repository of its own, then the reused tasks' commits, then the manifest's
/// injects. Returns the commit the rerun starts from.
pub(crate) fn restore_for_rerun(
    manifest_path: &Path,
    m: &manifest::Manifest,
    parent: &crate::plan::resume::RunStart,
    commits: &[crucible_vcs::git_memory::TaskCommit],
    fetched: &[String],
) -> Result<String> {
    use crucible_vcs::git_memory;
    let manifest_dir = manifest::manifest_dir(manifest_path);
    let workspace = manifest_dir.join(&m.workspace.dir);
    crate::workspace::fetch_blobs(&workspace, fetched)
        .context("fetching the file contents the parent run's workspace held")?;
    let base = if git_memory::tree_of(&workspace, &parent.base)?.is_some() {
        parent.base.clone()
    } else {
        let head = vcs::head_sha(&workspace)?;
        let own_baseline = m.workspace.setup_cmd.is_none() && m.repo.source().is_none();
        match git_memory::tree_of(&workspace, &head)? {
            _ if own_baseline => head,
            Some(tree) if !parent.base_tree.is_empty() && tree == parent.base_tree => head,
            _ => {
                return Err(RerunBaseUnavailable {
                    base: parent.base.clone(),
                }
                .into());
            }
        }
    };
    git_memory::restore(&workspace, &base, &[])
        .with_context(|| format!("restoring the workspace to the parent's base ({base})"))?;
    git_memory::replay(&workspace, commits)?;
    for (src, dst, _frozen) in m.resolved_injects(&manifest_dir, &workspace)? {
        crate::inject::apply(&src, &dst)
            .context("applying [workspace].inject to the rerun's workspace")?;
    }
    Ok(base)
}

/// Where a manifest run keeps its state: the session log, captured files, and run records.
pub(crate) fn state_dir(manifest_path: &Path) -> std::path::PathBuf {
    manifest::manifest_dir(manifest_path).join("state")
}

pub(crate) fn prep_plan_runner_with_params(
    manifest_path: &Path,
    variant: Option<&crate::knobs::VariantName>,
    params: &std::collections::BTreeMap<String, String>,
    compute_driver: crate::openshell::gateway::ComputeDriver,
    agent: crate::args::AgentOverride,
    engine: Option<tokio::runtime::Handle>,
    start: Start,
) -> Result<(crate::plan_run::harness::HarnessRunner, manifest::Manifest)> {
    let mut m = manifest::Manifest::load_frozen_variant(manifest_path, variant)?;
    let manifest_dir = manifest::manifest_dir(manifest_path);
    m.resolve_workflow_with(&manifest_dir, params)?;
    warn_inherited_env(crate::exposure::inherits_engine_env(&m));
    if let Some(all) = m.launch.all() {
        for (setting, flag, value) in [
            (
                Setting::Harness,
                agent.harness.is_some(),
                all.harness.is_some(),
            ),
            (Setting::Model, agent.model.is_some(), all.model.is_some()),
        ] {
            if flag && value {
                return Err(LaunchSettingTwice { setting }.into());
            }
        }
    }
    let workspace = manifest_dir.join(&m.workspace.dir);
    let state = state_dir(manifest_path);
    let skills = m.agent.toolbox_dir.as_ref().map(|d| manifest_dir.join(d));
    let p = crate::args::Paths::for_manifest(workspace.clone(), state, &manifest_dir, skills);
    // Before the workspace clone: crucible-git reads the draft-pr bound to authorize it.
    let frozen = frozen_projection(
        &m,
        m.publish.as_ref().and_then(|p| p.pr_repo.as_deref()),
        &manifest::launch::split(params).0,
        &p.session_log,
    )?;
    let launched_ref = m.launch.repo_ref();
    if workspace.exists() {
        if let Some(r) = launched_ref
            && start == Start::Fresh
        {
            return Err(LaunchRefNeedsFreshWorkspace {
                git_ref: r.clone(),
                workspace,
            }
            .into());
        }
    } else {
        crate::workspace::manifest_setup(&m, &manifest_dir, &workspace)?;
        if let Some(commit) = launched_ref.filter(|r| r.is_commit()) {
            check_pinned_commit(&workspace, commit, &m.repo.overrides)?;
        }
        for (src, dst, _frozen) in m.resolved_injects(&manifest_dir, &workspace)? {
            crate::inject::apply(&src, &dst).context("applying [workspace].inject after setup")?;
        }
    }
    vcs::ensure_repo(&workspace).context("ensuring workspace is a git repo")?;
    // The harness's own files, plus where a task's staged inputs land. Without this a playbook's
    // per-task commit sweeps in the toolbox, the results log, and every artifact an ancestor
    // handed this task, and each task's commit reads as though it produced all of them.
    let excludes: Vec<String> = m
        .workspace
        .carried_paths()
        .into_iter()
        .chain(std::iter::once(format!("{}/", crate::plan::STAGED_INPUTS)))
        .collect();
    crucible_vcs::git_memory::install_harness_excludes(&workspace, &excludes);
    std::fs::create_dir_all(&p.state)
        .with_context(|| format!("creating state dir {}", p.state.display()))?;
    // Default Args (as if `crucible` ran flagless) carrying the launch's own agent flags, then
    // the manifest's [agent] folded on top — the same resolution a loop run does.
    let mut args = Args::defaults().context("constructing default args")?;
    args.engine = engine;
    args.manifest = Some(manifest_path.to_path_buf());
    args.compute_driver = compute_driver;
    args.harness = agent.harness;
    args.model = agent.model;
    let harness = pin_agent(&mut args, &m.agent)?;
    crate::workspace::install_toolbox(&p, &m.agent.toolbox_exclude, harness.spec().skills_dir)?;
    apply_agent_cfg(&mut args, &m.agent, &m.secrets, &p.workspace, &frozen)?;
    start_mcp(&mut args, &m.mcp).map_err(|e| end_run_at_setup(&p, e))?;
    args.workflow_frozen_injects = m.frozen_inject_pairs(&manifest_dir)?;
    args.workflow_toolbox_exclude = m.agent.toolbox_exclude.clone();
    args.workflow_env = m
        .workflow
        .as_ref()
        .map(crate::plan::workflow::WorkflowCfg::host_env)
        .transpose()?
        .unwrap_or_default();
    args.workflow_servers = m
        .workflow
        .as_ref()
        .map(|w| w.servers.clone())
        .unwrap_or_default();
    args.buildit_socket = Some(p.buildit_socket());
    // A playbook's git memory is per task; the scored loop owns the same repository for
    // keep/discard of whole candidates and must not find per-task commits inside an iteration.
    let commit_per_task = m
        .workflow
        .as_ref()
        .is_some_and(|w| w.workflow_type == crate::plan::workflow::WorkflowType::Playbook);
    Ok((
        crate::plan_run::harness::HarnessRunner {
            args,
            paths: p,
            commit_per_task,
            captured_bytes: std::sync::atomic::AtomicU64::new(0),
            staged: Default::default(),
            budget_left: f64::INFINITY,
        },
        m,
    ))
}

#[cfg(test)]
mod tests {
    use crate::agent::sandbox_env::SandboxEnv;
    use crate::manifest;
    use crate::plan_run::setup::prep_plan_runner;
    use crate::setup::{FrozenProjection, apply_agent_cfg};
    use crate::testing::{args_from, tempdir};
    use std::fs;
    use std::path::Path;

    #[test]
    fn a_playbook_keeps_carried_artifacts_out_of_git_memory() {
        let dir = tempdir("playbook-carried-evidence");
        fs::write(dir.join("tool.py"), "print('ok')\n").unwrap();
        fs::write(
            dir.join("workflow.star"),
            "t = command(name = \"t\", run = \"python3 tool.py\")\nworkflow(type = \"playbook\", tasks = [t])\n",
        ).unwrap();
        fs::write(
            dir.join("crucible.toml"),
            r#"
            [workspace]
            inject = ["tool.py"]
            carry_forward = ["evidence/"]
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

        prep_plan_runner(&dir.join("crucible.toml")).unwrap();

        let ws = dir.join("workspace");
        fs::create_dir(ws.join("evidence")).unwrap();
        fs::write(ws.join("evidence/probe.zip"), b"retained separately").unwrap();
        fs::write(ws.join("change.py"), "print('changed')\n").unwrap();

        crucible_vcs::git_memory::snapshot(&ws, "task probe").unwrap();

        let repo = git2::Repository::open(&ws).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();

        assert!(tree.get_path(Path::new("evidence/probe.zip")).is_err());
        assert!(tree.get_path(Path::new("change.py")).is_ok());
        assert_eq!(
            fs::read(ws.join("evidence/probe.zip")).unwrap(),
            b"retained separately"
        );
    }

    /// A named sandbox's env script carries the secrets it lists and none of the others, through
    /// the same relay a loop pod runs.
    #[test]
    fn a_named_sandbox_env_script_holds_only_its_secrets() {
        let _guard = crucible::test_support::env_lock();
        unsafe {
            std::env::set_var("JIRA_TOKEN", "jira-value");
            std::env::set_var("REGISTRY_TOKEN", "registry-value");
            std::env::set_var(
                crate::agent::sandbox_env::AGENT_VISIBLE_ENV,
                "JIRA_TOKEN,REGISTRY_TOKEN",
            );
        }
        let m: manifest::Manifest = toml::from_str(
            r#"
            [repo]
            path = "."
            [agent]
            backend = "openshell"
            goal = "g"
            [agent.sandbox.go]
            image = "ghcr.io/acme/sandbox-go@sha256:bb"
            secrets = ["registry"]
            [agent.sandbox.bare]
            image = "ghcr.io/acme/bare@sha256:cc"
            [[secret]]
            name = "jira"
            kind = "opaque"
            env = "JIRA_TOKEN"
            [[secret]]
            name = "registry"
            kind = "opaque"
            env = "REGISTRY_TOKEN"
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
            std::env::remove_var("JIRA_TOKEN");
            std::env::remove_var("REGISTRY_TOKEN");
            std::env::remove_var(crate::agent::sandbox_env::AGENT_VISIBLE_ENV);
        }
        result.unwrap();
        let script = |sandbox: Option<&str>| {
            let mut turn = a.clone();
            if let Some(name) = sandbox {
                crate::plan_run::harness::enter_sandbox(&mut turn, name).unwrap();
            }
            SandboxEnv::from_manifest(turn.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                .script(turn.harness().spec())
        };

        let default = script(None);
        assert!(default.contains("jira-value") && default.contains("registry-value"));
        let go = script(Some("go"));
        assert!(go.contains("registry-value"), "{go}");
        assert!(
            !go.contains("jira-value") && !go.contains("JIRA_TOKEN"),
            "{go}"
        );
        let bare = script(Some("bare"));
        assert!(!bare.contains("-value"), "{bare}");
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
