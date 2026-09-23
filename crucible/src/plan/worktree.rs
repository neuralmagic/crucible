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

/// What a readonly batch has to leave as it found it: the content git memory would record, and
/// the commit and branch HEAD names. A task that commits changes the second even when the first
/// reads the same, and its commit would otherwise be the base of the next task's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    tree: String,
    head: Head,
}

/// Where HEAD points: the branch it is attached to, if any, and the commit it resolves to, if
/// the repository has one.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Head {
    branch: Option<String>,
    commit: Option<String>,
}

impl std::fmt::Display for Head {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let short = |commit: &str| commit.get(..12).unwrap_or(commit).to_string();
        match (&self.branch, &self.commit) {
            (Some(branch), Some(commit)) => write!(f, "{branch} at {}", short(commit)),
            (Some(branch), None) => write!(f, "{branch} with no commits"),
            (None, Some(commit)) => write!(f, "detached at {}", short(commit)),
            (None, None) => f.write_str("no HEAD"),
        }
    }
}

/// Read `workspace` as a readonly batch has to leave it. Stages the tree (`git add -A`) as a side
/// effect, as [`capture_diff`] does.
pub fn snapshot(workspace: &Path) -> Result<Snapshot> {
    Ok(Snapshot {
        tree: tree(workspace)?,
        head: head(workspace)?,
    })
}

/// What differs between two snapshots of `workspace`: each changed path as git names it, then
/// HEAD if it moved.
pub fn changes(workspace: &Path, before: &Snapshot, after: &Snapshot) -> Result<Vec<String>> {
    let mut changed = if before.tree == after.tree {
        Vec::new()
    } else {
        changed_paths(workspace, &before.tree, &after.tree)?
    };
    if before.head != after.head {
        changed.push(format!("HEAD moved from {} to {}", before.head, after.head));
    }
    Ok(changed)
}

/// Put `workspace` back to `snapshot`: HEAD to the branch and commit it named, then the index and
/// working tree to its content. Ignored files are not touched, since the snapshot never saw them.
pub fn restore(workspace: &Path, snapshot: &Snapshot) -> Result<()> {
    if head(workspace)? != snapshot.head {
        restore_head(workspace, &snapshot.head)?;
    }
    reset_to_tree(workspace, &snapshot.tree)
}

/// The tree the workspace's content would commit as: tracked and untracked files, ignored ones
/// left out, as git memory would record them.
fn tree(workspace: &Path) -> Result<String> {
    checked(workspace, &["add", "-A"], "git add -A")?;
    checked(workspace, &["write-tree"], "git write-tree")
}

fn head(workspace: &Path) -> Result<Head> {
    let symbolic = git(workspace, &["symbolic-ref", "-q", "HEAD"])?;
    let branch = match symbolic.status.code() {
        Some(0) => Some(String::from_utf8_lossy(&symbolic.stdout).trim().to_string()),
        // `-q` exits 1, silently, for a detached HEAD.
        Some(1) => None,
        _ => return Err(GitFailed::new("git symbolic-ref HEAD", &symbolic.stderr).into()),
    };
    Ok(Head {
        branch,
        commit: commit_of(workspace, "HEAD")?,
    })
}

/// The commit `name` resolves to, or `None` when it names none (an unborn branch).
fn commit_of(workspace: &Path, name: &str) -> Result<Option<String>> {
    let spec = format!("{name}^{{commit}}");
    let resolved = git(workspace, &["rev-parse", "-q", "--verify", &spec])?;
    Ok(resolved
        .status
        .success()
        .then(|| String::from_utf8_lossy(&resolved.stdout).trim().to_string()))
}

fn restore_head(workspace: &Path, head: &Head) -> Result<()> {
    match (&head.branch, &head.commit) {
        (Some(branch), commit) => {
            checked(
                workspace,
                &["symbolic-ref", "HEAD", branch],
                "git symbolic-ref HEAD",
            )?;
            match commit {
                Some(commit) => {
                    checked(workspace, &["update-ref", branch, commit], "git update-ref")?;
                }
                None if commit_of(workspace, branch)?.is_some() => {
                    checked(
                        workspace,
                        &["update-ref", "-d", branch],
                        "git update-ref -d",
                    )?;
                }
                None => {}
            }
        }
        (None, Some(commit)) => {
            checked(
                workspace,
                &["update-ref", "--no-deref", "HEAD", commit],
                "git update-ref HEAD",
            )?;
        }
        (None, None) => {}
    }
    Ok(())
}

/// The paths that differ between two trees of `workspace`, as git names them.
fn changed_paths(workspace: &Path, from: &str, to: &str) -> Result<Vec<String>> {
    Ok(checked(
        workspace,
        &["diff", "--name-only", "--no-renames", from, to],
        "git diff --name-only",
    )?
    .lines()
    .filter(|line| !line.is_empty())
    .map(str::to_string)
    .collect())
}

/// Put the index and working tree back to `tree`, a tree [`tree`] read earlier: changed files
/// get their content back, deleted ones return, and files the index holds that `tree` does not
/// are removed.
fn reset_to_tree(workspace: &Path, tree: &str) -> Result<()> {
    checked(
        workspace,
        &["read-tree", "-u", "--reset", tree],
        "git read-tree",
    )
    .map(|_| ())
}

