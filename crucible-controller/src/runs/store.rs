//! Raw SQL over the run and candidate tables.

#[cfg(feature = "autoresearch")]
use crate::issues::model::KeptPr;
use crate::model::SortDir;
use crate::runs::model::{Candidate, NewCandidate, NewRun, Run, RunKindFilter, RunQuery, RunRow};
use anyhow::{Context, Result};
use sqlx::{PgExecutor, Row};
use std::collections::HashMap;

/// Every issue a contract rejection parked, as `(key, parked_reason)`.
#[tracing::instrument(name = "db.contract_parked", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub async fn contract_parked(ex: impl PgExecutor<'_>) -> Result<Vec<(String, String)>> {
    let rows = sqlx::query(
        "SELECT key, parked_reason FROM issues \
         WHERE status = 'parked' AND parked_reason LIKE 'contract rejection: %' ORDER BY key",
    )
    .fetch_all(ex)
    .await
    .context("contract_parked")?;
    rows.into_iter()
        .map(|r| Ok((r.try_get("key")?, r.try_get("parked_reason")?)))
        .collect()
}

/// The `runs` column list, in decode order. Every `runs` read that yields a [`Run`] builds its
/// SELECT from this so a new column is a one-line edit here.
pub(crate) const RUN_COLS: &str = "run_id, name, scope, issue, identity_digest, status, pod, session_uri, best_score, cost_usd, created_at, dispatch, cluster, namespace, image_ref, image_digest, capability_digest, image_override";

/// Record the image provenance the preflight resolved for a run. Written once, at dispatch.
#[tracing::instrument(name = "db.set_run_image", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", run_id = %run_id), err)]
pub(crate) async fn set_run_image(
    ex: impl PgExecutor<'_>,
    run_id: &str,
    image: &crate::runs::model::RunImage,
) -> Result<()> {
    sqlx::query(
        "UPDATE runs SET image_ref = $2, image_digest = $3, capability_digest = $4, image_override = $5 WHERE run_id = $1",
    )
    .bind(run_id)
    .bind(&image.reference)
    .bind(&image.digest)
    .bind(&image.capability_digest)
    .bind(image.overridden)
    .execute(ex)
    .await
    .context("set_run_image")?;
    Ok(())
}

/// Every run launched from one scope, oldest first.
#[tracing::instrument(name = "db.list_runs_for_scope", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", scope = scope), err)]
pub async fn list_runs_for_scope(ex: impl PgExecutor<'_>, scope: i64) -> Result<Vec<Run>> {
    let sql = const_format::formatcp!(
        "SELECT {RUN_COLS} FROM runs WHERE scope = $1 ORDER BY created_at, seq"
    );
    sqlx::query_as::<_, Run>(sql)
        .bind(scope)
        .fetch_all(ex)
        .await
        .context("list_runs_for_scope")
}

/// Fetch one run by id, or `None` if untracked.
#[tracing::instrument(name = "db.get_run", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", run_id = %run_id), err)]
pub(crate) async fn get_run(ex: impl PgExecutor<'_>, run_id: &str) -> Result<Option<Run>> {
    let sql = const_format::formatcp!("SELECT {RUN_COLS} FROM runs WHERE run_id = $1");
    sqlx::query_as::<_, Run>(sql)
        .bind(run_id)
        .fetch_optional(ex)
        .await
        .context("get_run")
}

/// Every candidate recorded for one run (one wide-round lane or deep-loop iteration each), in
/// insertion order.
#[tracing::instrument(name = "db.list_candidates_for_run", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", run_id = %run_id), err)]
pub async fn list_candidates_for_run(
    ex: impl PgExecutor<'_>,
    run_id: &str,
) -> Result<Vec<Candidate>> {
    let sql = const_format::formatcp!(
        "SELECT {CANDIDATE_COLS} FROM candidates WHERE run_id = $1 ORDER BY seq"
    );
    sqlx::query_as::<_, Candidate>(sql)
        .bind(run_id)
        .fetch_all(ex)
        .await
        .context("list_candidates_for_run")
}

/// The `candidates` column list, in decode order. Every `candidates` read that yields a
/// [`Candidate`] builds its SELECT from this so a new column is a one-line edit here.
pub(crate) const CANDIDATE_COLS: &str =
    "run_id, kind, lane, iter, score, decision, worktree, sandbox, pr_url, branch";

