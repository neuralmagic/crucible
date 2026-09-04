//! Git-as-memory on the agent's workspace, via libgit2 (no `git` subprocess).
//! Wraps the porcelain equivalents of `add -A`, `diff --cached [--shortstat]`,
//! `commit`, `reset --hard`, and `clean -fd -e ...`.

use anyhow::{Context, Result};
use git2::{
    DiffFormat, DiffStatsFormat, IndexAddOption, ObjectType, Repository, ResetType, Status,
    StatusOptions,
};
use std::path::Path;

/// Guarantee `ws` is a git repo with at least one commit AND a commit identity, so the
/// keep/discard memory (commit / staged-diff / reset) actually works. In-pod loop images
/// bake the workspace as a plain source tree, and pre-baked clones carry no local
/// `user.name`/`email`, so both must be seeded here.
pub fn ensure_repo(ws: &Path) -> Result<()> {
    let existed = ws.join(".git").exists();
    let repo = if existed {
        Repository::open(ws).context("open existing workspace repo")?
    } else {
        Repository::init(ws).context("git init workspace")?
    };
    {
        let mut cfg = repo.config().context("open repo config")?;
        if cfg.get_string("user.name").is_err() {
            cfg.set_str("user.name", "autoresearch")
                .context("set user.name")?;
        }
        if cfg.get_string("user.email").is_err() {
            cfg.set_str("user.email", "autoresearch@crucible.local")
                .context("set user.email")?;
        }
    }
    if existed {
        return Ok(());
    }
    let mut index = repo.index().context("open index")?;
    index
        .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
        .context("index add_all")?;
    index.write().context("write index")?;
    let tree_oid = index.write_tree().context("write tree")?;
    let tree = repo.find_tree(tree_oid).context("find tree")?;
    let sig = repo.signature().context("repo signature")?;
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "autoresearch: baseline workspace",
        &tree,
        &[],
    )
    .context("initial commit")?;
    Ok(())
}

/// `git add -A`: stage new, modified, and deleted paths (honoring .gitignore, like porcelain).
pub(crate) fn stage_all(ws: &Path) -> Result<()> {
    let repo = Repository::open(ws).context("open workspace repo")?;
    let mut index = repo.index().context("open index")?;
    index
        .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
        .context("index add_all")?;
    index.write().context("write index")?;
    Ok(())
}

/// The staged change as a unified diff (`git diff --cached`).
pub(crate) fn staged_diff(ws: &Path) -> Result<String> {
    let repo = Repository::open(ws).context("open workspace repo")?;
    let index = repo.index().context("open index")?;
    let head_tree = match repo.head() {
        Ok(h) => Some(h.peel_to_tree().context("peel HEAD to tree")?),
        Err(_) => None, // unborn HEAD: diff against the empty tree
    };
    let diff = repo
        .diff_tree_to_index(head_tree.as_ref(), Some(&index), None)
        .context("diff tree to index")?;

    let mut buf = String::new();
    diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
        // Patch lines carry an origin marker for +/-/context; headers don't.
        if matches!(line.origin(), '+' | '-' | ' ') {
            buf.push(line.origin());
        }
        buf.push_str(&String::from_utf8_lossy(line.content()));
        true
    })
    .context("render patch")?;
    Ok(buf)
}

/// Does the index differ from HEAD? Counts deltas instead of checking shortstat text: some libgit2 builds
/// render a clean diff as `"0 files changed, …"`, so a string-emptiness check spuriously
/// commits empty snapshots.
pub(crate) fn has_staged_changes(ws: &Path) -> Result<bool> {
    let repo = Repository::open(ws).context("open workspace repo")?;
    let index = repo.index().context("open index")?;
    let head_tree = match repo.head() {
        Ok(h) => Some(h.peel_to_tree().context("peel HEAD to tree")?),
        Err(_) => None, // unborn HEAD: any staged path is a change
    };
    let diff = repo
        .diff_tree_to_index(head_tree.as_ref(), Some(&index), None)
        .context("diff tree to index")?;
    Ok(diff.stats().context("diff stats")?.files_changed() > 0)
}

