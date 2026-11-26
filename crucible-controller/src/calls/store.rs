//! `call_target_runs` and the call columns of `playbook_launches` (migration 0092).

use anyhow::{Context, Result};
use crucible_contract::calls::{
    CallAlias, CallBudget, CallId, CallKind, CallStatus, CallTarget, FileRef, NodeLink,
    ProgressGraph, TargetHandle, TargetInterface,
};
use serde_json::Value;
use sqlx::{PgExecutor, PgPool, Row as _};

use crate::calls::{CallRecord, Dispatched, Outcome, Parent};

pub(crate) fn status_str(status: CallStatus) -> &'static str {
    match status {
        CallStatus::Pending => "pending",
        CallStatus::Running => "running",
        CallStatus::Passed => "passed",
        CallStatus::Failed => "failed",
        CallStatus::Cancelled => "cancelled",
    }
}

fn parse_status(raw: &str) -> Result<CallStatus> {
    serde_json::from_value(Value::String(raw.to_owned()))
        .with_context(|| format!("unknown call status {raw:?}"))
}

fn parse_kind(raw: &str) -> Result<CallKind> {
    serde_json::from_value(Value::String(raw.to_owned()))
        .with_context(|| format!("unknown call kind {raw:?}"))
}

const COLUMNS: &str = "id, parent_run, task, instance, attempt, alias, kind, target, interface, \
     handle, detail, budget, status, note, result, result_files, progress, link, created_at";

fn decode(row: &sqlx::postgres::PgRow) -> Result<CallRecord> {
    let json = |name: &str| -> Result<Value> {
        row.try_get::<Value, _>(name)
            .with_context(|| format!("reading call column {name}"))
    };
    let optional = |name: &str| -> Result<Option<Value>> {
        row.try_get::<Option<Value>, _>(name)
            .with_context(|| format!("reading call column {name}"))
    };
    let attempt: i32 = row.try_get("attempt")?;
    Ok(CallRecord {
        id: CallId(row.try_get("id")?),
        parent_run: row.try_get("parent_run")?,
        task: row.try_get("task")?,
        instance: row.try_get("instance")?,
        attempt: u32::try_from(attempt).context("a stored call attempt is negative")?,
        alias: CallAlias::parse(&row.try_get::<String, _>("alias")?)?,
        kind: parse_kind(&row.try_get::<String, _>("kind")?)?,
        target: serde_json::from_value::<CallTarget>(json("target")?)
            .context("decoding a call's target")?,
        interface: serde_json::from_value::<TargetInterface>(json("interface")?)
            .context("decoding a call's interface")?,
        handle: optional("handle")?
            .map(serde_json::from_value::<TargetHandle>)
            .transpose()
            .context("decoding a call's handle")?,
        detail: json("detail")?,
        budget: optional("budget")?
            .map(serde_json::from_value::<CallBudget>)
            .transpose()
            .context("decoding a call's budget")?,
        status: parse_status(&row.try_get::<String, _>("status")?)?,
        note: row.try_get("note")?,
        result: json("result")?,
        result_files: serde_json::from_value::<Vec<FileRef>>(json("result_files")?)
            .context("decoding a call's result files")?,
        progress: serde_json::from_value::<ProgressGraph>(json("progress")?)
            .context("decoding a call's progress")?,
        link: optional("link")?
            .map(serde_json::from_value::<NodeLink>)
            .transpose()
            .context("decoding a call's link")?,
        created_at: row.try_get("created_at")?,
    })
}

/// A call about to be dispatched.
pub(crate) struct NewCall<'a> {
    pub parent_run: &'a str,
    pub task: &'a str,
    pub instance: Option<&'a str>,
    pub attempt: u32,
    pub alias: &'a CallAlias,
    pub target: &'a CallTarget,
    pub interface: &'a TargetInterface,
    /// The audit of the bound values: every value, and each file as its digest and size.
    pub bound: Value,
    pub budget: Option<CallBudget>,
}

/// What a claim found.
#[derive(Debug)]
pub(crate) enum Claimed {
    /// This request holds the identity and dispatches.
    Fresh(CallId),
    /// The identity already holds a call, dispatched or in flight.
    Existing(Box<CallRecord>),
}

