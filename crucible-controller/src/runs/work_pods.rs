use crate::runs::workpod::{NewWorkPod, WorkPodRow, WorkPodState};
use anyhow::{Context, Result};
use sqlx::{PgExecutor, Row};

// ---------------------------------------------------------------------------------------------
// WorkPod dispatch primitive: the controller-owned work-pod ledger (`work_pods`).
// ---------------------------------------------------------------------------------------------

/// The `work_pods` column list, in decode order. Every read builds its SELECT from this so a new
/// column is a one-line edit here plus [`decode_work_pod`]. Bare names (no table prefix) — the only
/// reader that joins ([`queued_candidates`]) projects a different shape, not [`WorkPodRow`].
const WORK_POD_COLS: &str = "pod_name, kind, issue_key, state, cost_tag, result, error, created_at, updated_at, terminal_at, cluster, pod_uid";

/// Decode one `work_pods` row into its strong [`WorkPodState`]. Shared by every `work_pods` read;
/// these use `sqlx::query` (not the `query!` macro), so a new column needs no `.sqlx` cache entry,
/// at the cost of compile-time column checks on these static reads.
fn decode_work_pod(r: &sqlx::postgres::PgRow) -> Result<WorkPodRow> {
    let state: String = r.try_get("state")?;
    Ok(WorkPodRow {
        pod_name: r.try_get("pod_name")?,
        kind: r.try_get("kind")?,
        issue_key: r.try_get("issue_key")?,
        state: WorkPodState::parse(&state)?,
        cost_tag: r.try_get("cost_tag")?,
        result: r.try_get("result")?,
        error: r.try_get("error")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
        terminal_at: r.try_get("terminal_at")?,
        cluster: r.try_get("cluster")?,
        pod_uid: r.try_get("pod_uid")?,
    })
}

/// Insert a `work_pods` row (a freshly dispatched or queued pod). `pod_name` is the PRIMARY KEY, so a
/// duplicate is a caller bug (the name is minted unique per dispatch), not an upsert.
#[tracing::instrument(name = "db.insert_work_pod", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", pod_name = %wp.pod_name, kind = %wp.kind), err)]
pub(crate) async fn insert_work_pod(ex: impl PgExecutor<'_>, wp: &NewWorkPod) -> Result<()> {
    let now = crate::clock::now_rfc3339();
    let state = wp.state.as_str();
    let terminal_at = wp.state.is_terminal().then_some(&now);
    sqlx::query!(
        r#"
        INSERT INTO work_pods
            (pod_name, kind, issue_key, state, cost_tag, result, error, created_at, updated_at, terminal_at, cluster)
        VALUES ($1, $2, $3, $4, $5, NULL, NULL, $6, $7, $8, $9)
        -- The pod name is derived from the run id, and the row is written before the render that
        -- can fail. A retry of the same run must reach that render and report its real error, not
        -- collide here on the row its own previous attempt left.
        ON CONFLICT (pod_name) DO NOTHING
        "#,
        wp.pod_name,
        wp.kind,
        wp.issue_key,
        state,
        wp.cost_tag,
        now,
        now,
        terminal_at,
        wp.cluster,
    )
    .execute(ex)
    .await
    .with_context(|| format!("recording the work-pod ledger row for {}", wp.pod_name))?;
    Ok(())
}

/// Record the pod UID the API server returned on create. The controller observed this UID itself,
/// so it is the only pod identity a grant redemption may be checked against.
#[tracing::instrument(name = "db.set_work_pod_uid", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", pod_name = %pod_name), err)]
pub(crate) async fn set_work_pod_uid(
    ex: impl PgExecutor<'_>,
    pod_name: &str,
    pod_uid: &str,
) -> Result<()> {
    let now = crate::clock::now_rfc3339();
    sqlx::query!(
        "UPDATE work_pods SET pod_uid = $2, updated_at = $3 WHERE pod_name = $1",
        pod_name,
        pod_uid,
        now,
    )
    .execute(ex)
    .await
    .context("set_work_pod_uid")?;
    Ok(())
}

