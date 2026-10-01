//! The run task-graph index (migration 0027): the admitted plan(s) a run executed and each task
//! attempt's terminal status. Written by the pull-ingest while folding a session log, read by
//! `GET /api/runs/{run_id}/graph`.
//!
//! Both writers are upserts because a re-ingest replays the same events: folding a log twice must
//! land the same rows, not a PK violation.

use crate::runs::model::{RunPlan, TaskResult};
use anyhow::{Context, Result};
use sqlx::PgExecutor;

/// Record one admitted plan version. `graph_json` is the `plan_admitted` event's `tasks` array.
#[tracing::instrument(name = "db.upsert_run_plan", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", run_id = %run_id), err)]
pub(crate) async fn upsert_run_plan(
    ex: impl PgExecutor<'_>,
    run_id: &str,
    plan_version: i64,
    graph_json: &str,
) -> Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO run_plans (run_id, plan_version, graph_json) VALUES ($1, $2, $3)
        ON CONFLICT (run_id, plan_version) DO UPDATE SET graph_json = excluded.graph_json
        "#,
        run_id,
        plan_version,
        graph_json,
    )
    .execute(ex)
    .await
    .context("upsert_run_plan")?;
    Ok(())
}

/// Record one task attempt's terminal status, last write wins per `(run_id, iter, task)`.
#[tracing::instrument(name = "db.upsert_task_result", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", run_id = %run_id), err)]
pub(crate) async fn upsert_task_result(
    ex: impl PgExecutor<'_>,
    run_id: &str,
    result: &TaskResult,
) -> Result<()> {
    let blocked = result
        .blocked
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .context("encoding the blocked reason")?;
    let links = serde_json::to_value(&result.links).context("encoding the reported links")?;
    sqlx::query!(
        r#"
        INSERT INTO run_task_results (run_id, iter, task, status, note, cost_usd, secs, blocked, links)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (run_id, iter, task) DO UPDATE SET
            status = excluded.status,
            note = excluded.note,
            cost_usd = excluded.cost_usd,
            secs = excluded.secs,
            blocked = excluded.blocked,
            links = excluded.links
        "#,
        run_id,
        result.iter,
        result.task,
        result.status,
        result.note,
        result.cost_usd,
        result.secs,
        blocked,
        links,
    )
    .execute(ex)
    .await
    .context("upsert_task_result")?;
    Ok(())
}

/// The run's newest admitted plan (a replan bumps `plan_version`), or `None` for a run that never
/// emitted one — every run logged before the work-graph executor existed.
#[tracing::instrument(name = "db.latest_run_plan", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", run_id = %run_id), err)]
pub(crate) async fn latest_run_plan(
    ex: impl PgExecutor<'_>,
    run_id: &str,
) -> Result<Option<RunPlan>> {
    let row = sqlx::query!(
        r#"
        SELECT plan_version AS "plan_version!: i64", graph_json AS "graph_json!"
        FROM run_plans WHERE run_id = $1 ORDER BY plan_version DESC LIMIT 1
        "#,
        run_id,
    )
    .fetch_optional(ex)
    .await
    .context("latest_run_plan")?;
    Ok(row.map(|r| RunPlan {
        plan_version: r.plan_version,
        graph_json: r.graph_json,
    }))
}

/// Every task attempt the run recorded, oldest iteration first.
#[tracing::instrument(name = "db.list_task_results", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", run_id = %run_id), err)]
pub(crate) async fn list_task_results(
    ex: impl PgExecutor<'_>,
    run_id: &str,
) -> Result<Vec<TaskResult>> {
    let rows = sqlx::query!(
        r#"
        SELECT iter AS "iter!: i64", task AS "task!", status AS "status!", note AS "note!",
               cost_usd, secs, blocked, links AS "links!"
        FROM run_task_results WHERE run_id = $1 ORDER BY iter, task
        "#,
        run_id,
    )
    .fetch_all(ex)
    .await
    .context("list_task_results")?;
    rows.into_iter()
        .map(|r| {
            let blocked = r
                .blocked
                .map(serde_json::from_value)
                .transpose()
                .with_context(|| format!("decoding the blocked reason of {}/{}", r.iter, r.task))?;
            // A stored link this build cannot read drops out of the list; failing here would
            // take the whole run's attempt history with it.
            let links = crucible_contract::decode_links(&r.links);
            Ok(TaskResult {
                iter: r.iter,
                task: r.task,
                status: r.status,
                note: r.note,
                cost_usd: r.cost_usd,
                secs: r.secs,
                blocked,
                links,
            })
        })
        .collect()
}