/// Take the call identity (run, task, instance, attempt), or find who holds it.
pub(crate) async fn claim(pool: &PgPool, new: &NewCall<'_>) -> Result<Claimed> {
    let id = uuid::Uuid::now_v7().to_string();
    let now = crate::clock::now_rfc3339();
    let attempt = i32::try_from(new.attempt).context("a call attempt past i32")?;
    let inserted: Option<String> = sqlx::query_scalar(
        "INSERT INTO call_target_runs (id, parent_run, task, instance, attempt, alias, kind, \
         target, interface, bound, budget, progress, status, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $13, 'pending', $12, $12) \
         ON CONFLICT (parent_run, task, (COALESCE(instance, '')), attempt) DO NOTHING \
         RETURNING id",
    )
    .bind(&id)
    .bind(new.parent_run)
    .bind(new.task)
    .bind(new.instance)
    .bind(attempt)
    .bind(new.alias.as_str())
    .bind(new.target.kind().as_str())
    .bind(serde_json::to_value(new.target)?)
    .bind(serde_json::to_value(new.interface)?)
    .bind(&new.bound)
    .bind(new.budget.map(serde_json::to_value).transpose()?)
    .bind(&now)
    .bind(serde_json::to_value(new.interface.graph.pending())?)
    .fetch_optional(pool)
    .await
    .context("claiming a call identity")?;
    if let Some(id) = inserted {
        return Ok(Claimed::Fresh(CallId(id)));
    }
    let row = sqlx::query(const_format::formatcp!(
        "SELECT {COLUMNS} FROM call_target_runs \
         WHERE parent_run = $1 AND task = $2 AND COALESCE(instance, '') = COALESCE($3, '') \
         AND attempt = $4"
    ))
    .bind(new.parent_run)
    .bind(new.task)
    .bind(new.instance)
    .bind(attempt)
    .fetch_one(pool)
    .await
    .context("reading the call that holds an identity")?;
    Ok(Claimed::Existing(Box::new(decode(&row)?)))
}

/// The started, unended call an earlier attempt of the same call task and instance holds.
pub(crate) async fn live_earlier(
    pool: &PgPool,
    run: &str,
    task: &str,
    instance: Option<&str>,
    attempt: u32,
) -> Result<Option<CallRecord>> {
    let attempt = i32::try_from(attempt).context("a call attempt past i32")?;
    let row = sqlx::query(const_format::formatcp!(
        "SELECT {COLUMNS} FROM call_target_runs \
         WHERE parent_run = $1 AND task = $2 AND COALESCE(instance, '') = COALESCE($3, '') \
         AND attempt < $4 AND handle IS NOT NULL AND status IN ('pending', 'running') \
         ORDER BY attempt DESC LIMIT 1"
    ))
    .bind(run)
    .bind(task)
    .bind(instance)
    .bind(attempt)
    .fetch_optional(pool)
    .await
    .context("reading a live call of an earlier attempt")?;
    row.as_ref().map(decode).transpose()
}

/// Record what the dispatch of a claimed call started. `true` when the call ended while the
/// dispatch ran, or its calling run was cancelled, so the target run must be stopped.
pub(crate) async fn dispatched(pool: &PgPool, id: &CallId, started: &Dispatched) -> Result<bool> {
    let stop: Option<bool> = sqlx::query_scalar(
        "UPDATE call_target_runs c SET handle = $2, link = $3, detail = $4, updated_at = $5 \
         WHERE id = $1 RETURNING c.ended_at IS NOT NULL OR EXISTS ( \
           SELECT 1 FROM runs r WHERE r.run_id = c.parent_run AND r.status = 'cancelled')",
    )
    .bind(&id.0)
    .bind(serde_json::to_value(&started.handle)?)
    .bind(
        started
            .link
            .as_ref()
            .map(serde_json::to_value)
            .transpose()?,
    )
    .bind(&started.detail)
    .bind(crate::clock::now_rfc3339())
    .fetch_optional(pool)
    .await
    .context("recording a call's dispatch")?;
    stop.with_context(|| format!("call {id} holds no claim to record a dispatch on"))
}

/// Give back a claimed identity whose dispatch started nothing.
pub(crate) async fn release(pool: &PgPool, id: &CallId) -> Result<()> {
    sqlx::query("DELETE FROM call_target_runs WHERE id = $1 AND handle IS NULL")
        .bind(&id.0)
        .execute(pool)
        .await
        .context("releasing a call identity")?;
    Ok(())
}

