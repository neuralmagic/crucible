//! A standing launch: the authorization to launch a playbook without a session, split from the
//! trigger that fires it. A schedule (a cron window) and a watch (a tracker query hit) are 1:1
//! sidecars on one `playbook_standing_launches` row, keyed by the same id. Everything a firing
//! needs that is not the trigger lives here: the adopted pack snapshot, the stored params, the
//! ceilings, the owner principal and group snapshot with its refresh state, the dispatch target,
//! the agent pin, the enabled flag, and the failure count.
//!
//! A trigger claims its own sidecar row, then hands this module the id, a per-firing param overlay
//! (a cursor value, an item identifier), and its [`LaunchOrigin`]; [`fire`] does the rest on the
//! trigger's transaction, so a claimed row can never end up without the launch it was claimed for.

use crate::client::Db;
use crate::daemon::queue::DiscoverySource;
use crate::daemon::queue::{BoxFuture, Enqueue, IssueKey};
use crate::event_log::Event;
use crate::launches::model::NewPlaybookLaunch;
use crate::model::LaunchOrigin;
use crate::model::MaxTime;
use crate::model::Trigger;
use crate::model::{ParkReason, ParkedBy, Status};
use anyhow::{Context, Result};
use jiff::Timestamp;
use sqlx::PgPool;
use std::collections::BTreeSet;
use std::sync::Arc;

/// The authorization every trigger stores: the same one a manual launch carries, validated at the
/// endpoint exactly as an immediate launch is.
#[derive(Debug, Clone)]
pub(crate) struct NewStanding<'a> {
    pub playbook: &'a str,
    pub target_kind: &'a str,
    pub eligible_draft_version: Option<i64>,
    /// The validated `{name: value}` object every firing launches.
    pub params: &'a serde_json::Value,
    pub schema_digest: &'a str,
    pub max_cost: f64,
    pub max_time: &'a MaxTime,
    pub advance_dedupe: bool,
    pub enabled: bool,
    pub created_by: Option<&'a str>,
    /// The principal this save owns the row to, and the groups their claims carried. A save by
    /// anyone else re-owns the row, because a firing launches under whoever saved it last.
    pub owner_principal: Option<&'a str>,
    pub owner_groups: Option<&'a serde_json::Value>,
    /// The cluster every firing dispatches onto; `None` uses the controller default.
    pub dispatch_target: Option<&'a str>,
    /// The model provider every firing dispatches against, and the model to ask it for; `None`
    /// resolves the dispatch defaults at fire time.
    pub agent_provider: Option<&'a str>,
    pub agent_model: Option<&'a str>,
}

/// One `playbook_standing_launches` row. Sidecar reads `#[sqlx(flatten)]` this into their own
/// row type, so the column list below is the one place it is spelled.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub(crate) struct Standing {
    pub id: String,
    pub playbook: String,
    pub target_kind: String,
    pub adopted_repo: Option<String>,
    pub adopted_path: Option<String>,
    pub adopted_rev: Option<String>,
    pub eligible_draft_version: Option<i64>,
    pub params: serde_json::Value,
    pub schema_digest: String,
    pub max_cost: f64,
    pub max_time: String,
    pub advance_dedupe: bool,
    pub enabled: bool,
    pub consecutive_failures: i64,
    pub created_by: Option<String>,
    /// Who the last save owned this row to, the groups their claims carried, and when the snapshot
    /// was taken. A row saved before snapshots existed carries none and parks any firing whose
    /// scope binds secrets.
    pub owner_principal: Option<String>,
    pub owner_groups: Option<serde_json::Value>,
    pub owner_groups_at: Option<String>,
    /// The fire-time group refresh's state. `owner_signin_required` is what a definitive refresh
    /// refusal past the snapshot TTL and an explicit revoke both set, and what a fresh login and a
    /// successful refresh both clear; `owner_refresh_error` says why, transient failures included.
    pub owner_signin_required: bool,
    pub owner_refresh_error: Option<String>,
    pub owner_refresh_at: Option<String>,
    pub dispatch_target: Option<String>,
    pub agent_provider: Option<String>,
    pub agent_model: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// The [`Standing`] projection, qualified as `c` so a sidecar read can join its own table beside
/// it: `SELECT {COLUMNS}, s.cron_expr FROM playbook_standing_launches c JOIN playbook_schedules s
/// USING (id)`.
pub(crate) const COLUMNS: &str = "c.id, c.playbook, c.target_kind, c.adopted_repo, c.adopted_path, \
    c.adopted_rev, c.eligible_draft_version, c.params, c.schema_digest, c.max_cost, c.max_time, \
    c.advance_dedupe, c.enabled, c.consecutive_failures, c.created_by, c.owner_principal, \
    c.owner_groups, c.owner_groups_at, c.owner_signin_required, c.owner_refresh_error, \
    c.owner_refresh_at, c.dispatch_target, c.agent_provider, c.agent_model, c.created_at, \
    c.updated_at";