/// The runs leaderboard: `runs` joined out to their issue key + repo, under the
/// `status=`/`repo=`/`dispatch_target=` filter + sorting and paging. A plain `sqlx::query`
/// (not the macro) because the ORDER BY column is only known at runtime — the column comes from a
/// [`RunSort`](crate::runs::model::RunSort) match, never string-interpolated, so this stays injection-safe;
/// every filter/paging value is a bound `?`. `seq` breaks ties in the requested direction.
#[tracing::instrument(name = "db.list_runs_page", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn list_runs_page(ex: impl PgExecutor<'_>, q: &RunQuery) -> Result<Vec<RunRow>> {
    let order_col = q.sort.order_col();
    // NULLS placement pinned to the SQLite-era behavior the UI expects (NULLs sort as
    // smallest: first ASC, last DESC); Postgres's default is the opposite on DESC.
    let (order_dir, seq_dir) = match q.dir {
        SortDir::Asc => ("ASC NULLS FIRST", "ASC"),
        SortDir::Desc => ("DESC NULLS LAST", "DESC"),
    };
    let sql = format!(
        r#"
        SELECT r.run_id AS run_id, r.name AS name, r.issue AS issue_key, i.repo AS repo,
               r.status AS status, r.best_score AS best_score, r.cost_usd AS cost_usd,
               r.created_at AS created_at,
               (SELECT c.pr_url FROM candidates c
                 WHERE c.run_id = r.run_id AND c.pr_url IS NOT NULL AND c.decision = 'keep'
                 ORDER BY c.seq DESC LIMIT 1) AS pr_url,
               (SELECT COUNT(*) FROM run_task_results t
                 WHERE t.run_id = r.run_id AND t.status = 'transport') AS transport_losses
        FROM runs r
        LEFT JOIN issues i ON i.key = r.issue
        LEFT JOIN playbook_launches pl ON pl.key = r.issue
        WHERE ($1 IS NULL OR r.status = $2)
          AND ($3 IS NULL OR i.repo = $4)
          AND ($8 IS NULL OR r.cluster = $9)
          -- `autoresearch` is every run that is NOT a playbook launch, not merely the scoped
          -- ones: a goal run or an adopted run carries no scope row and still belongs on the
          -- autoresearch surfaces. Anchoring it to `scope IS NOT NULL` hid them.
          AND ($7::text IS NULL
               OR ($7 = 'autoresearch' AND pl.key IS NULL)
               OR ($7 = 'playbook' AND pl.key IS NOT NULL))
        ORDER BY {order_col} {order_dir}, r.seq {seq_dir}
        LIMIT $5 OFFSET $6
        "#
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(&q.status)
        .bind(&q.status)
        .bind(&q.repo)
        .bind(&q.repo)
        .bind(q.limit)
        .bind(q.offset)
        .bind(q.kind.map(RunKindFilter::as_str))
        .bind(&q.dispatch_target)
        .bind(&q.dispatch_target)
        .fetch_all(ex)
        .await
        .context("list_runs_page")?;
    rows.into_iter()
        .map(|row| {
            Ok(RunRow {
                run_id: row.try_get("run_id")?,
                name: row.try_get("name")?,
                issue_key: row.try_get("issue_key")?,
                repo: row.try_get("repo")?,
                status: row.try_get("status")?,
                best_score: row.try_get("best_score")?,
                cost_usd: row.try_get("cost_usd")?,
                pr_url: row.try_get("pr_url")?,
                transport_losses: row.try_get("transport_losses")?,
                created_at: row.try_get("created_at")?,
            })
        })
        .collect()
}

/// Every run's measured scores in iteration order, keyed by run id. One query for the whole page
/// rather than one per row.
#[tracing::instrument(name = "db.score_series_for_runs", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn score_series_for_runs(
    ex: impl PgExecutor<'_>,
    run_ids: &[String],
) -> Result<HashMap<String, Vec<f64>>> {
    if run_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(
        "SELECT run_id, score FROM candidates \
         WHERE run_id = ANY($1) AND score IS NOT NULL \
         ORDER BY run_id, COALESCE(iter, lane), seq",
    )
    .bind(run_ids)
    .fetch_all(ex)
    .await
    .context("score_series_for_runs")?;

    let mut out: HashMap<String, Vec<f64>> = HashMap::new();
    for row in &rows {
        let run_id: String = row
            .try_get("run_id")
            .context("score_series_for_runs.run_id")?;
        let score: f64 = row
            .try_get("score")
            .context("score_series_for_runs.score")?;
        out.entry(run_id).or_default().push(score);
    }
    Ok(out)
}

/// Every candidate for one run, ordered by iteration/lane index (the order the SSG's per-run
/// decision table + score curve render in) — `COALESCE(iter, lane)` picks the deep iter or the wide
/// lane, whichever this row carries, then `rowid` breaks ties.
#[tracing::instrument(name = "db.list_candidates_for_run_by_iter", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", run_id = %run_id), err)]
pub(crate) async fn list_candidates_for_run_by_iter(
    ex: impl PgExecutor<'_>,
    run_id: &str,
) -> Result<Vec<Candidate>> {
    let sql = const_format::formatcp!(
        "SELECT {CANDIDATE_COLS} FROM candidates WHERE run_id = $1 ORDER BY COALESCE(iter, lane), seq"
    );
    sqlx::query_as::<_, Candidate>(sql)
        .bind(run_id)
        .fetch_all(ex)
        .await
        .context("list_candidates_for_run_by_iter")
}

/// Every run projected for the `runs.parquet` analytics export: the `runs` row joined to its issue
/// key + repo, plus the deep-iteration count and kept count folded from `candidates`. See
/// [`crate::runs::export`] for which artifact-only columns are intentionally omitted.
#[tracing::instrument(name = "db.export_run_rows", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn export_run_rows(
    ex: impl PgExecutor<'_>,
) -> Result<Vec<crate::runs::export::RunParquetRow>> {
    let rows = sqlx::query!(
        r#"
        SELECT r.run_id AS "run_id!", s.issue AS "issue_key?", i.repo AS "repo?",
               r.status AS "status!", r.best_score, r.cost_usd,
               (SELECT COUNT(*) FROM candidates c WHERE c.run_id = r.run_id AND c.kind = 'deep') AS "iterations!: i64",
               (SELECT COUNT(*) FROM candidates c WHERE c.run_id = r.run_id AND c.decision = 'keep') AS "kept!: i64"
        FROM runs r
        LEFT JOIN scopes s ON r.scope = s.id
        LEFT JOIN issues i ON s.issue = i.key
        ORDER BY r.run_id
        "#,
    )
    .fetch_all(ex)
    .await
    .context("export_run_rows")?;
    Ok(rows
        .into_iter()
        .map(|r| crate::runs::export::RunParquetRow {
            run_id: r.run_id,
            issue_key: r.issue_key,
            repo: r.repo,
            status: r.status,
            improved: r.kept > 0,
            best: r.best_score,
            cost_usd: r.cost_usd,
            iterations: r.iterations,
            kept: r.kept,
        })
        .collect())
}

