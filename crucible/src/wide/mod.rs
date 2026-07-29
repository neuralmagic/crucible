//! Wide/explore phase: fan out N independent candidate approaches in parallel, measure them
//! serially on the shared deployment, rank by the gate, and seed the deep loop with the winner(s).
//!
//! Each candidate gets one PROPOSE turn in its own git worktree (thread-per-candidate),
//! biased to a distinct `[search].approaches` entry. Measurement serializes on the main
//! workspace: the candidate's diff is applied (cherry-picked) to the main workspace, the
//! judge measures, then the workspace is restored before the next candidate. The policy
//! (v1: TopK) picks winners from the scored set.
//!
//! The wide round is a PRE-LOOP step: it runs before the `for it in start_iter..=iterations`
//! deep loop. `run_tournament` returns the winning candidate(s); the caller patches the main
//! workspace with the winner's diff to seed the deep loop.

pub mod candidate;
pub mod policy;

use crate::crucible::{Judge, MeasureCtx, World};
use crate::reporter::{Reporter, Row};
use crate::{Args, Paths, Prepared, STOP, agent};
use anyhow::{Context, Result};
use candidate::{CandidateResult, WideCandidate};
use policy::ScoredCandidate;
use std::path::Path;
use std::sync::atomic::Ordering;

/// The resolved wide-round config, merged from CLI flags + manifest `[search]`.
pub struct WideConfig {
    pub n: u32,
    pub k: u32,
    pub approaches: Vec<String>,
    pub direction: String,
}

impl WideConfig {
    /// Merge CLI flags (`--wide`, `--wide-keep`) with the manifest's `[search]`. CLI wins.
    pub fn resolve(
        args: &Args,
        search: Option<&crate::manifest::SearchCfg>,
        direction: &str,
    ) -> Option<Self> {
        let n = if args.wide > 0 {
            args.wide
        } else {
            search.map(|s| s.wide).unwrap_or(0)
        };
        if n == 0 {
            return None;
        }
        let k = if args.wide_keep > 0 && args.wide > 0 {
            args.wide_keep
        } else {
            search.map(|s| s.policy_k).unwrap_or(1)
        };
        let approaches = search.map(|s| s.approaches.clone()).unwrap_or_default();
        if approaches.len() < n as usize {
            return None;
        }
        Some(WideConfig {
            n,
            k,
            approaches,
            direction: direction.to_string(),
        })
    }
}

/// Result of the wide tournament: the winning candidate(s) and their rows.
pub struct TournamentResult {
    /// Winning candidate indices (by score), in rank order.
    pub winners: Vec<u32>,
    /// All candidates' rows (for the session log).
    pub rows: Vec<Row>,
    /// All candidates with their results (winners have `result.is_some()`).
    pub candidates: Vec<WideCandidate>,
}

