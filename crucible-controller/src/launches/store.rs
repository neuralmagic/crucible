//! Raw SQL over playbook launches, their runs and the emission ledger.
//!
//! The tracker-emission idempotency ledger (migration 0025): one row per emitted tracker issue,
//! keyed (issue_key, artifact). Lookup-before-create is the whole contract — see
//! [`crate::launches::emission`].

#![allow(clippy::disallowed_macros)]

use crate::launches::model::{NewPlaybookLaunch, PlaybookLaunch, PlaybookRun};
use crate::model::{LaunchOrigin, Status};
use crate::playbooks::exposure::Exposure;
use crate::runs::model::Run;
use crate::runs::store::RUN_COLS;
use anyhow::{Context, Result};
use crucible_contract::Tier;
use sqlx::{PgExecutor, PgPool};
use std::collections::BTreeMap;

/// Mint one playbook launch: the `issues` row, the `playbook_launches` row carrying the validated
/// values and the launcher's ceilings, and the launch's own copy of the registered pack, in one
/// transaction. The copy is what makes a launch immutable against a later re-pin of the registry
/// row, and what lets the dispatch materialize the pack through the ordinary
/// [`crate::playbooks::packs::materialize_pack`] path.
///
/// The endpoint has already validated the values against the stored schema and bounded the
/// ceilings; this is the only way a launch row is written, so the schedule sweep and the deferred
/// one-shots land the same row through the same function. `Ok(false)` when the registry id is
/// unknown (nothing is written).
///
/// A draft one-shot records its exposure on the launch row itself. A registered launch passes
/// [`Extraction::Absent`](crate::playbooks::exposure::Extraction::Absent): its disclosure is the registry
/// row's, not a per-launch copy.
#[tracing::instrument(name = "db.insert_playbook_launch_with", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub(crate) async fn insert_playbook_launch_with(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    key: &str,
    launch: &NewPlaybookLaunch<'_>,
    exposure: &crate::playbooks::exposure::Extraction,
) -> Result<bool> {
    let (exposure_json, exposure_digest) = exposure.stored()?;
    let now = crate::clock::now_rfc3339();
    let tier = Tier::T1.as_str();
    let max_time = launch.max_time.as_str();
    let origin = launch.origin.as_str();
    sqlx::query!(
        "INSERT INTO issues (key, repo, tier, status, priority, input_kind, title, updated_at) \
         VALUES ($1, $2, $3, 'new', 0, 'playbook', $4, $5)",
        key,
        launch.repo,
        tier,
        launch.title,
        now,
    )
    .execute(&mut **tx)
    .await
    .context("insert_playbook_launch: insert issue")?;
    sqlx::query!(
        "INSERT INTO playbook_launches (key, playbook, params, schema_digest, max_cost, max_time, \
         advance_dedupe, dedupe_schedule, origin, draft_version, created_by, launcher_groups, \
         created_at, exposure, exposure_digest) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
        key,
        launch.playbook,
        launch.params,
        launch.schema_digest,
        launch.max_cost,
        max_time,
        launch.advance_dedupe,
        launch.dedupe_schedule,
        origin,
        launch.draft_version,
        launch.created_by,
        launch.launcher_groups,
        now,
        exposure_json,
        exposure_digest,
    )
    .execute(&mut **tx)
    .await
    .context("insert_playbook_launch: insert launch")?;
    let slug = crate::model::sanitize_key(key);
    match launch.draft_version {
        Some(version) => {
            crate::playbooks::drafts::copy_draft_pack_to(&mut **tx, launch.playbook, version, &slug)
                .await
        }
        None => crate::playbooks::registry::copy_pack_to(&mut **tx, launch.playbook, &slug).await,
    }
}