/// Every candidate projected for the `iterations.parquet` analytics export, ordered by run then
/// iteration/lane index (see [`crate::runs::export`]).
#[tracing::instrument(name = "db.export_iteration_rows", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn export_iteration_rows(
    ex: impl PgExecutor<'_>,
) -> Result<Vec<crate::runs::export::IterParquetRow>> {
    let rows = sqlx::query!(
        r#"
        SELECT run_id AS "run_id!", kind, lane, iter, score, decision
        FROM candidates ORDER BY run_id, COALESCE(iter, lane), seq
        "#,
    )
    .fetch_all(ex)
    .await
    .context("export_iteration_rows")?;
    Ok(rows
        .into_iter()
        .map(|r| crate::runs::export::IterParquetRow {
            run_id: r.run_id,
            kind: r.kind,
            lane: r.lane,
            iter: r.iter,
            score: r.score,
            decision: r.decision,
        })
        .collect())
}

/// The issue key + repo a run belongs to, via `runs.issue → issues.repo`. Both `None` when the
/// run has no issue or is unknown; the run-detail API renders its issue backlink + repo header
/// from this.
#[tracing::instrument(name = "db.run_issue_repo", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", run_id = %run_id), err)]
pub(crate) async fn run_issue_repo(
    ex: impl PgExecutor<'_>,
    run_id: &str,
) -> Result<(Option<String>, Option<String>)> {
    let row = sqlx::query!(
        r#"
        SELECT r.issue AS "issue_key?", i.repo AS "repo?"
        FROM runs r
        LEFT JOIN issues i ON i.key = r.issue
        WHERE r.run_id = $1
        "#,
        run_id,
    )
    .fetch_optional(ex)
    .await
    .context("run_issue_repo")?;
    Ok(row.map(|r| (r.issue_key, r.repo)).unwrap_or((None, None)))
}

/// Stamp the owner principal of the launch a run was made from (RFC-0003 C-HIERARCHY): the
/// launch's recorded owner, else a legacy login-shaped `created_by`, else the platform.
pub async fn attribute_run(ex: impl PgExecutor<'_>, run_id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE runs SET attributed_to = COALESCE(
            (SELECT l.owner FROM playbook_launches l
              WHERE l.key = runs.issue AND l.owner IS NOT NULL),
            (SELECT 'user:' || lower(trim(l.created_by)) FROM playbook_launches l
              WHERE l.key = runs.issue AND l.created_by IS NOT NULL
                AND trim(l.created_by) ~ '^[A-Za-z0-9._@-]+$'),
            $2)
         WHERE run_id = $1",
    )
    .bind(run_id)
    .bind(crate::authz::model::Principal::platform().to_string())
    .execute(ex)
    .await
    .context("attributing a run's spend")?;
    Ok(())
}

/// Stamp the principal a run is dispatched for: whoever asked for this dispatch of the issue
/// ([`crate::issues::store::request_dispatch`]), else the owner the run is attributed to. The
/// request is consumed in the same statement, so it applies to exactly the dispatch it asked for
/// and a later schedule or auto-dispatch acts for the owner again.
pub async fn stamp_dispatcher(ex: impl PgExecutor<'_>, run_id: &str) -> Result<()> {
    sqlx::query(
        "WITH requested AS (
             SELECT i.key, i.dispatch_requested_by
               FROM issues i JOIN runs r ON r.issue = i.key
              WHERE r.run_id = $1
                FOR UPDATE OF i
         ), consumed AS (
             UPDATE issues SET dispatch_requested_by = NULL
              WHERE key IN (SELECT key FROM requested)
         )
         UPDATE runs SET dispatched_by = COALESCE(
             (SELECT dispatch_requested_by FROM requested),
             attributed_to)
          WHERE run_id = $1",
    )
    .bind(run_id)
    .execute(ex)
    .await
    .context("stamping a run's dispatcher")?;
    Ok(())
}

#[tracing::instrument(name = "db.insert_run", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", run_id = %run.run_id), err)]
pub async fn insert_run(ex: impl PgExecutor<'_>, run: &NewRun) -> Result<()> {
    // Upsert on `run_id`: the launch path records the run at `running`, and the pod-completion edge
    // folds the finished outcome onto the same row — status/score/cost/evidence advance,
    // the run stays one row. A fresh run_id is a plain insert.
    sqlx::query!(
        r#"
        INSERT INTO runs (run_id, scope, issue, identity_digest, status, pod, session_uri, best_score, cost_usd)
        VALUES ($1, $2, COALESCE($9, (SELECT s.issue FROM scopes s WHERE s.id = $2)), $3, $4, $5, $6, $7, $8)
        ON CONFLICT(run_id) DO UPDATE SET
            scope           = excluded.scope,
            issue           = COALESCE(excluded.issue, runs.issue),
            identity_digest = excluded.identity_digest,
            status          = excluded.status,
            pod             = excluded.pod,
            session_uri     = excluded.session_uri,
            best_score      = excluded.best_score,
            cost_usd        = excluded.cost_usd
        "#,
        run.run_id,
        run.scope,
        run.identity_digest,
        run.status,
        run.pod,
        run.session_uri,
        run.best_score,
        run.cost_usd,
        run.issue,
    )
    .execute(ex)
    .await
    .context("insert_run")?;
    Ok(())
}

/// Stamp which cluster and namespace a run's engine was dispatched onto. Written once by the
/// dispatch that started it, for the same reason as [`set_run_dispatch`]: the completion edge
/// re-runs [`insert_run`]'s upsert, which must not reset provenance.
#[tracing::instrument(name = "db.set_run_location", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", %run_id), err)]
pub(crate) async fn set_run_location(
    ex: impl PgExecutor<'_>,
    run_id: &str,
    location: &crate::runs::model::RunLocation,
) -> Result<()> {
    sqlx::query("UPDATE runs SET cluster = $2, namespace = $3 WHERE run_id = $1")
        .bind(run_id)
        .bind(&location.cluster)
        .bind(location.namespace.as_deref())
        .execute(ex)
        .await
        .context("set_run_location")?;
    Ok(())
}