/// Store the core row under `id`. The adopted snapshot is copied from the registry in the same
/// statement, so a later repin cannot move an existing recurrence.
pub(crate) async fn insert(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: &str,
    trigger: Trigger,
    new: &NewStanding<'_>,
    now: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO playbook_standing_launches (
            id, trigger, playbook, target_kind, eligible_draft_version, params, schema_digest,
            max_cost, max_time, advance_dedupe, enabled, created_by, owner_principal,
            owner_groups, owner_groups_at, dispatch_target, agent_provider, agent_model,
            created_at, updated_at,
            adopted_repo, adopted_path, adopted_rev, adopted_tar_gz, adopted_tar_digest,
            adopted_tar_bytes, adopted_params_schema)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                CASE WHEN $13::text IS NULL THEN NULL ELSE $18 END, $15, $16, $17, $18, $18,
                (SELECT repo FROM playbooks WHERE id = $3),
                (SELECT path FROM playbooks WHERE id = $3),
                (SELECT rev FROM playbooks WHERE id = $3),
                (SELECT tar_gz FROM playbooks WHERE id = $3),
                (SELECT tar_digest FROM playbooks WHERE id = $3),
                (SELECT tar_bytes FROM playbooks WHERE id = $3),
                (SELECT params_schema FROM playbooks WHERE id = $3))
        "#,
    )
    .bind(id)
    .bind(trigger.as_str())
    .bind(new.playbook)
    .bind(new.target_kind)
    .bind(new.eligible_draft_version)
    .bind(new.params)
    .bind(new.schema_digest)
    .bind(new.max_cost)
    .bind(new.max_time.as_str())
    .bind(new.advance_dedupe)
    .bind(new.enabled)
    .bind(new.created_by)
    .bind(new.owner_principal)
    .bind(new.owner_groups)
    .bind(new.dispatch_target)
    .bind(new.agent_provider)
    .bind(new.agent_model)
    .bind(now)
    .execute(&mut **tx)
    .await
    .context("insert standing launch")?;
    Ok(())
}

/// Replace the authorization under `id`. The failure count starts over and the owner flags clear:
/// an edited row is not still N failures deep into the run it used to be, and the re-save is a
/// fresh snapshot. `false` when there is no row under that id.
pub(crate) async fn replace(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: &str,
    new: &NewStanding<'_>,
    now: &str,
) -> Result<bool> {
    let updated = sqlx::query(
        r#"
        UPDATE playbook_standing_launches
        SET playbook = $2, target_kind = $3, eligible_draft_version = $4, params = $5,
            schema_digest = $6, max_cost = $7, max_time = $8, advance_dedupe = $9, enabled = $10,
            created_by = $11, owner_principal = $12, owner_groups = $13,
            owner_groups_at = CASE WHEN $12::text IS NULL THEN NULL ELSE $17 END,
            owner_signin_required = false, owner_refresh_error = NULL,
            dispatch_target = $14, agent_provider = $15, agent_model = $16,
            consecutive_failures = 0, updated_at = $17,
            adopted_repo = (SELECT repo FROM playbooks WHERE id = $2),
            adopted_path = (SELECT path FROM playbooks WHERE id = $2),
            adopted_rev = (SELECT rev FROM playbooks WHERE id = $2),
            adopted_tar_gz = (SELECT tar_gz FROM playbooks WHERE id = $2),
            adopted_tar_digest = (SELECT tar_digest FROM playbooks WHERE id = $2),
            adopted_tar_bytes = (SELECT tar_bytes FROM playbooks WHERE id = $2),
            adopted_params_schema = (SELECT params_schema FROM playbooks WHERE id = $2)
        WHERE id = $1
        "#,
    )
    .bind(id)
    .bind(new.playbook)
    .bind(new.target_kind)
    .bind(new.eligible_draft_version)
    .bind(new.params)
    .bind(new.schema_digest)
    .bind(new.max_cost)
    .bind(new.max_time.as_str())
    .bind(new.advance_dedupe)
    .bind(new.enabled)
    .bind(new.created_by)
    .bind(new.owner_principal)
    .bind(new.owner_groups)
    .bind(new.dispatch_target)
    .bind(new.agent_provider)
    .bind(new.agent_model)
    .bind(now)
    .execute(&mut **tx)
    .await
    .context("replace standing launch")?;
    Ok(updated.rows_affected() > 0)
}

/// Flip `enabled` without touching the authorization. `None` when there is no row under that id.
pub(crate) async fn set_enabled(pool: &PgPool, id: &str, enabled: bool) -> Result<Option<bool>> {
    let now = crate::clock::now_rfc3339();
    let prior: Option<bool> = sqlx::query_scalar(
        "UPDATE playbook_standing_launches c SET enabled = $2, updated_at = $3,
             consecutive_failures = CASE WHEN $2 THEN 0 ELSE consecutive_failures END
         FROM (SELECT id, enabled FROM playbook_standing_launches WHERE id = $1) prior
         WHERE c.id = prior.id
         RETURNING prior.enabled",
    )
    .bind(id)
    .bind(enabled)
    .bind(&now)
    .fetch_optional(pool)
    .await
    .context("set standing launch enabled")?;
    Ok(prior)
}

/// Delete the core row; the sidecar cascades. `false` when there was none under that id. The
/// launches it already fired are ordinary runs and stay.
pub(crate) async fn delete(pool: &PgPool, id: &str) -> Result<bool> {
    let deleted = sqlx::query("DELETE FROM playbook_standing_launches WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .context("delete standing launch")?;
    Ok(deleted.rows_affected() > 0)
}

/// A core row locked for firing, with the registry columns the launch copies: what the pack is
/// called, where it came from, and the schema an overlaid value is validated against exactly as a
/// launcher's typed one is.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct Authorized {
    #[sqlx(flatten)]
    pub core: Standing,
    pub repo: String,
    /// `None` when the pack is no longer registered (or the draft is gone): the firing refuses.
    pub description: Option<String>,
    pub params_schema: Option<serde_json::Value>,
    /// The draft version a draft-head firing freezes.
    pub draft_version: Option<i64>,
    pub adopted_tar_gz: Option<Vec<u8>>,
    pub adopted_tar_digest: Option<String>,
    pub adopted_tar_bytes: Option<i64>,
}

