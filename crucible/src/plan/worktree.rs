//! Workspace isolation for plan tasks: a private clone per task, and the diff plumbing to
//! carry work out of one. An isolated task's edits never touch the shared workspace — what
//! leaves is its structured output (and, where the runner asks for it, a captured diff).
//!
//! Used by the wide tournament's parallel proposers and by any plan task marked
//! `isolation = "worktree"`.

use std::path::Path;

use anyhow::{Context, Result};

/// Create a task worktree as a shallow copy of `workspace`. Uses `git clone --local`, which
/// hard-links objects, so a fan-out of N candidates costs ~one checkout each rather than N
/// full copies.
pub(crate) fn setup(workspace: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    let status = std::process::Command::new("git")
        .args([
            "clone",
            "--local",
            "--no-checkout",
            &workspace.to_string_lossy(),
            &dest.to_string_lossy(),
        ])
        .output()
        .context("git clone --local for a task worktree")?;
    if !status.status.success() {
        anyhow::bail!(
            "git clone --local failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }
    // Check out HEAD so the task has a working tree.
    let checkout = std::process::Command::new("git")
        .args(["-C", &dest.to_string_lossy(), "checkout", "HEAD"])
        .output()
        .context("git checkout HEAD in a task worktree")?;
    if !checkout.status.success() {
        anyhow::bail!(
            "git checkout in a task worktree failed: {}",
            String::from_utf8_lossy(&checkout.stderr)
        );
    }
    // A clone only carries committed state, but isolation has to mean "the workspace as it
    // stands right now" — an upstream task's uncommitted edits are exactly what the isolated
    // task is usually there to look at. Carry the working tree over as a patch.
    // Side effect worth knowing: capturing the diff stages the source workspace (`git add -A`),
    // which is what every snapshot does a moment later anyway.
    apply(dest, &capture_diff(workspace))
        .context("carrying the workspace's uncommitted state into a task worktree")
}

/// Capture what a task changed in its worktree (staged + unstaged). `--binary` so the text
/// survives a later [`apply`] losslessly.
pub(crate) fn capture_diff(worktree: &Path) -> String {
    let _ = std::process::Command::new("git")
        .args(["-C", &worktree.to_string_lossy(), "add", "-A"])
        .output();
    let output = std::process::Command::new("git")
        .args([
            "-C",
            &worktree.to_string_lossy(),
            "diff",
            "--cached",
            "--binary",
        ])
        .output();
    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => String::new(),
    }
}

/// Apply a captured diff to a workspace via `git apply` on stdin. An empty diff is a no-op.
pub(crate) fn apply(main_ws: &Path, diff: &str) -> Result<()> {
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
        anyhow::bail!(
            "git apply failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}