/// Stamp where a run's engine ran. Written once by the dispatch that started it; deliberately not
/// part of [`insert_run`]'s upsert, whose completion-edge call would otherwise reset it.
#[tracing::instrument(name = "db.set_run_dispatch", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", %run_id), err)]
pub(crate) async fn set_run_dispatch(
    ex: impl PgExecutor<'_>,
    run_id: &str,
    dispatch: crate::runs::model::RunDispatch,
) -> Result<()> {
    sqlx::query("UPDATE runs SET dispatch = $2 WHERE run_id = $1")
        .bind(run_id)
        .bind(dispatch.as_str())
        .execute(ex)
        .await
        .context("set_run_dispatch")?;
    Ok(())
}

/// Drop a run and its folded candidates so an external re-upload can re-ingest in place.
/// External-run replace only: a dispatched run's row is the pod-watch's to own.
pub async fn delete_run_cascade(pool: &sqlx::PgPool, run_id: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM candidates WHERE run_id = $1")
        .bind(run_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM runs WHERE run_id = $1")
        .bind(run_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Drop the row of a run whose dispatch never started it, handing back the dispatch request
/// [`stamp_dispatcher`] consumed so the next dispatch of the issue acts for the same principal.
pub(crate) async fn delete_unstarted_run(tx: &mut sqlx::PgConnection, run_id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE issues i SET dispatch_requested_by = r.dispatched_by FROM runs r
          WHERE r.run_id = $1 AND i.key = r.issue AND i.dispatch_requested_by IS NULL
            AND r.dispatched_by IS DISTINCT FROM r.attributed_to",
    )
    .bind(run_id)
    .execute(&mut *tx)
    .await
    .context("restoring a released run's dispatch request")?;
    sqlx::query("DELETE FROM run_agent_tools WHERE run_id = $1")
        .bind(run_id)
        .execute(&mut *tx)
        .await
        .context("deleting a released run's agent tools")?;
    sqlx::query("DELETE FROM runs WHERE run_id = $1")
        .bind(run_id)
        .execute(&mut *tx)
        .await
        .context("deleting a released run")?;
    Ok(())
}

/// Drop a run's folded candidate rows so a re-ingest replaces instead of appending (`candidates`
/// has no primary key and [`insert_candidate`] is a plain INSERT — without this an adopt re-run
/// silently duplicates rows).
pub(crate) async fn delete_candidates_for_run(
    ex: impl sqlx::PgExecutor<'_>,
    run_id: &str,
) -> Result<()> {
    sqlx::query("DELETE FROM candidates WHERE run_id = $1")
        .bind(run_id)
        .execute(ex)
        .await
        .context("delete_candidates_for_run")?;
    Ok(())
}

/// Whether this run's cost is already booked as a `kind='run'` ledger row. The cost caps SUM the
/// ledger, so a re-adopt that appended again would double-charge the day.
pub(crate) async fn run_cost_ledgered(ex: impl sqlx::PgExecutor<'_>, run_id: &str) -> Result<bool> {
    let booked: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM ledger WHERE kind = 'run' AND run_id = $1)",
    )
    .bind(run_id)
    .fetch_one(ex)
    .await
    .context("run_cost_ledgered")?;
    Ok(booked)
}

/// The playbook launches whose run is still `running` under local dispatch, as `(issue key, run
/// id)`.
#[tracing::instrument(name = "db.running_local_runs", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn running_local_runs(ex: impl PgExecutor<'_>) -> Result<Vec<(String, String)>> {
    let rows = sqlx::query!(
        r#"
        SELECT r.issue AS "key!", r.run_id AS "run_id!"
        FROM runs r
        WHERE r.dispatch = 'local' AND r.status = 'running' AND r.issue IS NOT NULL
        ORDER BY r.seq
        "#,
    )
    .fetch_all(ex)
    .await
    .context("running_local_runs")?;
    Ok(rows.into_iter().map(|r| (r.key, r.run_id)).collect())
}

/// The issues of runs still `running` on a pod, each once, oldest run first.
#[tracing::instrument(name = "db.running_pod_run_issues", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn running_pod_run_issues(ex: impl PgExecutor<'_>) -> Result<Vec<String>> {
    sqlx::query_scalar::<_, String>(
        r#"
        SELECT issue FROM runs
        WHERE status = 'running' AND pod IS NOT NULL AND issue IS NOT NULL
        GROUP BY issue
        ORDER BY min(seq)
        "#,
    )
    .fetch_all(ex)
    .await
    .context("running_pod_run_issues")
}

/// Flip a run row's status without touching the ingest-owned columns — the completion edge's
/// terminal stamp for a run whose pod finished but published no session log (nothing to fold, but
/// the row must stop reading `running`).
#[tracing::instrument(name = "db.set_run_status", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", run_id = %run_id, status = %status), err)]
pub(crate) async fn set_run_status(
    ex: impl PgExecutor<'_>,
    run_id: &str,
    status: &str,
) -> Result<()> {
    sqlx::query!(
        "UPDATE runs SET status = $1 WHERE run_id = $2",
        status,
        run_id,
    )
    .execute(ex)
    .await
    .context("set_run_status")?;
    Ok(())
}

/// Record one candidate row (per wide-round lane / deep-loop iteration).
#[tracing::instrument(name = "db.insert_candidate", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", run_id = %cand.run_id), err)]
pub async fn insert_candidate(ex: impl PgExecutor<'_>, cand: &NewCandidate) -> Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO candidates (run_id, kind, lane, iter, score, decision, worktree, sandbox, pr_url, branch)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        "#,
        cand.run_id,
        cand.kind,
        cand.lane,
        cand.iter,
        cand.score,
        cand.decision,
        cand.worktree,
        cand.sandbox,
        cand.pr_url,
        cand.branch,
    )
    .execute(ex)
    .await
    .context("insert_candidate")?;
    Ok(())
}