/// Lock the core row for the firing the trigger just claimed.
pub(crate) async fn authorized(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: &str,
) -> Result<Option<Authorized>> {
    let sql = const_format::formatcp!(
        r#"
        SELECT {COLUMNS},
               COALESCE(p.repo, '(draft)') AS repo,
               COALESCE(p.description, d.description) AS description,
               COALESCE(c.adopted_params_schema, dv.params_schema) AS params_schema,
               dv.version AS draft_version,
               c.adopted_tar_gz, c.adopted_tar_digest, c.adopted_tar_bytes
        FROM playbook_standing_launches c
        LEFT JOIN playbooks p ON p.id = c.playbook AND c.target_kind = 'adopted'
        LEFT JOIN playbook_drafts d ON d.id = c.playbook AND c.target_kind = 'draft_head'
        LEFT JOIN LATERAL (
            SELECT version, params_schema FROM playbook_draft_versions
            WHERE draft_id = c.playbook AND schema_digest IS NOT NULL
            ORDER BY version DESC LIMIT 1
        ) dv ON c.target_kind = 'draft_head'
        WHERE c.id = $1
        FOR UPDATE OF c
        "#
    );
    sqlx::query_as::<_, Authorized>(sql)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
        .context("lock the standing launch for firing")
}

/// What one firing adds to the stored authorization.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Firing<'a> {
    /// A param the trigger supplies per firing (a cursor value, an item identifier), overlaid on
    /// the stored ones and validated with them.
    pub overlay: Option<(&'a str, &'a str)>,
    pub origin: LaunchOrigin,
    /// The schedule whose cursor the run may advance; `None` moves nothing.
    pub dedupe_schedule: Option<&'a str>,
    /// The launch title; `None` uses the pack's description.
    pub title: Option<&'a str>,
}

/// Why a firing that minted its launch must park instead of dispatching: the owner snapshot does
/// not authorize it. The trigger wraps this in its own park reason.
pub(crate) type StaleOwner = Option<String>;

/// Mint the launch for a locked core row on the trigger's transaction: the launch row, its
/// dispatch and agent columns, and the adopted tarball copy. `exposure` is the disclosure the
/// launch row records: a draft-head firing's recomputed one, absent for an adopted pack whose
/// disclosure is the registry row's. Returns how the owner snapshot stands; an `Err` is the
/// message the trigger records as a firing failure.
pub(crate) async fn fire(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    row: &Authorized,
    firing: &Firing<'_>,
    exposure: &crate::playbooks::exposure::Extraction,
    key: &str,
    now: Timestamp,
    owner_ttl: std::time::Duration,
) -> Result<StaleOwner, String> {
    let core = &row.core;
    let noun = format!("standing launch {}", core.id);
    let (Some(description), Some(schema)) = (row.description.as_deref(), &row.params_schema) else {
        return Err(format!(
            "{noun} names playbook {}, which is no longer registered",
            core.playbook
        ));
    };
    let max_time =
        MaxTime::parse(&core.max_time).map_err(|e| format!("{noun} stored max_time: {e}"))?;
    let params = overlaid_params(core, schema, firing.overlay)?;
    let launch = NewPlaybookLaunch {
        playbook: &core.playbook,
        repo: &row.repo,
        title: firing.title.unwrap_or(description),
        params: &params,
        schema_digest: &core.schema_digest,
        max_cost: core.max_cost,
        max_time: &max_time,
        advance_dedupe: core.advance_dedupe,
        dedupe_schedule: firing.dedupe_schedule,
        origin: firing.origin,
        draft_version: row.draft_version,
        created_by: core
            .owner_principal
            .as_deref()
            .map(strip_user_prefix)
            .or(core.created_by.as_deref()),
        launcher_groups: core.owner_groups.as_ref(),
    };
    match crate::launches::store::insert_playbook_launch_with(tx, key, &launch, exposure).await {
        Ok(true) => {}
        Ok(false) => {
            return Err(format!(
                "{noun} names playbook {}, which is no longer registered",
                core.playbook
            ));
        }
        Err(e) => return Err(format!("{noun} launch insert: {e:#}")),
    }
    crate::issues::store::set_dispatch_target(&mut **tx, key, core.dispatch_target.as_deref())
        .await
        .map_err(|e| format!("{noun} dispatch target: {e:#}"))?;
    crate::issues::store::set_agent_dispatch(
        &mut **tx,
        key,
        core.agent_provider.as_deref(),
        core.agent_model.as_deref(),
    )
    .await
    .map_err(|e| format!("{noun} agent dispatch: {e:#}"))?;
    // `insert_playbook_launch_with` preserves its one shared path by initially copying the current
    // registry row. Replace that copy inside the same transaction with the revision this row
    // explicitly adopted, so a later registry repin cannot move an existing recurrence.
    if core.target_kind == "adopted" {
        let (Some(tar_gz), Some(digest), Some(bytes)) = (
            row.adopted_tar_gz.as_ref(),
            row.adopted_tar_digest.as_ref(),
            row.adopted_tar_bytes,
        ) else {
            return Err(format!("{noun} has no adopted pack"));
        };
        let slug = crate::model::sanitize_key(key);
        sqlx::query(
            r#"UPDATE pack_tarballs SET tar_gz = $2, digest = $3, bytes = $4, created_at = $5
               WHERE issue_slug = $1"#,
        )
        .bind(&slug)
        .bind(tar_gz)
        .bind(digest)
        .bind(bytes)
        .bind(crate::clock::now_rfc3339())
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("{noun} adopted pack copy: {e}"))?;
    }
    sqlx::query(
        "UPDATE playbook_standing_launches SET consecutive_failures = 0, updated_at = $2 WHERE id = $1",
    )
    .bind(&core.id)
    .bind(now.to_string())
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("{noun} reset failures: {e}"))?;
    // Only a scope that actually binds secrets needs an owner snapshot: a row that launches
    // nothing credentialed keeps firing exactly as it did before the registry.
    let binds = scope_binds_secrets(&mut **tx, &core.playbook)
        .await
        .map_err(|e| format!("{noun} binding lookup: {e:#}"))?;
    Ok(binds.then(|| owner_stale(core, now, owner_ttl)).flatten())
}