/// Advance a `work_pods` row's state, folding in the result/error and (on a terminal transition) the
/// terminal timestamp — the GC clock a FAILED pod's retention window counts from. `terminal_at` is
/// only stamped when it isn't already set (the first terminal observation wins), so a later
/// `collected`/`swept` transition doesn't reset the retention clock.
#[tracing::instrument(name = "db.set_work_pod_state", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", pod_name = %pod_name), err)]
pub(crate) async fn set_work_pod_state(
    ex: impl PgExecutor<'_>,
    pod_name: &str,
    state: WorkPodState,
    result: Option<&str>,
    error: Option<&str>,
) -> Result<()> {
    let now = crate::clock::now_rfc3339();
    // Stamp terminal_at on the first terminal transition; COALESCE keeps an earlier stamp.
    let terminal_at = state.is_terminal().then_some(&now);
    let state = state.as_str();
    sqlx::query!(
        r#"
        UPDATE work_pods
        SET state = $1,
            result = COALESCE($2, result),
            error = COALESCE($3, error),
            updated_at = $4,
            terminal_at = COALESCE(terminal_at, $5)
        WHERE pod_name = $6
        "#,
        state,
        result,
        error,
        now,
        terminal_at,
        pod_name,
    )
    .execute(ex)
    .await
    .context("set_work_pod_state")?;
    Ok(())
}

/// Count the active (running) work pods of one kind — the per-kind concurrency cap query.
#[cfg(feature = "autoresearch")]
#[tracing::instrument(name = "db.count_active_work_pods", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", kind = %kind), err)]
pub(crate) async fn count_active_work_pods(ex: impl PgExecutor<'_>, kind: &str) -> Result<u32> {
    let row = sqlx::query!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM work_pods WHERE kind = $1 AND state = 'running'"#,
        kind,
    )
    .fetch_one(ex)
    .await
    .context("count_active_work_pods")?;
    Ok(u32::try_from(row.n).unwrap_or(u32::MAX))
}

/// Count the turns of one kind dispatched on a UTC day (`YYYY-MM-DD`) — the per-kind daily turn
/// budget query. A still-`queued` row hasn't spent a turn, so it's excluded; every other state has.
#[cfg(feature = "autoresearch")]
#[tracing::instrument(name = "db.count_work_pod_turns_on_day", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", kind = %kind, day = %day), err)]
pub(crate) async fn count_work_pod_turns_on_day(
    ex: impl PgExecutor<'_>,
    kind: &str,
    day: &str,
) -> Result<u32> {
    let row = sqlx::query!(
        r#"
        SELECT COUNT(*) AS "n!: i64" FROM work_pods
        WHERE kind = $1 AND state <> 'queued' AND substr(created_at, 1, 10) = $2
        "#,
        kind,
        day,
    )
    .fetch_one(ex)
    .await
    .context("count_work_pod_turns_on_day")?;
    Ok(u32::try_from(row.n).unwrap_or(u32::MAX))
}

/// The queued work pod for one kind + issue, or `None` — the dedupe check before queueing, and the
/// row a spawn consumes (its reserved pod name is reused). At most one row per (kind, issue) is ever
/// `queued`, enforced by this lookup at the only insert site.
#[cfg(feature = "autoresearch")]
#[tracing::instrument(name = "db.find_queued_work_pod", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", kind = %kind, issue_key = %issue_key), err)]
pub(crate) async fn find_queued_work_pod(
    ex: impl PgExecutor<'_>,
    kind: &str,
    issue_key: &str,
) -> Result<Option<WorkPodRow>> {
    let sql = format!(
        "SELECT {WORK_POD_COLS} FROM work_pods \
         WHERE kind = $1 AND issue_key = $2 AND state = 'queued' \
         ORDER BY created_at ASC, pod_name ASC LIMIT 1"
    );
    sqlx::query(&sql)
        .bind(kind)
        .bind(issue_key)
        .fetch_optional(ex)
        .await
        .context("find_queued_work_pod")?
        .map(|r| decode_work_pod(&r))
        .transpose()
}

