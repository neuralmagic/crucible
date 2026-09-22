#![allow(clippy::disallowed_macros)]

use crate::builds::model::{BuildRow, BuildState, NewBuild};
use anyhow::{Context, Result};
use sqlx::FromRow;
use sqlx::{PgExecutor, Row};

// ---------------------------------------------------------------------------------------------
// Declarative image builds: the `builds` ledger that gates the `building` state.
// ---------------------------------------------------------------------------------------------

/// The `builds` column list, in decode order. Every read builds its SELECT from this via
/// [`build_cols`] so a new column is a one-line edit here.
const BUILD_COLS: &[&str] = &[
    "id",
    "scope",
    "name",
    "image",
    "tag",
    "context_digest",
    "backend",
    "state",
    "dispatch_id",
    "digest_ref",
    "evidence_url",
    "dispatch_attempts",
    "timeout_secs",
    "created_at",
    "dispatched_at",
    "finished_at",
];

/// [`BUILD_COLS`] joined with `prefix` — `"b."` for a join query (disambiguates from the joined
/// `scopes`/`issues`), `""` for a bare `FROM builds`. Result column names stay unprefixed, so
/// the `FromRow` derive reads them by bare name either way.
fn build_cols(prefix: &str) -> String {
    BUILD_COLS
        .iter()
        .map(|c| format!("{prefix}{c}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Insert a `builds` row (a freshly declared build entering `pending`); returns its autoincrement
/// id (reconcile keys the dispatch/poll transitions on it).
#[tracing::instrument(name = "db.insert_build", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", name = %b.name), err)]
pub(crate) async fn insert_build(ex: impl PgExecutor<'_>, b: &NewBuild) -> Result<i64> {
    let now = crate::clock::now_rfc3339();
    let backend = b.backend.as_str();
    let res = sqlx::query!(
        r#"
        INSERT INTO builds (scope, name, image, tag, context_digest, backend, state, timeout_secs, created_at)
        VALUES ($1, $2, $3, $4, $5, $6, 'pending', $7, $8)
        RETURNING id AS "id!"
        "#,
        b.scope,
        b.name,
        b.image,
        b.tag,
        b.context_digest,
        backend,
        b.timeout_secs,
        now,
    )
    .fetch_one(ex)
    .await
    .context("insert_build")?;
    Ok(res.id)
}

/// Every `builds` row for a scope, oldest first — the set reconcile gates the scope's run on (the
/// run launches only when every row is `succeeded` with a `digest_ref`).
#[tracing::instrument(name = "db.builds_for_scope", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", scope = scope), err)]
pub(crate) async fn builds_for_scope(ex: impl PgExecutor<'_>, scope: i64) -> Result<Vec<BuildRow>> {
    let sql = format!(
        "SELECT {} FROM builds WHERE scope = $1 ORDER BY id ASC",
        build_cols("")
    );
    sqlx::query_as::<_, BuildRow>(&sql)
        .bind(scope)
        .fetch_all(ex)
        .await
        .context("builds_for_scope")
}

/// Every `builds` row for one issue (via `builds.scope → scopes.issue`), oldest first — the set the
/// issue-journey folds a `build` step from. Ordered by id so re-declared builds keep insertion order.
#[tracing::instrument(name = "db.builds_for_issue", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", issue = %issue), err)]
pub(crate) async fn builds_for_issue(
    ex: impl PgExecutor<'_>,
    issue: &str,
) -> Result<Vec<BuildRow>> {
    let sql = format!(
        "SELECT {} FROM builds b JOIN scopes s ON b.scope = s.id \
         WHERE s.issue = $1 ORDER BY b.id ASC",
        build_cols("b.")
    );
    sqlx::query_as::<_, BuildRow>(&sql)
        .bind(issue)
        .fetch_all(ex)
        .await
        .context("builds_for_issue")
}

/// The builds ledger, newest first: `builds` left-joined to the issue key + repo it blocks, under the
/// optional `state`/`backend`/`issue` filter set, paged. Backs `GET /api/builds` + the builds UI page.
/// The `state`/`backend` binds are the strong-typed values' canonical strings, so the dynamic filter
/// stays injection-free without interpolation.
#[tracing::instrument(name = "db.list_builds_page", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn list_builds_page(
    ex: impl PgExecutor<'_>,
    q: &crate::builds::model::BuildQuery,
) -> Result<Vec<crate::builds::model::BuildListRow>> {
    let state = q.state.map(|s| s.as_str());
    let backend = q.backend.map(|b| b.as_str());
    let sql = format!(
        "SELECT {}, s.issue AS issue_key, i.repo AS repo \
         FROM builds b \
         LEFT JOIN scopes s ON b.scope = s.id \
         LEFT JOIN issues i ON s.issue = i.key \
         WHERE ($1 IS NULL OR b.state = $2) \
           AND ($3 IS NULL OR b.backend = $4) \
           AND ($5 IS NULL OR s.issue = $6) \
         ORDER BY b.id DESC LIMIT $7 OFFSET $8",
        build_cols("b.")
    );
    let rows = sqlx::query(&sql)
        .bind(state)
        .bind(state)
        .bind(backend)
        .bind(backend)
        .bind(&q.issue)
        .bind(&q.issue)
        .bind(q.limit)
        .bind(q.offset)
        .fetch_all(ex)
        .await
        .context("list_builds_page")?;
    rows.iter()
        .map(|r| {
            Ok(crate::builds::model::BuildListRow {
                row: BuildRow::from_row(r)?,
                issue_key: r.try_get("issue_key")?,
                repo: r.try_get("repo")?,
            })
        })
        .collect()
}

/// Move a `pending` build to `dispatched`, recording the backend's dispatch identity + the dispatch
/// time (the timeout clock counts from it). A CAS on `pending` so a double-drive can't re-dispatch.
#[tracing::instrument(name = "db.set_build_dispatched", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", id = id), err)]
pub(crate) async fn set_build_dispatched(
    ex: impl PgExecutor<'_>,
    id: i64,
    dispatch_id: &str,
) -> Result<bool> {
    let now = crate::clock::now_rfc3339();
    let res = sqlx::query!(
        "UPDATE builds SET state = 'dispatched', dispatch_id = $1, dispatched_at = $2 \
         WHERE id = $3 AND state = 'pending'",
        dispatch_id,
        now,
        id,
    )
    .execute(ex)
    .await
    .context("set_build_dispatched")?;
    Ok(res.rows_affected() == 1)
}

/// Charge a failed dispatch attempt against a still-`pending` build, returning the new count. The
/// reconcile driver parks the issue once this crosses its attempt cap, so a build the backend keeps
/// refusing (a transient error that never clears) can't spin the issue at `building` forever.
#[tracing::instrument(name = "db.bump_build_dispatch_attempt", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", id = id), err)]
pub(crate) async fn bump_build_dispatch_attempt(ex: impl PgExecutor<'_>, id: i64) -> Result<i64> {
    let row = sqlx::query!(
        r#"UPDATE builds SET dispatch_attempts = dispatch_attempts + 1
           WHERE id = $1 RETURNING dispatch_attempts AS "dispatch_attempts!: i64""#,
        id,
    )
    .fetch_one(ex)
    .await
    .context("bump_build_dispatch_attempt")?;
    Ok(row.dispatch_attempts)
}

/// Pin the resolved `image@sha256:…` onto a build and mark it `succeeded` — the transition that
/// unblocks the scope's run (consumers read `digest_ref`).
#[tracing::instrument(name = "db.set_build_succeeded", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", id = id), err)]
pub(crate) async fn set_build_succeeded(
    ex: impl PgExecutor<'_>,
    id: i64,
    digest_ref: &str,
) -> Result<()> {
    let now = crate::clock::now_rfc3339();
    sqlx::query!(
        "UPDATE builds SET state = 'succeeded', digest_ref = $1, finished_at = $2 WHERE id = $3",
        digest_ref,
        now,
        id,
    )
    .execute(ex)
    .await
    .context("set_build_succeeded")?;
    Ok(())
}

/// Mark a build terminal-but-failed with the build-log pointer as evidence (`failed` on a reported
/// failure, `timed-out` when the timeout cap fired). One function, the terminal state passed in;
/// rejects a non-failure state so a caller can't stamp `succeeded` without a digest through here.
#[tracing::instrument(name = "db.set_build_failed", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", id = id), err)]
pub(crate) async fn set_build_failed(
    ex: impl PgExecutor<'_>,
    id: i64,
    state: BuildState,
    evidence: Option<&str>,
) -> Result<()> {
    let now = crate::clock::now_rfc3339();
    let state = match state {
        BuildState::Failed | BuildState::TimedOut => state.as_str(),
        other => anyhow::bail!(
            "set_build_failed refuses non-failure state `{}`",
            other.as_str()
        ),
    };
    sqlx::query!(
        "UPDATE builds SET state = $1, evidence_url = COALESCE($2, evidence_url), finished_at = $3 \
         WHERE id = $4",
        state,
        evidence,
        now,
        id,
    )
    .execute(ex)
    .await
    .context("set_build_failed")?;
    Ok(())
}

/// Fetch one `builds` row by id, or `None` if unknown — the admin rebuild endpoint's existence
/// check.
#[tracing::instrument(name = "db.get_build", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", id = id), err)]
pub(crate) async fn get_build(ex: impl PgExecutor<'_>, id: i64) -> Result<Option<BuildRow>> {
    let sql = format!("SELECT {} FROM builds WHERE id = $1", build_cols(""));
    sqlx::query_as::<_, BuildRow>(&sql)
        .bind(id)
        .fetch_optional(ex)
        .await
        .context("get_build")
}

/// Reset a terminal (`succeeded`/`failed`/`timed-out`) build row back to `pending` for an admin
/// force-rebuild: clears the dispatch identity, digest, evidence, and timestamps, and zeroes the
/// dispatch-attempt counter, so the next [`crate::builds::lifecycle::drive_scope_builds`] pass treats it exactly
/// like a never-dispatched build (no row, or `pending` with no `dispatch_id`, is its `needs_dispatch`
/// test) — the SAME re-drive path a crash-interrupted dispatch takes, not a parallel one. A CAS on
/// the terminal states: a build that raced into `pending`/`dispatched` between the caller's read and
/// this write is left alone, and the caller reads 0 rows affected as "already in flight."
#[tracing::instrument(name = "db.reset_build_for_rebuild", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", id = id), err)]
pub(crate) async fn reset_build_for_rebuild(ex: impl PgExecutor<'_>, id: i64) -> Result<bool> {
    let res = sqlx::query!(
        "UPDATE builds SET state = 'pending', dispatch_id = NULL, digest_ref = NULL, \
         evidence_url = NULL, dispatch_attempts = 0, dispatched_at = NULL, finished_at = NULL \
         WHERE id = $1 AND state IN ('succeeded', 'failed', 'timed-out')",
        id,
    )
    .execute(ex)
    .await
    .context("reset_build_for_rebuild")?;
    Ok(res.rows_affected() == 1)
}

/// The in-flight build count (`pending` + `dispatched`) — the per-kind concurrency cap query, the
/// build analogue of [`count_active_work_pods`].
#[tracing::instrument(name = "db.count_builds_in_flight", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn count_builds_in_flight(ex: impl PgExecutor<'_>) -> Result<i64> {
    let row = sqlx::query!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM builds WHERE state IN ('pending', 'dispatched')"#
    )
    .fetch_one(ex)
    .await
    .context("count_builds_in_flight")?;
    Ok(row.n)
}

/// Every `builds` row currently in one of `states`, oldest first — the startup adoption sweep and
/// the metrics scrape (counts by backend/state). The `IN (…)` list is built from the internal
/// [`BuildState`] spellings, never caller input, so interpolating it is injection-safe.
#[tracing::instrument(name = "db.builds_in_states", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn builds_in_states(
    ex: impl PgExecutor<'_>,
    states: &[BuildState],
) -> Result<Vec<BuildRow>> {
    if states.is_empty() {
        return Ok(Vec::new());
    }
    let list = states
        .iter()
        .map(|s| format!("'{}'", s.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT {} FROM builds WHERE state IN ({list}) ORDER BY id ASC",
        build_cols("")
    );
    sqlx::query_as::<_, BuildRow>(&sql)
        .fetch_all(ex)
        .await
        .context("builds_in_states")
}