/// The values this firing launches with, after overlaying the trigger's per-firing param. The
/// complete object goes back through the exact adopted or draft-version schema, so schema drift
/// or an invalid overlay becomes a firing failure that counts toward auto-disable rather than an
/// invalid launch.
fn overlaid_params(
    core: &Standing,
    schema: &serde_json::Value,
    overlay: Option<(&str, &str)>,
) -> Result<serde_json::Value, String> {
    let mut values = crate::launches::store::stored_params(&core.params);
    if let Some((param, value)) = overlay {
        values.insert(param.to_string(), value.to_string());
    }
    crate::playbooks::registry::validate_params(schema, &values).map_err(|fields| {
        let detail = fields
            .iter()
            .map(|f| format!("{}: {}", f.field, f.message))
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            "standing launch {} parameters were refused by playbook {}: {detail}",
            core.id, core.playbook
        )
    })
}

/// Whether the playbook scope this row fires binds any secret.
async fn scope_binds_secrets(ex: impl sqlx::PgExecutor<'_>, playbook: &str) -> Result<bool> {
    let bound: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM secret_bindings WHERE scope_kind = $1 AND scope_id = $2",
    )
    .bind(crate::secrets::ScopeKind::Playbook.as_str())
    .bind(playbook)
    .fetch_one(ex)
    .await
    .context("count a playbook scope's bindings")?;
    Ok(bound > 0)
}

/// The principal spelling a launch row stores. `playbook_launches.created_by` is a login, and the
/// snapshot is a full principal, so the prefix comes off on the way in.
pub(crate) fn strip_user_prefix(principal: &str) -> &str {
    principal.strip_prefix("user:").unwrap_or(principal)
}

/// Why this firing's owner snapshot does not authorize a launch, or `None` when it does. A row
/// saved before snapshots existed has none; one saved longer ago than `ttl` is past the staleness
/// window the TTL bounds. Either way the firing parks and waits for an owner to re-save.
fn owner_stale(core: &Standing, now: Timestamp, ttl: std::time::Duration) -> StaleOwner {
    if core.owner_signin_required {
        return Some("its owner has to sign in again".to_string());
    }
    let (Some(_), Some(at)) = (
        core.owner_principal.as_deref(),
        core.owner_groups_at.as_deref(),
    ) else {
        return Some("the row was saved before it recorded one".to_string());
    };
    let Ok(taken) = at.parse::<Timestamp>() else {
        return Some(format!(
            "its snapshot carries an unreadable timestamp {at:?}"
        ));
    };
    let Ok(span) = jiff::SignedDuration::try_from(ttl) else {
        return Some(format!("the configured TTL {ttl:?} is unusable"));
    };
    match taken.checked_add(span) {
        Ok(expires) if expires > now => None,
        _ => Some(format!("its snapshot was taken at {at}")),
    }
}

/// What the fire-time refresh decided about one row that is about to fire.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OwnerCheck {
    /// Claim it. Either the refresh landed and the snapshot is now the issuer's live word, or there
    /// was nothing to refresh and the snapshot stands on its own TTL.
    Proceed,
    /// The issuer could not be reached. Nothing is recorded and nothing counts toward auto-disable;
    /// the row is left where it was and the next tick tries again.
    Defer,
}