/// Consume a queued row by promoting it to `running` in place (the spawn that drains it). Resets
/// `created_at` to now: for a spent turn that column means "when the turn dispatched" (the daily
/// budget's clock), not "when it entered the queue". A no-op on a row that isn't `queued`.
#[cfg(feature = "autoresearch")]
#[tracing::instrument(name = "db.promote_queued_work_pod", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", pod_name = %pod_name), err)]
pub(crate) async fn promote_queued_work_pod(ex: impl PgExecutor<'_>, pod_name: &str) -> Result<()> {
    let now = crate::clock::now_rfc3339();
    sqlx::query!(
        "UPDATE work_pods SET state = 'running', created_at = $1, updated_at = $2 \
         WHERE pod_name = $3 AND state = 'queued'",
        now,
        now,
        pod_name,
    )
    .execute(ex)
    .await
    .context("promote_queued_work_pod")?;
    Ok(())
}

/// The running work pod for one kind + issue, or `None` — the ADOPTION lookup a dispatch runs
/// before it mints a fresh pod. A turn already in flight for this issue (its in-band collector died
/// on a controller restart, or a level-triggered re-drive raced the original dispatch) is collected
/// on its existing pod, never relaunched: relaunching would spend the turn's money twice and orphan
/// the first pod's verdict. At most one row per (kind, issue) is ever `running` (a spawn
/// promotes/inserts exactly one), so this `LIMIT 1` is exact. Mirrors [`find_queued_work_pod`].
#[cfg(feature = "autoresearch")]
#[tracing::instrument(name = "db.find_running_work_pod", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", kind = %kind, issue_key = %issue_key), err)]
pub(crate) async fn find_running_work_pod(
    ex: impl PgExecutor<'_>,
    kind: &str,
    issue_key: &str,
) -> Result<Option<WorkPodRow>> {
    let sql = format!(
        "SELECT {WORK_POD_COLS} FROM work_pods \
         WHERE kind = $1 AND issue_key = $2 AND state = 'running' \
         ORDER BY created_at ASC, pod_name ASC LIMIT 1"
    );
    sqlx::query(&sql)
        .bind(kind)
        .bind(issue_key)
        .fetch_optional(ex)
        .await
        .context("find_running_work_pod")?
        .map(|r| decode_work_pod(&r))
        .transpose()
}

/// Compare-and-swap a `work_pods` row out of `running` into a terminal-collection state
/// (`collected`/`failed`), returning whether THIS caller won the transition. The single-booking
/// guard: the in-band collector and a level-triggered re-drive can both reach one terminal turn pod
/// at the same instant, so the turn's cost (and the succeeded-pod GC) is booked only by the CAS
/// winner. A read-then-write couldn't close that race — the `WHERE state = 'running'` does, atomically.
/// `terminal_at` is stamped on the first observation (COALESCE keeps an earlier one), the retention
/// clock a `failed` pod's sweep counts from.
#[cfg(feature = "autoresearch")]
#[tracing::instrument(name = "db.try_finish_running_work_pod", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", pod_name = %pod_name), err)]
pub(crate) async fn try_finish_running_work_pod(
    ex: impl PgExecutor<'_>,
    pod_name: &str,
    to: WorkPodState,
    result: Option<&str>,
    error: Option<&str>,
) -> Result<bool> {
    let now = crate::clock::now_rfc3339();
    let state = to.as_str();
    let affected = sqlx::query!(
        r#"
        UPDATE work_pods
        SET state = $1,
            result = COALESCE($2, result),
            error = COALESCE($3, error),
            updated_at = $4,
            terminal_at = COALESCE(terminal_at, $5)
        WHERE pod_name = $6 AND state = 'running'
        "#,
        state,
        result,
        error,
        now,
        now,
        pod_name,
    )
    .execute(ex)
    .await
    .context("try_finish_running_work_pod")?
    .rows_affected();
    Ok(affected == 1)
}

/// The oldest still-queued work pod of one kind (FIFO backpressure drain), or `None`.
#[cfg(feature = "autoresearch")]
#[cfg(test)]
#[tracing::instrument(name = "db.next_queued_work_pod", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", kind = %kind), err)]
pub(crate) async fn next_queued_work_pod(
    ex: impl PgExecutor<'_>,
    kind: &str,
) -> Result<Option<WorkPodRow>> {
    let sql = format!(
        "SELECT {WORK_POD_COLS} FROM work_pods \
         WHERE kind = $1 AND state = 'queued' \
         ORDER BY created_at ASC, pod_name ASC LIMIT 1"
    );
    sqlx::query(&sql)
        .bind(kind)
        .fetch_optional(ex)
        .await
        .context("next_queued_work_pod")?
        .map(|r| decode_work_pod(&r))
        .transpose()
}

