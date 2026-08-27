//! Workspace isolation for plan tasks: a private clone per task, and the diff plumbing to
//! carry work out of one. An isolated task's edits never touch the shared workspace: what
//! leaves is its structured output (and, where the runner asks for it, a captured diff).
//!
//! Used by the wide tournament's parallel proposers and by any plan task marked
//! `isolation = "worktree"`.

use std::path::Path;

use anyhow::{Context, Result};

/// A git invocation that ran but exited nonzero. Every worktree operation fails the same way,
/// so the operation name is a field rather than five near-identical messages.
#[derive(Debug, thiserror::Error)]
#[error("{operation} failed: {stderr}")]
pub struct GitFailed {
    operation: &'static str,
    stderr: String,
}

impl GitFailed {
    fn new(operation: &'static str, stderr: &[u8]) -> Self {
        Self {
            operation,
            stderr: String::from_utf8_lossy(stderr).into_owned(),
        }
    }
}

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
    let clone = |extra: &[&str]| -> Result<std::process::Output> {
        let mut args = vec!["clone", "--local", "--no-checkout"];
        args.extend_from_slice(extra);
        std::process::Command::new("git")
            .args(&args)
            .arg(workspace.as_os_str())
            .arg(dest.as_os_str())
            .output()
            .context("git clone --local for a task worktree")
    };
    let mut status = clone(&[])?;
    if !status.status.success() {
        // Hardlinks can't cross filesystems ("Invalid cross-device link"): a state dir on a PVC
        // puts the clone on a different device than the workspace. Copy objects instead; slower,
        // but a worktree either way.
        let _ = std::fs::remove_dir_all(dest);
        status = clone(&["--no-hardlinks"])?;
    }
    if !status.status.success() {
        return Err(GitFailed::new("git clone --local", &status.stderr).into());
    }
    // Check out HEAD so the task has a working tree.
    let checkout = std::process::Command::new("git")
        .args(["-C", &dest.to_string_lossy(), "checkout", "HEAD"])
        .output()
        .context("git checkout HEAD in a task worktree")?;
    if !checkout.status.success() {
        return Err(GitFailed::new("git checkout in a task worktree", &checkout.stderr).into());
    }
    // A clone only carries committed state, but isolation has to mean "the workspace as it
    // stands right now": an upstream task's uncommitted edits are exactly what the isolated
    // task is usually there to look at. Carry the working tree over as a patch.
    apply(dest, pending).context("carrying the workspace's uncommitted state into a task worktree")
}

/// Capture what a task changed in its worktree (staged + unstaged). `--binary` so the text
/// survives a later [`apply`] losslessly. Stages the tree (`git add -A`) as a side effect,
/// which is what every snapshot does a moment later anyway.
pub fn capture_diff(worktree: &Path) -> Result<String> {
    let add = std::process::Command::new("git")
        .args(["-C", &worktree.to_string_lossy(), "add", "-A"])
        .output()
        .context("git add -A in a task worktree")?;
    if !add.status.success() {
        return Err(GitFailed::new("git add -A", &add.stderr).into());
    }
    let output = std::process::Command::new("git")
        .args([
            "-C",
            &worktree.to_string_lossy(),
            "diff",
            "--cached",
            "--binary",
        ])
        .output()
        .context("git diff --cached in a task worktree")?;
    if !output.status.success() {
        return Err(GitFailed::new("git diff --cached", &output.stderr).into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Apply a captured diff to a workspace via `git apply` on stdin. An empty diff is a no-op.
pub fn apply(main_ws: &Path, diff: &str) -> Result<()> {
    if diff.trim().is_empty() {
        return Ok(());
    }
    let mut apply = std::process::Command::new("git")
        .args([
            "-C",
            &main_ws.to_string_lossy(),
            "apply",
            "--allow-empty",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("git apply in the main workspace")?;

    if let Some(mut stdin) = apply.stdin.take() {
        use std::io::Write;
        stdin.write_all(diff.as_bytes())?;
    }

    let output = apply.wait_with_output()?;
    if !output.status.success() {
        return Err(GitFailed::new("git apply", &output.stderr).into());
    }
    Ok(())
}