/// One call of `run`.
pub(crate) async fn get(pool: &PgPool, run: &str, id: &str) -> Result<Option<CallRecord>> {
    let row = sqlx::query(const_format::formatcp!(
        "SELECT {COLUMNS} FROM call_target_runs WHERE id = $1 AND parent_run = $2"
    ))
    .bind(id)
    .bind(run)
    .fetch_optional(pool)
    .await
    .context("reading a call")?;
    row.as_ref().map(decode).transpose()
}

/// Every dispatched call of `run`, oldest first.
pub(crate) async fn list(pool: &PgPool, run: &str) -> Result<Vec<CallRecord>> {
    let rows = sqlx::query(const_format::formatcp!(
        "SELECT {COLUMNS} FROM call_target_runs \
         WHERE parent_run = $1 AND handle IS NOT NULL ORDER BY created_at, id"
    ))
    .bind(run)
    .fetch_all(pool)
    .await
    .context("listing a run's calls")?;
    rows.iter().map(decode).collect()
}

/// The calls not yet ended of every run of launch `key`, those still being dispatched included.
pub(crate) async fn open_for_launch(pool: &PgPool, key: &str) -> Result<Vec<CallRecord>> {
    let rows = sqlx::query(const_format::formatcp!(
        "SELECT {COLUMNS} FROM call_target_runs \
         WHERE ended_at IS NULL \
         AND parent_run IN (SELECT run_id FROM runs WHERE issue = $1)"
    ))
    .bind(key)
    .fetch_all(pool)
    .await
    .context("listing a launch's open calls")?;
    rows.iter().map(decode).collect()
}

/// Merge what a backend learned about a call's target run into its detail.
pub(crate) async fn noted(
    pool: &PgPool,
    id: &CallId,
    recorded: &serde_json::Map<String, Value>,
) -> Result<()> {
    sqlx::query(
        "UPDATE call_target_runs SET detail = COALESCE(detail, '{}'::jsonb) || $2 WHERE id = $1",
    )
    .bind(&id.0)
    .bind(Value::Object(recorded.clone()))
    .execute(pool)
    .await
    .context("recording what a call's target run showed")?;
    Ok(())
}

/// Mark a call looked at now, so the refresh loop leaves it until its next turn.
pub(crate) async fn touched(pool: &PgPool, id: &CallId) -> Result<()> {
    sqlx::query("UPDATE call_target_runs SET updated_at = $2 WHERE id = $1")
        .bind(&id.0)
        .bind(crate::clock::now_rfc3339())
        .execute(pool)
        .await
        .context("marking a call looked at")?;
    Ok(())
}

/// Record whether a call's target run still has to be stopped: its call ended, but asking the
/// target's system to stop it failed.
pub(crate) async fn owe_cancel(pool: &PgPool, id: &CallId, owed: bool) -> Result<()> {
    sqlx::query("UPDATE call_target_runs SET cancel_owed = $2, updated_at = $3 WHERE id = $1")
        .bind(&id.0)
        .bind(owed)
        .bind(crate::clock::now_rfc3339())
        .execute(pool)
        .await
        .context("recording a call's owed cancel")?;
    Ok(())
}

/// The calls the refresh loop has to look at: dispatched and unended, or owing a cancel.
const DUE: &str = "handle IS NOT NULL AND (ended_at IS NULL OR cancel_owed)";

/// When the earliest call the refresh loop has to look at was last looked at.
pub(crate) async fn earliest_due(pool: &PgPool) -> Result<Option<String>> {
    sqlx::query_scalar(const_format::formatcp!(
        "SELECT MIN(updated_at) FROM call_target_runs WHERE {DUE}"
    ))
    .fetch_one(pool)
    .await
    .context("reading when the next call is due")
}

/// The calls the refresh loop has to look at that were last looked at no later than `before`,
/// longest waiting first.
pub(crate) async fn due(
    pool: &PgPool,
    before: &str,
    limit: i64,
) -> Result<Vec<(CallRecord, bool)>> {
    let rows = sqlx::query(const_format::formatcp!(
        "SELECT {COLUMNS}, cancel_owed FROM call_target_runs \
         WHERE {DUE} AND updated_at <= $1 ORDER BY updated_at, id LIMIT $2"
    ))
    .bind(before)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("listing the calls due a refresh")?;
    rows.iter()
        .map(|row| Ok((decode(row)?, row.try_get("cancel_owed")?)))
        .collect()
}