/// A queued work pod considered for draining onto a freed slot: its reserved pod name, the issue to
/// re-drive, and whether that issue has since PARKED (a `LEFT JOIN issues`). A parked candidate is
/// future spend the park explicitly rejected — the drain purges it (row → `swept`) instead of
/// re-driving. A candidate whose issue row is missing entirely reads `parked = false` (there's
/// nothing to reject); the drain re-drives it and the reconcile pass no-ops if the key is untracked.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedCandidate {
    pub(crate) pod_name: String,
    pub(crate) issue_key: Option<String>,
    pub(crate) parked: bool,
}

/// The eldest-first queued work pods of `kind`, up to `limit`, each tagged with whether its issue is
/// parked — the queue drain's single candidate scan. The parked flag rides a `LEFT JOIN issues` so
/// the caller needn't re-query per row (the sweep backstop's "one query per kind" budget). FIFO by
/// `created_at` then `pod_name`, identical to [`next_queued_work_pod`], so the drain honors queue
/// order.
#[cfg(feature = "autoresearch")]
#[tracing::instrument(name = "db.queued_candidates", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", kind = %kind), err)]
pub(crate) async fn queued_candidates(
    ex: impl PgExecutor<'_>,
    kind: &str,
    limit: i64,
) -> Result<Vec<QueuedCandidate>> {
    let rows = sqlx::query!(
        r#"
        SELECT wp.pod_name AS "pod_name!", wp.issue_key,
               COALESCE(i.status = 'parked', FALSE) AS "parked!: bool"
        FROM work_pods wp
        LEFT JOIN issues i ON i.key = wp.issue_key
        WHERE wp.kind = $1 AND wp.state = 'queued'
        ORDER BY wp.created_at ASC, wp.pod_name ASC
        LIMIT $2
        "#,
        kind,
        limit,
    )
    .fetch_all(ex)
    .await
    .context("queued_candidates")?;
    Ok(rows
        .into_iter()
        .map(|r| QueuedCandidate {
            pod_name: r.pod_name,
            issue_key: r.issue_key,
            parked: r.parked,
        })
        .collect())
}

/// Delete every QUEUED work pod of an issue — the park purge. A park rejects the future spend those
/// rows represent, so they must never later promote into a paid turn. A queued row never created a
/// pod (only a spawn does), so there is nothing on the cluster to GC and no cost to unwind: the row
/// is deleted outright rather than marked terminal, which also keeps it out of the daily-turn budget
/// count (a purged row never ran). No ledger entry (a park moves no money). Returns how many rows it
/// deleted (for the debug log / test).
#[tracing::instrument(name = "db.purge_queued_work_pods", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", issue_key = %issue_key), err)]
pub(crate) async fn purge_queued_work_pods(
    ex: impl PgExecutor<'_>,
    issue_key: &str,
) -> Result<u64> {
    let affected = sqlx::query!(
        "DELETE FROM work_pods WHERE issue_key = $1 AND state = 'queued'",
        issue_key,
    )
    .execute(ex)
    .await
    .context("purge_queued_work_pods")?
    .rows_affected();
    Ok(affected)
}

/// Delete one specific QUEUED work pod by name — the drain's purge of a single parked-issue row it
/// skipped past. The `state = 'queued'` guard makes it a no-op if the row was meanwhile promoted, so
/// it can never race a spawn. Same rationale as [`purge_queued_work_pods`]: a queued row created no
/// pod, so deleting it leaks nothing and keeps it out of the turn budget.
#[cfg(feature = "autoresearch")]
#[tracing::instrument(name = "db.delete_queued_work_pod", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", pod_name = %pod_name), err)]
pub(crate) async fn delete_queued_work_pod(ex: impl PgExecutor<'_>, pod_name: &str) -> Result<u64> {
    let affected = sqlx::query!(
        "DELETE FROM work_pods WHERE pod_name = $1 AND state = 'queued'",
        pod_name,
    )
    .execute(ex)
    .await
    .context("delete_queued_work_pod")?
    .rows_affected();
    Ok(affected)
}

