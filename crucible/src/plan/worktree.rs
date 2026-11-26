//! Workspace isolation for plan tasks: a private clone per task, and the diff plumbing to
//! carry work out of one. An isolated task's edits never touch the shared workspace: what
//! leaves is its structured output (and, where the runner asks for it, a captured diff).
//!
//! Used by the wide tournament's parallel proposers and by any plan task marked
//! `isolation = "worktree"`. All of it is libgit2 in-process; the `git` binary is only
//! reached when the local clone cannot hard-link across devices.

use std::path::Path;

use anyhow::{Context, Result};
use crucible_vcs::vcs;

/// Create a task worktree as a shallow copy of `workspace`: a local clone whose objects are
/// hard-linked to the source's, so a fan-out of N candidates costs ~one checkout each rather
/// than N full copies.
///
/// `pending` is the source workspace's uncommitted state as a patch, from [`capture_diff`].
/// The caller captures it because a fan-out shares one source workspace: N threads staging
/// in it race on the index lock.
pub fn setup(workspace: &Path, dest: &Path, pending: &str) -> Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    if vcs::clone_local(workspace, dest).is_err() {
        // Hardlinks can't cross filesystems ("Invalid cross-device link"): a state dir on a PVC
        // puts the clone on a different device than the workspace. Copy objects instead; slower,
        // but a worktree either way.
        let _ = std::fs::remove_dir_all(dest);
        crucible_vcs::cli::git(
            None,
            [
                "clone",
                "--no-hardlinks",
                &workspace.to_string_lossy(),
                &dest.to_string_lossy(),
            ],
        )
        .read()
        .context("git clone --no-hardlinks for a task worktree")?;
    }
    // A clone only carries committed state, but isolation has to mean "the workspace as it
    // stands right now": an upstream task's uncommitted edits are exactly what the isolated
    // task is usually there to look at. Carry the working tree over as a patch.
    apply(dest, pending).context("carrying the workspace's uncommitted state into a task worktree")
}

/// Capture what a task changed in its worktree (staged + unstaged), as a binary-safe patch
/// that survives a later [`apply`] losslessly. Stages the tree as a side effect, which is
/// what every snapshot does a moment later anyway.
pub fn capture_diff(worktree: &Path) -> Result<String> {
    vcs::stage_all(worktree).context("staging a task worktree")?;
    Ok(String::from_utf8_lossy(&vcs::staged_patch(worktree)?).into_owned())
}

/// Apply a captured diff to a workspace's working tree. An empty diff is a no-op.
pub fn apply(main_ws: &Path, diff: &str) -> Result<()> {
    if diff.trim().is_empty() {
        return Ok(());
    }
    vcs::apply_patch(main_ws, diff.as_bytes()).context("applying a patch in the main workspace")
}