/// Run git in `workspace` and return its trimmed stdout, or fail naming `operation`.
fn checked(workspace: &Path, args: &[&str], operation: &'static str) -> Result<String> {
    let out = git(workspace, args)?;
    if !out.status.success() {
        return Err(GitFailed::new(operation, &out.stderr).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
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
    use crate::plan::worktree::{changes, restore, snapshot};
    use std::path::{Path, PathBuf};

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("worktree-tree-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.email", "t@example.com"]);
        git(&dir, &["config", "user.name", "t"]);
        git(&dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("kept.txt"), "kept\n").unwrap();
        std::fs::write(dir.join("edited.txt"), "original\n").unwrap();
        std::fs::write(dir.join("deleted.txt"), "here\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "ignored/\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "base"]);
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
        let before = snapshot(&dir).unwrap();
        assert_eq!(
            snapshot(&dir).unwrap(),
            before,
            "reading the tree changes nothing"
        );

        std::fs::create_dir_all(dir.join("ignored")).unwrap();
        std::fs::write(dir.join("ignored/cache"), "scratch\n").unwrap();
        assert_eq!(
            snapshot(&dir).unwrap(),
            before,
            "an ignored file is not workspace content"
        );

        std::fs::write(dir.join("edited.txt"), "rewritten\n").unwrap();
        std::fs::remove_file(dir.join("deleted.txt")).unwrap();
        std::fs::create_dir_all(dir.join("new")).unwrap();
        std::fs::write(dir.join("new/file.txt"), "added\n").unwrap();
        let after = snapshot(&dir).unwrap();
        assert_ne!(after, before);
        assert_eq!(
            changes(&dir, &before, &after).unwrap(),
            ["deleted.txt", "edited.txt", "new/file.txt"]
        );

        restore(&dir, &before).unwrap();
        assert_eq!(snapshot(&dir).unwrap(), before);
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

    /// A commit, a branch switch, and a detached checkout each move HEAD, and each is named and
    /// put back, the branch HEAD was on included, whether or not the content reads differently.
    #[test]
    fn a_moved_head_is_named_and_put_back() {
        let dir = repo("head");
        let branch = git(&dir, &["symbolic-ref", "HEAD"]);
        let base = git(&dir, &["rev-parse", "HEAD"]);
        let before = snapshot(&dir).unwrap();

        // Committing the pending state leaves the content as it was and moves only HEAD.
        git(&dir, &["commit", "-qm", "rogue"]);
        let after = snapshot(&dir).unwrap();
        let changed = changes(&dir, &before, &after).unwrap();
        assert_eq!(changed.len(), 1, "{changed:?}");
        assert!(
            changed[0].starts_with(&format!("HEAD moved from {branch} at {}", &base[..12])),
            "{changed:?}"
        );
        restore(&dir, &before).unwrap();
        assert_eq!(snapshot(&dir).unwrap(), before);
        assert_eq!(git(&dir, &["rev-parse", &branch]), base);
        assert_eq!(read(&dir, "pending.txt").as_deref(), Some("pending\n"));

        git(&dir, &["checkout", "-q", "-b", "elsewhere"]);
        std::fs::write(dir.join("elsewhere.txt"), "x\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "elsewhere"]);
        let changed = changes(&dir, &before, &snapshot(&dir).unwrap()).unwrap();
        assert_eq!(changed[0], "elsewhere.txt");
        assert!(changed[1].contains("refs/heads/elsewhere"), "{changed:?}");
        restore(&dir, &before).unwrap();
        assert_eq!(snapshot(&dir).unwrap(), before);
        assert_eq!(git(&dir, &["symbolic-ref", "HEAD"]), branch);
        assert_eq!(read(&dir, "elsewhere.txt"), None);

        git(&dir, &["checkout", "-q", "--detach"]);
        let changed = changes(&dir, &before, &snapshot(&dir).unwrap()).unwrap();
        assert!(
            changed[0].ends_with(&format!("detached at {}", &base[..12])),
            "{changed:?}"
        );
        restore(&dir, &before).unwrap();
        assert_eq!(git(&dir, &["symbolic-ref", "HEAD"]), branch);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A repository with no commit yet has a branch and no commit, and a first commit on it is
    /// put back by removing the branch again.
    #[test]
    fn a_first_commit_on_an_unborn_branch_is_put_back() {
        let dir = std::env::temp_dir().join(format!("worktree-unborn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.email", "t@example.com"]);
        git(&dir, &["config", "user.name", "t"]);
        git(&dir, &["config", "commit.gpgsign", "false"]);
        let branch = git(&dir, &["symbolic-ref", "HEAD"]);
        let before = snapshot(&dir).unwrap();

        std::fs::write(dir.join("first.txt"), "first\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "first"]);
        let changed = changes(&dir, &before, &snapshot(&dir).unwrap()).unwrap();
        assert!(
            changed
                .iter()
                .any(|c| c.contains(&format!("from {branch} with no commits"))),
            "{changed:?}"
        );
        restore(&dir, &before).unwrap();
        assert_eq!(snapshot(&dir).unwrap(), before);
        assert_eq!(read(&dir, "first.txt"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_workspace_that_is_not_a_repository_is_an_error_not_an_empty_tree() {
        let dir = std::env::temp_dir().join(format!("worktree-norepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(snapshot(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