/// Keep the bytes of one result file of a call's target run.
pub(crate) async fn put_result_file(
    pool: &PgPool,
    id: &CallId,
    file: &FileRef,
    bytes: &[u8],
) -> Result<()> {
    let path = file
        .path
        .as_deref()
        .with_context(|| format!("a result file of call {id} has no path"))?;
    let size = i64::try_from(bytes.len()).context("a result file past i64")?;
    sqlx::query(
        "INSERT INTO call_result_files (call_id, path, digest, size, bytes, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (call_id, path) DO UPDATE SET digest = EXCLUDED.digest, \
         size = EXCLUDED.size, bytes = EXCLUDED.bytes",
    )
    .bind(&id.0)
    .bind(path)
    .bind(&file.digest)
    .bind(size)
    .bind(bytes)
    .bind(crate::clock::now_rfc3339())
    .execute(pool)
    .await
    .context("storing a call result file")?;
    Ok(())
}

/// The bytes of a call's result file at `path` with `digest`.
pub(crate) async fn result_file(
    pool: &PgPool,
    id: &CallId,
    path: &str,
    digest: &str,
) -> Result<Option<Vec<u8>>> {
    sqlx::query_scalar(
        "SELECT bytes FROM call_result_files WHERE call_id = $1 AND path = $2 AND digest = $3",
    )
    .bind(&id.0)
    .bind(path)
    .bind(digest)
    .fetch_optional(pool)
    .await
    .context("reading a call result file")
}

/// Record a running call's latest progress.
pub(crate) async fn progressed(
    pool: &PgPool,
    id: &CallId,
    status: CallStatus,
    progress: &ProgressGraph,
) -> Result<()> {
    sqlx::query(
        "UPDATE call_target_runs SET status = $2, progress = $3, updated_at = $4 \
         WHERE id = $1 AND ended_at IS NULL",
    )
    .bind(&id.0)
    .bind(status_str(status))
    .bind(serde_json::to_value(progress)?)
    .bind(crate::clock::now_rfc3339())
    .execute(pool)
    .await
    .context("recording a call's progress")?;
    Ok(())
}

/// Settle a call. A call already settled keeps its first settlement; returns whether this one
/// landed.
pub(crate) async fn settle(
    pool: &PgPool,
    id: &CallId,
    outcome: &Outcome,
    progress: &ProgressGraph,
) -> Result<bool> {
    let now = crate::clock::now_rfc3339();
    let done = sqlx::query(
        "UPDATE call_target_runs SET status = $2, note = $3, result = $4, result_files = $5, \
         progress = $6, updated_at = $7, ended_at = $7 WHERE id = $1 AND ended_at IS NULL",
    )
    .bind(&id.0)
    .bind(status_str(outcome.status))
    .bind(&outcome.note)
    .bind(&outcome.result)
    .bind(serde_json::to_value(&outcome.result_files)?)
    .bind(serde_json::to_value(progress)?)
    .bind(&now)
    .execute(pool)
    .await
    .context("settling a call")?;
    Ok(done.rows_affected() == 1)
}

/// The most bytes of call input files one run may upload.
pub(crate) const MAX_RUN_INPUT_BYTES: i64 = 64 * 1024 * 1024;

/// Store the bytes of a file `run` binds, addressed by digest. `Ok(false)` when they would take
/// the run's uploads past [`MAX_RUN_INPUT_BYTES`].
pub(crate) async fn put_input_file(
    pool: &PgPool,
    run: &str,
    digest: &str,
    bytes: &[u8],
) -> Result<bool> {
    let size = i64::try_from(bytes.len()).context("an input file past i64")?;
    let stored = sqlx::query(
        "INSERT INTO call_input_files (run_id, digest, size, bytes, created_at) \
         SELECT $1, $2, $3, $4, $5 \
         WHERE EXISTS (SELECT 1 FROM call_input_files WHERE run_id = $1 AND digest = $2) \
            OR (SELECT COALESCE(SUM(size), 0) FROM call_input_files WHERE run_id = $1) + $3 <= $6 \
         ON CONFLICT (run_id, digest) DO NOTHING",
    )
    .bind(run)
    .bind(digest)
    .bind(size)
    .bind(bytes)
    .bind(crate::clock::now_rfc3339())
    .bind(MAX_RUN_INPUT_BYTES)
    .execute(pool)
    .await
    .context("storing a call input file")?;
    if stored.rows_affected() == 1 {
        return Ok(true);
    }
    input_file_size(pool, run, digest)
        .await
        .map(|s| s.is_some())
}