/// One-line `git diff --cached --shortstat` (e.g. "3 files changed, 12 insertions(+)").
pub(crate) fn staged_shortstat(ws: &Path) -> Result<String> {
    let repo = Repository::open(ws).context("open workspace repo")?;
    let index = repo.index().context("open index")?;
    let head_tree = match repo.head() {
        Ok(h) => Some(h.peel_to_tree().context("peel HEAD to tree")?),
        Err(_) => None,
    };
    let diff = repo
        .diff_tree_to_index(head_tree.as_ref(), Some(&index), None)
        .context("diff tree to index")?;
    let stats = diff.stats().context("diff stats")?;
    let buf = stats
        .to_buf(DiffStatsFormat::SHORT, 80)
        .context("format shortstat")?;
    Ok(String::from_utf8_lossy(&buf).trim().to_string())
}

/// `git commit -m <msg>` of the current index onto HEAD. Returns the new commit's SHA.
pub(crate) fn commit_all(ws: &Path, msg: &str) -> Result<git2::Oid> {
    let repo = Repository::open(ws).context("open workspace repo")?;
    let mut index = repo.index().context("open index")?;
    let tree_oid = index.write_tree().context("write tree")?;
    let tree = repo.find_tree(tree_oid).context("find tree")?;
    let sig = repo
        .signature()
        .context("repo signature (user.name/email)")?;
    let parent = repo
        .head()
        .context("HEAD")?
        .peel_to_commit()
        .context("peel HEAD to commit")?;
    let oid = repo
        .commit(Some("HEAD"), &sig, &sig, msg, &tree, &[&parent])
        .context("commit")?;
    Ok(oid)
}

/// `git reset --hard <sha>`: roll the working tree + index to a specific commit.
pub(crate) fn reset_to(ws: &Path, sha: &str) -> Result<()> {
    let repo = Repository::open(ws).context("open workspace repo")?;
    let oid = git2::Oid::from_str(sha).with_context(|| format!("parse sha {sha}"))?;
    let obj = repo
        .find_object(oid, Some(ObjectType::Commit))
        .with_context(|| format!("find commit {sha}"))?;
    repo.reset(&obj, ResetType::Hard, None)
        .context("reset --hard <sha>")?;
    Ok(())
}

/// The current `HEAD` commit sha (the snapshot token when nothing changed to commit).
pub fn head_sha(ws: &Path) -> Result<String> {
    let repo = Repository::open(ws).context("open workspace repo")?;
    let commit = repo
        .head()
        .context("HEAD")?
        .peel_to_commit()
        .context("peel HEAD to commit")?;
    Ok(commit.id().to_string())
}

/// `git status --porcelain -- <path>`: whether the workspace-relative `path` differs from what
/// the repository has committed. A path git has never tracked is not part of that state, so it
/// counts as changed.
pub fn differs_from_committed(ws: &Path, path: &str) -> Result<bool> {
    let repo = Repository::open(ws).context("open workspace repo")?;
    let status = repo
        .status_file(Path::new(path))
        .with_context(|| format!("status of {path}"))?;
    Ok(status != Status::CURRENT)
}

/// `git clean -fd -e <keep> ...`: remove untracked (not ignored) files and fully-untracked
/// directories, preserving the given workspace-relative paths (at any depth); leaves dirs that
/// still hold tracked files.
pub(crate) fn clean_untracked(ws: &Path, keep: &[&str]) -> Result<()> {
    let repo = Repository::open(ws).context("open workspace repo")?;
    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(false) // fully-untracked dirs collapse to one entry
        .include_ignored(false);
    let statuses = repo.statuses(Some(&mut opts)).context("status")?;
    for entry in statuses.iter() {
        if !entry.status().contains(Status::WT_NEW) {
            continue;
        }
        let Ok(raw) = entry.path() else { continue };
        let rel = raw.trim_end_matches('/'); // untracked dirs carry a trailing slash
        if is_kept(rel, keep) {
            continue;
        }
        remove_unless_kept(&ws.join(rel), rel, keep);
    }
    Ok(())
}