/// Every work pod in one of `states` — startup re-adoption (`running`) and orphan/GC sweeps
/// (`succeeded`/`failed` uncollected) both read this. `states` is an internal whitelist of
/// [`WorkPodState`] spellings, never caller input, so interpolating it into the `IN (…)` is safe.
#[tracing::instrument(name = "db.work_pods_in_states", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub async fn work_pods_in_states(
    ex: impl PgExecutor<'_>,
    states: &[WorkPodState],
) -> Result<Vec<WorkPodRow>> {
    if states.is_empty() {
        return Ok(Vec::new());
    }
    let list = states
        .iter()
        .map(|s| format!("'{}'", s.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT {WORK_POD_COLS} FROM work_pods WHERE state IN ({list}) ORDER BY created_at ASC"
    );
    let rows = sqlx::query(&sql)
        .fetch_all(ex)
        .await
        .context("work_pods_in_states")?;
    rows.iter().map(decode_work_pod).collect()
}

/// Fetch one work pod by name, or `None` if untracked.
#[tracing::instrument(name = "db.get_work_pod", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", pod_name = %pod_name), err)]
pub(crate) async fn get_work_pod(
    ex: impl PgExecutor<'_>,
    pod_name: &str,
) -> Result<Option<WorkPodRow>> {
    let sql = format!("SELECT {WORK_POD_COLS} FROM work_pods WHERE pod_name = $1");
    sqlx::query(&sql)
        .bind(pod_name)
        .fetch_optional(ex)
        .await
        .context("get_work_pod")?
        .map(|r| decode_work_pod(&r))
        .transpose()
}

/// The newest `limit` work-pod rows, optionally narrowed by state and/or kind — the
/// `GET /api/turns` audit view. `state`/`kind` arrive pre-validated (parsed into the strong
/// [`WorkPodState`]/[`crate::runs::workpod::WorkKind`] by the API layer, then spelled back), never raw
/// caller input. Ties on `created_at` (second-resolution stamps) break on `pod_name` for a
/// stable page.
#[cfg(feature = "autoresearch")]
#[tracing::instrument(name = "db.list_work_pods", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn list_work_pods(
    ex: impl PgExecutor<'_>,
    state: Option<&str>,
    kind: Option<&str>,
    limit: i64,
) -> Result<Vec<WorkPodRow>> {
    let sql = format!(
        "SELECT {WORK_POD_COLS} FROM work_pods \
         WHERE ($1 IS NULL OR state = $2) AND ($3 IS NULL OR kind = $4) \
         ORDER BY created_at DESC, pod_name DESC LIMIT $5"
    );
    let rows = sqlx::query(&sql)
        .bind(state)
        .bind(state)
        .bind(kind)
        .bind(kind)
        .bind(limit)
        .fetch_all(ex)
        .await
        .context("list_work_pods")?;
    rows.iter().map(decode_work_pod).collect()
}

/// Stash a ScopeNow override on the issue row so reconcile_new can detect and honor it.
#[tracing::instrument(name = "db.set_scope_now", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub(crate) async fn set_scope_now(
    ex: impl PgExecutor<'_>,
    key: &str,
    justification: &str,
    max_cost: Option<f64>,
) -> Result<()> {
    sqlx::query!(
        "UPDATE issues SET scope_now_justification = $1, scope_now_max_cost = $2 WHERE key = $3",
        justification,
        max_cost,
        key,
    )
    .execute(ex)
    .await
    .context("set_scope_now")?;
    Ok(())
}

/// Clear the ScopeNow override after the scope dispatch completes (or is declined).
#[cfg(feature = "autoresearch")]
#[tracing::instrument(name = "db.clear_scope_now", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub(crate) async fn clear_scope_now(ex: impl PgExecutor<'_>, key: &str) -> Result<()> {
    sqlx::query!(
        "UPDATE issues SET scope_now_justification = NULL, scope_now_max_cost = NULL WHERE key = $1",
        key,
    )
    .execute(ex)
    .await
    .context("clear_scope_now")?;
    Ok(())
}

