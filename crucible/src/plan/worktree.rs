//! Workspace plumbing for plan tasks that leave the shared tree alone: a private clone per
//! worktree task, the diff plumbing to carry work out of one, and the tree reads that hold a
//! readonly batch to its word. A worktree task's edits never touch the shared workspace: what
//! leaves is its structured output (and, where the runner asks for it, a captured diff).
//!
//! Used by the wide tournament's parallel proposers and by any plan task declaring
//! `workspace = "worktree"` or `workspace = "readonly"`.

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
    // A clone only carries committed state, but a worktree has to mean "the workspace as it
    // stands right now": an upstream task's uncommitted edits are exactly what the worktree
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

/// The tree the workspace's content would commit as: tracked and untracked files, ignored ones
/// left out, as git memory would record them. Stages the tree (`git add -A`) as a side effect,
/// as [`capture_diff`] does.
pub fn tree(workspace: &Path) -> Result<String> {
    let add = git(workspace, &["add", "-A"])?;
    if !add.status.success() {
        return Err(GitFailed::new("git add -A", &add.stderr).into());
    }
    let written = git(workspace, &["write-tree"])?;
    if !written.status.success() {
        return Err(GitFailed::new("git write-tree", &written.stderr).into());
    }
    Ok(String::from_utf8_lossy(&written.stdout).trim().to_string())
}

/// The paths that differ between two trees of `workspace`, as git names them.
pub fn changed_paths(workspace: &Path, from: &str, to: &str) -> Result<Vec<String>> {
    let diff = git(
        workspace,
        &["diff", "--name-only", "--no-renames", from, to],
    )?;
    if !diff.status.success() {
        return Err(GitFailed::new("git diff --name-only", &diff.stderr).into());
    }
    Ok(String::from_utf8_lossy(&diff.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// Put the index and working tree back to `tree`, a tree [`tree`] read earlier: changed files
/// get their content back, deleted ones return, and files the index holds that `tree` does not
/// are removed. Ignored files are not touched, since [`tree`] never saw them.
pub fn reset_to_tree(workspace: &Path, tree: &str) -> Result<()> {
    let reset = git(workspace, &["read-tree", "-u", "--reset", tree])?;
    if !reset.status.success() {
        return Err(GitFailed::new("git read-tree", &reset.stderr).into());
    }
    Ok(())
}

fn git(workspace: &Path, args: &[&str]) -> Result<std::process::Output> {
    std::process::Command::new("git")
        .arg("-C")
        .arg(workspace.as_os_str())
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))
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

#[cfg(test)]
mod tests {
    use crate::plan::worktree::{changed_paths, reset_to_tree, tree};
    use std::path::{Path, PathBuf};

    fn repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("worktree-tree-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("kept.txt"), "kept\n").unwrap();
        std::fs::write(dir.join("edited.txt"), "original\n").unwrap();
        std::fs::write(dir.join("deleted.txt"), "here\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "ignored/\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base"]);
        // Uncommitted state an earlier task left, which the reset must keep as it was.
        std::fs::write(dir.join("pending.txt"), "pending\n").unwrap();
        dir
    }

    fn read(dir: &Path, file: &str) -> Option<String> {
        std::fs::read_to_string(dir.join(file)).ok()
    }

    #[test]
    fn an_unchanged_tree_reads_the_same_and_a_change_is_named_and_put_back() {
        let dir = repo("reset");
        let before = tree(&dir).unwrap();
        assert_eq!(
            tree(&dir).unwrap(),
            before,
            "reading the tree changes nothing"
        );

        std::fs::create_dir_all(dir.join("ignored")).unwrap();
        std::fs::write(dir.join("ignored/cache"), "scratch\n").unwrap();
        assert_eq!(
            tree(&dir).unwrap(),
            before,
            "an ignored file is not workspace content"
        );

        std::fs::write(dir.join("edited.txt"), "rewritten\n").unwrap();
        std::fs::remove_file(dir.join("deleted.txt")).unwrap();
        std::fs::create_dir_all(dir.join("new")).unwrap();
        std::fs::write(dir.join("new/file.txt"), "added\n").unwrap();
        let after = tree(&dir).unwrap();
        assert_ne!(after, before);
        assert_eq!(
            changed_paths(&dir, &before, &after).unwrap(),
            ["deleted.txt", "edited.txt", "new/file.txt"]
        );

        reset_to_tree(&dir, &before).unwrap();
        assert_eq!(tree(&dir).unwrap(), before);
        assert_eq!(read(&dir, "edited.txt").as_deref(), Some("original\n"));
        assert_eq!(read(&dir, "deleted.txt").as_deref(), Some("here\n"));
        assert_eq!(read(&dir, "new/file.txt"), None);
        assert_eq!(read(&dir, "pending.txt").as_deref(), Some("pending\n"));
        assert_eq!(read(&dir, "kept.txt").as_deref(), Some("kept\n"));
        assert_eq!(
            read(&dir, "ignored/cache").as_deref(),
            Some("scratch\n"),
            "the reset leaves what the tree never saw"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_workspace_that_is_not_a_repository_is_an_error_not_an_empty_tree() {
        let dir = std::env::temp_dir().join(format!("worktree-norepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(tree(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