/// The bytes of the file of `digest` that `run` uploaded.
pub(crate) async fn input_file(pool: &PgPool, run: &str, digest: &str) -> Result<Option<Vec<u8>>> {
    sqlx::query_scalar("SELECT bytes FROM call_input_files WHERE run_id = $1 AND digest = $2")
        .bind(run)
        .bind(digest)
        .fetch_optional(pool)
        .await
        .context("reading a call input file")
}

/// The size of the file of `digest` that `run` uploaded; `None` when it uploaded none.
pub(crate) async fn input_file_size(pool: &PgPool, run: &str, digest: &str) -> Result<Option<u64>> {
    let size: Option<i64> =
        sqlx::query_scalar("SELECT size FROM call_input_files WHERE run_id = $1 AND digest = $2")
            .bind(run)
            .bind(digest)
            .fetch_optional(pool)
            .await
            .context("finding a call input file")?;
    size.map(|s| u64::try_from(s).context("a stored size is negative"))
        .transpose()
}

/// The run `run_id` as a caller: its launch and that launch's owner and depth. `None` when the run
/// belongs to no playbook launch.
pub(crate) async fn parent(pool: &PgPool, run_id: &str) -> Result<Option<Parent>> {
    let row = sqlx::query(
        "SELECT l.key, l.owner, l.launcher_groups, l.created_by, l.call_depth, l.draft_version \
         FROM runs r JOIN playbook_launches l ON l.key = r.issue WHERE r.run_id = $1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await
    .context("reading a calling run's launch")?;
    let Some(row) = row else {
        return Ok(None);
    };
    let depth: i32 = row.try_get("call_depth")?;
    Ok(Some(Parent {
        run: run_id.to_string(),
        launch: row.try_get("key")?,
        owner: row.try_get("owner")?,
        groups: row.try_get("launcher_groups")?,
        created_by: row.try_get("created_by")?,
        depth: u32::try_from(depth).context("a stored call depth is negative")?,
        draft: row.try_get::<Option<i64>, _>("draft_version")?.is_some(),
    }))
}

/// The call a launch is the target run of.
pub(crate) struct ChildLink<'a> {
    pub parent: &'a Parent,
    pub task: &'a str,
    pub instance: Option<&'a str>,
    /// The run id the launch's first dispatch takes, which the call's handle names.
    pub reserved_run: &'a str,
}

/// Mark launch `key` as the target run of `link`'s call.
pub(crate) async fn link_child(
    ex: impl PgExecutor<'_>,
    key: &str,
    link: &ChildLink<'_>,
) -> Result<()> {
    let depth =
        i32::try_from(link.parent.depth.saturating_add(1)).context("a call depth past i32")?;
    sqlx::query(
        "UPDATE playbook_launches SET parent_run = $2, parent_task = $3, parent_instance = $4, \
         call_depth = $5, reserved_run = $6 WHERE key = $1",
    )
    .bind(key)
    .bind(&link.parent.run)
    .bind(link.task)
    .bind(link.instance)
    .bind(depth)
    .bind(link.reserved_run)
    .execute(ex)
    .await
    .context("linking a call's launch to its caller")?;
    Ok(())
}

/// A playbook a run of a calling chain runs: a registered playbook's id, or a draft's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Chained {
    pub playbook: String,
    pub draft: bool,
}

/// The playbooks of run `run` and every run above it in its chain of calling runs, the run's own
/// first.
pub(crate) async fn chain(pool: &PgPool, run: &str) -> Result<Vec<Chained>> {
    let rows: Vec<(String, bool)> = sqlx::query_as(
        "WITH RECURSIVE chain (playbook, draft, parent, depth) AS ( \
           SELECT l.playbook, l.draft_version IS NOT NULL, l.parent_run, 0 \
           FROM runs r JOIN playbook_launches l ON l.key = r.issue WHERE r.run_id = $1 \
           UNION ALL \
           SELECT l.playbook, l.draft_version IS NOT NULL, l.parent_run, c.depth + 1 \
           FROM chain c JOIN runs r ON r.run_id = c.parent \
           JOIN playbook_launches l ON l.key = r.issue WHERE c.depth < $2 \
         ) SELECT playbook, draft FROM chain ORDER BY depth",
    )
    .bind(run)
    .bind(i32::try_from(crucible_contract::calls::MAX_CALL_DEPTH).context("depth past i32")?)
    .fetch_all(pool)
    .await
    .context("reading a run's chain of calling runs")?;
    Ok(rows
        .into_iter()
        .map(|(playbook, draft)| Chained { playbook, draft })
        .collect())
}