/// Record (or dedup) a Tier 2 drop-box pointer (`pod_artifacts`). Content-addressed: a re-POST of
/// the same (pod, kind) with the same digest is idempotent (`INSERT OR REPLACE` with an identical
/// row), so the caller can treat the pre-existing digest as the dedup signal. Runtime query (not
/// the `query!` macro) so a fresh migration needs no `.sqlx` offline-cache entry.
#[tracing::instrument(name = "db.record_pod_artifact", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn record_pod_artifact(
    ex: impl PgExecutor<'_>,
    a: &crate::runs::model::NewPodArtifact,
) -> Result<()> {
    let now = crate::clock::now_rfc3339();
    sqlx::query(
        r#"
        INSERT INTO pod_artifacts (pod, kind, digest, bytes, created_at)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT(pod, kind) DO UPDATE SET
            digest = excluded.digest, bytes = excluded.bytes,
            created_at = excluded.created_at
        "#,
    )
    .bind(&a.pod)
    .bind(&a.kind)
    .bind(&a.digest)
    .bind(a.bytes)
    .bind(now)
    .execute(ex)
    .await
    .context("record_pod_artifact")?;
    Ok(())
}

/// The drop-box pointer for one (pod, kind), or `None`. The ingest approval reads it to answer "do I
/// already hold this digest?" (idempotent dedup); the fold reads it to resolve + validate a Tier 1
/// manifest entry against the stored bytes.
#[tracing::instrument(name = "db.pod_artifact", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", pod = %pod, kind = %kind), err)]
pub(crate) async fn pod_artifact(
    ex: impl PgExecutor<'_>,
    pod: &str,
    kind: &str,
) -> Result<Option<crate::runs::model::PodArtifactRow>> {
    let row = sqlx::query(
        r#"SELECT pod, kind, digest, bytes, created_at
           FROM pod_artifacts WHERE pod = $1 AND kind = $2"#,
    )
    .bind(pod)
    .bind(kind)
    .fetch_optional(ex)
    .await
    .context("pod_artifact")?;
    Ok(row.map(|r| crate::runs::model::PodArtifactRow {
        pod: r.get("pod"),
        kind: r.get("kind"),
        digest: r.get("digest"),
        bytes: r.get("bytes"),
        created_at: r.get("created_at"),
    }))
}

