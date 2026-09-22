//! Raw SQL over the cost ledger.

use crate::model::{LedgerDay, LedgerTagTotal};
use anyhow::{Context, Result};
use sqlx::PgExecutor;

/// The last `limit` days with any ledgered cost, newest first — the daily-summary API/UI query.
#[tracing::instrument(name = "db.ledger_summary", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn ledger_summary(ex: impl PgExecutor<'_>, limit: i64) -> Result<Vec<LedgerDay>> {
    let rows = sqlx::query!(
        r#"
        SELECT substr(ts, 1, 10) AS "day!: String", SUM(cost_usd) AS "total_usd!: f64"
        FROM ledger GROUP BY substr(ts, 1, 10) ORDER BY substr(ts, 1, 10) DESC LIMIT $1
        "#,
        limit,
    )
    .fetch_all(ex)
    .await
    .context("ledger_summary")?;
    Ok(rows
        .into_iter()
        .map(|r| LedgerDay {
            day: r.day,
            total_usd: r.total_usd,
        })
        .collect())
}

/// One UTC day's ledgered cost grouped by `kind` (the cost tag: rank-grounded / scope / run / …),
/// biggest spender first — the spend-by-kind dashboard breakdown of [`ledger_day_total`].
#[tracing::instrument(name = "db.ledger_day_by_tag", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", day = %day), err)]
pub(crate) async fn ledger_day_by_tag(
    ex: impl PgExecutor<'_>,
    day: &str,
) -> Result<Vec<LedgerTagTotal>> {
    let rows = sqlx::query!(
        r#"
        SELECT kind AS "tag!: String", SUM(cost_usd) AS "total_usd!: f64"
        FROM ledger WHERE substr(ts, 1, 10) = $1
        GROUP BY kind ORDER BY SUM(cost_usd) DESC, kind ASC
        "#,
        day,
    )
    .fetch_all(ex)
    .await
    .context("ledger_day_by_tag")?;
    Ok(rows
        .into_iter()
        .map(|r| LedgerTagTotal {
            tag: r.tag,
            total_usd: r.total_usd,
        })
        .collect())
}

/// Append one admission-ledger entry (the cost caps are SUM() queries over this table).
#[tracing::instrument(name = "db.ledger_append", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", kind = %kind), err)]
pub(crate) async fn ledger_append(
    ex: impl PgExecutor<'_>,
    ts: &str,
    run_id: Option<&str>,
    kind: &str,
    cost_usd: f64,
) -> Result<()> {
    sqlx::query!(
        "INSERT INTO ledger (ts, run_id, kind, cost_usd) VALUES ($1, $2, $3, $4)",
        ts,
        run_id,
        kind,
        cost_usd,
    )
    .execute(ex)
    .await
    .context("ledger_append")?;
    Ok(())
}

/// Total ledgered cost for a UTC day (`day` = `YYYY-MM-DD`), the daily-ceiling cap query. Matches
/// on the RFC3339 timestamp's leading date, so it needs no separate date column.
#[tracing::instrument(name = "db.ledger_day_total", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", day = %day), err)]
pub async fn ledger_day_total(ex: impl PgExecutor<'_>, day: &str) -> Result<f64> {
    let row = sqlx::query!(
        r#"SELECT COALESCE(SUM(cost_usd), 0.0) AS "total!: f64" FROM ledger WHERE substr(ts, 1, 10) = $1"#,
        day,
    )
    .fetch_one(ex)
    .await
    .context("ledger_day_total")?;
    Ok(row.total)
}
