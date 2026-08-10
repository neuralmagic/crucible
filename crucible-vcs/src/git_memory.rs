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
}
