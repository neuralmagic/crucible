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
         target, interface, bound, budget, status, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'pending', $12, $12) \
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

/// Record what the dispatch of a claimed call started.
pub(crate) async fn dispatched(pool: &PgPool, id: &CallId, started: &Dispatched) -> Result<()> {
    sqlx::query(
        "UPDATE call_target_runs SET handle = $2, link = $3, detail = $4, updated_at = $5 \
         WHERE id = $1",
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
    .execute(pool)
    .await
    .context("recording a call's dispatch")?;
    Ok(())
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

/// The dispatched calls not yet ended of every run of launch `key`.
pub(crate) async fn open_for_launch(pool: &PgPool, key: &str) -> Result<Vec<CallRecord>> {
    let rows = sqlx::query(const_format::formatcp!(
        "SELECT {COLUMNS} FROM call_target_runs \
         WHERE ended_at IS NULL AND handle IS NOT NULL \
         AND parent_run IN (SELECT run_id FROM runs WHERE issue = $1)"
    ))
    .bind(key)
    .fetch_all(pool)
    .await
    .context("listing a launch's open calls")?;
    rows.iter().map(decode).collect()
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

/// Store the bytes of a file `run` binds, addressed by digest.
pub(crate) async fn put_input_file(
    pool: &PgPool,
    run: &str,
    digest: &str,
    bytes: &[u8],
) -> Result<()> {
    sqlx::query(
        "INSERT INTO call_input_files (run_id, digest, size, bytes, created_at) \
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (run_id, digest) DO NOTHING",
    )
    .bind(run)
    .bind(digest)
    .bind(i64::try_from(bytes.len()).context("an input file past i64")?)
    .bind(bytes)
    .bind(crate::clock::now_rfc3339())
    .execute(pool)
    .await
    .context("storing a call input file")?;
    Ok(())
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

/// Whether `run` uploaded a file of `digest`.
pub(crate) async fn has_input_file(pool: &PgPool, run: &str, digest: &str) -> Result<bool> {
    let found: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM call_input_files WHERE run_id = $1 AND digest = $2")
            .bind(run)
            .bind(digest)
            .fetch_optional(pool)
            .await
            .context("finding a call input file")?;
    Ok(found.is_some())
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

/// Mark launch `key` as the target run of a call, and name the run its first dispatch takes.
pub(crate) async fn link_child(
    ex: impl PgExecutor<'_>,
    key: &str,
    parent: &Parent,
    task: &str,
    instance: Option<&str>,
    reserved_run: &str,
) -> Result<()> {
    let depth = i32::try_from(parent.depth.saturating_add(1)).context("a call depth past i32")?;
    sqlx::query(
        "UPDATE playbook_launches SET parent_run = $2, parent_task = $3, parent_instance = $4, \
         call_depth = $5, reserved_run = $6 WHERE key = $1",
    )
    .bind(key)
    .bind(&parent.run)
    .bind(task)
    .bind(instance)
    .bind(depth)
    .bind(reserved_run)
    .execute(ex)
    .await
    .context("linking a call's launch to its caller")?;
    Ok(())
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