/// Run the wide/explore tournament. Returns the result with winner(s) to seed the deep loop.
///
/// Sequence:
/// 1. Create worktrees under `<state_dir>/wide/<run_id>/candidate-{id}`
/// 2. Fan out: one thread per candidate, each runs one PROPOSE turn in its worktree
/// 3. Serialize: apply each candidate's diff to the main workspace, measure, restore
/// 4. Rank by the gate and return the top-K
#[allow(clippy::too_many_arguments)]
pub fn run_tournament<R: Reporter>(
    cfg: &WideConfig,
    args: &Args,
    p: &Paths,
    prep: &Prepared,
    r: &mut R,
    world: &dyn World,
    judge: &dyn Judge,
    baseline_score: f64,
) -> Result<TournamentResult> {
    r.note(&format!(
        "wide round: fanning out {} candidates (top-{} advance)",
        cfg.n, cfg.k
    ));

    let wide_dir = p.state.join("wide").join(&prep.run_id);
    std::fs::create_dir_all(&wide_dir)
        .with_context(|| format!("creating wide-round dir {}", wide_dir.display()))?;

    // Create per-candidate worktrees.
    let mut candidates: Vec<WideCandidate> = (0..cfg.n)
        .map(|id| {
            let wt = wide_dir.join(format!("candidate-{id}"));
            WideCandidate {
                id,
                approach: cfg.approaches[id as usize].clone(),
                worktree: wt,
                result: None,
            }
        })
        .collect();

    // Set up each candidate worktree as a copy of the main workspace.
    for c in &candidates {
        setup_candidate_worktree(&p.workspace, &c.worktree)
            .with_context(|| format!("setting up worktree for candidate {}", c.id))?;
    }

    // Fan out: one thread per candidate, each runs one PROPOSE turn.
    r.note("wide: starting parallel PROPOSE turns");
    let propose_results = fan_out_propose(args, p, prep, &candidates);

    // Collect results back into candidates.
    for (id, diff) in propose_results {
        if let Some(c) = candidates.iter_mut().find(|c| c.id == id) {
            c.result = Some(CandidateResult {
                row: Row::default(),
                score: None,
                diff,
                diffstat: String::new(),
            });
        }
    }

    // Measure serially: apply each candidate's diff to the main workspace, measure, restore.
    r.note("wide: measuring candidates serially");
    let mut scored: Vec<ScoredCandidate> = Vec::new();
    let mut all_rows: Vec<Row> = Vec::new();
    let snap = world
        .snapshot("wide-pre-measure")
        .context("wide pre-measure snapshot")?;

    for c in candidates.iter_mut() {
        if STOP.load(Ordering::SeqCst) {
            break;
        }
        if c.result.as_ref().is_none_or(|cr| cr.diff.is_empty()) {
            r.note(&format!(
                "wide candidate {}: no diff produced, skipping",
                c.id
            ));
            let row = Row {
                iter: 0,
                decision: "wide-skip".into(),
                note: format!("candidate {} ({}) produced no diff", c.id, c.approach),
                phase: Some("wide".into()),
                ..Default::default()
            };
            all_rows.push(row);
            continue;
        }

        // Apply the candidate's diff to the main workspace.
        r.note(&format!(
            "wide candidate {}: measuring (approach: {})",
            c.id, c.approach
        ));
        match apply_candidate_diff(&p.workspace, &c.worktree) {
            Ok(()) => {}
            Err(e) => {
                r.note(&format!("wide candidate {}: apply failed: {e:#}", c.id));
                world.restore(&snap)?;
                let row = Row {
                    iter: 0,
                    decision: "wide-fail".into(),
                    note: format!("candidate {} apply failed: {e:#}", c.id),
                    phase: Some("wide".into()),
                    ..Default::default()
                };
                all_rows.push(row);
                continue;
            }
        }

        // Apply the world (deploy domains build+push+set-image).
        if let Err(e) = world.apply() {
            r.note(&format!(
                "wide candidate {}: world apply failed: {e:#}",
                c.id
            ));
            world.restore(&snap)?;
            let row = Row {
                iter: 0,
                decision: "wide-fail".into(),
                note: format!("candidate {} world apply failed: {e:#}", c.id),
                phase: Some("wide".into()),
                ..Default::default()
            };
            all_rows.push(row);
            continue;
        }

        // Measure.
        let ctx = MeasureCtx {
            baseline_score: Some(baseline_score),
            baseline_total: None,
            best_score: Some(baseline_score),
        };
        match judge.measure(&ctx) {
            Ok(reading) => {
                let (diff, diffstat) = world.staged_diff();
                let score = reading.score.unwrap_or(f64::INFINITY);
                let decision_str = if judge.decide(&reading, baseline_score).keep {
                    "wide-keep"
                } else {
                    "wide-discard"
                };
                let row = Row {
                    iter: 0,
                    decision: decision_str.into(),
                    note: format!(
                        "[wide candidate {}] {} — {}",
                        c.id, c.approach, reading.note
                    ),
                    detail: judge.detail(&reading),
                    diff: diff.clone(),
                    diffstat: diffstat.clone(),
                    score: reading.score,
                    total: reading.detail.get("total").and_then(|v| v.as_u64()),
                    phase: Some("wide".into()),
                };
                r.row(&row, false);
                scored.push(ScoredCandidate { id: c.id, score });
                if let Some(cr) = c.result.as_mut() {
                    cr.score = reading.score;
                    cr.diffstat = diffstat;
                    cr.row = row.clone();
                }
                all_rows.push(row);
            }
            Err(e) => {
                r.note(&format!("wide candidate {}: measure failed: {e:#}", c.id));
                let row = Row {
                    iter: 0,
                    decision: "wide-fail".into(),
                    note: format!("candidate {} measure failed: {e:#}", c.id),
                    phase: Some("wide".into()),
                    ..Default::default()
                };
                all_rows.push(row);
            }
        }

        // Restore main workspace for the next candidate.
        world.restore(&snap)?;
    }

    // Rank and pick winners.
    let winners = if scored.is_empty() {
        r.note("wide round: no candidates scored, deep loop starts from baseline");
        vec![]
    } else {
        let w = policy::top_k(&scored, cfg.k, &cfg.direction);
        for (rank, &id) in w.iter().enumerate() {
            if let Some(sc) = scored.iter().find(|s| s.id == id) {
                r.note(&format!(
                    "wide round: rank {} = candidate {} (score: {:.2})",
                    rank + 1,
                    id,
                    sc.score
                ));
            }
        }
        w
    };

    // Clean up worktrees (best-effort).
    for c in &candidates {
        let _ = std::fs::remove_dir_all(&c.worktree);
    }

    Ok(TournamentResult {
        winners,
        rows: all_rows,
        candidates,
    })
}