/// Re-read a row's owner groups from their offline credential before the trigger claims it.
///
/// Runs outside any transaction: the refresh takes its own per-subject lock, and a claim held
/// across an issuer round trip would hold the row for the length of that call. A second sweep can
/// take the row between this and the claim; that costs one wasted refresh and nothing else.
///
/// A successful refresh rewrites the row's own snapshot, which is what makes the launch
/// authorization live rather than as-old-as-the-last-save. A definitive refusal (revoked, expired,
/// no credential at all) records the reason and, once the snapshot is outside its TTL, marks the
/// row as needing sign-in so the firing parks.
pub(crate) async fn refresh_owner(
    pool: &PgPool,
    refresh: &crate::identity::oidc::credentials::OwnerRefresh,
    id: &str,
    now: Timestamp,
    owner_ttl: std::time::Duration,
) -> Result<OwnerCheck> {
    use crate::identity::oidc::OidcError;
    use crate::identity::oidc::credentials::RefreshOutcome;

    let Some((owner_principal, binds_secrets)) = sqlx::query_as::<_, (Option<String>, bool)>(
        "SELECT c.owner_principal,
                EXISTS (SELECT 1 FROM secret_bindings b
                        WHERE b.scope_kind = 'playbook' AND b.scope_id = c.playbook)
         FROM playbook_standing_launches c WHERE c.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("read the owner about to fire")?
    else {
        return Ok(OwnerCheck::Proceed);
    };
    if !binds_secrets {
        return Ok(OwnerCheck::Proceed);
    }
    let Some(principal) = owner_principal.as_deref() else {
        return Ok(OwnerCheck::Proceed);
    };
    if !principal.starts_with("user:") {
        return Ok(OwnerCheck::Proceed);
    }
    let login = strip_user_prefix(principal);
    let Some(sub) = crate::identity::oidc::users::sub_for_login(pool, login).await? else {
        record_owner_refusal(
            pool,
            id,
            &format!("{login} has never signed in through the issuer"),
            now,
            owner_ttl,
        )
        .await?;
        return Ok(OwnerCheck::Proceed);
    };
    match refresh.refresh(&sub).await {
        Ok(RefreshOutcome::Claims(claims)) => {
            let groups = serde_json::to_value(&claims.groups).context("encode refreshed groups")?;
            sqlx::query(
                "UPDATE playbook_standing_launches
                 SET owner_groups = $2, owner_groups_at = $3, owner_refresh_at = $3,
                     owner_refresh_error = NULL, owner_signin_required = false
                 WHERE id = $1",
            )
            .bind(id)
            .bind(groups)
            .bind(now.to_string())
            .execute(pool)
            .await
            .context("store the refreshed owner groups")?;
            Ok(OwnerCheck::Proceed)
        }
        Ok(RefreshOutcome::Absent) => {
            record_owner_refusal(
                pool,
                id,
                &format!("{login} has no offline credential stored"),
                now,
                owner_ttl,
            )
            .await?;
            Ok(OwnerCheck::Proceed)
        }
        Err(OidcError::Unavailable(why)) => {
            tracing::warn!(standing = %id, error = %why, "standing: owner group refresh deferred");
            sqlx::query(
                "UPDATE playbook_standing_launches SET owner_refresh_error = $2, owner_refresh_at = $3
                 WHERE id = $1",
            )
            .bind(id)
            .bind(&why)
            .bind(now.to_string())
            .execute(pool)
            .await
            .context("note a deferred owner refresh")?;
            Ok(OwnerCheck::Defer)
        }
        Err(OidcError::Rejected(why)) => {
            record_owner_refusal(pool, id, &why, now, owner_ttl).await?;
            Ok(OwnerCheck::Proceed)
        }
    }
}

/// Record a definitive refresh refusal. The snapshot is left alone: inside its TTL it is still the
/// authorization the launch runs under, and only once it has aged out does the row need a sign-in.
async fn record_owner_refusal(
    pool: &PgPool,
    id: &str,
    why: &str,
    now: Timestamp,
    owner_ttl: std::time::Duration,
) -> Result<()> {
    let deadline = jiff::SignedDuration::try_from(owner_ttl)
        .ok()
        .and_then(|span| now.checked_sub(span).ok())
        .map(|t| t.to_string());
    // Inside the TTL the snapshot still stands; a NULL deadline (an unusable TTL) needs sign-in
    // outright, and so does a row that never took a snapshot. A refusal only ever adds the demand:
    // a revoke already set it deliberately, and nothing but a fresh credential takes it back off.
    sqlx::query(
        "UPDATE playbook_standing_launches
         SET owner_refresh_error = $2, owner_refresh_at = $3,
             owner_signin_required = owner_signin_required
                                     OR $4::text IS NULL
                                     OR owner_groups_at IS NULL
                                     OR owner_groups_at <= $4
         WHERE id = $1",
    )
    .bind(id)
    .bind(why)
    .bind(now.to_string())
    .bind(deadline)
    .execute(pool)
    .await
    .context("record an owner refresh refusal")?;
    Ok(())
}

/// The owner principal and group snapshot a standing launch carries, for an update that must keep
/// the row owned as it was.
pub(crate) struct OwnerSnapshot {
    pub principal: Option<String>,
    pub groups: Option<serde_json::Value>,
}

pub(crate) async fn owner_snapshot(pool: &PgPool, id: &str) -> Result<Option<OwnerSnapshot>> {
    sqlx::query_as::<_, (Option<String>, Option<serde_json::Value>)>(
        "SELECT owner_principal, owner_groups FROM playbook_standing_launches WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("read a standing launch's owner snapshot")
    .map(|row| row.map(|(principal, groups)| OwnerSnapshot { principal, groups }))
}

/// Clear the sign-in demand a failed refresh left on a user's rows. Signing in again stores a
/// fresh credential, so the reason those rows were parked is gone.
pub(crate) async fn clear_owner_signin(pool: &PgPool, login: &str) -> Result<()> {
    let principal = format!("user:{}", login.trim().to_lowercase());
    sqlx::query(
        "UPDATE playbook_standing_launches
         SET owner_signin_required = false, owner_refresh_error = NULL
         WHERE owner_principal = $1 AND (owner_signin_required OR owner_refresh_error IS NOT NULL)",
    )
    .bind(&principal)
    .execute(pool)
    .await
    .context("clear a signed-in owner's standing launches")?;
    Ok(())
}

/// Mark every row a revoked credential leaves unauthorized. Revocation is deliberate, so the
/// snapshot TTL does not get a say: the owner asked for those firings to stop.
pub(crate) async fn require_owner_signin(pool: &PgPool, login: &str, why: &str) -> Result<u64> {
    let principal = format!("user:{}", login.trim().to_lowercase());
    let updated = sqlx::query(
        "UPDATE playbook_standing_launches SET owner_signin_required = true, owner_refresh_error = $2
         WHERE owner_principal = $1
               AND EXISTS (SELECT 1 FROM secret_bindings b
                           WHERE b.scope_kind = 'playbook' AND b.scope_id = playbook)",
    )
    .bind(&principal)
    .bind(why)
    .execute(pool)
    .await
    .context("park a revoking owner's standing launches")?;
    Ok(updated.rows_affected())
}

/// Where a row's failure count landed after one more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FailureState {
    pub consecutive_failures: i64,
    pub enabled: bool,
}

/// Record a firing that did not launch, and disable the row once it has failed
/// `auto_disable_after` times in a row, or outright when the trigger has nothing left to wait for
/// (`force_disable`). Runs after the claim's transaction rolled back, so it is what keeps a
/// failing row from retrying on every discovery tick. `None` when the row is gone.
pub(crate) async fn record_failure(
    db: &Db,
    trigger: Trigger,
    id: &str,
    message: &str,
    auto_disable_after: i64,
    force_disable: bool,
    announce: bool,
) -> Result<Option<FailureState>> {
    let now = crate::clock::now_rfc3339();
    let updated = sqlx::query_as::<_, (i64, bool)>(
        r#"
        UPDATE playbook_standing_launches
        SET consecutive_failures = consecutive_failures + 1,
            enabled = NOT ($2 OR consecutive_failures + 1 >= $3),
            updated_at = $4
        WHERE id = $1
        RETURNING consecutive_failures, enabled
        "#,
    )
    .bind(id)
    .bind(force_disable)
    .bind(auto_disable_after)
    .bind(&now)
    .fetch_optional(db.pool())
    .await
    .context("record a standing launch failure")?;
    let Some((consecutive_failures, enabled)) = updated else {
        return Ok(None);
    };
    tracing::warn!(
        standing = %id,
        trigger = trigger.as_str(),
        consecutive_failures,
        error = %message,
        "standing: firing failed"
    );
    if !enabled && announce {
        let note = format!(
            "auto-disabled after {consecutive_failures} consecutive firing failures: {message}"
        );
        let key = trigger.event_key(id);
        let event = Event::now(&key, "enabled", "disabled", Some(&note), Some(id));
        crate::event_log::insert(db.pool(), &event).await?;
        db.events().publish(&event);
    }
    Ok(Some(FailureState {
        consecutive_failures,
        enabled,
    }))
}

/// Close the launches a standing row parked for a stale owner snapshot. A re-save re-owns the
/// row, so the firings that could not launch under the old snapshot will never launch: they close
/// rather than sit parked forever. Returns the keys closed.
pub(crate) async fn expire_stale_owner_parks(
    pool: &PgPool,
    trigger: Trigger,
    id: &str,
) -> Result<Vec<String>> {
    let now = crate::clock::now_rfc3339();
    let prefix = ParkReason::OwnerStale {
        trigger,
        id: id.to_string(),
        detail: String::new(),
    }
    .to_string();
    let prefix = prefix.trim_end().to_string();
    const EXPIRE: &str = "UPDATE issues SET status = 'done', parked_reason = NULL, parked_by = NULL, \
         updated_at = $3 WHERE status = 'parked' AND parked_reason LIKE $1 || '%' AND key IN";
    let sql = match trigger {
        Trigger::Schedule => const_format::concatcp!(
            EXPIRE,
            " (SELECT key FROM playbook_launches WHERE dedupe_schedule = $2) RETURNING key"
        ),
        Trigger::Deferred => const_format::concatcp!(
            EXPIRE,
            " (SELECT fired_key FROM playbook_one_shots WHERE id = $2) RETURNING key"
        ),
        Trigger::Watch => const_format::concatcp!(
            EXPIRE,
            " (SELECT launch_key FROM playbook_watch_hits WHERE watch_id = $2) RETURNING key"
        ),
    };
    sqlx::query_scalar(sql)
        .bind(&prefix)
        .bind(id)
        .bind(&now)
        .fetch_all(pool)
        .await
        .context("expire a standing launch's stale-owner parks")
}

/// An event a trigger records beside the launch's own, owned because the trigger builds it on a
/// transaction the sweep commits later.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub key: String,
    pub from: &'static str,
    pub to: &'static str,
    pub reason: Option<String>,
    pub evidence: Option<String>,
    pub actor: Option<String>,
}