/// Read back what a launch was authorized to run with. `None` when the key names no launch — a
/// row whose launch went missing parks rather than dispatching on guessed values.
#[tracing::instrument(name = "db.get_playbook_launch", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub(crate) async fn get_playbook_launch(
    pool: &PgPool,
    key: &str,
) -> Result<Option<PlaybookLaunch>> {
    let row = sqlx::query!(
        "SELECT playbook, params, schema_digest, max_cost, max_time, created_by, \
         launcher_groups, draft_version, exposure FROM playbook_launches WHERE key = $1",
        key,
    )
    .fetch_optional(pool)
    .await
    .context("get_playbook_launch")?;
    let Some(row) = row else {
        return Ok(None);
    };
    let max_time = crate::model::MaxTime::parse(&row.max_time)
        .map_err(|e| anyhow::anyhow!("stored max_time for {key}: {e}"))?;
    Ok(Some(PlaybookLaunch {
        playbook: row.playbook,
        params: decode_params(&row.params),
        schema_digest: row.schema_digest,
        max_cost: row.max_cost,
        max_time,
        created_by: row.created_by,
        launcher_groups: decode_groups(row.launcher_groups.as_ref()),
        draft_version: row.draft_version,
        exposure: row
            .exposure
            .map(crate::playbooks::exposure::Exposure::from_value)
            .transpose()?,
    }))
}

/// The stored group snapshot as plain strings. A row written before the snapshot existed, or one
/// holding anything but an array of strings, reads back as no groups — which refuses a
/// group-owned secret rather than guessing at a membership.
fn decode_groups(groups: Option<&serde_json::Value>) -> Vec<String> {
    groups
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|g| g.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The stored `{name: value}` object as the string map validation takes. A non-string value
/// (nothing any launch surface writes) carries as its JSON so it is refused by name rather than
/// silently dropped.
pub(crate) fn stored_params(params: &serde_json::Value) -> BTreeMap<String, String> {
    params
        .as_object()
        .map(|map| {
            map.iter()
                .map(|(k, v)| {
                    let value = match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    (k.clone(), value)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The stored `{name: value}` object as ordered pairs. Sorted by name so a launch always renders
/// the same argv; a non-string value (nothing the launch endpoint writes) renders as its JSON.
fn decode_params(params: &serde_json::Value) -> Vec<(String, String)> {
    stored_params(params).into_iter().collect()
}

/// The runs surface: launches with the authorization they froze, what the run did, and what it
/// cost. `key = Some(..)` reads one launch of any origin (a relaunch prefills from it); `None`
/// lists the ad-hoc ones, since a schedule's firings belong to the schedules surface.
///
/// A launch's runs are the `runs` rows carrying its key as `issue`.
#[tracing::instrument(name = "db.list_playbook_runs", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn list_playbook_runs(
    pool: &PgPool,
    key: Option<&str>,
    limit: i64,
) -> Result<Vec<PlaybookRun>> {
    let rows = sqlx::query!(
        r#"
        SELECT pl.key AS "key!", pl.playbook AS "playbook!", p.description AS "description?",
               pl.params AS "params!", pl.schema_digest AS "schema_digest!",
               p.schema_digest AS "current_schema_digest?", pl.max_cost AS "max_cost!",
               pl.max_time AS "max_time!", pl.advance_dedupe AS "advance_dedupe!",
               pl.origin AS "origin!", pl.draft_version, pl.dedupe_schedule,
               i.status AS "status!", i.parked_reason, i.agent_provider, i.agent_model,
               c.cost_usd AS "cost_usd?", c.runs AS "runs!: i64",
               c.transport_losses AS "transport_losses!: i64",
               pl.created_by, pl.created_at AS "created_at!"
        FROM playbook_launches pl
        JOIN issues i ON i.key = pl.key
        LEFT JOIN playbooks p ON p.id = pl.playbook
        LEFT JOIN LATERAL (
            SELECT SUM(r.cost_usd) AS cost_usd, COUNT(*) AS runs,
                   COALESCE(SUM((SELECT COUNT(*) FROM run_task_results t
                                 WHERE t.run_id = r.run_id AND t.status = 'transport')), 0)::bigint
                       AS transport_losses
            FROM runs r
            WHERE r.issue = pl.key
        ) c ON TRUE
        WHERE ($1::text IS NULL AND pl.origin <> 'schedule') OR pl.key = $1
        ORDER BY pl.created_at DESC, pl.key DESC
        LIMIT $2
        "#,
        key,
        limit,
    )
    .fetch_all(pool)
    .await
    .context("list_playbook_runs")?;
    rows.into_iter()
        .map(|r| {
            Ok(PlaybookRun {
                key: r.key,
                playbook: r.playbook,
                description: r.description,
                params: r.params,
                schema_digest: r.schema_digest,
                current_schema_digest: r.current_schema_digest,
                max_cost: r.max_cost,
                max_time: r.max_time,
                advance_dedupe: r.advance_dedupe,
                origin: LaunchOrigin::parse(&r.origin)?,
                draft_version: r.draft_version,
                schedule: r.dedupe_schedule,
                status: Status::parse(&r.status)?,
                parked_reason: r.parked_reason,
                agent_provider: r.agent_provider,
                agent_model: r.agent_model,
                cost_usd: r.cost_usd,
                runs: r.runs,
                transport_losses: r.transport_losses,
                created_by: r.created_by,
                created_at: r.created_at,
            })
        })
        .collect()
}

/// Every run one launch dispatched, newest first.
#[tracing::instrument(name = "db.list_runs_for_launch", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub(crate) async fn list_runs_for_launch(ex: impl PgExecutor<'_>, key: &str) -> Result<Vec<Run>> {
    let sql = format!("SELECT {RUN_COLS} FROM runs WHERE issue = $1 ORDER BY run_id DESC");
    sqlx::query_as::<_, crate::runs::model::Run>(&sql)
        .bind(key)
        .fetch_all(ex)
        .await
        .context("list_runs_for_launch")
}

/// The tracker-local id previously emitted for this artifact, or `None` if never emitted.
#[tracing::instrument(name = "db.emission_for", skip(ex), fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn emission_for(
    ex: impl PgExecutor<'_>,
    issue_key: &str,
    artifact: &str,
) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT tracker_issue_id FROM emissions WHERE issue_key = $1 AND artifact = $2",
    )
    .bind(issue_key)
    .bind(artifact)
    .fetch_optional(ex)
    .await?;
    Ok(row.map(|(id,)| id))
}

/// Record an emitted tracker issue. A duplicate (issue_key, artifact) is an error — the caller
/// must have checked [`emission_for`] first; failing loud here beats silently shadowing a filed
/// issue.
#[tracing::instrument(name = "db.record_emission", skip(ex), fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn record_emission(
    ex: impl PgExecutor<'_>,
    issue_key: &str,
    artifact: &str,
    tracker_issue_id: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO emissions (issue_key, artifact, tracker_issue_id) VALUES ($1, $2, $3)",
    )
    .bind(issue_key)
    .bind(artifact)
    .bind(tracker_issue_id)
    .execute(ex)
    .await?;
    Ok(())
}

