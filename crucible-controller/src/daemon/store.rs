//! Whole-table reads the rebuild and export paths use.

use crate::issues::model::Issue;
use crate::issues::store::{ISSUE_COLS, issue_from_row};
use crate::runs::model::{Candidate, Run};
use crate::runs::store::{CANDIDATE_COLS, RUN_COLS};
use anyhow::{Context, Result};
use sqlx::PgExecutor;

/// Every `issues` row, decoded into strong types — [`crate::daemon::rebuild`]'s row-by-row diff source.
#[tracing::instrument(name = "db.list_all_issues", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn list_all_issues(ex: impl PgExecutor<'_>) -> Result<Vec<Issue>> {
    let sql = const_format::formatcp!("SELECT {ISSUE_COLS} FROM issues ORDER BY key");
    let rows = sqlx::query(sql)
        .fetch_all(ex)
        .await
        .context("list_all_issues")?;
    rows.iter().map(issue_from_row).collect()
}

/// Every `runs` row, in full (unlike [`insert_run`]'s write-only [`NewRun`]).
#[tracing::instrument(name = "db.list_all_runs", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn list_all_runs(ex: impl PgExecutor<'_>) -> Result<Vec<Run>> {
    let sql = const_format::formatcp!("SELECT {RUN_COLS} FROM runs ORDER BY run_id");
    sqlx::query_as::<_, Run>(sql)
        .fetch_all(ex)
        .await
        .context("list_all_runs")
}

/// Every `candidates` row, in full. No stable ORDER-worthy PK on this table, so ordering is by
/// `run_id` then `iter`/`lane` (whichever this row carries) for a deterministic diff.
#[tracing::instrument(name = "db.list_all_candidates", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn list_all_candidates(ex: impl PgExecutor<'_>) -> Result<Vec<Candidate>> {
    let sql = const_format::formatcp!(
        "SELECT {CANDIDATE_COLS} FROM candidates ORDER BY run_id, lane, iter"
    );
    sqlx::query_as::<_, Candidate>(sql)
        .fetch_all(ex)
        .await
        .context("list_all_candidates")
}

/// Every UTC day (`YYYY-MM-DD`) with at least one ledger entry — the drift check's daily-sum
/// comparison walks this list rather than diffing the (high-volume, append-only) ledger row by row.
#[tracing::instrument(name = "db.list_ledger_days", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn list_ledger_days(ex: impl PgExecutor<'_>) -> Result<Vec<String>> {
    let rows = sqlx::query!(
        r#"SELECT DISTINCT substr(ts, 1, 10) AS "day!: String" FROM ledger ORDER BY 1"#
    )
    .fetch_all(ex)
    .await
    .context("list_ledger_days")?;
    Ok(rows.into_iter().map(|r| r.day).collect())
}