impl Recorded {
    pub(crate) fn event(&self) -> Event<'_> {
        Event::now(
            &self.key,
            self.from,
            self.to,
            self.reason.as_deref(),
            self.evidence.as_deref(),
        )
        .by(self.actor.as_deref())
    }
}

/// One firing a trigger has found due: which core row, and what this firing adds to it. The
/// trigger's own bookkeeping rides in `note` and `reference` (the event-log line the launch gets).
#[derive(Debug, Clone)]
pub struct Claim {
    pub id: String,
    /// A param the trigger supplies per firing, overlaid on the stored ones.
    pub overlay: Option<(String, String)>,
    /// The schedule whose cursor the run may advance.
    pub dedupe_schedule: Option<String>,
    /// The launch title; `None` uses the pack's description.
    pub title: Option<String>,
    /// What this firing is about, appended to the pack's description as the launch title when
    /// `title` is not set (`Evidence-gated backport: ACME-10192`).
    pub subject: Option<String>,
    /// The event-log note and reference recorded on the launch key.
    pub note: String,
    pub reference: Option<String>,
    /// Trigger-private state carried from `due` through `claim` to `settle`.
    pub payload: serde_json::Value,
}

impl Claim {
    pub(crate) fn new(id: &str, note: String) -> Self {
        Claim {
            id: id.to_string(),
            overlay: None,
            dedupe_schedule: None,
            title: None,
            subject: None,
            note,
            reference: Some(id.to_string()),
            payload: serde_json::Value::Null,
        }
    }
}

/// What a trigger does with a claim that could not fire. The core counts every failure; the
/// trigger decides whether the row is also out of windows to wait for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failed {
    pub force_disable: bool,
    /// The trigger already recorded the transition on its own key; the core stays quiet.
    pub announced: bool,
}

/// A future a trigger method returns; it may borrow the sweep's transaction.
pub type TriggerFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// A trigger: the sidecar that knows when a core row is due, how to claim one firing of it, and
/// what to write on the sidecar once the firing landed or failed. The generic [`sweep`] owns
/// everything else: the pre-claim owner refresh, the fire builder, parking, failure accounting,
/// and enqueueing.
pub trait LaunchTrigger: Send + Sync {
    fn trigger(&self) -> Trigger;