/// The run id launch `key` reserved for its first dispatch, while no run holds it yet.
pub(crate) async fn reserved_run(pool: &PgPool, key: &str) -> Result<Option<String>> {
    sqlx::query_scalar(
        "SELECT l.reserved_run FROM playbook_launches l WHERE l.key = $1 \
         AND l.reserved_run IS NOT NULL \
         AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.run_id = l.reserved_run)",
    )
    .bind(key)
    .fetch_optional(pool)
    .await
    .map(Option::flatten)
    .context("reading a launch's reserved run")
}

/// Where a run was called from: the calling run, call task and instance its launch records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CalledFrom {
    pub run: String,
    pub task: String,
    pub instance: Option<String>,
}

pub(crate) async fn called_from(pool: &PgPool, run_id: &str) -> Result<Option<CalledFrom>> {
    let row: Option<(Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT l.parent_run, l.parent_task, l.parent_instance \
         FROM runs r JOIN playbook_launches l ON l.key = r.issue WHERE r.run_id = $1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await
    .context("reading where a run was called from")?;
    Ok(match row {
        Some((Some(run), Some(task), instance)) => Some(CalledFrom {
            run,
            task,
            instance,
        }),
        _ => None,
    })
}

/// Where each of the launches `keys` was called from, for those a call made.
pub(crate) async fn called_by(
    pool: &PgPool,
    keys: &[String],
) -> Result<std::collections::BTreeMap<String, CalledBy>> {
    let rows: Vec<(String, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT l.key, l.parent_run, p.issue, l.parent_task, l.parent_instance \
         FROM playbook_launches l JOIN runs p ON p.run_id = l.parent_run \
         WHERE l.key = ANY($1) AND l.parent_task IS NOT NULL AND p.issue IS NOT NULL",
    )
    .bind(keys)
    .fetch_all(pool)
    .await
    .context("reading where launches were called from")?;
    Ok(rows
        .into_iter()
        .map(|(key, run, launch, task, instance)| {
            (
                key,
                CalledBy {
                    run,
                    launch,
                    task,
                    instance,
                },
            )
        })
        .collect())
}

/// The calling run, its launch, the call task and the instance a launch was dispatched by.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub(crate) struct CalledBy {
    pub run: String,
    pub launch: String,
    pub task: String,
    pub instance: Option<String>,
}

/// A target run a run's call dispatched.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Child {
    pub call: CallId,
    pub task: String,
    pub instance: Option<String>,
    pub attempt: u32,
    pub alias: CallAlias,
    pub handle: TargetHandle,
    pub status: CallStatus,
}

pub(crate) async fn children(pool: &PgPool, run_id: &str) -> Result<Vec<Child>> {
    list(pool, run_id).await.map(|calls| {
        calls
            .into_iter()
            .filter_map(|c| {
                Some(Child {
                    handle: c.handle?,
                    call: c.id,
                    task: c.task,
                    instance: c.instance,
                    attempt: c.attempt,
                    alias: c.alias,
                    status: c.status,
                })
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use crate::calls::store::*;

    #[test]
    fn every_status_round_trips_through_its_column() {
        for status in [
            CallStatus::Pending,
            CallStatus::Running,
            CallStatus::Passed,
            CallStatus::Failed,
            CallStatus::Cancelled,
        ] {
            assert_eq!(parse_status(status_str(status)).ok(), Some(status));
            assert_eq!(
                serde_json::to_value(status).ok(),
                Some(Value::String(status_str(status).to_string())),
                "the column spells a status as the wire does"
            );
        }
        assert!(parse_status("queued").is_err());
        assert_eq!(parse_kind("playbook").ok(), Some(CallKind::Playbook));
        assert!(parse_kind("gitlab").is_err());
    }
}