/// Create a candidate worktree as a shallow copy of the main workspace. Uses git worktree if
/// the workspace is a git repo, otherwise falls back to a filesystem copy.
fn setup_candidate_worktree(workspace: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    // Use git clone --local for efficiency (hard-links objects).
    let status = std::process::Command::new("git")
        .args([
            "clone",
            "--local",
            "--no-checkout",
            &workspace.to_string_lossy(),
            &dest.to_string_lossy(),
        ])
        .output()
        .context("git clone --local for candidate worktree")?;
    if !status.status.success() {
        anyhow::bail!(
            "git clone --local failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }
    // Check out HEAD so the candidate has a working tree.
    let checkout = std::process::Command::new("git")
        .args(["-C", &dest.to_string_lossy(), "checkout", "HEAD"])
        .output()
        .context("git checkout HEAD in candidate worktree")?;
    if !checkout.status.success() {
        anyhow::bail!(
            "git checkout in candidate worktree failed: {}",
            String::from_utf8_lossy(&checkout.stderr)
        );
    }
    Ok(())
}

/// Apply a candidate's edits to the main workspace by diffing the candidate worktree against
/// its own HEAD and applying the patch to the main workspace.
fn apply_candidate_diff(main_ws: &Path, candidate_ws: &Path) -> Result<()> {
    // Stage everything in the candidate worktree so we can get a diff.
    let _ = std::process::Command::new("git")
        .args(["-C", &candidate_ws.to_string_lossy(), "add", "-A"])
        .output();

    // Get the diff.
    let diff_out = std::process::Command::new("git")
        .args([
            "-C",
            &candidate_ws.to_string_lossy(),
            "diff",
            "--cached",
            "--binary",
        ])
        .output()
        .context("git diff in candidate worktree")?;

    let diff = String::from_utf8_lossy(&diff_out.stdout);
    if diff.trim().is_empty() {
        return Ok(());
    }

    // Apply the diff to the main workspace.
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
        .context("git apply in main workspace")?;

    if let Some(mut stdin) = apply.stdin.take() {
        use std::io::Write;
        stdin.write_all(diff_out.stdout.as_slice())?;
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

/// Fan out PROPOSE turns: one thread per candidate, each running in its own worktree.
/// Returns `(candidate_id, diff_text)` for each candidate that produced output.
fn fan_out_propose(
    args: &Args,
    p: &Paths,
    prep: &Prepared,
    candidates: &[WideCandidate],
) -> Vec<(u32, String)> {
    use std::thread;

    let skills = p.skills.clone();

    let handles: Vec<_> = candidates
        .iter()
        .map(|c| {
            let id = c.id;
            let approach = c.approach.clone();
            let worktree = c.worktree.clone();
            let args = args.clone();
            let goal = prep.goal.clone();
            let template = prep.template.clone();
            let skills = skills.clone();

            thread::spawn(move || {
                if STOP.load(Ordering::SeqCst) {
                    return (id, String::new());
                }

                let prompt = render_wide_prompt(&template, &goal, &approach);

                let cand_paths = Paths {
                    workspace: worktree.clone(),
                    skills,
                    steer: worktree.join("STEER.md"),
                    state: worktree.join("state"),
                    session_log: worktree.join("state/session.jsonl"),
                    control: worktree.join("state/control.json"),
                    escalation: worktree.join("ESCALATION.json"),
                    provisioning: worktree.join("PROVISIONING_PENDING.json"),
                };
                let _ = std::fs::create_dir_all(&cand_paths.state);

                // Register any agent child PID so Ctrl+C kills wide-round children.
                let _cost =
                    agent::run_turn(&args, &cand_paths, &prompt, false, |_line, _stream, _ev| {});

                let diff = capture_candidate_diff(&worktree);
                (id, diff)
            })
        })
        .collect();

    handles.into_iter().filter_map(|h| h.join().ok()).collect()
}

/// Capture the diff a candidate produced in its worktree (staged + unstaged).
fn capture_candidate_diff(worktree: &Path) -> String {
    let _ = std::process::Command::new("git")
        .args(["-C", &worktree.to_string_lossy(), "add", "-A"])
        .output();
    let output = std::process::Command::new("git")
        .args(["-C", &worktree.to_string_lossy(), "diff", "--cached"])
        .output();
    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => String::new(),
    }
}

/// Build the wide-round prompt. The approach biases the candidate; the template provides
/// the domain's structure. The status is omitted (no prior score in the wide round).
fn render_wide_prompt(template: &str, goal: &str, approach: &str) -> String {
    let status = "no prior score (wide round, first attempt)";
    let mut out = template
        .replace("{{GOAL}}", goal.trim())
        .replace("{{STATUS}}", status)
        .replace("{{BEST_SCORE}}", status);
    if out.contains("{{STEER}}") {
        out = out.replace("{{STEER}}", "");
    }
    // Prepend the approach bias.
    format!(
        "## Approach constraint (MANDATORY)\n\
         You MUST use this specific approach: {approach}\n\
         Implement a minimal, working version of this approach. Do not deviate.\n\n\
         {out}"
    )
}

/// Seed the main workspace with the winning candidate's diff. Called after the tournament
/// returns, before the deep loop starts.
pub fn apply_winner(workspace: &Path, candidates: &[WideCandidate], winner_id: u32) -> Result<()> {
    let winner = candidates
        .iter()
        .find(|c| c.id == winner_id)
        .context("winner candidate not found")?;
    apply_candidate_diff(workspace, &winner.worktree)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_prompt_includes_approach_constraint() {
        let prompt = render_wide_prompt(
            "goal={{GOAL}} status={{BEST_SCORE}}",
            "lower p99",
            "metrics-scrape approach",
        );
        assert!(prompt.contains("metrics-scrape approach"));
        assert!(prompt.contains("MUST use this specific approach"));
        assert!(prompt.contains("goal=lower p99"));
    }

    #[test]
    fn wide_config_resolve_none_when_no_wide() {
        let args = crate::Args {
            wide: 0,
            wide_keep: 1,
            ..default_args()
        };
        assert!(WideConfig::resolve(&args, None, "lower").is_none());
    }

    #[test]
    fn wide_config_resolve_from_cli() {
        let search = crate::manifest::SearchCfg {
            wide: 3,
            approaches: vec!["a".into(), "b".into(), "c".into()],
            policy: "top-k".into(),
            policy_k: 1,
        };
        let args = crate::Args {
            wide: 3,
            wide_keep: 2,
            ..default_args()
        };
        let cfg = WideConfig::resolve(&args, Some(&search), "lower").unwrap();
        assert_eq!(cfg.n, 3);
        assert_eq!(cfg.k, 2);
    }

    #[test]
    fn wide_config_resolve_from_manifest() {
        let search = crate::manifest::SearchCfg {
            wide: 4,
            approaches: vec!["a".into(), "b".into(), "c".into(), "d".into()],
            policy: "top-k".into(),
            policy_k: 2,
        };
        let args = crate::Args {
            wide: 0,
            wide_keep: 1,
            ..default_args()
        };
        let cfg = WideConfig::resolve(&args, Some(&search), "higher").unwrap();
        assert_eq!(cfg.n, 4);
        assert_eq!(cfg.k, 2);
        assert_eq!(cfg.direction, "higher");
    }

    #[test]
    fn wide_config_cli_wide_wins_over_manifest_wide() {
        let search = crate::manifest::SearchCfg {
            wide: 5,
            approaches: vec!["a".into(), "b".into(), "c".into()],
            policy: "top-k".into(),
            policy_k: 1,
        };
        let args = crate::Args {
            wide: 3,
            wide_keep: 1,
            ..default_args()
        };
        let cfg = WideConfig::resolve(&args, Some(&search), "lower").unwrap();
        assert_eq!(cfg.n, 3);
    }

    #[test]
    fn wide_config_none_when_too_few_approaches() {
        let search = crate::manifest::SearchCfg {
            wide: 3,
            approaches: vec!["a".into()],
            policy: "top-k".into(),
            policy_k: 1,
        };
        let args = crate::Args {
            wide: 3,
            wide_keep: 1,
            ..default_args()
        };
        assert!(WideConfig::resolve(&args, Some(&search), "lower").is_none());
    }

    #[test]
    fn wide_prompt_replaces_steer_placeholder() {
        let prompt = render_wide_prompt(
            "{{GOAL}} {{STATUS}} steer:{{STEER}}",
            "improve throughput",
            "batch-all",
        );
        assert!(prompt.contains("improve throughput"));
        assert!(prompt.contains("steer:"));
        assert!(!prompt.contains("{{STEER}}"));
    }

    #[test]
    fn wide_prompt_without_steer_placeholder() {
        let prompt = render_wide_prompt("goal={{GOAL}}", "my goal", "approach-x");
        assert!(prompt.contains("goal=my goal"));
        assert!(!prompt.contains("{{STEER}}"));
    }

    fn default_args() -> crate::Args {
        use clap::Parser;
        crate::Cli::parse_from(["crucible"]).run
    }
}