    /// The firings due at `now`, oldest-first, bounded by the trigger's own cap. Read without
    /// claiming: a second sweep may take one before [`claim`](Self::claim) does, which then
    /// returns `None` for it.
    fn due<'a>(
        &'a self,
        db: &'a Db,
        cfg: SweepCfg,
        now: Timestamp,
    ) -> TriggerFuture<'a, Result<Vec<Claim>>>;

    /// Claim one firing on the sweep's transaction; `false` when another sweep already did.
    fn claim<'a, 'c>(
        &'a self,
        tx: &'a mut sqlx::Transaction<'c, sqlx::Postgres>,
        claim: &'a mut Claim,
        now: Timestamp,
    ) -> TriggerFuture<'a, Result<bool>>;

    /// The sidecar bookkeeping after the launch was minted, on the same transaction: the next
    /// window, the launch key, the watermark. Returns extra events to record beside the launch's.
    fn settle<'a, 'c>(
        &'a self,
        tx: &'a mut sqlx::Transaction<'c, sqlx::Postgres>,
        claim: &'a Claim,
        key: &'a str,
        now: Timestamp,
    ) -> TriggerFuture<'a, Result<Vec<Recorded>>>;

    /// The core disabled the row after a failure: the sidecar drops whatever it was waiting at.
    fn retire<'a>(&'a self, _db: &'a Db, _claim: &'a Claim) -> TriggerFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// The sidecar's side of a firing that did not launch, after the claim rolled back.
    fn fail<'a>(
        &'a self,
        db: &'a Db,
        claim: &'a Claim,
        message: &'a str,
        now: Timestamp,
    ) -> TriggerFuture<'a, Result<Failed>>;
}

/// The per-sweep knobs every trigger shares.
#[derive(Debug, Clone, Copy)]
pub struct SweepCfg {
    pub auto_disable_after: i64,
    pub owner_ttl: std::time::Duration,
}

/// Fire everything one trigger has due at `now`. Returns the keys of the launches that may
/// dispatch; a launch whose owner snapshot is stale is minted, parked, and left out.
///
/// A firing that fails is recorded against its core row and the sweep moves on to the next claim,
/// skipping the rest of that row's claims this pass: one broken row must not starve the others,
/// and at `auto_disable_after` consecutive failures it takes itself out of the rotation.
///
/// `refresh` is the offline credential the owner's live groups are re-read through before a
/// row's first claim of the pass; `None` leaves every firing on the stored snapshot alone.
pub(crate) async fn sweep(
    db: &Db,
    trigger: &dyn LaunchTrigger,
    cfg: SweepCfg,
    now: Timestamp,
    refresh: Option<&crate::identity::oidc::credentials::OwnerRefresh>,
    policy: Option<&crate::authz::policy::ActivePolicy>,
) -> Result<Vec<String>> {
    let kind = trigger.trigger();
    let mut fired = Vec::new();
    let mut refreshed: BTreeSet<String> = BTreeSet::new();
    let mut skipped: BTreeSet<String> = BTreeSet::new();
    for mut claim in trigger.due(db, cfg, now).await? {
        if skipped.contains(&claim.id) {
            continue;
        }
        // The owner's live groups are re-read BEFORE the row is claimed, outside any transaction:
        // the refresh takes its own per-subject lock, and a claim held across an issuer round trip
        // would hold the row for the length of that call.
        if let Some(refresh) = refresh
            && refreshed.insert(claim.id.clone())
            && refresh_owner(db.pool(), refresh, &claim.id, now, cfg.owner_ttl).await?
                == OwnerCheck::Defer
        {
            skipped.insert(claim.id.clone());
            continue;
        }
        let mut tx = db.pool().begin().await.context("sweep: begin")?;
        if !trigger.claim(&mut tx, &mut claim, now).await? {
            tx.rollback().await.context("sweep: rollback")?;
            continue;
        }
        match fire_claim(
            &mut tx,
            db.pool(),
            trigger,
            &claim,
            now,
            cfg.owner_ttl,
            policy,
        )
        .await
        {
            Ok(outcome) => {
                tx.commit().await.context("sweep: commit")?;
                for event in &outcome.events {
                    db.events().publish(&event.event());
                }
                tracing::info!(
                    issue_key = %outcome.key,
                    trigger = kind.as_str(),
                    standing = %claim.id,
                    "standing: firing launched"
                );
                // The launch row exists either way; a firing with no usable owner snapshot parks
                // rather than dispatching under an authorization nobody can vouch for.
                match outcome.stale {
                    Some(detail) => {
                        crate::issues::transitions::park(
                            db.pool(),
                            db.events(),
                            &outcome.key,
                            Status::New,
                            &ParkReason::OwnerStale {
                                trigger: kind,
                                id: claim.id.clone(),
                                detail,
                            },
                            ParkedBy::Machine,
                        )
                        .await?;
                    }
                    None => fired.push(outcome.key),
                }
            }
            Err(message) => {
                tx.rollback().await.context("sweep: rollback")?;
                let failed = trigger.fail(db, &claim, &message, now).await?;
                let state = record_failure(
                    db,
                    kind,
                    &claim.id,
                    &message,
                    cfg.auto_disable_after,
                    failed.force_disable,
                    !failed.announced,
                )
                .await?;
                if state.is_some_and(|s| !s.enabled) {
                    trigger.retire(db, &claim).await?;
                }
                skipped.insert(claim.id.clone());
            }
        }
    }
    Ok(fired)
}