/// True when `rel` is a keep entry or lives under one.
fn is_kept(rel: &str, keep: &[&str]) -> bool {
    keep.iter()
        .any(|k| rel == *k || rel.starts_with(&format!("{k}/")))
}

/// True when `rel` is a strict ancestor of some keep entry, i.e. deleting it would take a kept
/// path with it.
fn holds_kept(rel: &str, keep: &[&str]) -> bool {
    keep.iter().any(|k| k.starts_with(&format!("{rel}/")))
}

/// Delete `full` (workspace-relative `rel`), except that a directory holding a nested keep entry
/// is descended into instead: status collapses a fully-untracked dir to a single entry, so a keep
/// like `out/traces` arrives here as the parent `out`, and `remove_dir_all` would take the kept
/// artifacts with it (it bypasses git, so the exclude buys nothing).
fn remove_unless_kept(full: &Path, rel: &str, keep: &[&str]) {
    if !full.is_dir() {
        let _ = std::fs::remove_file(full);
        return;
    }
    if !holds_kept(rel, keep) {
        let _ = std::fs::remove_dir_all(full);
        return;
    }
    let Ok(children) = std::fs::read_dir(full) else {
        return;
    };
    for child in children.flatten() {
        let name = child.file_name();
        let Some(name) = name.to_str() else { continue };
        let child_rel = format!("{rel}/{name}");
        if is_kept(&child_rel, keep) {
            continue;
        }
        remove_unless_kept(&child.path(), &child_rel, keep);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn init_repo(dir: &Path) -> Repository {
        let repo = Repository::init(dir).expect("init");
        {
            let mut cfg = repo.config().expect("config");
            cfg.set_str("user.name", "test").expect("name");
            cfg.set_str("user.email", "test@test").expect("email");
        }
        repo
    }

    fn commit_initial(repo: &Repository, ws: &Path) {
        fs::write(ws.join("tracked.txt"), "v1\n").expect("write");
        let mut index = repo.index().expect("index");
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
            .expect("add");
        index.write().expect("write index");
        let tree = repo
            .find_tree(index.write_tree().expect("tree"))
            .expect("find");
        let sig = repo.signature().expect("sig");
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .expect("commit");
    }

    #[test]
    fn clean_removes_untracked_but_keeps_excludes_and_tracked() {
        let tmp = tempdir();
        let ws = tmp.as_path();
        let repo = init_repo(ws);
        commit_initial(&repo, ws);

        fs::write(ws.join("junk.txt"), "x").expect("junk");
        fs::create_dir_all(ws.join("scratch/nested")).expect("scratch");
        fs::write(ws.join("scratch/nested/a.txt"), "x").expect("nested");
        fs::create_dir_all(ws.join(".claude/skills")).expect("claude");
        fs::write(ws.join(".claude/skills/s.md"), "keep").expect("skill");
        fs::write(ws.join("RESULTS.md"), "keep").expect("results");

        clean_untracked(ws, &[".claude", "RESULTS.md"]).expect("clean");

        assert!(!ws.join("junk.txt").exists(), "untracked file removed");
        assert!(!ws.join("scratch").exists(), "untracked dir removed");
        assert!(ws.join(".claude/skills/s.md").exists(), ".claude kept");
        assert!(ws.join("RESULTS.md").exists(), "RESULTS.md kept");
        assert!(ws.join("tracked.txt").exists(), "tracked file kept");
    }

    #[test]
    fn clean_keeps_a_nested_path_inside_an_otherwise_doomed_dir() {
        let tmp = tempdir();
        let ws = tmp.as_path();
        let repo = init_repo(ws);
        commit_initial(&repo, ws);

        // `out/` is fully untracked, so status collapses it to one entry even though only
        // `out/traces` is kept.
        fs::create_dir_all(ws.join("out/traces/deep")).expect("traces");
        fs::write(ws.join("out/traces/deep/t.json"), "trace").expect("trace");
        fs::write(ws.join("out/junk.txt"), "x").expect("junk");
        fs::create_dir_all(ws.join("out/build")).expect("build");
        fs::write(ws.join("out/build/o.a"), "x").expect("obj");

        clean_untracked(ws, &["out/traces"]).expect("clean");

        assert_eq!(
            fs::read_to_string(ws.join("out/traces/deep/t.json")).expect("kept"),
            "trace"
        );
        assert!(!ws.join("out/junk.txt").exists(), "sibling file removed");
        assert!(!ws.join("out/build").exists(), "sibling dir removed");
    }

    #[test]
    fn ensure_repo_inits_baked_tree_and_enables_commits() {
        let tmp = tempdir();
        let ws = tmp.as_path();
        fs::write(ws.join("go.mod"), "module x\n").expect("write");
        assert!(!ws.join(".git").exists());

        ensure_repo(ws).expect("ensure_repo");
        assert!(ws.join(".git").exists(), "repo initialized");

        fs::write(ws.join("go.mod"), "module x\n// edit\n").expect("edit");
        stage_all(ws).expect("stage");
        let oid = commit_all(ws, "iter 1: keep").expect("commit on top of root");
        assert!(!oid.is_zero(), "got a real commit sha");

        ensure_repo(ws).expect("idempotent");
    }

    /// A same-size edit landing in the same second as the previous index write. libgit2
    /// compares mtime at one-second granularity unless built with USE_NSEC, so a stat-cache
    /// hit here would skip re-hashing and the edit would be staged as nothing.
    #[test]
    fn same_size_edit_in_the_same_second_is_still_staged() {
        let tmp = tempdir();
        let ws = tmp.as_path();
        fs::write(ws.join("value.txt"), "1\n").expect("write");
        ensure_repo(ws).expect("baseline commit");
        for want in ["2", "3", "4", "5"] {
            fs::write(ws.join("value.txt"), format!("{want}\n")).expect("edit");
            stage_all(ws).expect("stage");
            assert!(
                has_staged_changes(ws).expect("dirty check"),
                "a same-size edit to {want} must be seen as a staged change"
            );
            commit_all(ws, "keep").expect("commit");
        }
    }

    #[test]
    fn has_staged_changes_is_false_on_a_clean_tree() {
        let tmp = tempdir();
        let ws = tmp.as_path();
        fs::write(ws.join("f.txt"), "v0\n").expect("write");
        ensure_repo(ws).expect("ensure_repo commits the baked tree");
        stage_all(ws).expect("stage");
        assert!(
            !has_staged_changes(ws).expect("clean check"),
            "clean tree has no staged changes"
        );
        fs::write(ws.join("f.txt"), "v0\nedit\n").expect("edit");
        stage_all(ws).expect("stage");
        assert!(
            has_staged_changes(ws).expect("dirty check"),
            "an edit is a staged change"
        );
    }

    #[test]
    fn commit_then_reset_hard_round_trips() {
        let tmp = tempdir();
        let ws = tmp.as_path();
        let repo = init_repo(ws);
        commit_initial(&repo, ws);

        fs::write(ws.join("tracked.txt"), "v2\n").expect("modify");
        stage_all(ws).expect("stage");
        let stat = staged_shortstat(ws).expect("shortstat");
        assert!(stat.contains("1 file changed"), "shortstat: {stat}");
        let diff = staged_diff(ws).expect("diff");
        assert!(diff.contains("-v1") && diff.contains("+v2"), "diff: {diff}");

        let head = head_sha(ws).expect("head sha");
        reset_to(ws, &head).expect("reset");
        assert_eq!(fs::read_to_string(ws.join("tracked.txt")).unwrap(), "v1\n");
    }

    /// Minimal unique temp dir without pulling in a crate.
    fn tempdir() -> TempDir {
        let base = std::env::temp_dir().join(format!(
            "epp-vcs-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).expect("mkdir tmp");
        TempDir(base)
    }

    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn as_path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
