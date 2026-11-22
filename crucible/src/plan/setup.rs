//! Setting a manifest run up: the manifest resolved for its launch, its workspace checked out or
//! put back to where an earlier run of it started, and the runner over that workspace.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::manifest::Manifest;
use crate::manifest::variant::VariantName;
use crate::paths::Paths;
use crate::plan::workspace_runner::WorkspaceRunner;

/// A manifest resolved for one launch, and where that run lives.
pub struct Loaded {
    pub manifest: Manifest,
    pub manifest_dir: PathBuf,
    pub paths: Paths,
}

/// Load `manifest_path` as `variant` and resolve its workflow against the launch's `params`.
pub fn load(
    manifest_path: &Path,
    variant: Option<&VariantName>,
    params: &BTreeMap<String, String>,
) -> Result<Loaded> {
    let mut manifest = Manifest::load_frozen_variant(manifest_path, variant)?;
    let manifest_dir = crate::manifest::manifest_dir(manifest_path);
    manifest.resolve_workflow_with(&manifest_dir, params)?;
    warn_inherited_env(crate::exposure::inherits_engine_env(&manifest));
    let workspace = manifest_dir.join(&manifest.workspace.dir);
    let skills = manifest
        .agent
        .toolbox_dir
        .as_ref()
        .map(|d| manifest_dir.join(d));
    let paths = Paths::for_manifest(workspace, state_dir(manifest_path), &manifest_dir, skills);
    Ok(Loaded {
        manifest,
        manifest_dir,
        paths,
    })
}

impl Loaded {
    /// Check the workspace out, or take the one an earlier run left, and make it the run's git
    /// memory.
    pub fn checkout(&self, start: Start) -> Result<()> {
        let m = &self.manifest;
        let workspace = &self.paths.workspace;
        let launched_ref = m.launch.repo_ref();
        if workspace.exists() {
            if let Some(r) = launched_ref
                && start == Start::Fresh
            {
                return Err(LaunchRefNeedsFreshWorkspace {
                    git_ref: r.clone(),
                    workspace: workspace.clone(),
                }
                .into());
            }
        } else {
            crate::workspace::manifest_setup(m, &self.manifest_dir, workspace)?;
            if let Some(commit) = launched_ref.filter(|r| r.is_commit()) {
                check_pinned_commit(workspace, commit, &m.repo.overrides)?;
            }
            for (src, dst, _frozen) in m.resolved_injects(&self.manifest_dir, workspace)? {
                crate::manifest::apply_inject(&src, &dst)
                    .context("applying [workspace].inject after setup")?;
            }
        }
        crucible_vcs::vcs::ensure_repo(workspace).context("ensuring workspace is a git repo")?;
        // The harness's own files, plus where a task's staged inputs land. Without this a playbook's
        // per-task commit sweeps in the toolbox, the results log, and every artifact an ancestor
        // handed this task, and each task's commit reads as though it produced all of them.
        let excludes: Vec<String> = m
            .workspace
            .carried_paths()
            .into_iter()
            .chain(std::iter::once(format!("{}/", crate::plan::STAGED_INPUTS)))
            .collect();
        crucible_vcs::git_memory::install_harness_excludes(workspace, &excludes);
        std::fs::create_dir_all(&self.paths.state)
            .with_context(|| format!("creating state dir {}", self.paths.state.display()))
    }

    /// What `[workflow].env` passes to the run's `command` and `evaluate` tasks.
    pub fn host_env(&self) -> Result<crate::host_env::HostEnv> {
        Ok(self
            .manifest
            .workflow
            .as_ref()
            .map(crate::plan::workflow::WorkflowCfg::host_env)
            .transpose()?
            .unwrap_or_default())
    }

    /// The runner over this run's workspace, and the manifest it was built from.
    pub fn runner<D>(self, dispatch: D) -> Result<(WorkspaceRunner<D>, Manifest)> {
        let frozen_injects = self.manifest.frozen_inject_pairs(&self.manifest_dir)?;
        // A playbook's git memory is per task; the scored loop owns the same repository for
        // keep/discard of whole candidates and must not find per-task commits inside an iteration.
        let commit_per_task = self
            .manifest
            .workflow
            .as_ref()
            .is_some_and(|w| w.workflow_type == crate::plan::workflow::WorkflowType::Playbook);
        Ok((
            WorkspaceRunner {
                dispatch,
                paths: self.paths,
                frozen_injects,
                commit_per_task,
                captured_bytes: std::sync::atomic::AtomicU64::new(0),
                staged: Default::default(),
                budget_left: f64::INFINITY,
            },
            self.manifest,
        ))
    }
}

/// Whether a run checks out its own workspace or continues one an earlier run started (a resume
/// or a rerun), whose recorded commit decides the checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
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
pub struct LaunchRefNeedsFreshWorkspace {
    git_ref: crate::manifest::GitRef,
    workspace: std::path::PathBuf,
}

/// A launched commit that no branch or tag the pack admits contains.
#[derive(Debug, thiserror::Error)]
#[error(
    "repo.ref {commit} is on no branch or tag [repo.overrides].refs admits; the branches and tags \
     holding it are [{}]",
    .on.join(", ")
)]
pub struct PinOutsideAllowedRefs {
    commit: crate::manifest::GitRef,
    on: Vec<String>,
}

/// Keep a freshly cloned commit only when a branch or tag the pack admits holds it; otherwise
/// remove the clone, so no later run reuses it.
fn check_pinned_commit(
    workspace: &Path,
    commit: &crate::manifest::GitRef,
    gate: &crate::manifest::RepoOverridesCfg,
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
pub fn restore_pristine(
    manifest_path: &Path,
    m: &crate::manifest::Manifest,
    base: &str,
) -> Result<()> {
    let manifest_dir = crate::manifest::manifest_dir(manifest_path);
    let workspace = manifest_dir.join(&m.workspace.dir);
    crucible_vcs::git_memory::restore(&workspace, base, &[]).with_context(|| {
        format!("restoring the workspace to the commit the run started from ({base})")
    })?;
    for (src, dst, _frozen) in m.resolved_injects(&manifest_dir, &workspace)? {
        crate::manifest::apply_inject(&src, &dst)
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
pub fn restore_for_rerun(
    manifest_path: &Path,
    m: &crate::manifest::Manifest,
    parent: &crate::plan::resume::RunStart,
    commits: &[crucible_vcs::git_memory::TaskCommit],
    fetched: &[String],
) -> Result<String> {
    use crucible_vcs::git_memory;
    let manifest_dir = crate::manifest::manifest_dir(manifest_path);
    let workspace = manifest_dir.join(&m.workspace.dir);
    crate::workspace::fetch_blobs(&workspace, fetched)
        .context("fetching the file contents the parent run's workspace held")?;
    let base = if git_memory::tree_of(&workspace, &parent.base)?.is_some() {
        parent.base.clone()
    } else {
        let head = crucible_vcs::vcs::head_sha(&workspace)?;
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
        crate::manifest::apply_inject(&src, &dst)
            .context("applying [workspace].inject to the rerun's workspace")?;
    }
    Ok(base)
}

/// Say at run start that the pack's host commands inherit the engine's environment.
pub fn warn_inherited_env(inherits: bool) {
    if inherits {
        eprintln!(
            "[crucible] WARNING: {}",
            crate::manifest::edition::INHERITED_ENV_WARNING
        );
    }
}

/// Where a manifest run keeps its state: the session log, captured files, and run records.
pub fn state_dir(manifest_path: &Path) -> std::path::PathBuf {
    crate::manifest::manifest_dir(manifest_path).join("state")
}