struct Fired {
    key: String,
    stale: StaleOwner,
    events: Vec<Recorded>,
}

/// One claimed firing on its transaction: lock the core row, mint the launch, let the trigger
/// settle its sidecar, and record the events. `Err` is the message the failure is recorded under.
async fn fire_claim(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    pool: &PgPool,
    trigger: &dyn LaunchTrigger,
    claim: &Claim,
    now: Timestamp,
    owner_ttl: std::time::Duration,
    policy: Option<&crate::authz::policy::ActivePolicy>,
) -> Result<Fired, String> {
    let kind = trigger.trigger();
    let noun = format!("{} {}", kind.as_str(), claim.id);
    let authorized = authorized(tx, &claim.id)
        .await
        .map_err(|e| format!("{noun}: {e:#}"))?
        .ok_or_else(|| format!("{noun} has no standing launch"))?;
    if let Some(policy) = policy {
        let decision = crate::authz::firing::decide_firing(
            pool,
            policy,
            &claim.id,
            authorized.core.owner_principal.as_deref(),
            authorized.core.owner_groups.as_ref(),
            now,
        )
        .await
        .map_err(|e| format!("{noun}: deciding the firing: {e:#}"))?;
        if !decision.allowed {
            return Err(format!(
                "{noun}: policy refused the firing for {} ({})",
                authorized
                    .core
                    .owner_principal
                    .as_deref()
                    .unwrap_or("the platform"),
                decision.reason()
            ));
        }
    }
    let key = format!(
        "playbook:{}:{}",
        authorized.core.playbook,
        uuid::Uuid::now_v7()
    );
    let subject = match (&claim.title, &claim.subject, &authorized.description) {
        (Some(title), _, _) => Some(title.clone()),
        (None, Some(subject), Some(description)) => Some(format!("{description}: {subject}")),
        _ => None,
    };
    let firing = Firing {
        overlay: claim
            .overlay
            .as_ref()
            .map(|(p, v)| (p.as_str(), v.as_str())),
        origin: kind.origin(),
        dedupe_schedule: claim.dedupe_schedule.as_deref(),
        title: subject.as_deref(),
    };
    let exposure = match authorized.draft_version {
        Some(version) => {
            crate::playbooks::drafts::exposure_of(pool, &authorized.core.playbook, version)
                .await
                .map_err(|e| format!("{noun} exposure of draft version {version}: {e}"))?
                .ok_or_else(|| {
                    format!(
                        "{noun} names draft {} version {version}, which is gone",
                        authorized.core.playbook
                    )
                })?
        }
        None => crate::playbooks::exposure::Extraction::Absent,
    };
    let stale = fire(tx, &authorized, &firing, &exposure, &key, now, owner_ttl).await?;
    let mut events = vec![Recorded {
        key: key.clone(),
        from: "new",
        to: "new",
        reason: Some(claim.note.clone()),
        evidence: claim.reference.clone(),
        actor: authorized.core.created_by.clone(),
    }];
    events.extend(
        trigger
            .settle(tx, claim, &key, now)
            .await
            .map_err(|e| format!("{noun} settle: {e:#}"))?,
    );
    for event in &events {
        crate::event_log::insert(&mut **tx, &event.event())
            .await
            .map_err(|e| format!("{noun} event: {e:#}"))?;
    }
    Ok(Fired { key, stale, events })
}

/// The daemon's one launch-trigger sweep: every registered trigger, in order, on the discovery
/// cadence, so `POST /api/reconcile` forces a pass over all of them.
pub struct TriggerSweep {
    db: Db,
    triggers: Vec<Arc<dyn LaunchTrigger>>,
    cfg: SweepCfg,
    /// The offline credential the owner's live groups are re-read through. `None` — proxy mode, no
    /// issuer, no mounted key — leaves every firing on the stored snapshot alone.
    refresh: Option<Arc<crate::identity::oidc::credentials::OwnerRefresh>>,
    /// The policy set every firing is decided under. `None` only in harnesses that predate the
    /// decision.
    policy: Option<crate::authz::policy::ActivePolicy>,
}

impl TriggerSweep {
    pub fn new(
        db: Db,
        triggers: Vec<Arc<dyn LaunchTrigger>>,
        auto_disable_after: i64,
        owner_ttl: std::time::Duration,
        refresh: Option<Arc<crate::identity::oidc::credentials::OwnerRefresh>>,
        policy: Option<crate::authz::policy::ActivePolicy>,
    ) -> Self {
        TriggerSweep {
            db,
            triggers,
            cfg: SweepCfg {
                auto_disable_after,
                owner_ttl,
            },
            refresh,
            policy,
        }
    }
}

impl DiscoverySource for TriggerSweep {
    fn poll(&self, enqueue: Arc<dyn Enqueue>) -> BoxFuture<Result<()>> {
        let db = self.db.clone();
        let triggers = self.triggers.clone();
        let cfg = self.cfg;
        let refresh = self.refresh.clone();
        let policy = self.policy.clone();
        Box::pin(async move {
            let now = Timestamp::now();
            for trigger in &triggers {
                // One trigger's error is logged and the rest still run: a tracker outage must not
                // stop the schedules from firing.
                match sweep(
                    &db,
                    trigger.as_ref(),
                    cfg,
                    now,
                    refresh.as_deref(),
                    policy.as_ref(),
                )
                .await
                {
                    Ok(keys) => {
                        for key in keys {
                            enqueue.enqueue(IssueKey(key));
                        }
                    }
                    Err(e) => tracing::error!(
                        trigger = trigger.trigger().as_str(),
                        error = format!("{e:#}"),
                        "standing: sweep failed"
                    ),
                }
            }
            Ok(())
        })
    }
}
