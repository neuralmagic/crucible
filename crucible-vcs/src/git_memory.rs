//! Git-as-memory as a World concern: a snapshot stages + commits the workspace (the
//! kept-commit chain is the memory), a restore resets + cleans it. Shared by every
//! git-backed World so the engine never names git.

use crate::vcs;
use anyhow::Result;
use std::path::Path;

/// Paths a discard must NOT delete: the agent's toolbox (`.claude`) and the results log live
/// untracked in the workspace and must survive a `git clean`.
const KEEP_ON_CLEAN: &[&str] = &[".claude", "RESULTS.md"];

/// Loop-owned files in the workspace root that are not candidate content. No candidate
/// diff, snapshot commit, or external tree hash may see them, or a turn with zero agent
/// edits still produces a "changed" candidate.
const HARNESS_EXCLUDES: &[&str] = &[
    ".claude/",
    "RESULTS.md",
    "CANDIDATE.md",
    "ESCALATION.json",
    "PROVISIONING_PENDING.json",
];

/// Idempotently append [`HARNESS_EXCLUDES`] to the workspace's `.git/info/exclude`, the
/// repo-local ignore file every git consumer honors. Best-effort: a workspace that isn't a
/// git checkout is left alone.
pub fn install_harness_excludes(ws: &Path) {
    let Ok(repo) = git2::Repository::open(ws) else {
        return;
    };
    let exclude = repo.path().join("info").join("exclude");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    let missing: Vec<&str> = HARNESS_EXCLUDES
        .iter()
        .copied()
        .filter(|e| !existing.lines().any(|l| l.trim() == *e))
        .collect();
    if missing.is_empty() {
        return;
    }
    let sep = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let _ = std::fs::create_dir_all(exclude.parent().unwrap_or(repo.path()));
    let _ = std::fs::write(&exclude, format!("{existing}{sep}{}\n", missing.join("\n")));
}

/// The git half of a World snapshot: stage the workspace and, if anything changed, commit it
/// with `label`. Returns the sha to roll back to: the new commit, or current `HEAD` when the
/// tree is unchanged.
pub fn snapshot(ws: &Path, label: &str) -> Result<String> {
    vcs::stage_all(ws)?;
    if vcs::has_staged_changes(ws)? {
        Ok(vcs::commit_all(ws, label)?.to_string())
    } else {
        vcs::head_sha(ws)
    }
}

/// The staged diff of the agent's edits in `ws`: stage everything, then return (diff text,
/// shortstat). Best-effort: any git error yields empty strings; the diff is never load-bearing.
pub fn staged_diff(ws: &Path) -> (String, String) {
    let _ = vcs::stage_all(ws);
    let diff = vcs::staged_diff(ws).unwrap_or_default();
    let stat = vcs::staged_shortstat(ws).unwrap_or_default();
    (diff, stat.trim().to_string())
}

/// The git half of a World restore: hard-reset to `sha`, then drop untracked files (keeping
/// the toolbox + results log).
pub fn restore(ws: &Path, sha: &str) -> Result<()> {
    vcs::reset_to(ws, sha)?;
    vcs::clean_untracked(ws, KEEP_ON_CLEAN)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tempdir() -> TempDir {
        let base = std::env::temp_dir().join(format!(
            "git-memory-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).expect("mkdir tmp");
        TempDir(base)
    }

    #[test]
    fn harness_excludes_install_idempotently_and_hide_furniture_from_git() {
        let tmp = tempdir();
        let ws = tmp.0.as_path();
        let repo = git2::Repository::init(ws).expect("init");
        {
            let mut cfg = repo.config().expect("config");
            cfg.set_str("user.name", "test").expect("name");
            cfg.set_str("user.email", "test@test").expect("email");
        }

        install_harness_excludes(ws);
        install_harness_excludes(ws);
        let exclude =
            std::fs::read_to_string(repo.path().join("info/exclude")).expect("exclude exists");
        for entry in HARNESS_EXCLUDES {
            assert_eq!(
                exclude.lines().filter(|l| l.trim() == *entry).count(),
                1,
                "{entry} written exactly once"
            );
        }

        std::fs::write(ws.join("RESULTS.md"), "log").expect("results");
        std::fs::write(ws.join("CANDIDATE.md"), "note").expect("note");
        std::fs::create_dir_all(ws.join(".claude/skills")).expect("toolbox");
        std::fs::write(ws.join(".claude/skills/S.md"), "skill").expect("skill");
        std::fs::write(ws.join("kernel.cuh"), "__global__").expect("edit");
        vcs::stage_all(ws).expect("stage");
        let index = repo.index().expect("index");
        let staged: Vec<String> = index
            .iter()
            .map(|e| String::from_utf8_lossy(&e.path).to_string())
            .collect();
        assert_eq!(staged, ["kernel.cuh"], "only the real edit is staged");
    }

    #[test]
    fn harness_excludes_skip_a_non_git_dir() {
        let tmp = tempdir();
        install_harness_excludes(tmp.0.as_path());
        assert!(!tmp.0.join(".git").exists());
    }
}
