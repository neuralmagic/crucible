//! `crucible-controller rank-compare` (measurement harness): for each already-ranked issue in a repo, run the
//! code-grounded ranker and compare its tier against the stored API-ranker tier. This is a *measurement
//! tool*, not part of the loop — it never writes a tier or touches the issues table; it only appends
//! `rank-compare` ledger rows (the grounded turns it spends on), emits a markdown report + a JSONL
//! sidecar, and is resumable (an issue already in the JSONL is skipped on a re-run).
//!
//! The experiment: the cheap text-only ranker already tiered these issues from the issue text alone;
//! grounding lets the model read the code first. This harness quantifies how often the two agree,
//! and where they diverge — the evidence for whether the escalation tier earns its cost.

use crate::client::Db;
use crate::config::ControllerCfg;
use crate::issues::engine;
use crate::issues::model::IssueQuery;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The tier vocabulary in a fixed order, for the disagreement matrix + stable iteration. The API
/// ranker never emits `stale` (only the grounded side can), so this stays the closed T0..N set.
const TIERS: [&str; 5] = ["T0", "T1", "T2", "T3", "N"];

/// The grounded side's vocabulary: every [`TIERS`] value plus `stale` — the disagreement matrix's
/// column set, since a grounded verdict can land there even though the API tier never does.
const GROUNDED_TIERS: [&str; 6] = ["T0", "T1", "T2", "T3", "N", "stale"];

/// CLI options for the comparison harness.
pub struct CompareOpts {
    /// The repo whose already-ranked issues to compare (owner/repo).
    pub repo: String,
    /// Cap on how many *new* issues to compare this run (0 = all not-yet-compared). The tool is
    /// resumable, so this batches a large repo across several runs.
    pub limit: usize,
    /// Restrict to issues the API ranker actually ranked (a non-null ranked-content hash).
    pub only_ranked: bool,
    /// The markdown report path; the raw rows land in its `.jsonl` sibling.
    pub out: PathBuf,
    /// A `command`-backend script standing in for the grounded turn's agent (the test seam),
    /// threaded down to each `rank-grounded` invocation.
    pub agent_cmd: Option<String>,
}

/// One compared issue — the JSONL row and the report's per-issue row. Serialized to the JSONL
/// sidecar; a re-run reads it back to skip work already done.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompareRow {
    key: String,
    title: String,
    api_tier: String,
    grounded_tier: String,
    agree: bool,
    api_rationale: String,
    grounded_rationale: String,
    cost_usd: f64,
}

/// What `run_compare` reports back to the CLI.
pub struct CompareSummary {
    /// Total rows in the final report (existing JSONL + newly compared this run).
    pub total: usize,
    /// Issues compared for the first time this run.
    pub newly_compared: usize,
    /// How many of `total` agree (api_tier == grounded_tier).
    pub agreements: usize,
    pub report_path: PathBuf,
}

/// The JSONL sidecar beside the report (`report.md` -> `report.jsonl`).
fn jsonl_path(out: &Path) -> PathBuf {
    out.with_extension("jsonl")
}

/// Read the JSONL sidecar back (resume): every row already compared, in file order. A missing file
/// is an empty history.
fn read_existing_rows(path: &Path) -> Result<Vec<CompareRow>> {
    let body = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str::<CompareRow>(l).with_context(|| format!("parsing row {l:?}"))
        })
        .collect()
}

/// Append one row to the JSONL sidecar (created on first write).
fn append_row(path: &Path, row: &CompareRow) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating out dir {}", parent.display()))?;
    }
    let mut line = serde_json::to_string(row).context("serialize compare row")?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    f.write_all(line.as_bytes())
        .with_context(|| format!("appending to {}", path.display()))?;
    Ok(())
}

/// The API ranker's recorded rationale for an issue: the reason on its last `new -> new` event
/// (what `reconcile::apply_verdict` logs when it applies a tier). Empty when none was recorded.
async fn api_rationale(db: &Db, key: &str) -> Result<String> {
    Ok(db
        .events()
        .read_for_key(key)
        .await?
        .into_iter()
        .rev()
        .find(|e| e.from == "new" && e.to == "new" && e.reason.is_some())
        .and_then(|e| e.reason)
        .unwrap_or_default())
}