/// What adopting a registered playbook launch did, decided under a share lock on the registry row.
pub(crate) enum AdoptPlaybookOutcome {
    Adopted,
    UnknownPlaybook,
    /// The registry row's schema moved between the endpoint's validation and this transaction;
    /// the values were validated against a schema the pack no longer declares.
    SchemaDrifted {
        current: String,
    },
}

/// Mint one draft launch. The registry foreign key cannot cover a draft, so this is where the
/// integrity lives: the draft version the endpoint authorized against is re-read `FOR SHARE`, and
/// a save that landed in between is the same drift refusal the registered path answers with.
#[tracing::instrument(name = "db.adopt_draft_launch", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub(crate) async fn adopt_draft_launch(
    pool: &PgPool,
    key: &str,
    launch: &NewPlaybookLaunch<'_>,
    exposure: &crate::playbooks::exposure::Extraction,
) -> Result<AdoptPlaybookOutcome> {
    let version = launch
        .draft_version
        .context("adopt_draft_launch: a draft launch names its version")?;
    let mut tx = pool.begin().await.context("adopt_draft_launch: begin")?;
    let current = sqlx::query_scalar!(
        "SELECT schema_digest FROM playbook_draft_versions WHERE draft_id = $1 AND version = $2          FOR SHARE",
        launch.playbook,
        version,
    )
    .fetch_optional(&mut *tx)
    .await
    .context("adopt_draft_launch: re-read the draft version")?;
    let newest = sqlx::query_scalar!(
        "SELECT max(version) FROM playbook_draft_versions WHERE draft_id = $1",
        launch.playbook,
    )
    .fetch_one(&mut *tx)
    .await
    .context("adopt_draft_launch: re-read the newest version")?;
    let outcome = match current {
        None => AdoptPlaybookOutcome::UnknownPlaybook,
        Some(stored) if stored.as_deref() != Some(launch.schema_digest) => {
            AdoptPlaybookOutcome::SchemaDrifted {
                current: stored.unwrap_or_default(),
            }
        }
        Some(_) if newest != Some(version) => AdoptPlaybookOutcome::SchemaDrifted {
            current: format!("version {}", newest.unwrap_or_default()),
        },
        Some(_) => {
            if insert_playbook_launch_with(&mut tx, key, launch, exposure).await? {
                tx.commit().await.context("adopt_draft_launch: commit")?;
                return Ok(AdoptPlaybookOutcome::Adopted);
            }
            AdoptPlaybookOutcome::UnknownPlaybook
        }
    };
    tx.rollback()
        .await
        .context("adopt_draft_launch: rollback")?;
    Ok(outcome)
}