/// The terminal runs the MLflow exporter still owes a push: a run whose bookkeeping row is absent
/// or not yet `exported`. `running` runs are skipped — the export is post-fold, over a finished
/// run's evidence. Ordered oldest-first (by `run_id`, which carries a time prefix) so a backlog
/// drains in creation order; `limit` bounds one sweep's batch.
#[tracing::instrument(name = "db.runs_awaiting_mlflow_export", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn runs_awaiting_mlflow_export(
    ex: impl PgExecutor<'_>,
    limit: i64,
) -> Result<Vec<String>> {
    let rows = sqlx::query(
        r#"
        SELECT r.run_id
        FROM runs r
        LEFT JOIN mlflow_exports m ON m.run_id = r.run_id
        WHERE r.status != 'running'
          AND (m.state IS NULL OR m.state != 'exported')
        ORDER BY r.run_id ASC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(ex)
    .await
    .context("runs_awaiting_mlflow_export")?;
    Ok(rows.iter().map(|r| r.get::<String, _>("run_id")).collect())
}

/// Claim a run for an export attempt: upsert its `mlflow_exports` row to `pending` and bump the
/// attempt counter. Idempotent per sweep; the terminal `mark_mlflow_exported`/`mark_mlflow_failed`
/// records the outcome.
#[tracing::instrument(name = "db.mark_mlflow_pending", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", run_id = %run_id), err)]
pub(crate) async fn mark_mlflow_pending(ex: impl PgExecutor<'_>, run_id: &str) -> Result<()> {
    let now = crate::clock::now_rfc3339();
    sqlx::query(
        r#"
        INSERT INTO mlflow_exports (run_id, state, attempts, created_at, updated_at)
        VALUES ($1, 'pending', 1, $2, $3)
        ON CONFLICT(run_id) DO UPDATE SET
            state = 'pending', attempts = mlflow_exports.attempts + 1, updated_at = excluded.updated_at
        "#,
    )
    .bind(run_id)
    .bind(&now)
    .bind(&now)
    .execute(ex)
    .await
    .context("mark_mlflow_pending")?;
    Ok(())
}

/// Record a successful export: `exported` plus the tracking-server ids, error cleared. The
/// idempotency stop — a later sweep skips this run.
#[tracing::instrument(name = "db.mark_mlflow_exported", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", run_id = %run_id), err)]
pub(crate) async fn mark_mlflow_exported(
    ex: impl PgExecutor<'_>,
    run_id: &str,
    mlflow_run_id: &str,
    experiment_id: &str,
) -> Result<()> {
    let now = crate::clock::now_rfc3339();
    sqlx::query(
        r#"
        UPDATE mlflow_exports
        SET state = 'exported', mlflow_run_id = $1, experiment_id = $2, error = NULL, updated_at = $3
        WHERE run_id = $4
        "#,
    )
    .bind(mlflow_run_id)
    .bind(experiment_id)
    .bind(now)
    .bind(run_id)
    .execute(ex)
    .await
    .context("mark_mlflow_exported")?;
    Ok(())
}

/// Record a failed export: `failed` with the error preserved, so the next sweep retries loudly
/// (the ADR's allow-failure ethos — a failed export marks the row and comes back later).
#[tracing::instrument(name = "db.mark_mlflow_failed", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", run_id = %run_id), err)]
pub(crate) async fn mark_mlflow_failed(
    ex: impl PgExecutor<'_>,
    run_id: &str,
    error: &str,
) -> Result<()> {
    let now = crate::clock::now_rfc3339();
    sqlx::query(
        r#"UPDATE mlflow_exports SET state = 'failed', error = $1, updated_at = $2 WHERE run_id = $3"#,
    )
    .bind(error)
    .bind(now)
    .bind(run_id)
    .execute(ex)
    .await
    .context("mark_mlflow_failed")?;
    Ok(())
}

/// One run's export bookkeeping row, or `None` if no sweep has touched it yet.
#[cfg(test)]
#[tracing::instrument(name = "db.mlflow_export", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", run_id = %run_id), err)]
pub(crate) async fn mlflow_export(
    ex: impl PgExecutor<'_>,
    run_id: &str,
) -> Result<Option<crate::runs::model::MlflowExportRow>> {
    let row = sqlx::query(
        r#"SELECT run_id, state, mlflow_run_id, experiment_id, attempts, error, created_at, updated_at
           FROM mlflow_exports WHERE run_id = $1"#,
    )
    .bind(run_id)
    .fetch_optional(ex)
    .await
    .context("mlflow_export")?;
    row.map(|r| {
        let state = r
            .get::<String, _>("state")
            .parse::<crate::runs::model::MlflowExportState>()
            .map_err(anyhow::Error::msg)?;
        Ok(crate::runs::model::MlflowExportRow {
            run_id: r.get("run_id"),
            state,
            mlflow_run_id: r.get("mlflow_run_id"),
            experiment_id: r.get("experiment_id"),
            attempts: r.get("attempts"),
            error: r.get("error"),
            created_at: r.get("created_at"),
            updated_at: r.get("updated_at"),
        })
    })
    .transpose()
}

/// Stash a redispatch justification on the issue row so the awaiting-approval reconcile detects the
/// human-authorized re-run and honors it past the autopilot pause.
#[tracing::instrument(name = "db.set_redispatch", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub(crate) async fn set_redispatch(
    ex: impl PgExecutor<'_>,
    key: &str,
    justification: &str,
) -> Result<()> {
    sqlx::query!(
        "UPDATE issues SET redispatch_justification = $1 WHERE key = $2",
        justification,
        key,
    )
    .execute(ex)
    .await
    .context("set_redispatch")?;
    Ok(())
}