/// Run the harness: compare each not-yet-compared ranked issue, append its row, ledger the grounded
/// turn's cost as `rank-compare`, then render the report from every row (existing + new).
pub async fn run_compare(
    db: &Db,
    cfg: &ControllerCfg,
    opts: &CompareOpts,
) -> Result<CompareSummary> {
    let started = std::time::Instant::now();
    let jsonl = jsonl_path(&opts.out);
    let mut rows = read_existing_rows(&jsonl)?;
    let done: BTreeSet<String> = rows.iter().map(|r| r.key.clone()).collect();

    // Target set: the repo's ranked issues (that's the experiment), key-sorted for determinism.
    let query = IssueQuery {
        repo: Some(opts.repo.clone()),
        ..IssueQuery::default()
    };
    let mut targets = crate::issues::store::list_issues_filtered(db.pool(), &query).await?;
    targets.retain(|i| i.tier.is_some() && (!opts.only_ranked || i.ranked_content_hash.is_some()));
    targets.sort_by(|a, b| a.key.cmp(&b.key));

    // Maintain the shared per-repo checkout once for the whole batch (each grounded turn runs in its
    // own throwaway worktree of it).
    let bin = crate::runs::engine::resolve_bin();
    let workspace = engine::checkout_dir(cfg.scratch_root(), &opts.repo);
    let repo_url = crate::runs::engine::repo_clone_url(&opts.repo);

    let mut newly = 0usize;
    for iss in &targets {
        if done.contains(&iss.key) {
            continue;
        }
        if opts.limit > 0 && newly >= opts.limit {
            break;
        }
        let api_tier = iss.tier.clone().unwrap_or_default();
        let title = iss.title.clone().unwrap_or_default();
        let api_rat = api_rationale(db, &iss.key).await?;

        let verdict = {
            let bin = bin.clone();
            let ws = workspace.clone();
            let url = repo_url.clone();
            let key = iss.key.clone();
            let max_cost = cfg.profile.per_reconcile_cost;
            let agent_cmd = opts.agent_cmd.clone();
            tokio::task::spawn_blocking(move || -> Result<engine::GroundedVerdict> {
                engine::ensure_checkout(&url, &ws)?;
                engine::rank_grounded(&bin, &key, &ws, max_cost, agent_cmd.as_deref())
            })
            .await?
            .with_context(|| format!("grounded ranking {}", iss.key))?
        };

        let grounded_tier = verdict.disposition.as_str().to_string();
        let row = CompareRow {
            key: iss.key.clone(),
            title,
            agree: api_tier == grounded_tier,
            api_tier,
            grounded_tier,
            api_rationale: api_rat,
            grounded_rationale: verdict.rationale.clone(),
            cost_usd: verdict.cost_usd,
        };
        append_row(&jsonl, &row)?;
        // Measurement spend only — a ledger row, never a tier write.
        db.ledger_append(None, "rank-compare", verdict.cost_usd)
            .await?;
        rows.push(row);
        newly += 1;
    }

    rows.sort_by(|a, b| a.key.cmp(&b.key));
    let agreements = rows.iter().filter(|r| r.agree).count();
    let report = render_report(&opts.repo, &rows, started.elapsed().as_secs_f64());
    if let Some(parent) = opts.out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating out dir {}", parent.display()))?;
    }
    std::fs::write(&opts.out, report)
        .with_context(|| format!("writing report {}", opts.out.display()))?;

    Ok(CompareSummary {
        total: rows.len(),
        newly_compared: newly,
        agreements,
        report_path: opts.out.clone(),
    })
}

