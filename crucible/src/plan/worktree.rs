//! Workspace isolation for plan tasks: a private clone per task, and the diff plumbing to
//! carry work out of one. An isolated task's edits never touch the shared workspace: what
//! leaves is its structured output (and, where the runner asks for it, a captured diff).
//!
//! Used by the wide tournament's parallel proposers and by any plan task marked
//! `isolation = "worktree"`.

use std::path::Path;

use anyhow::{Context, Result};
use crucible_vcs::cli::git;

/// Create a task worktree as a shallow copy of `workspace`. Uses `git clone --local`, which
/// hard-links objects, so a fan-out of N candidates costs ~one checkout each rather than N
/// full copies.
///
/// `pending` is the source workspace's uncommitted state as a patch, from [`capture_diff`].
/// The caller captures it because a fan-out shares one source workspace: N threads running
/// `git add -A` in it race on `.git/index.lock`.
pub fn setup(workspace: &Path, dest: &Path, pending: &str) -> Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    let clone = |extra: &[&str]| {
        let mut args: Vec<&std::ffi::OsStr> = vec!["clone".as_ref(), "--local".as_ref()];
        args.push("--no-checkout".as_ref());
        args.extend(extra.iter().map(std::ffi::OsStr::new));
        args.extend([workspace.as_os_str(), dest.as_os_str()]);
        git(None, args).read()
    };
    if clone(&[]).is_err() {
        // Hardlinks can't cross filesystems ("Invalid cross-device link"): a state dir on a PVC
        // puts the clone on a different device than the workspace. Copy objects instead; slower,
        // but a worktree either way.
        let _ = std::fs::remove_dir_all(dest);
        clone(&["--no-hardlinks"]).context("git clone --local for a task worktree")?;
    }
    // Check out HEAD so the task has a working tree.
    git(dest, ["checkout", "HEAD"])
        .read()
        .context("git checkout HEAD in a task worktree")?;
    // A clone only carries committed state, but isolation has to mean "the workspace as it
    // stands right now": an upstream task's uncommitted edits are exactly what the isolated
    // task is usually there to look at. Carry the working tree over as a patch.
    apply(dest, pending).context("carrying the workspace's uncommitted state into a task worktree")
}

/// Capture what a task changed in its worktree (staged + unstaged). `--binary` so the text
/// survives a later [`apply`] losslessly. Stages the tree (`git add -A`) as a side effect,
/// which is what every snapshot does a moment later anyway.
pub fn capture_diff(worktree: &Path) -> Result<String> {
    git(worktree, ["add", "-A"])
        .read()
        .context("git add -A in a task worktree")?;
    git(worktree, ["diff", "--cached", "--binary"])
        .read()
        .context("git diff --cached in a task worktree")
}

/// Apply a captured diff to a workspace via `git apply` on stdin. An empty diff is a no-op.
pub fn apply(main_ws: &Path, diff: &str) -> Result<()> {
    if diff.trim().is_empty() {
        return Ok(());
    }
    git(main_ws, ["apply", "--allow-empty", "-"])
        .stdin_bytes(diff)
        .read()
        .context("git apply in the main workspace")?;
    Ok(())
}