/// Every kept-candidate draft PR tied back to the issue that produced it — the working
/// set the review-comment poll checks. One row per (issue, distinct pr_url) with a `keep` decision.
#[cfg(feature = "autoresearch")]
#[tracing::instrument(name = "db.kept_candidate_prs", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn kept_candidate_prs(ex: impl PgExecutor<'_>) -> Result<Vec<KeptPr>> {
    let rows = sqlx::query!(
        r#"
        SELECT DISTINCT s.issue AS "issue!", c.pr_url AS "pr_url!"
        FROM candidates c
        JOIN runs r ON r.run_id = c.run_id
        JOIN scopes s ON s.id = r.scope
        WHERE c.pr_url IS NOT NULL AND c.decision = 'keep'
        ORDER BY s.issue, c.pr_url
        "#,
    )
    .fetch_all(ex)
    .await
    .context("kept_candidate_prs")?;
    Ok(rows
        .into_iter()
        .map(|r| KeptPr {
            issue: r.issue,
            pr_url: r.pr_url,
        })
        .collect())
}

/// The live (`running`) run behind an issue — run id, scope, evidence pointer, pod — for the
/// pod-completion edge to ingest against. Newest by insertion order (`rowid`); `None`
/// when the issue has no run recorded as `running`.
#[tracing::instrument(name = "db.running_run_for_issue", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", issue = %issue), err)]
pub(crate) async fn running_run_for_issue(
    ex: impl PgExecutor<'_>,
    issue: &str,
) -> Result<Option<crate::runs::model::RunningRun>> {
    let row = sqlx::query!(
        r#"
        SELECT r.run_id AS "run_id!", r.scope, r.session_uri, r.pod
        FROM runs r
        WHERE r.issue = $1 AND r.status = 'running'
        ORDER BY r.seq DESC LIMIT 1
        "#,
        issue,
    )
    .fetch_optional(ex)
    .await
    .context("running_run_for_issue")?;
    Ok(row.map(|r| crate::runs::model::RunningRun {
        run_id: r.run_id,
        scope: r.scope,
        session_uri: r.session_uri,
        pod: r.pod,
    }))
}

/// Count issues currently at `running` — the `max_concurrent_pods` admission cap,
/// read straight off the ledger of live rows rather than a live `ps` (the DB is the truth the
/// controller launched against).
#[tracing::instrument(name = "db.count_running", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn count_running(ex: impl PgExecutor<'_>) -> Result<i64> {
    let row = sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM issues WHERE status = 'running'"#)
        .fetch_one(ex)
        .await
        .context("count_running")?;
    Ok(row.n)
}
#[cfg(test)]
mod tests {
    use crate::runs::store::*;

    #[cfg(feature = "autoresearch")]
    use crate::issues::model::NewScope;
    #[cfg(feature = "autoresearch")]
    use crate::issues::store::tests::seed_rows;
    #[cfg(feature = "autoresearch")]
    use crate::issues::store::*;
    #[cfg(feature = "autoresearch")]
    use crate::launches::model::NewPlaybookLaunch;

    #[cfg(feature = "autoresearch")]
    use crate::launches::store::*;

    #[cfg(feature = "autoresearch")]
    use anyhow::Result;
    use sqlx::PgPool;

    async fn set_run_created_at(
        ex: impl PgExecutor<'_>,
        run_id: &str,
        created_at: &str,
    ) -> Result<()> {
        sqlx::query("UPDATE runs SET created_at = $2 WHERE run_id = $1")
            .bind(run_id)
            .bind(created_at)
            .execute(ex)
            .await
            .context("set_run_created_at")?;
        Ok(())
    }

