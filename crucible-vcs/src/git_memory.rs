//! Git-as-memory as a World concern: a snapshot stages + commits the workspace (the
//! kept-commit chain is the memory), a restore resets + cleans it. Shared by every
//! git-backed World so the engine never names git.

use crate::vcs;
use anyhow::{Context, Result};
use std::path::Path;

/// Paths a discard must NOT delete: the agent's toolbox (`.claude`), the results log, and any
/// pending operator note live untracked in the workspace and must survive a `git clean`.
const KEEP_ON_CLEAN: &[&str] = &[".claude", "RESULTS.md", "OPERATOR_NOTE.md"];

/// Loop-owned files in the workspace root that are not candidate content. No candidate
/// diff, snapshot commit, or external tree hash may see them, or a turn with zero agent
/// edits still produces a "changed" candidate. `OPERATOR_NOTE.md` is the operator's steer
/// side channel; a keep that fires before the agent deletes it must not sweep it into the
/// kept commit (one shipped verbatim in a vLLM draft PR).
const HARNESS_EXCLUDES: &[&str] = &[
    ".claude/",
    "RESULTS.md",
    "CANDIDATE.md",
    "ESCALATION.json",
    "PROVISIONING_PENDING.json",
    "OPERATOR_NOTE.md",
];

/// Idempotently append [`HARNESS_EXCLUDES`] plus `extra` to the workspace's `.git/info/exclude`,
/// the repo-local ignore file every git consumer honors. `extra` is the manifest's carried
/// pipeline artifacts: they must be excluded so a turn that only regenerates them still reads as
/// "no candidate change", and they are also passed to [`restore`] so the clean spares them.
/// Best-effort: a workspace that isn't a git checkout is left alone.
pub fn install_harness_excludes(ws: &Path, extra: &[String]) {
    let Ok(repo) = git2::Repository::open(ws) else {
        return;
    };
    let exclude = repo.path().join("info").join("exclude");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    let wanted: Vec<String> = HARNESS_EXCLUDES
        .iter()
        .map(|e| (*e).to_string())
        .chain(extra.iter().map(|e| exclude_line(ws, e)))
        .collect();
    // Dedup ignores a trailing slash: a carried dir's entry is written slashless until the first
    // run creates the dir, and re-appending the `/` form afterwards would accrete near-duplicates
    // for a pattern that already matches.
    let mut missing: Vec<&str> = Vec::new();
    for line in &wanted {
        let want = line.trim_end_matches('/');
        let known = existing
            .lines()
            .any(|l| l.trim().trim_end_matches('/') == want)
            || missing.iter().any(|m| m.trim_end_matches('/') == want);
        if !known {
            missing.push(line.as_str());
        }
    }
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

/// The `.git/info/exclude` line for a carried path. A directory gets a trailing `/` so the
/// pattern can't also swallow a same-named file; entries that already end in `/` are declaring
/// themselves a directory even before the first run creates it.
fn exclude_line(ws: &Path, entry: &str) -> String {
    if entry.ends_with('/') || ws.join(entry).is_dir() {
        format!("{}/", entry.trim_end_matches('/'))
    } else {
        entry.to_string()
    }
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
/// the toolbox + results log, plus `keep_extra`: the manifest's carried pipeline artifacts).
///
/// `keep_extra` only spares UNTRACKED paths. A carried path that the repo tracks is reverted by
/// the reset like any other tracked file, which is why the manifest documents carry_forward as
/// pipeline output only.
pub fn restore(ws: &Path, sha: &str, keep_extra: &[String]) -> Result<()> {
    vcs::reset_to(ws, sha)?;
    let keep: Vec<&str> = KEEP_ON_CLEAN
        .iter()
        .copied()
        .chain(keep_extra.iter().map(|k| k.trim_end_matches('/')))
        .collect();
    vcs::clean_untracked(ws, &keep)?;
    Ok(())
}

/// One commit of a playbook run's git memory: the task that made it and its change as a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCommit {
    pub task: String,
    pub patch: Vec<u8>,
}

const TASK_LABEL: &str = "task ";

/// The commit message a settled task's snapshot carries.
pub fn task_label(task: &str) -> String {
    format!("{TASK_LABEL}{task}")
}

/// A commit between a run's base and `HEAD` that no task made.
#[derive(Debug, thiserror::Error)]
#[error("commit {summary:?} after the run's base was not made by a task")]
pub struct UnlabelledCommit {
    pub summary: String,
}

/// Every commit since `base`, oldest first, each with the task that made it.
pub fn task_commits(ws: &Path, base: &str) -> Result<Vec<TaskCommit>> {
    vcs::commits_since(ws, base)?
        .into_iter()
        .map(|(summary, patch)| match summary.strip_prefix(TASK_LABEL) {
            Some(task) => Ok(TaskCommit {
                task: task.to_string(),
                patch,
            }),
            None => Err(UnlabelledCommit { summary }.into()),
        })
        .collect()
}

/// Apply each commit onto `HEAD` in order, labelled with its task as the original was.
pub fn replay(ws: &Path, commits: &[TaskCommit]) -> Result<()> {
    for commit in commits {
        vcs::apply_and_commit(ws, &commit.patch, &task_label(&commit.task))
            .with_context(|| format!("replaying the commit of task {}", commit.task))?;
    }
    Ok(())
}

/// The tree `sha` points at, or `None` when this repository does not have that commit.
pub fn tree_of(ws: &Path, sha: &str) -> Result<Option<String>> {
    vcs::commit_tree(ws, sha)
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

    /// A workspace with one tracked file and a first commit, the shape every restore test needs.
    fn seeded_repo() -> (TempDir, String) {
        let tmp = tempdir();
        let ws = tmp.0.clone();
        std::fs::write(ws.join("kernel.cuh"), "v0\n").expect("seed");
        vcs::ensure_repo(&ws).expect("ensure repo");
        let sha = vcs::head_sha(&ws).expect("head");
        (tmp, sha)
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

        install_harness_excludes(ws, &[]);
        install_harness_excludes(ws, &[]);
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
        std::fs::write(ws.join("OPERATOR_NOTE.md"), "steer").expect("steer");
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
        install_harness_excludes(tmp.0.as_path(), &[]);
        assert!(!tmp.0.join(".git").exists());
    }

    #[test]
    fn carried_paths_are_excluded_idempotently_with_dir_lines() {
        let (tmp, _) = seeded_repo();
        let ws = tmp.0.as_path();
        std::fs::create_dir_all(ws.join("existing-dir")).expect("dir");
        let carried = [
            "codegen-out/".to_string(),
            "existing-dir".to_string(),
            "trace.json".to_string(),
        ];

        install_harness_excludes(ws, &carried);
        install_harness_excludes(ws, &carried);
        let exclude = std::fs::read_to_string(ws.join(".git/info/exclude")).expect("exclude");
        let count = |line: &str| exclude.lines().filter(|l| l.trim() == line).count();
        // A dir gets one trailing slash whether the manifest wrote one or the dir exists on disk;
        // a plain file entry is left alone.
        assert_eq!(count("codegen-out/"), 1, "{exclude}");
        assert_eq!(count("existing-dir/"), 1, "{exclude}");
        assert_eq!(count("trace.json"), 1, "{exclude}");
        assert_eq!(count("codegen-out"), 0, "no slashless duplicate: {exclude}");
    }

    #[test]
    fn restore_keeps_carried_output_and_still_cleans_siblings() {
        let (tmp, base) = seeded_repo();
        let ws = tmp.0.as_path();
        let carried = ["codegen-out/".to_string()];
        install_harness_excludes(ws, &carried);

        std::fs::create_dir_all(ws.join("codegen-out/traces")).expect("carried dir");
        std::fs::write(ws.join("codegen-out/traces/t.json"), "trace").expect("carried file");
        std::fs::write(ws.join("scratch.txt"), "junk").expect("junk");
        std::fs::write(ws.join("kernel.cuh"), "v1\n").expect("edit");

        restore(ws, &base, &carried).expect("restore");

        assert_eq!(
            std::fs::read_to_string(ws.join("codegen-out/traces/t.json")).expect("carried kept"),
            "trace"
        );
        assert!(!ws.join("scratch.txt").exists(), "sibling junk cleaned");
        assert_eq!(
            std::fs::read_to_string(ws.join("kernel.cuh")).expect("tracked"),
            "v0\n",
            "tracked edit rolled back"
        );
    }

    #[test]
    fn a_turn_that_only_writes_carried_output_is_not_a_candidate_change() {
        let (tmp, base) = seeded_repo();
        let ws = tmp.0.as_path();
        let carried = ["codegen-out/".to_string()];
        install_harness_excludes(ws, &carried);

        std::fs::create_dir_all(ws.join("codegen-out")).expect("carried dir");
        std::fs::write(ws.join("codegen-out/plan.md"), "port plan").expect("carried file");

        assert_eq!(
            snapshot(ws, "turn1").expect("snapshot"),
            base,
            "no memory commit for carried-only output"
        );
        let (diff, _) = staged_diff(ws);
        assert!(
            diff.is_empty(),
            "carried output stays out of the diff: {diff}"
        );
    }

    #[test]
    fn a_nested_carried_path_survives_a_dirty_parent() {
        let (tmp, base) = seeded_repo();
        let ws = tmp.0.as_path();
        let carried = ["out/codegen".to_string()];
        install_harness_excludes(ws, &carried);

        std::fs::create_dir_all(ws.join("out/codegen")).expect("carried dir");
        std::fs::write(ws.join("out/codegen/gen.rs"), "fn main() {}").expect("carried file");
        std::fs::write(ws.join("out/junk.log"), "noise").expect("junk");

        restore(ws, &base, &carried).expect("restore");

        assert_eq!(
            std::fs::read_to_string(ws.join("out/codegen/gen.rs")).expect("carried kept"),
            "fn main() {}"
        );
        assert!(!ws.join("out/junk.log").exists(), "parent still cleaned");
    }

    #[test]
    fn exclude_install_is_a_no_op_once_the_carried_dir_exists() {
        let (tmp, _) = seeded_repo();
        let ws = tmp.0.as_path();
        let carried = ["codegen-out".to_string()];

        install_harness_excludes(ws, &carried);
        std::fs::create_dir_all(ws.join("codegen-out")).expect("carried dir");
        install_harness_excludes(ws, &carried);

        let exclude = std::fs::read_to_string(ws.join(".git/info/exclude")).expect("exclude");
        let hits = exclude
            .lines()
            .filter(|l| l.trim().trim_end_matches('/') == "codegen-out")
            .count();
        assert_eq!(
            hits, 1,
            "no near-duplicate after the dir appears: {exclude}"
        );
    }

    #[test]
    fn a_tracked_carried_path_is_still_reverted() {
        let (tmp, base) = seeded_repo();
        let ws = tmp.0.as_path();
        // kernel.cuh is tracked, so listing it as carried buys nothing: excludes never apply to
        // tracked files and `reset --hard` reverts it. This pins the documented limitation.
        let carried = ["kernel.cuh".to_string()];
        install_harness_excludes(ws, &carried);
        std::fs::write(ws.join("kernel.cuh"), "v1\n").expect("edit");

        restore(ws, &base, &carried).expect("restore");
        assert_eq!(
            std::fs::read_to_string(ws.join("kernel.cuh")).expect("tracked"),
            "v0\n"
        );
    }

    /// A copy of `ws` at its first commit, as a fresh checkout of the same base would be.
    fn clone_at_base(ws: &Path) -> TempDir {
        let tmp = tempdir();
        git2::Repository::clone(ws.to_str().expect("utf-8 path"), &tmp.0).expect("clone");
        let first = vcs::commits_since(ws, &first_commit(ws)).expect("history");
        let back = first.len();
        let repo = git2::Repository::open(&tmp.0).expect("open clone");
        let mut head = repo.head().unwrap().peel_to_commit().unwrap();
        for _ in 0..back {
            head = head.parent(0).unwrap();
        }
        repo.reset(head.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "t").unwrap();
        cfg.set_str("user.email", "t@t").unwrap();
        tmp
    }

    fn first_commit(ws: &Path) -> String {
        let repo = git2::Repository::open(ws).unwrap();
        let mut walk = repo.revwalk().unwrap();
        walk.push_head().unwrap();
        walk.set_sorting(git2::Sort::REVERSE).unwrap();
        walk.next().unwrap().unwrap().to_string()
    }

    #[test]
    fn task_commits_replay_onto_a_fresh_checkout_of_the_base() {
        let (tmp, base) = seeded_repo();
        let ws = tmp.0.clone();
        std::fs::write(ws.join("kernel.cuh"), "v1\n").unwrap();
        std::fs::write(ws.join("blob.bin"), [0u8, 159, 146, 150, 255, 0, 7]).unwrap();
        snapshot(&ws, &task_label("edit")).unwrap();
        std::fs::remove_file(ws.join("kernel.cuh")).unwrap();
        std::fs::write(ws.join("notes.md"), "fanned\n").unwrap();
        snapshot(&ws, &task_label("audit[a b]")).unwrap();

        let commits = task_commits(&ws, &base).unwrap();
        assert_eq!(
            commits.iter().map(|c| c.task.as_str()).collect::<Vec<_>>(),
            ["edit", "audit[a b]"]
        );

        let fresh = clone_at_base(&ws);
        assert_eq!(vcs::head_sha(&fresh.0).unwrap(), base);
        replay(&fresh.0, &commits).unwrap();
        assert!(!fresh.0.join("kernel.cuh").exists());
        assert_eq!(
            std::fs::read(fresh.0.join("blob.bin")).unwrap(),
            [0u8, 159, 146, 150, 255, 0, 7]
        );
        assert_eq!(
            std::fs::read_to_string(fresh.0.join("notes.md")).unwrap(),
            "fanned\n"
        );
        assert_eq!(
            tree_of(&fresh.0, &vcs::head_sha(&fresh.0).unwrap()).unwrap(),
            tree_of(&ws, &vcs::head_sha(&ws).unwrap()).unwrap(),
            "the replayed tree is the original's"
        );
        assert_eq!(
            task_commits(&fresh.0, &base).unwrap(),
            commits,
            "a replayed history exports as the one it came from"
        );
    }

    #[test]
    fn a_commit_no_task_made_is_refused() {
        let (tmp, base) = seeded_repo();
        let ws = tmp.0.clone();
        std::fs::write(ws.join("kernel.cuh"), "v1\n").unwrap();
        snapshot(&ws, "operator fixup").unwrap();
        let err = task_commits(&ws, &base).unwrap_err();
        assert!(
            err.downcast_ref::<UnlabelledCommit>()
                .is_some_and(|e| e.summary == "operator fixup"),
            "{err:#}"
        );
    }

    #[test]
    fn a_patch_that_no_longer_applies_names_its_task() {
        let (tmp, base) = seeded_repo();
        let ws = tmp.0.clone();
        std::fs::write(ws.join("kernel.cuh"), "v1\n").unwrap();
        snapshot(&ws, &task_label("edit")).unwrap();
        let commits = task_commits(&ws, &base).unwrap();
        restore(&ws, &base, &[]).unwrap();
        std::fs::write(ws.join("kernel.cuh"), "diverged\n").unwrap();
        snapshot(&ws, "elsewhere").unwrap();
        let err = replay(&ws, &commits).unwrap_err();
        assert!(format!("{err:#}").contains("task edit"), "{err:#}");
    }

    #[test]
    fn tree_of_an_unknown_commit_is_none() {
        let (tmp, base) = seeded_repo();
        assert!(tree_of(&tmp.0, &base).unwrap().is_some());
        assert_eq!(
            tree_of(&tmp.0, "0123456789012345678901234567890123456789").unwrap(),
            None
        );
        assert_eq!(tree_of(&tmp.0, "not-a-sha").unwrap(), None);
    }
}