/// Clear the redispatch stash once the re-dispatched run launches.
#[cfg(feature = "autoresearch")]
#[tracing::instrument(name = "db.clear_redispatch", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub(crate) async fn clear_redispatch(ex: impl PgExecutor<'_>, key: &str) -> Result<()> {
    sqlx::query!(
        "UPDATE issues SET redispatch_justification = NULL WHERE key = $1",
        key,
    )
    .execute(ex)
    .await
    .context("clear_redispatch")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn pod_artifact_record_get_and_overwrite(pool: PgPool) -> Result<()> {
        use crate::runs::model::NewPodArtifact;
        let a = NewPodArtifact {
            pod: "crucible-turn-x".into(),
            kind: "scope-pack".into(),
            digest: "sha256:aaaa".into(),
            bytes: 100,
        };
        record_pod_artifact(&pool, &a).await?;
        let got = pod_artifact(&pool, "crucible-turn-x", "scope-pack")
            .await?
            .expect("row");
        assert_eq!(got.digest, "sha256:aaaa");
        assert_eq!(got.bytes, 100);

        // A different kind for the same pod is a distinct (absent) row (PK is (pod, kind)).
        assert!(
            pod_artifact(&pool, "crucible-turn-x", "scope-transcript")
                .await?
                .is_none()
        );

        // Re-recording the same (pod, kind) with a new digest overwrites in place — no duplicate row.
        let b = NewPodArtifact {
            digest: "sha256:bbbb".into(),
            bytes: 200,
            ..a.clone()
        };
        record_pod_artifact(&pool, &b).await?;
        let got = pod_artifact(&pool, "crucible-turn-x", "scope-pack")
            .await?
            .expect("row");
        assert_eq!(got.digest, "sha256:bbbb");
        assert_eq!(got.bytes, 200);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pod_artifacts")
            .fetch_one(&pool)
            .await?;
        assert_eq!(n, 1, "overwrite, not insert");
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn mlflow_bookkeeping_is_idempotent_and_resumable(pool: PgPool) -> Result<()> {
        use crate::runs::model::MlflowExportState;

        // Two terminal runs + one still running; the running one is never a candidate.
        sqlx::query("INSERT INTO runs (run_id, status) VALUES ('run-a', 'done'), ('run-b', 'failed'), ('run-live', 'running')")
            .execute(&pool)
            .await?;

        // Both terminal runs are awaiting export; the running one is excluded.
        let mut awaiting = runs_awaiting_mlflow_export(&pool, 10).await?;
        awaiting.sort();
        assert_eq!(awaiting, vec!["run-a".to_string(), "run-b".to_string()]);

        // Claim + succeed run-a: it leaves the awaiting set (the idempotency stop).
        mark_mlflow_pending(&pool, "run-a").await?;
        mark_mlflow_exported(&pool, "run-a", "mlrun-a", "exp-1").await?;
        let a = mlflow_export(&pool, "run-a").await?.expect("row");
        assert_eq!(a.state, MlflowExportState::Exported);
        assert_eq!(a.mlflow_run_id.as_deref(), Some("mlrun-a"));
        assert_eq!(a.attempts, 1);
        assert_eq!(
            runs_awaiting_mlflow_export(&pool, 10).await?,
            vec!["run-b".to_string()]
        );

        // Claim + fail run-b: it stays awaiting (resumable) with the error and a bumped attempt.
        mark_mlflow_pending(&pool, "run-b").await?;
        mark_mlflow_failed(&pool, "run-b", "connection refused").await?;
        let b = mlflow_export(&pool, "run-b").await?.expect("row");
        assert_eq!(b.state, MlflowExportState::Failed);
        assert_eq!(b.error.as_deref(), Some("connection refused"));
        assert_eq!(
            runs_awaiting_mlflow_export(&pool, 10).await?,
            vec!["run-b".to_string()]
        );

        // A retry sweep re-claims it (attempts increments), then succeeds and clears the error.
        mark_mlflow_pending(&pool, "run-b").await?;
        let b = mlflow_export(&pool, "run-b").await?.expect("row");
        assert_eq!(b.attempts, 2, "retry bumped the attempt counter");
        mark_mlflow_exported(&pool, "run-b", "mlrun-b", "exp-1").await?;
        let b = mlflow_export(&pool, "run-b").await?.expect("row");
        assert_eq!(b.state, MlflowExportState::Exported);
        assert_eq!(b.error, None, "success clears the prior failure detail");
        assert!(runs_awaiting_mlflow_export(&pool, 10).await?.is_empty());
        Ok(())
    }
}