    /// Provenance is written once and survives the completion edge. `insert_run` is an upsert the
    /// pod-completion path re-runs, so a location folded into it would be reset to nothing by the
    /// very event that ends the run.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_runs_location_is_written_once_and_survives_completion(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let running = NewRun {
            run_id: "run-1".into(),
            scope: None,
            issue: None,
            identity_digest: None,
            status: "running".into(),
            pod: Some("crucible-run-1".into()),
            session_uri: None,
            best_score: None,
            cost_usd: None,
        };
        insert_run(&pool, &running).await?;
        assert_eq!(
            get_run(&pool, "run-1").await?.expect("row").location,
            crate::runs::model::RunLocation::hub(),
            "an unstamped run reads as the controller's own cluster"
        );

        let location = crate::runs::model::RunLocation::new("wharf", Some("crucible".to_string()));
        set_run_location(&pool, "run-1", &location).await?;
        assert_eq!(
            get_run(&pool, "run-1").await?.expect("row").location,
            location
        );

        // The completion edge folds the outcome onto the same row through the same upsert.
        insert_run(
            &pool,
            &NewRun {
                status: "done".into(),
                best_score: Some(231.0),
                ..running
            },
        )
        .await?;
        let run = get_run(&pool, "run-1").await?.expect("row");
        assert_eq!(run.status, "done");
        assert_eq!(
            run.location, location,
            "the completion upsert must not reset where the run ran"
        );
        Ok(())
    }

    /// The runs leaderboard's `kind=autoresearch` means "not a playbook launch", not "has a scope
    /// row". A goal run and an adopted run carry no scope and still belong on the board; anchoring
    /// the filter to `scope IS NOT NULL` silently hid every one of them.
    #[cfg(feature = "autoresearch")]
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn autoresearch_runs_keep_the_scopeless_ones_and_drop_only_playbooks(
        pool: PgPool,
    ) -> Result<()> {
        seed_rows(&pool).await?;

        // A scoped run: an issue, its scope, and a run pointing at it.
        let scope = insert_scope(
            &pool,
            &NewScope {
                issue: "owner/repo#1".into(),
                pack_digest: Some("v1:beef".into()),
                check_outcome: Some("PASS".into()),
            },
        )
        .await?;
        for (run_id, scope) in [
            ("20260817T000000Z-scoped-run", Some(scope)),
            // No scope row: a goal run and an adopted run, exactly the shapes that vanished.
            ("20260817T000001Z-goal-generalize-the-fused-gemm", None),
            ("adopted-1786754192", None),
        ] {
            insert_run(
                &pool,
                &NewRun {
                    run_id: run_id.to_string(),
                    scope,
                    issue: None,
                    identity_digest: None,
                    status: "finished".to_string(),
                    pod: None,
                    session_uri: None,
                    best_score: None,
                    cost_usd: None,
                },
            )
            .await?;
        }

        // A playbook launch and the run its dispatch produced, keyed off the launch key.
        sqlx::query(
            r#"INSERT INTO playbooks (id, description, repo, git_ref, rev, path, tar_gz,
                                      tar_digest, tar_bytes, params_schema, schema_digest,
                                      core_rev, created_by, created_at, updated_at)
               VALUES ('survey', 'reads a paper', 'owner/packs', 'main', 'abc123', '', $1,
                       'sha256:tar', 3, '{"type":"object"}'::jsonb, 'sha256:schema', 'core1',
                       'wren', '2026-08-23T00:00:00Z', '2026-08-23T00:00:00Z')"#,
        )
        .bind(vec![1u8, 2, 3])
        .execute(&pool)
        .await?;
        let launch_key = "playbook:survey:0199c0de-7c2c-71a5-8000-9";
        let max_time = crate::model::MaxTime::parse("30m").expect("duration");
        let params = serde_json::json!({});
        assert!(matches!(
            adopt_playbook_launch(
                &pool,
                launch_key,
                &NewPlaybookLaunch {
                    choice_snapshots: None,
                    playbook: "survey",
                    repo: "owner/packs",
                    title: "reads a paper",
                    params: &params,
                    schema_digest: "sha256:schema",
                    max_cost: 1.0,
                    max_time: &max_time,
                    advance_dedupe: false,
                    dedupe_schedule: None,
                    origin: crate::model::LaunchOrigin::Manual,
                    draft_version: None,
                    created_by: Some("wren"),
                    owner: None,
                    launcher_groups: None,
                    rerun: None,
                    dispatch: Default::default(),
                },
                crate::playbooks::registry::PackRevision::Bytes("sha256:tar")
            )
            .await?,
            AdoptPlaybookOutcome::Adopted
        ));
        let playbook_run = format!("{}-1787528065", launch_key.replace(['/', '#', ':'], "_"));
        insert_run(
            &pool,
            &NewRun {
                run_id: playbook_run.clone(),
                scope: None,
                issue: Some(launch_key.to_string()),
                identity_digest: None,
                status: "finished".to_string(),
                pod: None,
                session_uri: None,
                best_score: None,
                cost_usd: None,
            },
        )
        .await?;

        let ids = |rows: Vec<RunRow>| {
            rows.into_iter()
                .map(|r| r.run_id)
                .collect::<std::collections::BTreeSet<_>>()
        };
        let query = |kind| RunQuery {
            status: None,
            repo: None,
            dispatch_target: None,
            kind,
            sort: crate::runs::model::RunSort::Created,
            dir: crate::model::SortDir::Desc,
            limit: 50,
            offset: 0,
        };

        let all = ids(list_runs_page(&pool, &query(None)).await?);
        assert_eq!(all.len(), 4, "no filter returns every run: {all:?}");

        let auto = ids(list_runs_page(
            &pool,
            &query(Some(crate::runs::model::RunKindFilter::Autoresearch)),
        )
        .await?);
        assert!(
            auto.contains("20260817T000000Z-scoped-run")
                && auto.contains("20260817T000001Z-goal-generalize-the-fused-gemm")
                && auto.contains("adopted-1786754192"),
            "a scopeless goal or adopted run still belongs on the board: {auto:?}"
        );
        assert!(
            !auto.contains(&playbook_run),
            "the playbook run is the one exclusion"
        );

        let play = ids(list_runs_page(
            &pool,
            &query(Some(crate::runs::model::RunKindFilter::Playbook)),
        )
        .await?);
        assert_eq!(play, std::iter::once(playbook_run).collect());
        Ok(())
    }

    /// A run's issue link lives on the row: derived from its scope for an autoresearch run,
    /// stamped at launch for a playbook run (which has no scope), and kept when a later fold
    /// upserts the row without knowing it. Every by-issue lookup reads that column.
    #[cfg(feature = "autoresearch")]
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn run_lookups_go_through_the_row_issue_link(pool: PgPool) -> Result<()> {
        seed_rows(&pool).await?;
        let scope = insert_scope(
            &pool,
            &NewScope {
                issue: "owner/repo#1".into(),
                pack_digest: Some("v1:beef".into()),
                check_outcome: Some("PASS".into()),
            },
        )
        .await?;
        let run = |run_id: &str, scope: Option<i64>, issue: Option<&str>, pod: &str| NewRun {
            run_id: run_id.to_string(),
            scope,
            issue: issue.map(str::to_string),
            identity_digest: None,
            status: "running".to_string(),
            pod: Some(pod.to_string()),
            session_uri: None,
            best_score: None,
            cost_usd: None,
        };
        insert_run(
            &pool,
            &run("20260817T000000Z-scoped", Some(scope), None, "loop-1"),
        )
        .await?;
        assert_eq!(
            running_run_for_issue(&pool, "owner/repo#1")
                .await?
                .map(|r| r.run_id)
                .as_deref(),
            Some("20260817T000000Z-scoped"),
            "a scoped run derives its issue from the scope"
        );

        sqlx::query(
            r#"INSERT INTO playbooks (id, description, repo, git_ref, rev, path, tar_gz,
                                      tar_digest, tar_bytes, params_schema, schema_digest,
                                      core_rev, created_by, created_at, updated_at)
               VALUES ('survey', 'reads a paper', 'owner/packs', 'main', 'abc123', '', $1,
                       'sha256:tar', 3, '{"type":"object"}'::jsonb, 'sha256:schema', 'core1',
                       'wren', '2026-08-23T00:00:00Z', '2026-08-23T00:00:00Z')"#,
        )
        .bind(vec![1u8, 2, 3])
        .execute(&pool)
        .await?;
        let key = "playbook:survey:0199c0de-7c2c-71a5-8000-9";
        let max_time = crate::model::MaxTime::parse("30m").expect("duration");
        let params = serde_json::json!({});
        adopt_playbook_launch(
            &pool,
            key,
            &NewPlaybookLaunch {
                choice_snapshots: None,
                playbook: "survey",
                repo: "owner/packs",
                title: "reads a paper",
                params: &params,
                schema_digest: "sha256:schema",
                max_cost: 1.0,
                max_time: &max_time,
                advance_dedupe: false,
                dedupe_schedule: None,
                owner: None,
                launcher_groups: None,
                rerun: None,
                dispatch: Default::default(),
                origin: crate::model::LaunchOrigin::Manual,
                draft_version: None,
                created_by: Some("wren"),
            },
            crate::playbooks::registry::PackRevision::Bytes("sha256:tar"),
        )
        .await?;
        let playbook_run = format!("{}-1787528065", crate::model::sanitize_key(key));
        insert_run(
            &pool,
            &run(&playbook_run, None, Some(key), "crucible-run-pb"),
        )
        .await?;

        let live = running_run_for_issue(&pool, key)
            .await?
            .expect("a scopeless playbook run is found by its issue");
        assert_eq!(live.run_id, playbook_run);
        assert!(live.scope.is_none());
        assert_eq!(
            latest_run_pod_for_issue(&pool, key).await?.as_deref(),
            Some("crucible-run-pb")
        );
        assert_eq!(
            run_issue_repo(&pool, &playbook_run).await?,
            (Some(key.to_string()), Some("owner/packs".to_string()))
        );
        let for_launch = list_runs_for_launch(&pool, key).await?;
        assert_eq!(for_launch.len(), 1);
        assert_eq!(for_launch[0].run_id, playbook_run);
        assert_eq!(for_launch[0].issue.as_deref(), Some(key));

        // The completion fold upserts the row without the issue; the launch's link survives.
        let mut folded = run(&playbook_run, None, None, "crucible-run-pb");
        folded.status = "finished".to_string();
        insert_run(&pool, &folded).await?;
        assert_eq!(
            sqlx::query_scalar::<_, Option<String>>("SELECT issue FROM runs WHERE run_id = $1")
                .bind(&playbook_run)
                .fetch_one(&pool)
                .await?
                .as_deref(),
            Some(key)
        );
        assert!(
            running_run_for_issue(&pool, key).await?.is_none(),
            "a finished run is no longer live"
        );
        Ok(())
    }

    fn bare_run(run_id: &str, status: &str) -> NewRun {
        NewRun {
            run_id: run_id.to_string(),
            scope: None,
            issue: None,
            identity_digest: None,
            status: status.to_string(),
            pod: None,
            session_uri: None,
            best_score: None,
            cost_usd: None,
        }
    }

    fn created_query(dir: crate::model::SortDir) -> RunQuery {
        RunQuery {
            status: None,
            repo: None,
            dispatch_target: None,
            kind: None,
            sort: crate::runs::model::RunSort::Created,
            dir,
            limit: 50,
            offset: 0,
        }
    }

    /// The leaderboard's newest-first is the order runs were recorded in. Every id shape the
    /// controller and the engine mint is here, inserted in an order that matches neither the
    /// recorded times nor the ids' lexical order, so a sort on either of those fails.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn the_leaderboard_sorts_by_when_a_run_was_recorded_not_by_its_id(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let rows = [
            (
                "20260817T000000Z-goal-generalize-the-fused-gemm",
                "2026-08-17T00:00:00Z",
            ),
            (
                "scenario_01a0d53c-ffc3-7652-be92-9c77252a2352-1790283939",
                "2026-09-24T21:05:39Z",
            ),
            (
                "playbook_survey_0199c0de-7c2c-71a5-8000-9-1793620800",
                "2026-11-02T12:00:00Z",
            ),
            ("adopted-1786754192", "2026-08-15T00:36:32Z"),
            ("owner_repo_1-1790812800", "2026-10-01T00:00:00Z"),
            (
                "playbook_triage_019a4b1c-0000-7000-8000-1-1793620800",
                "2026-11-02T12:00:00Z",
            ),
        ];
        for (run_id, at) in rows {
            insert_run(&pool, &bare_run(run_id, "finished")).await?;
            set_run_created_at(&pool, run_id, at).await?;
        }

        let desc = list_runs_page(&pool, &created_query(crate::model::SortDir::Desc)).await?;
        let ids = |page: &[RunRow]| page.iter().map(|r| r.run_id.clone()).collect::<Vec<_>>();
        assert_eq!(
            ids(&desc),
            [
                "playbook_triage_019a4b1c-0000-7000-8000-1-1793620800",
                "playbook_survey_0199c0de-7c2c-71a5-8000-9-1793620800",
                "owner_repo_1-1790812800",
                "scenario_01a0d53c-ffc3-7652-be92-9c77252a2352-1790283939",
                "20260817T000000Z-goal-generalize-the-fused-gemm",
                "adopted-1786754192",
            ],
            "newest first; a tie goes to the row recorded later"
        );
        assert_eq!(desc[0].created_at, "2026-11-02T12:00:00Z");
        assert_eq!(desc[5].created_at, "2026-08-15T00:36:32Z");

        let asc = list_runs_page(&pool, &created_query(crate::model::SortDir::Asc)).await?;
        let mut reversed = ids(&desc);
        reversed.reverse();
        assert_eq!(ids(&asc), reversed, "ascending is the exact reverse");

        let second_page = list_runs_page(
            &pool,
            &RunQuery {
                limit: 2,
                offset: 2,
                ..created_query(crate::model::SortDir::Desc)
            },
        )
        .await?;
        assert_eq!(ids(&second_page), ids(&desc[2..4]));
        Ok(())
    }

    /// A run is stamped when its row is first written; the completion edge re-runs the same upsert
    /// and must not move the stamp.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_run_is_stamped_on_insert_and_completion_keeps_the_stamp(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let before = jiff::Timestamp::now() - jiff::SignedDuration::from_secs(1);
        insert_run(&pool, &bare_run("run-1", "running")).await?;
        let stamped = get_run(&pool, "run-1").await?.expect("row").created_at;
        let at: jiff::Timestamp = stamped.parse()?;
        assert!(
            at >= before && at <= jiff::Timestamp::now(),
            "{stamped} is the insert time"
        );
        assert_eq!(
            stamped,
            crate::clock::stamp(at),
            "the schema's stamp format"
        );

        set_run_created_at(&pool, "run-1", "2026-07-04T10:00:00Z").await?;
        insert_run(&pool, &bare_run("run-1", "finished")).await?;
        let run = get_run(&pool, "run-1").await?.expect("row");
        assert_eq!(run.status, "finished");
        assert_eq!(run.created_at, "2026-07-04T10:00:00Z");
        Ok(())
    }

    /// A launch's runs and a scope's runs read in recorded order too, whatever their ids say.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn per_launch_reads_follow_the_recorded_order(pool: PgPool) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO issues (key, repo, tier, status, priority, input_kind, title, updated_at) \
             VALUES ('playbook:survey:k', 'owner/repo', 'T1', 'running', 0, 'playbook', 's', \
             '2026-08-23T00:00:00Z')",
        )
        .execute(&pool)
        .await?;
        for (run_id, at) in [
            ("playbook_survey_k-1790000000", "2026-09-21T13:46:40Z"),
            ("playbook_survey_k-999999999", "2001-09-09T01:46:39Z"),
            ("playbook_survey_k-rerun", "2026-10-01T00:00:00Z"),
        ] {
            insert_run(
                &pool,
                &NewRun {
                    issue: Some("playbook:survey:k".to_string()),
                    ..bare_run(run_id, "finished")
                },
            )
            .await?;
            set_run_created_at(&pool, run_id, at).await?;
        }
        let runs = crate::launches::store::list_runs_for_launch(&pool, "playbook:survey:k").await?;
        assert_eq!(
            runs.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
            [
                "playbook_survey_k-rerun",
                "playbook_survey_k-1790000000",
                "playbook_survey_k-999999999",
            ]
        );
        Ok(())
    }

    /// The migration that adds `created_at` dates every existing row: an engine id from its
    /// leading stamp, a controller id from its trailing epoch, and an id that carries no real
    /// instant from its neighbours in insertion order (the newest dated row before it, else the
    /// oldest one after it).
    #[sqlx::test(migrations = false)]
    async fn the_migration_dates_existing_runs_from_their_ids(pool: PgPool) -> anyhow::Result<()> {
        crate::MIGRATOR.run_to(74, &pool).await?;
        for run_id in [
            "first-undated",
            "20260817T093000Z-goal-generalize-the-fused-gemm",
            "playbook_survey_01a0d53c-ffc3-7652-be92-9c77252a2352-1790283939",
            "between-undated",
            "adopted-1786754192",
            "20261340T999999Z-not-a-real-instant",
        ] {
            sqlx::query("INSERT INTO runs (run_id, status) VALUES ($1, 'finished')")
                .bind(run_id)
                .execute(&pool)
                .await?;
        }

        crate::MIGRATOR.run(&pool).await?;

        let dated: Vec<(String, String)> =
            sqlx::query_as("SELECT run_id, created_at FROM runs ORDER BY seq")
                .fetch_all(&pool)
                .await?;
        assert_eq!(
            dated,
            [
                ("first-undated", "2026-08-15T00:36:32Z"),
                (
                    "20260817T093000Z-goal-generalize-the-fused-gemm",
                    "2026-08-17T09:30:00Z"
                ),
                (
                    "playbook_survey_01a0d53c-ffc3-7652-be92-9c77252a2352-1790283939",
                    "2026-09-24T21:05:39Z"
                ),
                ("between-undated", "2026-09-24T21:05:39Z"),
                ("adopted-1786754192", "2026-08-15T00:36:32Z"),
                (
                    "20261340T999999Z-not-a-real-instant",
                    "2026-09-24T21:05:39Z"
                ),
            ]
            .map(|(id, at)| (id.to_string(), at.to_string()))
        );

        insert_run(&pool, &bare_run("after-migration", "running")).await?;
        let at: String =
            sqlx::query_scalar("SELECT created_at FROM runs WHERE run_id = 'after-migration'")
                .fetch_one(&pool)
                .await?;
        assert!(
            at.parse::<jiff::Timestamp>().is_ok(),
            "a new row is stamped by the column default: {at}"
        );
        Ok(())
    }
}
