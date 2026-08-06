//! The gate self-test (the frozen-judge safeguard): stage a known-good
//! and a known-bad configuration in turn, measure each through the domain's own [`Judge`], and assert the
//! gate can tell them apart (strictly, by `[judge].direction`) before it's trusted. Runs pre-loop
//! (`crucible check`); it never runs inside a loop iteration.

use crate::command_judge::Direction;
use crate::crucible::{Judge, MeasureCtx, World};
use crate::manifest::SelftestCfg;
use anyhow::{Context, Result};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
#[error("selftest control `{command}` exited nonzero: {status}")]
struct ControlCommandFailed {
    command: String,
    status: std::process::ExitStatus,
}

/// The self-test's verdict: each control's full outcome (per-run readings + mean), the direction it was
/// judged by, and whether the gate discriminated (`good` strictly better than `bad`, both valid, per
/// `direction`). Carries the per-run detail so a refine turn can be told
/// exactly *how* the gate failed, not just that it did.
#[derive(Debug, Clone)]
pub(crate) struct SelftestReport {
    pub good: ControlResult,
    pub bad: ControlResult,
    pub direction: Direction,
    pub passed: bool,
    pub runs: u32,
}

/// One control's outcome before the discrimination check: the staging command, every reading it
/// produced (in order), the mean score across `runs`, and whether every one of them was valid.
#[derive(Debug, Clone)]
pub(crate) struct ControlResult {
    pub cmd: String,
    pub readings: Vec<ReadingOutcome>,
    pub mean_score: f64,
    pub all_valid: bool,
}

/// A single measurement's contract-relevant fields, kept so evidence can quote the actual numbers
/// (and any invalid-reading note) back to the refiner.
#[derive(Debug, Clone)]
pub(crate) struct ReadingOutcome {
    pub valid: bool,
    pub score: Option<f64>,
    pub note: String,
}

/// Run the self-test in `workspace`: stage `cfg.good_cmd`, measure (averaged over `cfg.runs`
/// through `judge`), restore to pristine; then the same for `cfg.bad_cmd`. `workspace` must
/// already hold the pristine state the run started from; it is restored to that state on every
/// exit path (pass, fail, or error), via `world.snapshot`/`world.restore`.
pub(crate) fn run(
    world: &dyn World,
    judge: &dyn Judge,
    workspace: &Path,
    cfg: &SelftestCfg,
    direction: Direction,
) -> Result<SelftestReport> {
    let pristine = world
        .snapshot("selftest-pristine")
        .context("snapshotting the workspace before the gate self-test")?;
    let controls = (|| -> Result<(ControlResult, ControlResult)> {
        let good = run_control(world, judge, workspace, &cfg.good_cmd, &pristine, cfg.runs)
            .context("running the known-good control")?;
        let bad = run_control(world, judge, workspace, &cfg.bad_cmd, &pristine, cfg.runs)
            .context("running the known-bad control")?;
        Ok((good, bad))
    })();
    world
        .restore(&pristine)
        .context("restoring the workspace after the gate self-test")?;
    let (good, bad) = controls?;
    let passed =
        good.all_valid && bad.all_valid && direction.better(good.mean_score, bad.mean_score);
    Ok(SelftestReport {
        good,
        bad,
        direction,
        passed,
        runs: cfg.runs,
    })
}

/// Restore to `pristine`, stage `cmd`, measure `runs` times, restore to `pristine` again, so the
/// next control (or the caller) always starts from the same known state.
fn run_control(
    world: &dyn World,
    judge: &dyn Judge,
    workspace: &Path,
    cmd: &str,
    pristine: &str,
    runs: u32,
) -> Result<ControlResult> {
    world
        .restore(pristine)
        .context("restoring before staging control")?;
    stage(workspace, cmd)?;
    let runs = runs.max(1);
    let mut sum = 0.0;
    let mut all_valid = true;
    let mut readings = Vec::with_capacity(runs as usize);
    for _ in 0..runs {
        let reading = judge.measure(&MeasureCtx::default())?;
        all_valid &= reading.valid;
        sum += reading.score.unwrap_or(0.0);
        readings.push(ReadingOutcome {
            valid: reading.valid,
            score: reading.score,
            note: reading.note,
        });
    }
    world
        .restore(pristine)
        .context("restoring after measuring control")?;
    Ok(ControlResult {
        cmd: cmd.to_string(),
        readings,
        mean_score: sum / f64::from(runs),
        all_valid,
    })
}