/// How many of the run's task attempts ended `transport`: lost to infrastructure, not a verdict.
#[tracing::instrument(name = "db.count_transport_losses", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", run_id = %run_id), err)]
pub(crate) async fn count_transport_losses(ex: impl PgExecutor<'_>, run_id: &str) -> Result<i64> {
    let row = sqlx::query!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM run_task_results WHERE run_id = $1 AND status = 'transport'"#,
        run_id,
    )
    .fetch_one(ex)
    .await
    .context("count_transport_losses")?;
    Ok(row.n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;

    fn result(iter: i64, task: &str, status: &str) -> TaskResult {
        TaskResult {
            iter,
            task: task.to_string(),
            status: status.to_string(),
            note: String::new(),
            cost_usd: Some(0.5),
            secs: Some(2.0),
            blocked: None,
            links: Vec::new(),
        }
    }

    fn link(url: &str) -> crucible_contract::ExternalLink {
        crucible_contract::ExternalLink::parse(url).expect("a link")
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn upserts_are_last_write_wins(pool: PgPool) -> Result<()> {
        upsert_run_plan(&pool, "run-1", 1, r#"[{"name":"a"}]"#).await?;
        upsert_run_plan(&pool, "run-1", 2, r#"[{"name":"b"}]"#).await?;
        // A re-ingest replays plan_version 2; the row must be replaced, not rejected.
        upsert_run_plan(&pool, "run-1", 2, r#"[{"name":"b2"}]"#).await?;
        let plan = latest_run_plan(&pool, "run-1").await?.expect("a plan");
        assert_eq!(plan.plan_version, 2, "newest version wins");
        assert_eq!(plan.graph_json, r#"[{"name":"b2"}]"#);
        assert_eq!(latest_run_plan(&pool, "other").await?, None);

        upsert_task_result(&pool, "run-1", &result(0, "measure", "fail")).await?;
        upsert_task_result(&pool, "run-1", &result(0, "measure", "pass")).await?;
        upsert_task_result(&pool, "run-1", &result(1, "propose", "pass")).await?;
        let rows = list_task_results(&pool, "run-1").await?;
        assert_eq!(rows.len(), 2, "the retried task collapsed onto one row");
        assert_eq!(rows[0].status, "pass");
        assert_eq!(rows[1].task, "propose");
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_blocked_reason_round_trips_and_transport_losses_count_final_attempts(
        pool: PgPool,
    ) -> Result<()> {
        let blocked = crucible_contract::TaskBlocked {
            reason: crucible_contract::BlockedReasonKind::RequiredTaskFailed,
            task: Some("brief".to_string()),
        };
        upsert_task_result(
            &pool,
            "run-2",
            &TaskResult {
                blocked: Some(blocked.clone()),
                note: "required task brief failed".to_string(),
                ..result(0, "deliver", "blocked")
            },
        )
        .await?;
        upsert_task_result(&pool, "run-2", &result(0, "brief", "transport")).await?;
        upsert_task_result(&pool, "run-2", &result(0, "scan", "transport")).await?;
        upsert_task_result(&pool, "run-2", &result(0, "scan", "pass")).await?;
        let rows = list_task_results(&pool, "run-2").await?;
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].task, "deliver");
        assert_eq!(rows[1].blocked, Some(blocked));
        assert_eq!(rows[0].blocked, None, "a transport row carries no block");
        assert_eq!(
            count_transport_losses(&pool, "run-2").await?,
            1,
            "scan's retry passed, so only brief counts"
        );
        assert_eq!(count_transport_losses(&pool, "run-none").await?, 0);
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn reported_links_round_trip_and_a_rerun_replaces_them(pool: PgPool) -> Result<()> {
        let links = vec![
            link("https://github.com/neuralmagic/crucible/pull/123"),
            link("https://github.com/neuralmagic/crucible/tree/topic"),
        ];
        upsert_task_result(
            &pool,
            "run-3",
            &TaskResult {
                links: links.clone(),
                ..result(0, "deliver", "pass")
            },
        )
        .await?;
        upsert_task_result(&pool, "run-3", &result(0, "brief", "pass")).await?;
        let rows = list_task_results(&pool, "run-3").await?;
        assert_eq!(rows[1].links, links);
        assert_eq!(rows[0].links, Vec::new(), "a task that reported none");

        upsert_task_result(
            &pool,
            "run-3",
            &TaskResult {
                links: vec![link("https://github.com/neuralmagic/crucible/pull/124")],
                ..result(0, "deliver", "pass")
            },
        )
        .await?;
        let rows = list_task_results(&pool, "run-3").await?;
        assert_eq!(rows[1].links.len(), 1, "the retry's links replaced them");
        assert_eq!(rows[1].links[0].label, "#124");
        Ok(())
    }
}