/// Trim a rationale/title into a single table cell: pipes and newlines would break the markdown row,
/// and a long rationale would blow the column width, so replace the former and cap the latter.
fn cell(s: &str, max: usize) -> String {
    let flat: String = s
        .chars()
        .map(|c| {
            if c == '|' || c == '\n' || c == '\r' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let flat = flat.trim();
    if flat.chars().count() > max {
        let truncated: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{truncated}\u{2026}")
    } else {
        flat.to_string()
    }
}

/// Render the markdown report from every compared row. Pure (wall time is passed in, not read from a
/// clock) so it is golden-testable: summary (agreement rate, mean grounded cost, wall time), the
/// disagreement matrix, then the per-issue table.
fn render_report(repo: &str, rows: &[CompareRow], wall_secs: f64) -> String {
    let total = rows.len();
    let agreements = rows.iter().filter(|r| r.agree).count();
    let agree_pct = if total == 0 {
        0.0
    } else {
        100.0 * agreements as f64 / total as f64
    };
    let total_cost: f64 = rows.iter().map(|r| r.cost_usd).sum();
    let mean_cost = if total == 0 {
        0.0
    } else {
        total_cost / total as f64
    };

    let mut s = String::new();
    s.push_str(&format!("# Grounded vs API triage comparison — {repo}\n\n"));
    s.push_str("## Summary\n\n");
    s.push_str(&format!("- Issues compared: {total}\n"));
    s.push_str(&format!(
        "- Agreement: {agreements}/{total} ({agree_pct:.1}%)\n"
    ));
    s.push_str(&format!("- Mean grounded cost: ${mean_cost:.4}\n"));
    s.push_str(&format!("- Wall time: {wall_secs:.1}s\n\n"));

    s.push_str("## Disagreements (api → grounded)\n\n");
    let mut any_disagree = false;
    let mut matrix = String::from("| api | grounded | count |\n| --- | --- | --- |\n");
    for api in TIERS {
        for grounded in GROUNDED_TIERS {
            if api == grounded {
                continue;
            }
            let n = rows
                .iter()
                .filter(|r| r.api_tier == api && r.grounded_tier == grounded)
                .count();
            if n > 0 {
                any_disagree = true;
                matrix.push_str(&format!("| {api} | {grounded} | {n} |\n"));
            }
        }
    }
    if any_disagree {
        s.push_str(&matrix);
    } else {
        s.push_str("None — full agreement.\n");
    }
    s.push('\n');

    s.push_str("## Per-issue\n\n");
    s.push_str(
        "| key | title | api_tier | grounded_tier | agree | api_rationale | grounded_rationale |\n",
    );
    s.push_str("| --- | --- | --- | --- | --- | --- | --- |\n");
    for r in rows {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} |\n",
            r.key,
            cell(&r.title, 60),
            r.api_tier,
            r.grounded_tier,
            if r.agree { "yes" } else { "**no**" },
            cell(&r.api_rationale, 80),
            cell(&r.grounded_rationale, 80),
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, api: &str, grounded: &str, cost: f64) -> CompareRow {
        CompareRow {
            key: key.to_string(),
            title: format!("title of {key}"),
            api_tier: api.to_string(),
            grounded_tier: grounded.to_string(),
            agree: api == grounded,
            api_rationale: format!("api said {api}"),
            grounded_rationale: format!("grounded said {grounded}"),
            cost_usd: cost,
        }
    }

    #[test]
    fn render_report_is_a_stable_golden() {
        let rows = vec![
            row("owner/repo#1", "T0", "T0", 0.10),
            row("owner/repo#2", "T2", "T0", 0.20),
            row("owner/repo#3", "T1", "T1", 0.30),
        ];
        let got = render_report("owner/repo", &rows, 4.2);
        let want = "\
# Grounded vs API triage comparison — owner/repo

## Summary

- Issues compared: 3
- Agreement: 2/3 (66.7%)
- Mean grounded cost: $0.2000
- Wall time: 4.2s

## Disagreements (api → grounded)

| api | grounded | count |
| --- | --- | --- |
| T2 | T0 | 1 |

## Per-issue

| key | title | api_tier | grounded_tier | agree | api_rationale | grounded_rationale |
| --- | --- | --- | --- | --- | --- | --- |
| owner/repo#1 | title of owner/repo#1 | T0 | T0 | yes | api said T0 | grounded said T0 |
| owner/repo#2 | title of owner/repo#2 | T2 | T0 | **no** | api said T2 | grounded said T0 |
| owner/repo#3 | title of owner/repo#3 | T1 | T1 | yes | api said T1 | grounded said T1 |
";
        assert_eq!(got, want, "report golden drifted:\n{got}");
    }

    #[test]
    fn render_report_reports_full_agreement() {
        let rows = vec![row("owner/repo#1", "T0", "T0", 0.10)];
        let got = render_report("owner/repo", &rows, 1.0);
        assert!(got.contains("None — full agreement."));
        assert!(got.contains("Agreement: 1/1 (100.0%)"));
    }

    #[test]
    fn cell_flattens_pipes_and_newlines_and_truncates() {
        assert_eq!(cell("a|b\nc", 40), "a b c");
        let long = "x".repeat(100);
        let trimmed = cell(&long, 10);
        assert_eq!(trimmed.chars().count(), 10, "9 chars + ellipsis");
        assert!(trimmed.ends_with('\u{2026}'));
    }

    #[test]
    fn existing_rows_round_trip_through_the_jsonl() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("report.jsonl");
        let a = row("owner/repo#1", "T0", "T0", 0.1);
        let b = row("owner/repo#2", "T2", "T1", 0.2);
        append_row(&path, &a)?;
        append_row(&path, &b)?;
        let back = read_existing_rows(&path)?;
        assert_eq!(back, vec![a, b]);
        // A missing sidecar reads back empty, not an error.
        assert!(read_existing_rows(&dir.path().join("nope.jsonl"))?.is_empty());
        Ok(())
    }

    use crate::issues::model::NewIssue;
    use sqlx::PgPool;

    /// A fake `crucible` bin whose `rank-grounded` prints a grounded verdict of `tier`.
    fn fake_grounded_bin(dir: &Path, tier: &str) -> PathBuf {
        let json = format!(
            r#"{{"tier":"{tier}","rationale":"grounded: read the code","confidence":"high","cost_usd":0.15,"over_budget":false}}"#
        );
        let path = dir.join("crucible-grounded");
        crate::testing::write_exec(
            &path,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = rank-grounded ]; then\n printf '%s\\n' '{json}'\n exit 0\nfi\nexit 1\n"
            ),
        );
        path
    }

    fn seed_checkout(state_dir: &Path, repo: &str) {
        let dir = engine::checkout_dir(state_dir, repo);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-C", &dir.to_string_lossy(), "init", "-q"])
                .status()
                .unwrap()
                .success()
        );
    }

    fn cfg_with(state_dir: &Path) -> ControllerCfg {
        crate::testing::cfg_with(state_dir)
    }

    async fn seed_ranked(db: &Db, key: &str, tier: &str) -> Result<()> {
        crate::issues::store::upsert_issue(
            db.pool(),
            &NewIssue {
                key: key.to_string(),
                repo: "owner/repo".to_string(),
                priority: 0,
                evidence_url: None,
                title: Some(format!("title of {key}")),
                author: None,
                body: None,
                labels: Vec::new(),
                upstream_updated_at: None,
            },
        )
        .await?;
        crate::issues::store::set_ranked_tier(db.pool(), key, tier, "perf", "somehash").await?;
        Ok(())
    }

    /// End-to-end: run the harness over two ranked issues (shelling a scripted `rank-grounded`),
    /// then prove (a) it wrote the report + JSONL and ledgered `rank-compare` rows, (b) it never
    /// touched the stored tiers, and (c) a second run is a no-op — every issue is already in the
    /// JSONL, so nothing new is compared and no rows are re-appended.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn run_compare_writes_report_ledgers_only_and_resumes(pool: PgPool) -> Result<()> {
        let _g = crate::ENV_LOCK.lock().await;
        let dir = tempfile::tempdir()?;
        let db = Db::new(pool);
        let bin = fake_grounded_bin(dir.path(), "T0");
        seed_checkout(dir.path(), "owner/repo");
        unsafe {
            std::env::set_var("CRUCIBLE_BIN", &bin);
        }
        unsafe {
            std::env::remove_var("CONTROLLER_SANDBOX_IMAGE");
        }

        seed_ranked(&db, "owner/repo#1", "T1").await?;
        seed_ranked(&db, "owner/repo#2", "T2").await?;
        // An unranked issue must be excluded from the (already-ranked) target set.
        crate::issues::store::upsert_issue(
            db.pool(),
            &NewIssue {
                key: "owner/repo#3".to_string(),
                repo: "owner/repo".to_string(),
                priority: 0,
                evidence_url: None,
                title: Some("unranked".to_string()),
                author: None,
                body: None,
                labels: Vec::new(),
                upstream_updated_at: None,
            },
        )
        .await?;

        let cfg = cfg_with(dir.path());
        let out = dir.path().join("report.md");
        let opts = CompareOpts {
            repo: "owner/repo".to_string(),
            limit: 0,
            only_ranked: false,
            out: out.clone(),
            agent_cmd: None,
        };

        let summary = run_compare(&db, &cfg, &opts).await?;
        assert_eq!(summary.newly_compared, 2, "only the two ranked issues");
        assert_eq!(summary.total, 2);
        assert!(out.exists(), "report written");
        let jsonl = std::fs::read_to_string(jsonl_path(&out))?;
        assert_eq!(jsonl.lines().count(), 2, "one JSONL row per compared issue");

        let ledgered =
            sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM ledger WHERE kind='rank-compare'"#)
                .fetch_one(db.pool())
                .await?;
        assert_eq!(
            ledgered.n, 2,
            "one rank-compare ledger row per grounded turn"
        );
        // A measurement tool must not write tiers: the stored API tiers are untouched despite the
        // grounded verdict being T0 for both.
        assert_eq!(
            crate::issues::store::get_issue(db.pool(), "owner/repo#1")
                .await?
                .unwrap()
                .tier
                .as_deref(),
            Some("T1")
        );
        assert_eq!(
            crate::issues::store::get_issue(db.pool(), "owner/repo#2")
                .await?
                .unwrap()
                .tier
                .as_deref(),
            Some("T2")
        );

        // Resume: a second pass compares nothing new and appends no rows.
        let again = run_compare(&db, &cfg, &opts).await?;
        unsafe {
            std::env::remove_var("CRUCIBLE_BIN");
        }
        assert_eq!(
            again.newly_compared, 0,
            "already-compared issues are skipped"
        );
        assert_eq!(again.total, 2);
        let jsonl2 = std::fs::read_to_string(jsonl_path(&out))?;
        assert_eq!(jsonl2.lines().count(), 2, "no new rows on resume");
        let ledgered2 =
            sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM ledger WHERE kind='rank-compare'"#)
                .fetch_one(db.pool())
                .await?;
        assert_eq!(ledgered2.n, 2, "resume spends nothing new");
        Ok(())
    }

    /// `--limit` batches: with a cap of 1, only the first not-yet-compared issue is done this run,
    /// and a follow-up run picks up the rest.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn run_compare_honors_the_limit(pool: PgPool) -> Result<()> {
        let _g = crate::ENV_LOCK.lock().await;
        let dir = tempfile::tempdir()?;
        let db = Db::new(pool);
        let bin = fake_grounded_bin(dir.path(), "T0");
        seed_checkout(dir.path(), "owner/repo");
        unsafe {
            std::env::set_var("CRUCIBLE_BIN", &bin);
        }
        unsafe {
            std::env::remove_var("CONTROLLER_SANDBOX_IMAGE");
        }
        seed_ranked(&db, "owner/repo#1", "T1").await?;
        seed_ranked(&db, "owner/repo#2", "T2").await?;

        let cfg = cfg_with(dir.path());
        let out = dir.path().join("report.md");
        let opts = CompareOpts {
            repo: "owner/repo".to_string(),
            limit: 1,
            only_ranked: false,
            out: out.clone(),
            agent_cmd: None,
        };

        let first = run_compare(&db, &cfg, &opts).await?;
        assert_eq!(first.newly_compared, 1, "the cap stops after one");
        assert_eq!(first.total, 1);
        let second = run_compare(&db, &cfg, &opts).await?;
        unsafe {
            std::env::remove_var("CRUCIBLE_BIN");
        }
        assert_eq!(second.newly_compared, 1, "the follow-up run does the rest");
        assert_eq!(second.total, 2);
        Ok(())
    }
}