#[tracing::instrument(name = "db.adopt_playbook_launch", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", key = %key), err)]
pub(crate) async fn adopt_playbook_launch(
    pool: &PgPool,
    key: &str,
    launch: &NewPlaybookLaunch<'_>,
) -> Result<AdoptPlaybookOutcome> {
    let mut tx = pool.begin().await.context("adopt_playbook_launch: begin")?;
    let current = sqlx::query_scalar!(
        "SELECT schema_digest FROM playbooks WHERE id = $1 FOR SHARE",
        launch.playbook,
    )
    .fetch_optional(&mut *tx)
    .await
    .context("adopt_playbook_launch: re-read the schema digest")?;
    let outcome = match current {
        None => AdoptPlaybookOutcome::UnknownPlaybook,
        Some(current) if current != launch.schema_digest => {
            AdoptPlaybookOutcome::SchemaDrifted { current }
        }
        Some(_) => {
            if insert_playbook_launch_with(
                &mut tx,
                key,
                launch,
                &crate::playbooks::exposure::Extraction::Absent,
            )
            .await?
            {
                tx.commit().await.context("adopt_playbook_launch: commit")?;
                return Ok(AdoptPlaybookOutcome::Adopted);
            }
            AdoptPlaybookOutcome::UnknownPlaybook
        }
    };
    tx.rollback()
        .await
        .context("adopt_playbook_launch: rollback")?;
    Ok(outcome)
}

/// The exposure a run of `issue_key` executes under: the launch's own recomputed disclosure (a
/// draft one-shot), else the registry revision's, else the frozen scope pack's. `None` is
/// absent-legacy or an issue with neither a launch nor a scope.
pub async fn exposure_for_issue(pool: &sqlx::PgPool, issue_key: &str) -> Result<Option<Exposure>> {
    if let Some(launch) = get_playbook_launch(pool, issue_key).await? {
        if launch.exposure.is_some() {
            return Ok(launch.exposure);
        }
        return Ok(
            crate::playbooks::exposure::registered(pool, &launch.playbook)
                .await?
                .flatten(),
        );
    }
    Ok(
        crate::issues::store::latest_scope_for_issue(pool, issue_key)
            .await?
            .and_then(|s| s.exposure),
    )
}
#[cfg(test)]
mod tests {
    use super::*;

    use anyhow::Result;
    use sqlx::PgPool;

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn ledger_round_trips_and_rejects_duplicates(pool: PgPool) -> Result<()> {
        assert_eq!(emission_for(&pool, "k", "epic:sha").await?, None);
        record_emission(&pool, "k", "epic:sha", "PROJ-1").await?;
        assert_eq!(
            emission_for(&pool, "k", "epic:sha").await?.as_deref(),
            Some("PROJ-1")
        );
        // Same artifact, different issue key: independent.
        assert_eq!(emission_for(&pool, "other", "epic:sha").await?, None);
        assert!(
            record_emission(&pool, "k", "epic:sha", "PROJ-2")
                .await
                .is_err(),
            "double-record must fail loud"
        );
        Ok(())
    }
}