/// Run a control's staging command (`good_cmd`/`bad_cmd`) in `workspace`, same `sh -c` /
/// cwd-in-workspace convention as every other domain command (`apply_cmd`, `setup_cmd`, …).
fn stage(workspace: &Path, cmd: &str) -> Result<()> {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(workspace)
        .status()
        .with_context(|| format!("exec selftest control `{cmd}` in {}", workspace.display()))?;
    if !status.success() {
        return Err(ControlCommandFailed {
            command: cmd.to_owned(),
            status,
        }
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_world::GitWorld;
    use crate::manifest::SelftestCfg;
    use std::fs;
    use std::path::PathBuf;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crucible-selftest-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).expect("mkdir tmp");
        dir
    }

    fn init_git(dir: &Path) {
        let repo = git2::Repository::init(dir).expect("git init");
        let mut cfg = repo.config().expect("config");
        cfg.set_str("user.name", "t").expect("name");
        cfg.set_str("user.email", "t@t").expect("email");
        fs::write(dir.join("value.txt"), "0\n").expect("seed file");
        let mut index = repo.index().expect("index");
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .expect("add");
        index.write().expect("write index");
        let tree = repo
            .find_tree(index.write_tree().expect("tree"))
            .expect("find tree");
        let sig = repo.signature().expect("sig");
        repo.commit(Some("HEAD"), &sig, &sig, "seed", &tree, &[])
            .expect("commit");
    }

    /// A judge that reads `value.txt` in the workspace as its score. Mirrors `CommandJudge`
    /// closely enough for the discrimination assert without spawning a real measure_cmd per test.
    struct ValueJudge {
        workspace: PathBuf,
    }

    impl Judge for ValueJudge {
        fn measure(&self, _ctx: &MeasureCtx) -> Result<crate::crucible::Reading> {
            let text = fs::read_to_string(self.workspace.join("value.txt"))
                .context("reading value.txt")?;
            let score: f64 = text.trim().parse().context("parsing value.txt")?;
            Ok(crate::crucible::Reading {
                valid: true,
                score: Some(score),
                tiebreak: None,
                solved: false,
                note: String::new(),
                detail: serde_json::Value::Null,
            })
        }
        fn decide(
            &self,
            _r: &crate::crucible::Reading,
            _best_score: f64,
            _best_tiebreak: Option<f64>,
        ) -> crate::crucible::Decision {
            unreachable!("selftest doesn't call decide")
        }
        fn status(&self, _best_score: f64) -> String {
            String::new()
        }
        fn improved(&self, _best_score: f64, _baseline_score: f64, _solved_any: bool) -> bool {
            false
        }
        fn detail(&self, _r: &crate::crucible::Reading) -> String {
            String::new()
        }
        fn objective(&self) -> String {
            "value".into()
        }
        fn direction(&self) -> crate::command_judge::Direction {
            crate::command_judge::Direction::Higher
        }
    }

    fn write_cmd(workspace: &Path, name: &str, value: &str) -> String {
        // Stage a value + commit, so restoring to pristine actually undoes it (GitWorld
        // reversibility is git; an uncommitted-only edit would survive a hard reset anyway, but
        // committing is the honest shape of a real staging command).
        format!(
            "echo {value} > {}/value.txt && git -C {} add value.txt && git -C {} -c user.email=t@t -c user.name=t commit -qm {name}",
            workspace.display(),
            workspace.display(),
            workspace.display(),
        )
    }

    #[test]
    fn discriminating_gate_passes_higher_is_better() {
        let dir = tempdir("discriminates-higher");
        init_git(&dir);
        let world = GitWorld {
            workspace: dir.clone(),
        };
        let judge = ValueJudge {
            workspace: dir.clone(),
        };
        let cfg = SelftestCfg {
            good_cmd: write_cmd(&dir, "good", "100"),
            bad_cmd: write_cmd(&dir, "bad", "10"),
            runs: 1,
        };
        let report = run(&world, &judge, &dir, &cfg, Direction::Higher).expect("selftest runs");
        assert!(report.passed, "100 > 10 must discriminate: {report:?}");
        assert_eq!(report.good.mean_score, 100.0);
        assert_eq!(report.bad.mean_score, 10.0);
        assert_eq!(report.good.readings.len(), 1, "one reading per run");
        assert!(report.good.all_valid);

        // Pristine afterward: value.txt is back to the seed.
        let value = fs::read_to_string(dir.join("value.txt")).unwrap();
        assert_eq!(value.trim(), "0", "workspace restored to pristine");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discriminating_gate_passes_lower_is_better() {
        let dir = tempdir("discriminates-lower");
        init_git(&dir);
        let world = GitWorld {
            workspace: dir.clone(),
        };
        let judge = ValueJudge {
            workspace: dir.clone(),
        };
        let cfg = SelftestCfg {
            good_cmd: write_cmd(&dir, "good", "5"),
            bad_cmd: write_cmd(&dir, "bad", "50"),
            runs: 3,
        };
        let report = run(&world, &judge, &dir, &cfg, Direction::Lower).expect("selftest runs");
        assert!(report.passed, "5 < 50 must discriminate: {report:?}");
        assert_eq!(report.runs, 3);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_discriminating_gate_fails() {
        let dir = tempdir("non-discriminating");
        init_git(&dir);
        let world = GitWorld {
            workspace: dir.clone(),
        };
        let judge = ValueJudge {
            workspace: dir.clone(),
        };
        // good == bad: no discrimination at all.
        let cfg = SelftestCfg {
            good_cmd: write_cmd(&dir, "good", "10"),
            bad_cmd: write_cmd(&dir, "bad", "10"),
            runs: 1,
        };
        let report = run(&world, &judge, &dir, &cfg, Direction::Higher).expect("selftest runs");
        assert!(!report.passed, "equal scores must not discriminate");

        // good worse than bad (inverted): must also fail, not just "not better".
        let cfg = SelftestCfg {
            good_cmd: write_cmd(&dir, "good2", "1"),
            bad_cmd: write_cmd(&dir, "bad2", "99"),
            runs: 1,
        };
        let report = run(&world, &judge, &dir, &cfg, Direction::Higher).expect("selftest runs");
        assert!(!report.passed, "an inverted gate must not discriminate");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failing_selftest_carries_per_run_evidence() {
        // Drive the real selftest with a tiny sh fixture: good staged BELOW bad under `higher`, so
        // it can't discriminate, then confirm the report carries every reading and converts to the
        // serde evidence a refine turn is handed.
        let dir = tempdir("evidence");
        init_git(&dir);
        let world = GitWorld {
            workspace: dir.clone(),
        };
        let judge = ValueJudge {
            workspace: dir.clone(),
        };
        let cfg = SelftestCfg {
            good_cmd: write_cmd(&dir, "good", "10"),
            bad_cmd: write_cmd(&dir, "bad", "100"),
            runs: 3,
        };
        let report = run(&world, &judge, &dir, &cfg, Direction::Higher).expect("selftest runs");
        assert!(!report.passed, "10 < 100 under higher can't discriminate");
        assert_eq!(
            report.good.readings.len(),
            3,
            "one reading per run captured"
        );
        assert_eq!(report.bad.readings.len(), 3);
        assert_eq!(report.good.mean_score, 10.0);
        assert_eq!(report.bad.mean_score, 100.0);
        assert!(report.good.all_valid && report.bad.all_valid);

        let ev: crate::refine::SelftestEvidence = (&report).into();
        assert_eq!(ev.direction, "higher");
        assert_eq!(ev.runs, 3);
        assert_eq!(ev.good.readings.len(), 3);
        assert_eq!(ev.good.mean, 10.0);
        assert_eq!(ev.bad.mean, 100.0);
        assert!(
            ev.good.cmd.contains("value.txt"),
            "cmd carried: {}",
            ev.good.cmd
        );
        assert_eq!(ev.good.readings[0].score, Some(10.0));
        assert!(ev.good.readings[0].valid);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_restored_pristine_even_on_failure() {
        let dir = tempdir("restore-on-fail");
        init_git(&dir);
        let world = GitWorld {
            workspace: dir.clone(),
        };
        let judge = ValueJudge {
            workspace: dir.clone(),
        };
        let cfg = SelftestCfg {
            good_cmd: write_cmd(&dir, "good", "1"),
            bad_cmd: write_cmd(&dir, "bad", "99"),
            runs: 1,
        };
        let report = run(&world, &judge, &dir, &cfg, Direction::Higher).expect("selftest runs");
        assert!(!report.passed);
        let value = fs::read_to_string(dir.join("value.txt")).unwrap();
        assert_eq!(
            value.trim(),
            "0",
            "restored to pristine even though the self-test failed"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
