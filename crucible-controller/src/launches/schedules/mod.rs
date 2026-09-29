//! Recurring playbook launches. A schedule is a cron expression, the zone it is read in, and the
//! dedupe cursor; the authorization every firing launches with is its [`crate::launches::standing`] row,
//! and the sweep turns a due row into an ordinary `playbook_launches` row, so downstream a
//! scheduled run is a run like any other.
//!
//! The firing is [`ScheduleTrigger`] on the generic sweep beside the one-shot and watch triggers. It claims a due
//! row with a single `UPDATE ... WHERE next_due_at <= now RETURNING` — the claim nulls
//! `next_due_at`, so a second sweep (or a second daemon racing the advisory lock) has nothing left
//! to take — recomputes the next firing from *now*, and mints the launch in the same transaction.
//! Windows the controller slept through are counted and recorded, never replayed: a schedule that
//! was due six times while the process was down fires once.

#![allow(clippy::disallowed_macros)]

pub mod cron;

use crate::client::Db;
use crate::clock::stamp;
use crate::event_log::Event;
use crate::launches::model::CursorSpec;
use crate::launches::standing::{
    self, Claim, Failed, LaunchTrigger, NewStanding, Recorded, Standing, SweepCfg, TriggerFuture,
};
use crate::model::Trigger;
use crate::playbooks::registry::FieldError;
use anyhow::{Context, Result};
use cron::CronSpec;
use jiff::Timestamp;

/// How many due schedules one sweep fires. A deeper backlog is a controller that was down for a
/// while; the rest fire on the next discovery tick rather than holding the loop.
const FIRE_CAP: i64 = 32;

/// How far the missed-window count walks before it stops counting. Bounds the walk for a
/// once-a-minute schedule that slept through a long outage; the number is only ever reported.
const MISSED_CAP: usize = 100_000;

/// How many schedules the list surface returns.
pub(crate) const LIST_LIMIT: i64 = 200;

/// The schedule persistence adapter. It owns the controller state needed by schedule reads,
/// writes, cursor updates, and transition events, so callers do not thread a pool and event log
/// through every operation separately.
#[derive(Clone)]
pub(crate) struct ScheduleStore {
    db: Db,
}

impl ScheduleStore {
    pub(crate) fn new(db: Db) -> Self {
        Self { db }
    }

    /// When a schedule in this state fires next: nothing while disabled, otherwise the first
    /// occurrence after `now`.
    fn due_after(&self, spec: &CronSpec, enabled: bool, now: Timestamp) -> Option<String> {
        if !enabled {
            return None;
        }
        spec.next_after(now).map(stamp)
    }

    fn spec_of(&self, claim: &Claim) -> Result<CronSpec> {
        let expr = claim
            .payload
            .get("cron_expr")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let tz = claim
            .payload
            .get("tz")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        CronSpec::parse(expr, tz)
            .map_err(|e: FieldError| anyhow::anyhow!("schedule {}: {}", claim.id, e.message))
    }

    /// Move a schedule's cursor onto what a finished run produced: the named result field, or the
    /// named captured file out of the run's stored bundle.
    pub(crate) async fn advance_cursor(
        &self,
        key: &str,
        run_id: &str,
        result: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<()> {
        let target = sqlx::query_as::<_, CursorTarget>(
            "SELECT s.id, s.cursor_from, s.cursor_param, s.cursor_path
             FROM playbook_launches pl
             JOIN playbook_schedules s ON s.id = pl.dedupe_schedule
             WHERE pl.key = $1 AND pl.advance_dedupe",
        )
        .bind(key)
        .fetch_optional(self.db.pool())
        .await
        .context("read the launch's cursor target")?;
        let Some(target) = target else {
            return Ok(());
        };
        let Some(cursor) = CursorSpec::from_columns(
            target.cursor_from.as_deref(),
            target.cursor_param.as_deref(),
            target.cursor_path.as_deref(),
        ) else {
            return Ok(());
        };
        let schedule_key = Trigger::Schedule.event_key(&target.id);
        let (value, note) = match &cursor {
            CursorSpec::Field { from, param } => {
                let doc = serde_json::Value::Object(result.clone());
                match from.extract(&doc) {
                    Some(value) => {
                        let note = format!(
                            "{} = {value:?}; the next firing passes it as {param}",
                            from.as_str()
                        );
                        (value, note)
                    }
                    None => {
                        let note = format!(
                            "no scalar at {} in the run result; cursor unchanged",
                            from.as_str()
                        );
                        return self.leave_cursor(&schedule_key, key, &note).await;
                    }
                }
            }
            CursorSpec::File { from, path } => {
                let bundle = crate::runs::blob_store::get_run_files(self.db.pool(), run_id).await?;
                let found = match bundle {
                    Some(bundle) => crate::runs::run_files::read_bundle(&bundle, from.as_str())?,
                    None => None,
                };
                let Some(bytes) = found else {
                    let note = format!(
                        "{} is not among the run's captured files; cursor unchanged",
                        from.as_str()
                    );
                    return self.leave_cursor(&schedule_key, key, &note).await;
                };
                if bytes.len() > CURSOR_FILE_MAX_BYTES {
                    let note = format!(
                        "{} is {} bytes, over the {CURSOR_FILE_MAX_BYTES} byte cursor cap; cursor unchanged",
                        from.as_str(),
                        bytes.len()
                    );
                    return self.leave_cursor(&schedule_key, key, &note).await;
                }
                let Ok(text) = String::from_utf8(bytes) else {
                    let note = format!("{} is not UTF-8 text; cursor unchanged", from.as_str());
                    return self.leave_cursor(&schedule_key, key, &note).await;
                };
                let note = format!(
                    "{} ({} bytes, {}); the next firing writes it to {}",
                    from.as_str(),
                    text.len(),
                    content_digest(text.as_bytes()),
                    path.as_str()
                );
                (text, note)
            }
        };
        let now = crate::clock::now_rfc3339();
        sqlx::query(
            "UPDATE playbook_schedules SET cursor_value = $2, cursor_updated_at = $3 WHERE id = $1",
        )
        .bind(&target.id)
        .bind(&value)
        .bind(&now)
        .execute(self.db.pool())
        .await
        .context("advance the schedule cursor")?;
        let event = Event::now(&schedule_key, "cursor", "advanced", Some(&note), Some(key));
        crate::event_log::insert(self.db.pool(), &event).await?;
        self.db.events().publish(&event);
        tracing::info!(schedule = %target.id, issue_key = %key, from = cursor.source(), "schedules: cursor advanced");
        Ok(())
    }

    async fn leave_cursor(&self, schedule_key: &str, key: &str, note: &str) -> Result<()> {
        let event = Event::now(schedule_key, "cursor", "unchanged", Some(note), Some(key));
        crate::event_log::insert(self.db.pool(), &event).await?;
        self.db.events().publish(&event);
        tracing::warn!(schedule = %schedule_key, issue_key = %key, note, "schedules: cursor unchanged");
        Ok(())
    }

    /// Write the stored cursor file of the schedule that fired `issue_key` into the launch's
    /// materialized pack, where the pack's own `[[workspace.inject]]` picks it up. Returns the
    /// path written, or `None` when the launch is not a firing of a file-cursor schedule with a
    /// stored value, in which case the pack's own copy stands.
    pub(crate) async fn stage_cursor_file(
        &self,
        issue_key: &str,
        pack_dir: &std::path::Path,
    ) -> Result<Option<std::path::PathBuf>> {
        let stored = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT s.cursor_path, s.cursor_value
             FROM playbook_launches pl
             JOIN playbook_schedules s ON s.id = pl.dedupe_schedule
             WHERE pl.key = $1 AND pl.origin = 'schedule'",
        )
        .bind(issue_key)
        .fetch_optional(self.db.pool())
        .await
        .context("read the launch's cursor file")?;
        let Some((Some(path), Some(value))) = stored else {
            return Ok(None);
        };
        let path = crate::launches::model::PackPath::parse(&path)
            .map_err(|e| anyhow::anyhow!("stored cursor path for {issue_key}: {e}"))?
            .under(pack_dir);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        tokio::fs::write(&path, value.as_bytes())
            .await
            .with_context(|| format!("writing the cursor file {}", path.display()))?;
        tracing::info!(issue_key, path = %path.display(), bytes = value.len(), "schedules: cursor file staged into the pack");
        Ok(Some(path))
    }
}

/// The most a file cursor may hold. One bounded blob per schedule, not a database: a pack that
/// lets its state grow past this stops advancing and is told so in the event log.
pub(crate) const CURSOR_FILE_MAX_BYTES: usize = 256 * 1024;

/// `sha256:<hex>` of `bytes`, for an event note that names a file without printing it.
pub(crate) fn content_digest(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    format!("sha256:{:x}", sha2::Sha256::digest(bytes))
}

#[derive(sqlx::FromRow)]
struct CursorTarget {
    id: String,
    cursor_from: Option<String>,
    cursor_param: Option<String>,
    cursor_path: Option<String>,
}

/// What to schedule: the authorization every firing launches with, plus when it recurs.
#[derive(Debug, Clone)]
pub(crate) struct NewSchedule<'a> {
    pub standing: NewStanding<'a>,
    /// The recurring-input cursor. `None` leaves every firing launching the stored values and
    /// the pack's own files unchanged.
    pub cursor: Option<&'a CursorSpec>,
    pub spec: &'a CronSpec,
}

/// One schedule: its standing authorization and its recurrence, flattened.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Schedule {
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
    pub cron_expr: String,
    pub tz: String,
    pub enabled: bool,
    /// The cursor as stored, or `None` when this schedule declares none (or stored a path this
    /// binary can no longer parse).
    pub cursor: Option<CursorSpec>,
    /// The value the last successful firing left behind, `None` until one does. Read-only on the
    /// wire: only a completed run writes it.
    pub cursor_value: Option<String>,
    pub cursor_updated_at: Option<String>,
    /// When it fires next; `None` while disabled, while a sweep holds it claimed, or when the
    /// expression has no further occurrence.
    pub next_due_at: Option<String>,
    pub last_fired_at: Option<String>,
    pub consecutive_failures: i64,
    pub created_by: Option<String>,
    pub owner_principal: Option<String>,
    pub owner_groups: Option<serde_json::Value>,
    pub owner_groups_at: Option<String>,
    pub owner_signin_required: bool,
    pub owner_refresh_error: Option<String>,
    pub owner_refresh_at: Option<String>,
    pub dispatch_target: Option<String>,
    pub agent_provider: Option<String>,
    pub agent_model: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// The column set every read projects: the core row beside the sidecar.
#[derive(sqlx::FromRow)]
struct ScheduleRow {
    #[sqlx(flatten)]
    core: Standing,
    cron_expr: String,
    tz: String,
    cursor_from: Option<String>,
    cursor_param: Option<String>,
    cursor_path: Option<String>,
    cursor_value: Option<String>,
    cursor_updated_at: Option<String>,
    next_due_at: Option<String>,
    last_fired_at: Option<String>,
}

impl From<ScheduleRow> for Schedule {
    fn from(row: ScheduleRow) -> Schedule {
        let c = row.core;
        Schedule {
            id: c.id,
            playbook: c.playbook,
            target_kind: c.target_kind,
            adopted_repo: c.adopted_repo,
            adopted_path: c.adopted_path,
            adopted_rev: c.adopted_rev,
            eligible_draft_version: c.eligible_draft_version,
            params: c.params,
            schema_digest: c.schema_digest,
            max_cost: c.max_cost,
            max_time: c.max_time,
            advance_dedupe: c.advance_dedupe,
            cron_expr: row.cron_expr,
            tz: row.tz,
            enabled: c.enabled,
            cursor: CursorSpec::from_columns(
                row.cursor_from.as_deref(),
                row.cursor_param.as_deref(),
                row.cursor_path.as_deref(),
            ),
            cursor_value: row.cursor_value,
            cursor_updated_at: row.cursor_updated_at,
            next_due_at: row.next_due_at,
            last_fired_at: row.last_fired_at,
            consecutive_failures: c.consecutive_failures,
            created_by: c.created_by,
            owner_principal: c.owner_principal,
            owner_groups: c.owner_groups,
            owner_groups_at: c.owner_groups_at,
            owner_signin_required: c.owner_signin_required,
            owner_refresh_error: c.owner_refresh_error,
            owner_refresh_at: c.owner_refresh_at,
            dispatch_target: c.dispatch_target,
            agent_provider: c.agent_provider,
            agent_model: c.agent_model,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }
    }
}

const SELECT: &str = const_format::concatcp!(
    "SELECT ",
    standing::COLUMNS,
    ", s.cron_expr, s.tz, s.cursor_from, s.cursor_param, \
    s.cursor_path, s.cursor_value, s.cursor_updated_at, s.next_due_at, s.last_fired_at \
    FROM playbook_standing_launches c JOIN playbook_schedules s USING (id)"
);

impl ScheduleStore {
    /// Store a schedule. The values were validated against the pack's stored schema and the
    /// ceilings bounded by the admin caps at the endpoint, exactly as for an immediate launch.
    pub(crate) async fn create(&self, new: &NewSchedule<'_>, now: Timestamp) -> Result<Schedule> {
        let id = uuid::Uuid::now_v7().to_string();
        let created_at = crate::clock::now_rfc3339();
        let mut tx = self
            .db
            .pool()
            .begin()
            .await
            .context("create schedule: begin")?;
        standing::insert(&mut tx, &id, Trigger::Schedule, &new.standing, &created_at).await?;
        sqlx::query(
            "INSERT INTO playbook_schedules
                 (id, cron_expr, tz, next_due_at, cursor_from, cursor_param, cursor_path)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&id)
        .bind(new.spec.expr())
        .bind(new.spec.tz_name())
        .bind(self.due_after(new.spec, new.standing.enabled, now))
        .bind(new.cursor.map(|c| c.source()))
        .bind(new.cursor.and_then(|c| c.param()))
        .bind(new.cursor.and_then(|c| c.path()))
        .execute(&mut *tx)
        .await
        .context("create schedule")?;
        tx.commit().await.context("create schedule: commit")?;
        self.get(&id)
            .await?
            .context("the schedule just stored is gone")
    }

    /// Replace a schedule's authorization and recurrence. The next firing is recomputed from `now`
    /// and the failure count starts over: an edited schedule is not still N failures deep into the
    /// run it used to be. Repointing the cursor or the playbook drops the stored value: it was
    /// collected from a different pack's result, and no other pack's schema is bound to accept it.
    pub(crate) async fn update(
        &self,
        id: &str,
        new: &NewSchedule<'_>,
        now: Timestamp,
    ) -> Result<Option<Schedule>> {
        let updated_at = crate::clock::now_rfc3339();
        let mut tx = self
            .db
            .pool()
            .begin()
            .await
            .context("update schedule: begin")?;
        let prior_playbook: Option<String> =
            sqlx::query_scalar("SELECT playbook FROM playbook_standing_launches WHERE id = $1")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .context("update schedule: prior playbook")?;
        let Some(prior_playbook) = prior_playbook else {
            return Ok(None);
        };
        if !standing::replace(&mut tx, id, &new.standing, &updated_at).await? {
            return Ok(None);
        }
        let cursor_from = new.cursor.map(|c| c.source());
        let cursor_param = new.cursor.and_then(|c| c.param());
        let cursor_path = new.cursor.and_then(|c| c.path());
        let repointed = prior_playbook != new.standing.playbook;
        sqlx::query(
            "UPDATE playbook_schedules
             SET cron_expr = $2, tz = $3, next_due_at = $4,
                 cursor_from = $5, cursor_param = $6, cursor_path = $7,
                 cursor_value = CASE WHEN $8 OR cursor_from IS DISTINCT FROM $5
                                       OR cursor_param IS DISTINCT FROM $6
                                       OR cursor_path IS DISTINCT FROM $7
                                     THEN NULL ELSE cursor_value END,
                 cursor_updated_at = CASE WHEN $8 OR cursor_from IS DISTINCT FROM $5
                                            OR cursor_param IS DISTINCT FROM $6
                                            OR cursor_path IS DISTINCT FROM $7
                                          THEN NULL ELSE cursor_updated_at END
             WHERE id = $1",
        )
        .bind(id)
        .bind(new.spec.expr())
        .bind(new.spec.tz_name())
        .bind(self.due_after(new.spec, new.standing.enabled, now))
        .bind(cursor_from)
        .bind(cursor_param)
        .bind(cursor_path)
        .bind(repointed)
        .execute(&mut *tx)
        .await
        .context("update schedule")?;
        tx.commit().await.context("update schedule: commit")?;
        self.get(id).await
    }

    /// Every schedule, soonest-due first among the enabled ones.
    pub(crate) async fn list(&self, limit: i64) -> Result<Vec<Schedule>> {
        let sql = const_format::concatcp!(
            SELECT,
            " ORDER BY c.enabled DESC, s.next_due_at NULLS LAST, c.id LIMIT $1"
        );
        let rows = sqlx::query_as::<_, ScheduleRow>(sql)
            .bind(limit)
            .fetch_all(self.db.pool())
            .await
            .context("list schedules")?;
        Ok(rows.into_iter().map(Schedule::from).collect())
    }

    /// One schedule by id, or `None`.
    pub(crate) async fn get(&self, id: &str) -> Result<Option<Schedule>> {
        let sql = const_format::concatcp!(SELECT, " WHERE c.id = $1");
        let row = sqlx::query_as::<_, ScheduleRow>(sql)
            .bind(id)
            .fetch_optional(self.db.pool())
            .await
            .context("get schedule")?;
        Ok(row.map(Schedule::from))
    }

    /// Delete a schedule. `false` when there was none under that id. The launches it already fired
    /// are ordinary runs and stay.
    pub(crate) async fn delete(&self, id: &str) -> Result<bool> {
        standing::delete(self.db.pool(), id).await
    }

    /// Disable every mutable recurring target when the administrator restores adopted-only policy.
    pub(crate) async fn disable_draft_head_schedules(&self, reason: &str) -> Result<u64> {
        let now = crate::clock::now_rfc3339();
        let mut tx = self
            .db
            .pool()
            .begin()
            .await
            .context("begin policy disable")?;
        let ids: Vec<String> = sqlx::query_scalar(
            "WITH off AS (
                 UPDATE playbook_standing_launches SET enabled = false, updated_at = $1
                 WHERE trigger = 'schedule' AND target_kind = 'draft_head' AND enabled RETURNING id
             )
             UPDATE playbook_schedules s SET next_due_at = NULL FROM off WHERE s.id = off.id
             RETURNING s.id",
        )
        .bind(&now)
        .fetch_all(&mut *tx)
        .await
        .context("disable draft-head schedules")?;
        for id in &ids {
            let key = Trigger::Schedule.event_key(id);
            let event = Event::now(&key, "enabled", "disabled", Some(reason), Some(id));
            crate::event_log::insert(&mut *tx, &event).await?;
        }
        tx.commit().await.context("commit policy disable")?;
        for id in &ids {
            let key = Trigger::Schedule.event_key(id);
            self.db.events().publish(&Event::now(
                &key,
                "enabled",
                "disabled",
                Some(reason),
                Some(id),
            ));
        }
        Ok(ids.len() as u64)
    }

    /// Expire the launches a schedule parked for a stale owner snapshot. Returns the keys closed.
    pub(crate) async fn expire_stale_owner_parks(&self, schedule_id: &str) -> Result<Vec<String>> {
        standing::expire_stale_owner_parks(self.db.pool(), Trigger::Schedule, schedule_id).await
    }

    async fn due_rows(&self, now: Timestamp) -> Result<Vec<DueRow>> {
        sqlx::query_as::<_, DueRow>(
            r#"
            SELECT s.id, s.cron_expr, s.tz, s.cursor_param, s.cursor_value
            FROM playbook_schedules s
            JOIN playbook_standing_launches c USING (id)
            WHERE c.enabled AND s.next_due_at IS NOT NULL AND s.next_due_at <= $1
            ORDER BY s.next_due_at, s.id LIMIT $2
            "#,
        )
        .bind(stamp(now))
        .bind(FIRE_CAP)
        .fetch_all(self.db.pool())
        .await
        .context("the due schedules")
    }

    async fn claim_due(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        claim: &mut Claim,
        now: Timestamp,
    ) -> Result<bool> {
        // The window being fired is read under the row lock before the claim nulls it; a
        // second sweep re-checks the predicate after the lock and finds nothing.
        let due_at: Option<String> = sqlx::query_scalar(
            "WITH prior AS (
                 SELECT id, next_due_at FROM playbook_schedules
                 WHERE id = $1 AND next_due_at IS NOT NULL AND next_due_at <= $2
                 FOR UPDATE
             )
             UPDATE playbook_schedules s SET next_due_at = NULL, last_fired_at = $2
             FROM prior WHERE s.id = prior.id
             RETURNING prior.next_due_at",
        )
        .bind(&claim.id)
        .bind(stamp(now))
        .fetch_optional(&mut **tx)
        .await
        .context("claim due schedule")?;
        let Some(due_at) = due_at else {
            return Ok(false);
        };
        claim.payload["due_at"] = serde_json::Value::String(due_at);
        Ok(true)
    }

    async fn settle(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        claim: &Claim,
        key: &str,
        now: Timestamp,
    ) -> Result<Vec<Recorded>> {
        let spec = self.spec_of(claim)?;
        let next_due_at = spec.next_after(now);
        let due_at: Timestamp = claim
            .payload
            .get("due_at")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .parse()
            .with_context(|| format!("schedule {} stored next_due_at", claim.id))?;
        let missed = spec.firings_between(due_at, now, MISSED_CAP);
        sqlx::query("UPDATE playbook_schedules SET next_due_at = $2 WHERE id = $1")
            .bind(&claim.id)
            .bind(next_due_at.map(stamp))
            .execute(&mut **tx)
            .await
            .context("recompute the next window")?;
        tracing::info!(
            schedule = %claim.id,
            missed_windows = missed,
            next_due_at = ?next_due_at.map(stamp),
            "schedules: firing launched"
        );
        let mut events = Vec::new();
        if missed > 0 {
            events.push(Recorded {
                key: Trigger::Schedule.event_key(&claim.id),
                from: "due",
                to: "skipped",
                reason: Some(format!(
                    "skipped {missed} missed window(s) between {} and {}",
                    stamp(due_at),
                    stamp(now)
                )),
                evidence: Some(key.to_string()),
                actor: None,
            });
        }
        Ok(events)
    }

    async fn fail(&self, claim: &Claim, now: Timestamp) -> Result<Failed> {
        // The schedule waits at its next window; a spec with none left has nothing to wait for.
        let next_due_at = self
            .spec_of(claim)
            .ok()
            .and_then(|spec| spec.next_after(now));
        sqlx::query("UPDATE playbook_schedules SET next_due_at = $2 WHERE id = $1")
            .bind(&claim.id)
            .bind(next_due_at.map(stamp))
            .execute(self.db.pool())
            .await
            .context("record schedule failure")?;
        Ok(Failed {
            force_disable: next_due_at.is_none(),
            announced: false,
        })
    }

    async fn retire(&self, claim: &Claim) -> Result<()> {
        sqlx::query("UPDATE playbook_schedules SET next_due_at = NULL WHERE id = $1")
            .bind(&claim.id)
            .execute(self.db.pool())
            .await
            .context("retire the disabled schedule")?;
        Ok(())
    }
}

/// The cron trigger: a due window is a firing. The claim nulls `next_due_at`, so the row is
/// invisible to every other sweep until this transaction either commits the recomputed firing or
/// rolls the claim back; windows the controller slept through are counted, never replayed.
pub struct ScheduleTrigger {
    store: ScheduleStore,
    config: Option<crate::daemon::overrides_store::ConfigStore>,
    /// Whether a pass enforces the recurring-playbook policy first. The daemon's trigger does; a
    /// test driving firings directly does not, exactly as the sweep it replaced did not.
    enforce_policy: bool,
}

impl ScheduleTrigger {
    pub fn new(db: Db, config: Option<crate::daemon::overrides_store::ConfigStore>) -> Self {
        ScheduleTrigger {
            store: ScheduleStore::new(db),
            config,
            enforce_policy: true,
        }
    }

    #[cfg(test)]
    fn unpoliced(db: Db) -> Self {
        ScheduleTrigger {
            store: ScheduleStore::new(db),
            config: None,
            enforce_policy: false,
        }
    }

    #[cfg(test)]
    fn sweep_cfg(auto_disable_after: i64, owner_ttl: std::time::Duration) -> SweepCfg {
        SweepCfg {
            auto_disable_after,
            owner_ttl,
        }
    }
}

#[derive(sqlx::FromRow)]
struct DueRow {
    id: String,
    cron_expr: String,
    tz: String,
    cursor_param: Option<String>,
    cursor_value: Option<String>,
}

impl LaunchTrigger for ScheduleTrigger {
    fn trigger(&self) -> Trigger {
        Trigger::Schedule
    }

    fn due<'a>(
        &'a self,
        _db: &'a Db,
        _cfg: SweepCfg,
        now: Timestamp,
    ) -> TriggerFuture<'a, Result<Vec<Claim>>> {
        let store = self.store.clone();
        let config = self.config.clone();
        let enforce_policy = self.enforce_policy;
        Box::pin(async move {
            let _policy_operation = match config.as_ref() {
                Some(store) => Some(store.recurring_policy_read().await),
                None => None,
            };
            if enforce_policy
                && !config
                    .as_ref()
                    .is_some_and(|store| store.effective().allow_draft_head_schedules)
            {
                store
                    .disable_draft_head_schedules(
                        "disabled by adopted-only recurring-playbook policy",
                    )
                    .await?;
            }
            let rows = store.due_rows(now).await?;
            Ok(rows
                .into_iter()
                .map(|row| {
                    let mut claim = Claim::new(&row.id, format!("schedule {} fired", row.id));
                    claim.dedupe_schedule = Some(row.id.clone());
                    claim.overlay = match (row.cursor_param, row.cursor_value) {
                        (Some(param), Some(value)) => Some((param, value)),
                        _ => None,
                    };
                    claim.payload = serde_json::json!({"cron_expr": row.cron_expr, "tz": row.tz});
                    claim
                })
                .collect())
        })
    }

    fn claim<'a, 'c>(
        &'a self,
        tx: &'a mut sqlx::Transaction<'c, sqlx::Postgres>,
        claim: &'a mut Claim,
        now: Timestamp,
    ) -> TriggerFuture<'a, Result<bool>> {
        Box::pin(async move { self.store.claim_due(tx, claim, now).await })
    }

    fn settle<'a, 'c>(
        &'a self,
        tx: &'a mut sqlx::Transaction<'c, sqlx::Postgres>,
        claim: &'a Claim,
        key: &'a str,
        now: Timestamp,
    ) -> TriggerFuture<'a, Result<Vec<Recorded>>> {
        Box::pin(async move { self.store.settle(tx, claim, key, now).await })
    }

    fn fail<'a>(
        &'a self,
        _db: &'a Db,
        claim: &'a Claim,
        _message: &'a str,
        now: Timestamp,
    ) -> TriggerFuture<'a, Result<Failed>> {
        Box::pin(async move { self.store.fail(claim, now).await })
    }

    fn retire<'a>(&'a self, _db: &'a Db, claim: &'a Claim) -> TriggerFuture<'a, Result<()>> {
        Box::pin(async move { self.store.retire(claim).await })
    }
}

/// [`fire_due_refreshing`] with no offline credential: every firing launches under the
/// schedule-row snapshot alone, which is what a deployment with no issuer configured does.
#[cfg(test)]
pub(crate) async fn fire_due(
    db: &Db,
    now: Timestamp,
    cap: usize,
    auto_disable_after: i64,
    owner_ttl: std::time::Duration,
) -> Result<Vec<String>> {
    fire_due_refreshing(db, now, cap, auto_disable_after, owner_ttl, None).await
}

/// Fire every schedule due at `now`, up to `cap`, through the generic sweep. Returns the keys the
/// launches landed under.
#[cfg(test)]
pub(crate) async fn fire_due_refreshing(
    db: &Db,
    now: Timestamp,
    cap: usize,
    auto_disable_after: i64,
    owner_ttl: std::time::Duration,
    refresh: Option<&crate::identity::oidc::credentials::OwnerRefresh>,
) -> Result<Vec<String>> {
    let trigger = ScheduleTrigger::unpoliced(db.clone());
    let mut keys = standing::sweep(
        db,
        &trigger,
        ScheduleTrigger::sweep_cfg(auto_disable_after, owner_ttl),
        now,
        refresh,
        None,
    )
    .await?;
    keys.truncate(cap);
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::queue::DiscoverySource;
    use crate::daemon::queue::{Enqueue, IssueKey};
    use crate::launches::model::NewPlaybookLaunch;
    use crate::launches::standing::{clear_owner_signin, require_owner_signin};
    use crate::model::LaunchOrigin;
    use crate::model::MaxTime;
    use crate::model::Status;
    use sqlx::PgPool;
    use std::sync::Arc;

    /// A registry row without the clone-and-extract dance: a firing only reads `repo`,
    /// `description`, and the stored tarball it copies to the launch.
    async fn register_row(pool: &PgPool, id: &str) {
        sqlx::query(
            r#"INSERT INTO playbooks (id, description, repo, git_ref, rev, path, tar_gz,
                                      tar_digest, tar_bytes, params_schema, schema_digest,
                                      core_rev, created_by, created_at, updated_at)
               VALUES ($1, 'reads a paper', 'owner/packs', 'main', 'abc123', '', $2,
                       'sha256:tar', 3, '{"type":"object"}'::jsonb, 'sha256:schema', 'core1',
                       'wren', '2026-08-23T00:00:00Z', '2026-08-23T00:00:00Z')"#,
        )
        .bind(id)
        .bind(vec![1u8, 2, 3])
        .execute(pool)
        .await
        .expect("register");
    }

    fn params() -> serde_json::Value {
        serde_json::json!({"topic": "attention sinks"})
    }

    fn ts(raw: &str) -> Timestamp {
        raw.parse().expect(raw)
    }

    /// A schedule created "now", then backdated to a due window — the sweep only ever reads
    /// `next_due_at`, so this is how a test puts a row in the past without waiting for one.
    async fn schedule_due_at(pool: &PgPool, expr: &str, due: Option<&str>) -> Schedule {
        let max_time = MaxTime::parse("30m").expect("duration");
        let spec = CronSpec::parse(expr, "UTC").expect("expr");
        let stored = ScheduleStore::new(Db::new(pool.clone()))
            .create(
                &NewSchedule {
                    standing: NewStanding {
                        playbook: "survey",
                        target_kind: "adopted",
                        eligible_draft_version: None,
                        params: &params(),
                        schema_digest: "sha256:schema",
                        max_cost: 3.5,
                        max_time: &max_time,
                        advance_dedupe: true,
                        enabled: true,
                        created_by: Some("wren"),
                        owner_principal: None,
                        owner_groups: None,
                        dispatch_target: None,
                        agent_provider: None,
                        agent_model: None,
                    },
                    cursor: None,
                    spec: &spec,
                },
                Timestamp::now(),
            )
            .await
            .expect("create");
        sqlx::query("UPDATE playbook_schedules SET next_due_at = $2 WHERE id = $1")
            .bind(&stored.id)
            .bind(due)
            .execute(pool)
            .await
            .expect("backdate");
        ScheduleStore::new(Db::new(pool.clone()))
            .get(&stored.id)
            .await
            .expect("get")
            .expect("row")
    }

    /// A schedule adopts bytes once. Moving the registry pin afterward affects new schedules, not
    /// the launch this existing recurrence materializes.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_schedule_fires_the_revision_it_adopted_before_a_repin(pool: PgPool) -> Result<()> {
        register_row(&pool, "survey").await;
        let scheduled = schedule_due_at(&pool, "0 * * * *", Some("2026-08-23T12:00:00Z")).await;
        assert_eq!(scheduled.adopted_rev.as_deref(), Some("abc123"));
        sqlx::query(
            "UPDATE playbooks SET rev = 'def456', tar_gz = $2, tar_digest = 'sha256:new', \
             tar_bytes = 4 WHERE id = $1",
        )
        .bind("survey")
        .bind(vec![9u8, 8, 7, 6])
        .execute(&pool)
        .await?;

        let db = Db::new(pool.clone());
        let keys = fire_due(
            &db,
            ts("2026-08-23T12:01:00Z"),
            1,
            5,
            std::time::Duration::from_secs(3600),
        )
        .await?;
        assert_eq!(keys.len(), 1);
        let slug = crate::model::sanitize_key(&keys[0]);
        let digest: String =
            sqlx::query_scalar("SELECT digest FROM pack_tarballs WHERE issue_slug = $1")
                .bind(slug)
                .fetch_one(&pool)
                .await?;
        assert_eq!(digest, "sha256:tar");
        Ok(())
    }

    /// A firing carries the dispatch choice the save made onto the issue it mints: a schedule
    /// fires long after its author is gone, so the pair travels on the row rather than being
    /// resolved against nobody. Stored through create/update and read back off the minted issue,
    /// which is the whole chain a dropped column in the claim SELECT would break.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_firing_pins_the_pair_the_save_chose(pool: PgPool) -> Result<()> {
        register_row(&pool, "survey").await;
        let max_time = MaxTime::parse("30m").expect("duration");
        let spec = CronSpec::parse("0 * * * *", "UTC").expect("expr");
        let mut new = NewSchedule {
            standing: NewStanding {
                playbook: "survey",
                target_kind: "adopted",
                eligible_draft_version: None,
                params: &params(),
                schema_digest: "sha256:schema",
                max_cost: 3.5,
                max_time: &max_time,
                advance_dedupe: true,
                enabled: true,
                created_by: Some("wren"),
                owner_principal: None,
                owner_groups: None,
                dispatch_target: None,
                agent_provider: Some("plat-openai"),
                agent_model: Some("gpt-5.6-sol"),
            },
            cursor: None,
            spec: &spec,
        };
        let stored = ScheduleStore::new(Db::new(pool.clone()))
            .create(&new, Timestamp::now())
            .await?;
        assert_eq!(stored.agent_provider.as_deref(), Some("plat-openai"));
        assert_eq!(stored.agent_model.as_deref(), Some("gpt-5.6-sol"));

        new.standing.agent_model = Some("gpt-5.6-luna");
        let edited = ScheduleStore::new(Db::new(pool.clone()))
            .update(&stored.id, &new, Timestamp::now())
            .await?
            .expect("the row");
        assert_eq!(edited.agent_model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(
            ScheduleStore::new(Db::new(pool.clone()))
                .get(&stored.id)
                .await?
                .expect("row")
                .agent_model,
            Some("gpt-5.6-luna".to_string()),
            "the read-back SELECT carries the pair, not just the write"
        );

        sqlx::query("UPDATE playbook_schedules SET next_due_at = $2 WHERE id = $1")
            .bind(&stored.id)
            .bind("2026-08-23T12:00:00Z")
            .execute(&pool)
            .await?;
        let db = Db::new(pool.clone());
        let keys = fire_due(
            &db,
            ts("2026-08-23T12:01:00Z"),
            1,
            5,
            std::time::Duration::from_secs(3600),
        )
        .await?;
        assert_eq!(keys.len(), 1);
        let issue = crate::issues::store::get_issue(&pool, &keys[0])
            .await?
            .expect("the minted issue");
        assert_eq!(issue.agent_provider.as_deref(), Some("plat-openai"));
        assert_eq!(issue.agent_model.as_deref(), Some("gpt-5.6-luna"));
        Ok(())
    }

    /// A draft-head firing freezes the draft version it claimed, and records that version's
    /// recomputed exposure on the launch row, the way a manual draft launch does: a draft has no
    /// registry row for the dispatch to fall back to, so a launch without one can never resolve a
    /// secret the pack declares.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_draft_head_firing_freezes_the_version_it_claimed(pool: PgPool) -> Result<()> {
        let saved = crate::playbooks::drafts::create(
            &pool,
            "studio",
            "iterative pack",
            crate::playbooks::drafts::DraftSeed::Skeleton,
            Some("wren"),
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("draft");
        let schema_digest = saved.schema_digest.clone().expect("the skeleton compiles");
        let max_time = MaxTime::parse("30m").expect("duration");
        let spec = CronSpec::parse("0 * * * *", "UTC").expect("expr");
        let schedule = ScheduleStore::new(Db::new(pool.clone()))
            .create(
                &NewSchedule {
                    standing: NewStanding {
                        playbook: "studio",
                        target_kind: "draft_head",
                        eligible_draft_version: Some(saved.version),
                        params: &serde_json::json!({}),
                        schema_digest: &schema_digest,
                        max_cost: 1.0,
                        max_time: &max_time,
                        advance_dedupe: false,
                        enabled: true,
                        created_by: Some("wren"),
                        owner_principal: None,
                        owner_groups: None,
                        dispatch_target: None,
                        agent_provider: None,
                        agent_model: None,
                    },
                    cursor: None,
                    spec: &spec,
                },
                ts("2026-08-23T11:00:00Z"),
            )
            .await?;
        sqlx::query("UPDATE playbook_schedules SET next_due_at = $2 WHERE id = $1")
            .bind(&schedule.id)
            .bind("2026-08-23T12:00:00Z")
            .execute(&pool)
            .await?;

        let db = Db::new(pool.clone());
        let keys = fire_due(
            &db,
            ts("2026-08-23T12:01:00Z"),
            1,
            5,
            std::time::Duration::from_secs(3600),
        )
        .await?;
        let launch = crate::launches::store::get_playbook_launch(&pool, &keys[0])
            .await?
            .expect("the minted launch");
        assert_eq!(launch.draft_version, Some(saved.version));
        assert!(
            launch.exposure.is_some(),
            "a draft-head launch carries its version's exposure"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn adopted_only_policy_disables_mutable_schedules(pool: PgPool) -> Result<()> {
        sqlx::query(
            "INSERT INTO playbook_standing_launches \
             (id, trigger, playbook, params, schema_digest, max_cost, max_time, advance_dedupe, \
              enabled, target_kind, created_at, updated_at) VALUES \
             ('mutable', 'schedule', 'studio', '{}'::jsonb, 'sha256:schema', 1, '5m', false, \
              true, 'draft_head', $1, $1)",
        )
        .bind("2026-08-23T00:00:00Z")
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO playbook_schedules (id, cron_expr, tz, next_due_at) \
             VALUES ('mutable', '* * * * *', 'UTC', $1)",
        )
        .bind("2026-08-23T00:00:00Z")
        .execute(&pool)
        .await?;
        let db = Db::new(pool.clone());
        assert_eq!(
            ScheduleStore::new(db.clone())
                .disable_draft_head_schedules("policy tightened")
                .await?,
            1
        );
        let enabled: bool = sqlx::query_scalar(
            "SELECT enabled FROM playbook_standing_launches WHERE id = 'mutable'",
        )
        .fetch_one(&pool)
        .await?;
        assert!(!enabled);
        Ok(())
    }

    /// Bind a secret to the playbook scope, so a firing needs an owner snapshot to launch under.
    async fn bind_playbook_secret(pool: &PgPool, playbook: &str) {
        use crate::authz::model::Principal;
        use crate::secrets::store::{NewBinding, NewSecret};
        use crate::secrets::{
            ConsumerClass, ProjectionKind, ScopeKind, SecretKind, SecretMode, SecretName,
            Visibility,
        };
        let owner = Principal::parse("user:wren").expect("owner");
        let name = SecretName::parse("pr_token").expect("name");
        let id = uuid::Uuid::now_v7().to_string();
        let mut conn = pool.acquire().await.expect("conn");
        crate::secrets::store::insert(
            &mut conn,
            &NewSecret {
                id: &id,
                name: &name,
                owner: &owner,
                kind: SecretKind::Opaque,
                visibility: Visibility::BrokerOnly,
                consumer: ConsumerClass::Run,
                mode: SecretMode::Managed,
                vault_path: "user:wren/pr-token",
                current_version: Some(1),
                created_by: Some("wren"),
            },
        )
        .await
        .expect("register");
        crate::secrets::store::insert_binding(
            &mut conn,
            &NewBinding {
                id: &uuid::Uuid::now_v7().to_string(),
                secret_id: &id,
                scope_kind: ScopeKind::Playbook,
                scope_id: playbook,
                projection_kind: ProjectionKind::Env,
                projection: "AUTORESEARCH_PR_TOKEN",
                declared_name: &name,
                pack_rev: None,
                schema_digest: None,
                created_by: Some("wren"),
            },
        )
        .await
        .expect("bind");
    }

    /// Own a stored schedule to `principal` with `groups`, snapshotted at `at`.
    async fn own_schedule(pool: &PgPool, id: &str, principal: &str, groups: &[&str], at: &str) {
        let groups = serde_json::Value::from(groups.to_vec());
        sqlx::query(
            "UPDATE playbook_standing_launches
             SET owner_principal = $2, owner_groups = $3, owner_groups_at = $4 WHERE id = $1",
        )
        .bind(id)
        .bind(principal)
        .bind(&groups)
        .bind(at)
        .execute(pool)
        .await
        .expect("own");
    }

    /// A schedule saved inside its TTL fires and its launch carries the snapshot the save took, so
    /// the dispatch has the memberships it needs long after that session is gone.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_fresh_owner_snapshot_rides_onto_the_launch(pool: PgPool) -> Result<()> {
        register_row(&pool, "survey").await;
        bind_playbook_secret(&pool, "survey").await;
        let row = schedule_due_at(&pool, "0 * * * *", Some("2026-08-23T12:00:00Z")).await;
        own_schedule(
            &pool,
            &row.id,
            "user:wren",
            &["/groups/team-x"],
            "2026-08-23T12:00:00Z",
        )
        .await;
        let db = Db::new(pool.clone());
        let fired = fire_due(
            &db,
            ts("2026-08-23T12:00:30Z"),
            8,
            5,
            std::time::Duration::from_secs(3600),
        )
        .await?;
        assert_eq!(fired.len(), 1, "the firing launched");
        let launch = crate::launches::store::get_playbook_launch(&pool, &fired[0])
            .await?
            .expect("a launch row");
        assert_eq!(launch.created_by.as_deref(), Some("wren"));
        assert_eq!(launch.launcher_groups, vec!["/groups/team-x".to_string()]);
        Ok(())
    }

    /// Past the TTL there is no authorization left to launch a bound scope under, so the firing
    /// parks with the reason rather than dispatching. A re-save re-owns the row and expires it.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_fire_past_the_ttl_parks_and_a_resave_expires_it(pool: PgPool) -> Result<()> {
        register_row(&pool, "survey").await;
        bind_playbook_secret(&pool, "survey").await;
        let row = schedule_due_at(&pool, "0 * * * *", Some("2026-08-23T12:00:00Z")).await;
        own_schedule(
            &pool,
            &row.id,
            "user:wren",
            &["/groups/team-x"],
            "2026-08-01T00:00:00Z",
        )
        .await;
        let db = Db::new(pool.clone());
        let fired = fire_due(
            &db,
            ts("2026-08-23T12:00:30Z"),
            8,
            5,
            std::time::Duration::from_secs(3600),
        )
        .await?;
        assert!(fired.is_empty(), "a stale snapshot dispatches nothing");
        let parked: Vec<(String, String)> = sqlx::query_as(
            "SELECT key, parked_reason FROM issues WHERE status = 'parked' AND input_kind = 'playbook'",
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(parked.len(), 1, "the firing's launch parked");
        assert!(
            parked[0].1.contains(&row.id) && parked[0].1.contains("stale"),
            "the park names the schedule and why: {:?}",
            parked[0].1
        );

        // A re-save re-owns the row and closes the firings that could never launch under the old
        // snapshot.
        let expired = ScheduleStore::new(Db::new(pool.clone()))
            .expire_stale_owner_parks(&row.id)
            .await?;
        assert_eq!(expired, vec![parked[0].0.clone()]);
        let status: String = sqlx::query_scalar("SELECT status FROM issues WHERE key = $1")
            .bind(&parked[0].0)
            .fetch_one(&pool)
            .await?;
        assert_eq!(status, Status::Done.as_str());
        Ok(())
    }

    /// The offline credential of an owner whose issuer cannot be reached. A real dead endpoint,
    /// not a stub: the transport failure the sweep has to treat as transient is the genuine one.
    async fn dead_refresher(
        pool: &PgPool,
        login: &str,
    ) -> Arc<crate::identity::oidc::credentials::OwnerRefresh> {
        let keys = Arc::new(
            crate::identity::oidc::credentials::CredentialKeys::new(vec![vec![3u8; 32]])
                .expect("keys"),
        );
        let sub = format!("sub-{login}");
        crate::identity::oidc::users::record_login(pool, &sub, login, None, Timestamp::now())
            .await
            .expect("record login");
        let mut conn = pool.acquire().await.expect("conn");
        crate::identity::oidc::credentials::upsert(
            &mut conn,
            &keys,
            &sub,
            "offline",
            Timestamp::now(),
        )
        .await
        .expect("store the credential");
        drop(conn);
        let provider = Arc::new(
            crate::identity::oidc::OidcProvider::new(crate::identity::oidc::OidcCfg {
                issuer: "http://127.0.0.1:1/realms/nobody".to_string(),
                client_id: "rp".to_string(),
                client_secret: None,
                redirect_url: "http://localhost/auth/callback".to_string(),
                scopes: vec!["openid".to_string()],
                device_client_id: None,
                post_logout_redirect: None,
            })
            .expect("provider"),
        );
        Arc::new(crate::identity::oidc::credentials::OwnerRefresh::new(
            pool.clone(),
            provider,
            keys,
        ))
    }

    async fn schedule_state(
        pool: &PgPool,
        id: &str,
    ) -> (bool, Option<String>, i64, Option<String>) {
        let row = ScheduleStore::new(Db::new(pool.clone()))
            .get(id)
            .await
            .expect("get")
            .expect("row");
        (
            row.owner_signin_required,
            row.owner_refresh_error,
            row.consecutive_failures,
            row.next_due_at,
        )
    }

    /// An issuer that cannot be reached is transient: the firing is left for the next tick, nothing
    /// counts toward auto-disable, and the schedule stays due exactly where it was.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_unreachable_issuer_defers_the_firing_without_a_failure(pool: PgPool) -> Result<()> {
        register_row(&pool, "survey").await;
        bind_playbook_secret(&pool, "survey").await;
        let row = schedule_due_at(&pool, "0 * * * *", Some("2026-08-23T12:00:00Z")).await;
        own_schedule(
            &pool,
            &row.id,
            "user:wren",
            &["/groups/team-x"],
            "2026-08-23T12:00:00Z",
        )
        .await;
        let refresh = dead_refresher(&pool, "wren").await;
        let db = Db::new(pool.clone());
        let fired = fire_due_refreshing(
            &db,
            ts("2026-08-23T12:00:30Z"),
            8,
            5,
            std::time::Duration::from_secs(3600),
            Some(&refresh),
        )
        .await?;
        assert!(fired.is_empty(), "nothing fires while the issuer is down");
        let (signin, error, failures, next_due) = schedule_state(&pool, &row.id).await;
        assert!(!signin, "an outage is not a reason to demand a sign-in");
        assert!(error.is_some(), "the view still says why nothing fired");
        assert_eq!(failures, 0, "a transient failure counts nothing");
        assert_eq!(
            next_due.as_deref(),
            Some("2026-08-23T12:00:00Z"),
            "the window is still due, so the next tick retries it"
        );
        Ok(())
    }

    /// An owner with no offline credential at all is a definitive refusal. Inside the snapshot's
    /// TTL the firing still launches under it; the failure is only recorded.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_definitive_failure_falls_back_to_a_snapshot_inside_its_ttl(
        pool: PgPool,
    ) -> Result<()> {
        register_row(&pool, "survey").await;
        bind_playbook_secret(&pool, "survey").await;
        let row = schedule_due_at(&pool, "0 * * * *", Some("2026-08-23T12:00:00Z")).await;
        own_schedule(
            &pool,
            &row.id,
            "user:wren",
            &["/groups/team-x"],
            "2026-08-23T12:00:00Z",
        )
        .await;
        // The refresher exists, but this owner never signed in through it.
        let refresh = dead_refresher(&pool, "someone-else").await;
        let db = Db::new(pool.clone());
        let fired = fire_due_refreshing(
            &db,
            ts("2026-08-23T12:00:30Z"),
            8,
            5,
            std::time::Duration::from_secs(3600),
            Some(&refresh),
        )
        .await?;
        assert_eq!(fired.len(), 1, "the fresh snapshot still authorizes it");
        let launch = crate::launches::store::get_playbook_launch(&pool, &fired[0])
            .await?
            .expect("a launch row");
        assert_eq!(launch.launcher_groups, vec!["/groups/team-x".to_string()]);
        let (signin, error, _, _) = schedule_state(&pool, &row.id).await;
        assert!(
            !signin,
            "inside the TTL there is nothing to sign in for yet"
        );
        assert!(error.is_some(), "the refusal is still on the record");
        Ok(())
    }

    /// The same refusal past the TTL parks the firing and marks the schedule as needing sign-in.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_definitive_failure_past_the_ttl_parks_and_needs_a_sign_in(
        pool: PgPool,
    ) -> Result<()> {
        register_row(&pool, "survey").await;
        bind_playbook_secret(&pool, "survey").await;
        let row = schedule_due_at(&pool, "0 * * * *", Some("2026-08-23T12:00:00Z")).await;
        own_schedule(
            &pool,
            &row.id,
            "user:wren",
            &["/groups/team-x"],
            "2026-08-01T00:00:00Z",
        )
        .await;
        let refresh = dead_refresher(&pool, "someone-else").await;
        let db = Db::new(pool.clone());
        let fired = fire_due_refreshing(
            &db,
            ts("2026-08-23T12:00:30Z"),
            8,
            5,
            std::time::Duration::from_secs(3600),
            Some(&refresh),
        )
        .await?;
        assert!(
            fired.is_empty(),
            "nothing dispatches under a dead credential"
        );
        let (signin, error, _, _) = schedule_state(&pool, &row.id).await;
        assert!(signin, "the schedule needs its owner to sign in again");
        assert!(error.is_some());
        let parked: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM issues WHERE status = 'parked' AND input_kind = 'playbook'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(parked, 1, "the firing's launch parked");
        Ok(())
    }

    /// A revoke does not wait on the TTL: the owner asked for those firings to stop, so the next
    /// one parks even though the snapshot is minutes old. Signing in again clears it.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_revoke_parks_a_bound_schedule_until_the_owner_signs_in(pool: PgPool) -> Result<()> {
        register_row(&pool, "survey").await;
        bind_playbook_secret(&pool, "survey").await;
        let row = schedule_due_at(&pool, "0 * * * *", Some("2026-08-23T12:00:00Z")).await;
        own_schedule(
            &pool,
            &row.id,
            "user:wren",
            &["/groups/team-x"],
            "2026-08-23T12:00:00Z",
        )
        .await;
        let parked = require_owner_signin(&pool, "wren", "its owner revoked it").await?;
        assert_eq!(parked, 1, "the one schedule that binds a secret");

        let db = Db::new(pool.clone());
        let fired = fire_due(
            &db,
            ts("2026-08-23T12:00:30Z"),
            8,
            5,
            std::time::Duration::from_secs(3600),
        )
        .await?;
        assert!(fired.is_empty(), "a revoked owner dispatches nothing");
        assert!(schedule_state(&pool, &row.id).await.0);

        clear_owner_signin(&pool, "Wren").await?;
        let (signin, error, _, _) = schedule_state(&pool, &row.id).await;
        assert!(!signin, "signing in again re-arms the schedule");
        assert_eq!(error, None);
        Ok(())
    }

    /// A revoke leaves the schedules that launch nothing credentialed alone: the offline credential
    /// is only ever the authorization for a bound scope.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_revoke_leaves_an_unbound_schedule_firing(pool: PgPool) -> Result<()> {
        register_row(&pool, "survey").await;
        let row = schedule_due_at(&pool, "0 * * * *", Some("2026-08-23T12:00:00Z")).await;
        own_schedule(
            &pool,
            &row.id,
            "user:wren",
            &["/groups/team-x"],
            "2026-08-23T12:00:00Z",
        )
        .await;
        assert_eq!(
            require_owner_signin(&pool, "wren", "revoked").await?,
            0,
            "nothing to park: the scope binds no secret"
        );
        let db = Db::new(pool.clone());
        let fired = fire_due(
            &db,
            ts("2026-08-23T12:00:30Z"),
            8,
            5,
            std::time::Duration::from_secs(3600),
        )
        .await?;
        assert_eq!(fired.len(), 1);
        Ok(())
    }

    /// A scope that binds nothing keeps firing with no snapshot at all: the registry is not a new
    /// requirement on every schedule that predates it.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_schedule_on_an_unbound_scope_fires_without_a_snapshot(pool: PgPool) -> Result<()> {
        register_row(&pool, "survey").await;
        schedule_due_at(&pool, "0 * * * *", Some("2026-08-23T12:00:00Z")).await;
        let db = Db::new(pool.clone());
        let fired = fire_due(
            &db,
            ts("2026-08-23T12:00:30Z"),
            8,
            5,
            std::time::Duration::from_secs(3600),
        )
        .await?;
        assert_eq!(fired.len(), 1);
        Ok(())
    }

    /// A save owns the schedule to the saver and stamps when the snapshot was taken; a save by
    /// somebody else re-owns it.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_save_owns_the_schedule_to_the_saver(pool: PgPool) -> Result<()> {
        register_row(&pool, "survey").await;
        let max_time = MaxTime::parse("30m").expect("duration");
        let spec = CronSpec::parse("0 * * * *", "UTC").expect("expr");
        let alice_groups = serde_json::Value::from(vec!["/groups/team-x"]);
        let mut new = NewSchedule {
            standing: NewStanding {
                playbook: "survey",
                target_kind: "adopted",
                eligible_draft_version: None,
                params: &params(),
                schema_digest: "sha256:schema",
                max_cost: 3.5,
                max_time: &max_time,
                advance_dedupe: true,
                enabled: true,
                created_by: Some("alice"),
                owner_principal: Some("user:alice"),
                owner_groups: Some(&alice_groups),
                dispatch_target: None,
                agent_provider: None,
                agent_model: None,
            },
            cursor: None,
            spec: &spec,
        };
        let stored = ScheduleStore::new(Db::new(pool.clone()))
            .create(&new, Timestamp::now())
            .await?;
        assert_eq!(stored.owner_principal.as_deref(), Some("user:alice"));
        assert!(stored.owner_groups_at.is_some(), "the snapshot is stamped");

        let bob_groups = serde_json::Value::from(vec!["/groups/team-y"]);
        new.standing.created_by = Some("bob");
        new.standing.owner_principal = Some("user:bob");
        new.standing.owner_groups = Some(&bob_groups);
        let resaved = ScheduleStore::new(Db::new(pool.clone()))
            .update(&stored.id, &new, Timestamp::now())
            .await?
            .expect("the row");
        assert_eq!(resaved.owner_principal.as_deref(), Some("user:bob"));
        assert_eq!(resaved.owner_groups, Some(bob_groups));
        Ok(())
    }

    async fn events_for(pool: &PgPool, key: &str) -> Vec<(String, String, Option<String>)> {
        sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT from_status, to_status, reason FROM events WHERE key = $1 ORDER BY id",
        )
        .bind(key)
        .fetch_all(pool)
        .await
        .expect("events")
    }

    /// A schedule with a cursor, backdated to a due window. The value is written the way a
    /// completed run writes it, since nothing else may.
    async fn schedule_with_cursor(
        pool: &PgPool,
        cursor: &CursorSpec,
        value: Option<&str>,
    ) -> Schedule {
        let max_time = MaxTime::parse("30m").expect("duration");
        let spec = CronSpec::parse("0 * * * *", "UTC").expect("expr");
        let stored = ScheduleStore::new(Db::new(pool.clone()))
            .create(
                &NewSchedule {
                    standing: NewStanding {
                        playbook: "survey",
                        target_kind: "adopted",
                        eligible_draft_version: None,
                        params: &params(),
                        schema_digest: "sha256:schema",
                        max_cost: 3.5,
                        max_time: &max_time,
                        advance_dedupe: true,
                        enabled: true,
                        created_by: Some("wren"),
                        owner_principal: None,
                        owner_groups: None,
                        dispatch_target: None,
                        agent_provider: None,
                        agent_model: None,
                    },
                    cursor: Some(cursor),
                    spec: &spec,
                },
                Timestamp::now(),
            )
            .await
            .expect("create");
        sqlx::query(
            "UPDATE playbook_schedules SET next_due_at = $2, cursor_value = $3 WHERE id = $1",
        )
        .bind(&stored.id)
        .bind("2026-08-23T12:00:00Z")
        .bind(value)
        .execute(pool)
        .await
        .expect("backdate");
        ScheduleStore::new(Db::new(pool.clone()))
            .get(&stored.id)
            .await
            .expect("get")
            .expect("row")
    }

    fn cursor() -> CursorSpec {
        CursorSpec::parse("$.scan.newest_created_at", Some("since"), None).expect("cursor")
    }

    /// A stored cursor value is passed to the next firing as an ordinary param, overlaid onto the
    /// values the schedule froze. The pack sees one more `--param`; nothing else changes.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_stored_cursor_value_rides_the_next_firing_as_a_param(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        let scheduled = schedule_with_cursor(&pool, &cursor(), Some("2026-08-23T00:00:00Z")).await;
        assert_eq!(scheduled.cursor.as_ref(), Some(&cursor()));

        let fired = fire_due(
            &db,
            ts("2026-08-23T12:00:30Z"),
            8,
            5,
            std::time::Duration::from_secs(3600),
        )
        .await?;
        assert_eq!(fired.len(), 1, "{fired:?}");
        let launched =
            crate::launches::store::list_playbook_runs(&pool, Some(&fired[0]), 10).await?;
        assert_eq!(
            launched[0].params,
            serde_json::json!({
                "topic": "attention sinks",
                "since": "2026-08-23T00:00:00Z",
            }),
            "the cursor value overlays the frozen params"
        );
        let target: Option<String> =
            sqlx::query_scalar("SELECT dedupe_schedule FROM playbook_launches WHERE key = $1")
                .bind(&fired[0])
                .fetch_one(&pool)
                .await?;
        assert_eq!(target.as_deref(), Some(scheduled.id.as_str()));
        Ok(())
    }

    /// The first firing has nothing to pass, so it omits the param entirely and the pack's own
    /// declared default stands. That default is how a pack opts into the cursor at all.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn the_first_firing_omits_the_cursor_param(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        schedule_with_cursor(&pool, &cursor(), None).await;

        let fired = fire_due(
            &db,
            ts("2026-08-23T12:00:30Z"),
            8,
            5,
            std::time::Duration::from_secs(3600),
        )
        .await?;
        let launched =
            crate::launches::store::list_playbook_runs(&pool, Some(&fired[0]), 10).await?;
        assert_eq!(launched[0].params, params(), "no value, no param");
        Ok(())
    }

    /// The overlaid value is validated like a launcher's typed one. A pack re-pinned onto a form
    /// that refuses it fails the firing — which counts toward auto-disable — rather than
    /// dispatching a launch the pack will reject.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_cursor_value_the_schema_refuses_fails_the_firing(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        sqlx::query("UPDATE playbooks SET params_schema = $2::jsonb WHERE id = $1")
            .bind("survey")
            .bind(
                r#"{"type":"object","properties":{"topic":{"type":"string"}},
                    "additionalProperties":false}"#,
            )
            .execute(&pool)
            .await?;
        let scheduled = schedule_with_cursor(&pool, &cursor(), Some("2026-08-23T00:00:00Z")).await;

        assert!(
            fire_due(
                &db,
                ts("2026-08-23T12:00:30Z"),
                8,
                5,
                std::time::Duration::from_secs(3600)
            )
            .await?
            .is_empty(),
            "a value the pack refuses launches nothing"
        );
        let launched: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM playbook_launches")
            .fetch_one(&pool)
            .await?;
        assert_eq!(launched, 0);
        let row = ScheduleStore::new(Db::new(pool.clone()))
            .get(&scheduled.id)
            .await?
            .expect("row");
        assert_eq!(row.consecutive_failures, 1);
        assert_eq!(
            row.next_due_at.as_deref(),
            Some("2026-08-23T13:00:00Z"),
            "it waits for its next window instead of retrying every tick"
        );
        Ok(())
    }

    fn scan_result(value: &str) -> serde_json::Map<String, serde_json::Value> {
        let doc = serde_json::json!({"scan": {"newest_created_at": value, "seen": 3}});
        doc.as_object().expect("object").clone()
    }

    /// The cursor moves behind a run that finished, and the value it moves to is the field the
    /// schedule named. The next firing then passes it as the param.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_successful_run_advances_the_cursor(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        let scheduled = schedule_with_cursor(&pool, &cursor(), None).await;
        let fired = fire_due(
            &db,
            ts("2026-08-23T12:00:30Z"),
            8,
            5,
            std::time::Duration::from_secs(3600),
        )
        .await?;

        ScheduleStore::new(db.clone())
            .advance_cursor(&fired[0], "run-1", &scan_result("2026-08-23T11:59:00Z"))
            .await?;
        let row = ScheduleStore::new(Db::new(pool.clone()))
            .get(&scheduled.id)
            .await?
            .expect("row");
        assert_eq!(row.cursor_value.as_deref(), Some("2026-08-23T11:59:00Z"));
        assert!(row.cursor_updated_at.is_some());
        let events = events_for(&pool, &Trigger::Schedule.event_key(&scheduled.id)).await;
        assert_eq!(events.last().map(|e| e.1.as_str()), Some("advanced"));

        // The next window fires with the value the run left behind.
        sqlx::query("UPDATE playbook_schedules SET next_due_at = $2 WHERE id = $1")
            .bind(&scheduled.id)
            .bind("2026-08-23T13:00:00Z")
            .execute(&pool)
            .await?;
        let again = fire_due(
            &db,
            ts("2026-08-23T13:00:30Z"),
            8,
            5,
            std::time::Duration::from_secs(3600),
        )
        .await?;
        let launched =
            crate::launches::store::list_playbook_runs(&pool, Some(&again[0]), 10).await?;
        assert_eq!(launched[0].params["since"], "2026-08-23T11:59:00Z");
        Ok(())
    }

    /// A result that carries no scalar where the cursor points leaves the value alone and says so.
    /// The next firing re-reads the same window: dedupe degrades, runs do not.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_result_without_the_field_leaves_the_cursor_alone(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        let scheduled = schedule_with_cursor(&pool, &cursor(), Some("2026-08-01T00:00:00Z")).await;
        let fired = fire_due(
            &db,
            ts("2026-08-23T12:00:30Z"),
            8,
            5,
            std::time::Duration::from_secs(3600),
        )
        .await?;

        let empty = serde_json::Map::new();
        ScheduleStore::new(db.clone())
            .advance_cursor(&fired[0], "run-1", &empty)
            .await?;
        let row = ScheduleStore::new(Db::new(pool.clone()))
            .get(&scheduled.id)
            .await?
            .expect("row");
        assert_eq!(row.cursor_value.as_deref(), Some("2026-08-01T00:00:00Z"));
        let events = events_for(&pool, &Trigger::Schedule.event_key(&scheduled.id)).await;
        assert_eq!(events.last().map(|e| e.1.as_str()), Some("unchanged"));
        Ok(())
    }

    /// A run that was not opted into advancing dedupe state moves nothing, however successful it
    /// was. That is what keeps an ad-hoc launch from eating the next scheduled firing's inputs.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_run_that_did_not_opt_in_moves_no_cursor(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        let scheduled = schedule_with_cursor(&pool, &cursor(), None).await;
        let fired = fire_due(
            &db,
            ts("2026-08-23T12:00:30Z"),
            8,
            5,
            std::time::Duration::from_secs(3600),
        )
        .await?;
        sqlx::query("UPDATE playbook_launches SET advance_dedupe = FALSE WHERE key = $1")
            .bind(&fired[0])
            .execute(&pool)
            .await?;

        ScheduleStore::new(db.clone())
            .advance_cursor(&fired[0], "run-1", &scan_result("2026-08-23T11:59:00Z"))
            .await?;
        let row = ScheduleStore::new(Db::new(pool.clone()))
            .get(&scheduled.id)
            .await?
            .expect("row");
        assert_eq!(row.cursor_value, None);
        assert!(
            events_for(&pool, &Trigger::Schedule.event_key(&scheduled.id))
                .await
                .is_empty()
        );
        Ok(())
    }

    /// A launch naming no schedule has no cursor to advance, which is the default for every ad-hoc
    /// run and every deferred one-shot.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_launch_naming_no_schedule_advances_nothing(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        let scheduled = schedule_with_cursor(&pool, &cursor(), None).await;
        let max_time = MaxTime::parse("30m").expect("duration");
        let key = "playbook:survey:0199c0de-7c2c-71a5-8000-9";
        assert!(matches!(
            crate::launches::store::adopt_playbook_launch(
                &pool,
                key,
                &NewPlaybookLaunch {
                    playbook: "survey",
                    repo: "owner/packs",
                    title: "reads a paper",
                    params: &params(),
                    schema_digest: "sha256:schema",
                    max_cost: 3.5,
                    max_time: &max_time,
                    advance_dedupe: true,
                    dedupe_schedule: None,
                    origin: LaunchOrigin::Manual,
                    draft_version: None,
                    created_by: Some("wren"),
                    launcher_groups: None,
                },
            )
            .await?,
            crate::launches::store::AdoptPlaybookOutcome::Adopted
        ));

        ScheduleStore::new(db.clone())
            .advance_cursor(key, "run-1", &scan_result("2026-08-23T11:59:00Z"))
            .await?;
        let row = ScheduleStore::new(Db::new(pool.clone()))
            .get(&scheduled.id)
            .await?
            .expect("row");
        assert_eq!(row.cursor_value, None, "opted in, but pointed at nothing");
        Ok(())
    }

    /// The claim is what makes a firing exactly-once: one launch row, one `schedule fired` event,
    /// and a `next_due_at` that has moved past the window just fired.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_due_schedule_fires_one_launch_and_advances(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        let scheduled = schedule_due_at(&pool, "0 * * * *", Some("2026-08-23T12:00:00Z")).await;

        let now = ts("2026-08-23T12:00:30Z");
        let fired = fire_due(&db, now, 8, 5, std::time::Duration::from_secs(3600)).await?;
        assert_eq!(fired.len(), 1, "{fired:?}");
        assert!(
            fire_due(&db, now, 8, 5, std::time::Duration::from_secs(3600))
                .await?
                .is_empty(),
            "the window it just fired is not due again"
        );

        let row = ScheduleStore::new(Db::new(pool.clone()))
            .get(&scheduled.id)
            .await?
            .expect("row");
        assert_eq!(row.next_due_at.as_deref(), Some("2026-08-23T13:00:00Z"));
        assert_eq!(row.last_fired_at.as_deref(), Some("2026-08-23T12:00:30Z"));
        assert_eq!(row.consecutive_failures, 0);
        assert!(row.enabled);

        let launched =
            crate::launches::store::list_playbook_runs(&pool, Some(&fired[0]), 10).await?;
        assert_eq!(launched.len(), 1);
        assert_eq!(launched[0].origin, LaunchOrigin::Schedule);
        assert_eq!(launched[0].params, params(), "the stored values launched");
        assert!(launched[0].advance_dedupe);
        assert_eq!(launched[0].status, Status::New);
        assert_eq!(launched[0].created_by.as_deref(), Some("wren"));

        let events = events_for(&pool, &fired[0]).await;
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(
            events[0].2.as_deref(),
            Some(format!("schedule {} fired", scheduled.id).as_str())
        );
        Ok(())
    }

    /// Two sweeps racing the same due row: the claim nulls `next_due_at` inside the transaction,
    /// so one wins and the other finds nothing — one launch, no deadlock.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn concurrent_sweeps_fire_one_launch(pool: PgPool) -> Result<()> {
        register_row(&pool, "survey").await;
        schedule_due_at(&pool, "0 * * * *", Some("2026-08-23T12:00:00Z")).await;

        let a = Db::new(pool.clone());
        let b = Db::new(pool.clone());
        let now = ts("2026-08-23T12:00:30Z");
        let (first, second) = tokio::join!(
            tokio::spawn(async move {
                fire_due(&a, now, 8, 5, std::time::Duration::from_secs(3600)).await
            }),
            tokio::spawn(async move {
                fire_due(&b, now, 8, 5, std::time::Duration::from_secs(3600)).await
            }),
        );
        assert_eq!(
            first??.len() + second??.len(),
            1,
            "one sweep wins the claim"
        );
        let launched: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM playbook_launches")
            .fetch_one(&pool)
            .await?;
        assert_eq!(launched, 1);
        Ok(())
    }

    /// A schedule that slept through six windows fires once for the window it was claimed on, and
    /// records the rest as skipped. There is no catch-up.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn missed_windows_are_recorded_and_never_replayed(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        let scheduled = schedule_due_at(&pool, "0 * * * *", Some("2026-08-23T06:00:00Z")).await;

        let now = ts("2026-08-23T12:30:00Z");
        let fired = fire_due(&db, now, 8, 5, std::time::Duration::from_secs(3600)).await?;
        assert_eq!(fired.len(), 1, "one launch, not seven: {fired:?}");
        let launched: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM playbook_launches")
            .fetch_one(&pool)
            .await?;
        assert_eq!(launched, 1);

        let events = events_for(&pool, &Trigger::Schedule.event_key(&scheduled.id)).await;
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].0, "due");
        assert_eq!(events[0].1, "skipped");
        let reason = events[0].2.clone().expect("reason");
        assert!(reason.starts_with("skipped 6 missed window(s)"), "{reason}");

        let row = ScheduleStore::new(Db::new(pool.clone()))
            .get(&scheduled.id)
            .await?
            .expect("row");
        assert_eq!(
            row.next_due_at.as_deref(),
            Some("2026-08-23T13:00:00Z"),
            "the next window is computed from now, not from the window it was behind on"
        );
        Ok(())
    }

    /// A firing on the window it is due for skips nothing.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_on_time_firing_records_no_missed_windows(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        let scheduled = schedule_due_at(&pool, "0 * * * *", Some("2026-08-23T12:00:00Z")).await;

        fire_due(
            &db,
            ts("2026-08-23T12:00:05Z"),
            8,
            5,
            std::time::Duration::from_secs(3600),
        )
        .await?;
        assert!(
            events_for(&pool, &Trigger::Schedule.event_key(&scheduled.id))
                .await
                .is_empty()
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_schedule_that_is_not_due_or_not_enabled_is_left_alone(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        let later = schedule_due_at(&pool, "0 * * * *", Some("2026-08-23T13:00:00Z")).await;
        let disabled = schedule_due_at(&pool, "0 * * * *", Some("2026-08-23T01:00:00Z")).await;
        sqlx::query("UPDATE playbook_standing_launches SET enabled = FALSE WHERE id = $1")
            .bind(&disabled.id)
            .execute(&pool)
            .await?;

        assert!(
            fire_due(
                &db,
                ts("2026-08-23T12:00:00Z"),
                8,
                5,
                std::time::Duration::from_secs(3600)
            )
            .await?
            .is_empty()
        );
        assert_eq!(
            ScheduleStore::new(Db::new(pool.clone()))
                .get(&later.id)
                .await?,
            Some(later.clone()),
            "an undue row is untouched"
        );
        Ok(())
    }

    /// A firing that cannot launch leaves no half-written launch, counts against the schedule, and
    /// at the threshold takes it out of the rotation with the transition on the event log.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn consecutive_failures_auto_disable_the_schedule(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        let scheduled = schedule_due_at(&pool, "* * * * *", Some("2026-08-23T12:00:00Z")).await;
        // A ceiling no launch can render: the failure lands after the claim, which is exactly the
        // window the claim's transaction has to cover.
        sqlx::query("UPDATE playbook_standing_launches SET max_time = 'forever' WHERE id = $1")
            .bind(&scheduled.id)
            .execute(&pool)
            .await?;

        for (attempt, now, waits_for) in [
            (1, "2026-08-23T12:00:30Z", "2026-08-23T12:01:00Z"),
            (2, "2026-08-23T12:01:30Z", "2026-08-23T12:02:00Z"),
        ] {
            let fired = fire_due(&db, ts(now), 8, 3, std::time::Duration::from_secs(3600)).await?;
            assert!(fired.is_empty(), "attempt {attempt}: {fired:?}");
            let row = ScheduleStore::new(Db::new(pool.clone()))
                .get(&scheduled.id)
                .await?
                .expect("row");
            assert_eq!(row.consecutive_failures, attempt);
            assert!(row.enabled, "attempt {attempt} is below the threshold");
            assert_eq!(
                row.next_due_at.as_deref(),
                Some(waits_for),
                "it waits for its next window instead of retrying this tick"
            );
        }

        assert!(
            fire_due(
                &db,
                ts("2026-08-23T12:02:30Z"),
                8,
                3,
                std::time::Duration::from_secs(3600)
            )
            .await?
            .is_empty(),
            "the third failure is the threshold"
        );
        let row = ScheduleStore::new(Db::new(pool.clone()))
            .get(&scheduled.id)
            .await?
            .expect("row");
        assert_eq!(row.consecutive_failures, 3);
        assert!(!row.enabled, "auto-disabled");
        assert_eq!(row.next_due_at, None);

        let events = events_for(&pool, &Trigger::Schedule.event_key(&scheduled.id)).await;
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(
            (events[0].0.as_str(), events[0].1.as_str()),
            ("enabled", "disabled")
        );
        let reason = events[0].2.clone().expect("reason");
        assert!(
            reason.starts_with("auto-disabled after 3 consecutive firing failures"),
            "{reason}"
        );
        let launched: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM playbook_launches")
            .fetch_one(&pool)
            .await?;
        assert_eq!(launched, 0, "no half-written launch");

        assert!(
            fire_due(
                &db,
                ts("2026-08-23T13:00:00Z"),
                8,
                3,
                std::time::Duration::from_secs(3600)
            )
            .await?
            .is_empty(),
            "a disabled schedule is never claimed again"
        );
        Ok(())
    }

    /// One firing that launches wipes the failure count: the threshold counts consecutive
    /// failures, not lifetime ones.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_launch_resets_the_failure_count(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        let scheduled = schedule_due_at(&pool, "* * * * *", Some("2026-08-23T12:00:00Z")).await;
        sqlx::query("UPDATE playbook_standing_launches SET max_time = 'forever' WHERE id = $1")
            .bind(&scheduled.id)
            .execute(&pool)
            .await?;
        assert!(
            fire_due(
                &db,
                ts("2026-08-23T12:00:30Z"),
                8,
                5,
                std::time::Duration::from_secs(3600)
            )
            .await?
            .is_empty()
        );
        assert_eq!(
            ScheduleStore::new(Db::new(pool.clone()))
                .get(&scheduled.id)
                .await?
                .expect("row")
                .consecutive_failures,
            1
        );

        sqlx::query("UPDATE playbook_standing_launches SET max_time = '30m' WHERE id = $1")
            .bind(&scheduled.id)
            .execute(&pool)
            .await?;
        assert_eq!(
            fire_due(
                &db,
                ts("2026-08-23T12:01:30Z"),
                8,
                5,
                std::time::Duration::from_secs(3600)
            )
            .await?
            .len(),
            1
        );
        assert_eq!(
            ScheduleStore::new(Db::new(pool.clone()))
                .get(&scheduled.id)
                .await?
                .expect("row")
                .consecutive_failures,
            0
        );
        Ok(())
    }

    /// One broken schedule does not starve the rest of the sweep.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_failing_schedule_does_not_block_the_others(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        let broken = schedule_due_at(&pool, "0 * * * *", Some("2026-08-23T11:00:00Z")).await;
        sqlx::query("UPDATE playbook_standing_launches SET max_time = 'forever' WHERE id = $1")
            .bind(&broken.id)
            .execute(&pool)
            .await?;
        schedule_due_at(&pool, "0 * * * *", Some("2026-08-23T12:00:00Z")).await;

        let fired = fire_due(
            &db,
            ts("2026-08-23T12:00:30Z"),
            8,
            5,
            std::time::Duration::from_secs(3600),
        )
        .await?;
        assert_eq!(fired.len(), 1, "the healthy one still fired: {fired:?}");
        assert_eq!(
            ScheduleStore::new(Db::new(pool.clone()))
                .get(&broken.id)
                .await?
                .expect("row")
                .consecutive_failures,
            1
        );
        Ok(())
    }

    /// The sweep is a discovery source: what it launches is enqueued, so an ordinary reconcile
    /// dispatches a scheduled run exactly like a manual one.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn the_sweep_enqueues_every_launch_it_fires(pool: PgPool) -> Result<()> {
        #[derive(Default)]
        struct Recorder(std::sync::Mutex<Vec<String>>);
        impl Enqueue for Recorder {
            fn enqueue(&self, key: IssueKey) {
                self.0.lock().expect("lock").push(key.0);
            }
        }

        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        schedule_due_at(&pool, "0 * * * *", Some("2020-01-01T00:00:00Z")).await;
        schedule_due_at(&pool, "0 * * * *", Some("2020-01-02T00:00:00Z")).await;

        let recorder = Arc::new(Recorder::default());
        standing::TriggerSweep::new(
            db.clone(),
            vec![Arc::new(ScheduleTrigger::new(db, None))],
            5,
            std::time::Duration::from_secs(3600),
            None,
            None,
        )
        .poll(recorder.clone())
        .await?;

        let keys = recorder.0.lock().expect("lock").clone();
        assert_eq!(keys.len(), 2, "{keys:?}");
        assert!(
            keys.iter().all(|k| k.starts_with("playbook:survey:")),
            "{keys:?}"
        );
        Ok(())
    }

    /// Repointing a schedule at another pack drops the cursor value even when the cursor spec is
    /// unchanged. The old pack's value is not the new pack's window, and an in-flight run of the
    /// old pack must not write one back either.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn repointing_the_playbook_drops_the_cursor_value(pool: PgPool) -> Result<()> {
        register_row(&pool, "survey").await;
        register_row(&pool, "digest").await;
        let cursor = cursor();
        let scheduled = schedule_with_cursor(&pool, &cursor, Some("2026-08-23T00:00:00Z")).await;
        assert_eq!(
            scheduled.cursor_value.as_deref(),
            Some("2026-08-23T00:00:00Z")
        );
        sqlx::query("UPDATE playbook_schedules SET cursor_updated_at = $2 WHERE id = $1")
            .bind(&scheduled.id)
            .bind("2026-08-23T12:00:00Z")
            .execute(&pool)
            .await?;
        let max_time = MaxTime::parse("30m").expect("duration");
        let spec = CronSpec::parse("0 * * * *", "UTC").expect("expr");
        let mut new = NewSchedule {
            standing: NewStanding {
                playbook: "survey",
                target_kind: "adopted",
                eligible_draft_version: None,
                params: &params(),
                schema_digest: "sha256:schema",
                max_cost: 3.5,
                max_time: &max_time,
                advance_dedupe: true,
                enabled: true,
                created_by: Some("wren"),
                owner_principal: None,
                owner_groups: None,
                dispatch_target: None,
                agent_provider: None,
                agent_model: None,
            },
            cursor: Some(&cursor),
            spec: &spec,
        };
        let now = ts("2026-08-23T12:30:00Z");

        let same = ScheduleStore::new(Db::new(pool.clone()))
            .update(&scheduled.id, &new, now)
            .await?
            .expect("row");
        assert_eq!(
            same.cursor_value.as_deref(),
            Some("2026-08-23T00:00:00Z"),
            "an edit that changes neither the pack nor the cursor keeps the value"
        );
        assert!(same.cursor_updated_at.is_some());

        new.standing.playbook = "digest";
        let repointed = ScheduleStore::new(Db::new(pool.clone()))
            .update(&scheduled.id, &new, now)
            .await?
            .expect("row");
        assert_eq!(repointed.playbook, "digest");
        assert_eq!(repointed.cursor.as_ref(), Some(&cursor));
        assert_eq!(repointed.cursor_value, None, "another pack, another window");
        assert_eq!(repointed.cursor_updated_at, None);
        Ok(())
    }

    /// A disabled schedule stores no due window, and enabling one through an update computes the
    /// next window from now.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn enabling_and_disabling_moves_the_due_window(pool: PgPool) -> Result<()> {
        register_row(&pool, "survey").await;
        let max_time = MaxTime::parse("30m").expect("duration");
        let spec = CronSpec::parse("0 * * * *", "UTC").expect("expr");
        let mut new = NewSchedule {
            standing: NewStanding {
                playbook: "survey",
                target_kind: "adopted",
                eligible_draft_version: None,
                params: &params(),
                schema_digest: "sha256:schema",
                max_cost: 3.5,
                max_time: &max_time,
                advance_dedupe: true,
                enabled: false,
                created_by: Some("wren"),
                owner_principal: None,
                owner_groups: None,
                dispatch_target: None,
                agent_provider: None,
                agent_model: None,
            },
            cursor: None,
            spec: &spec,
        };
        let now = ts("2026-08-23T12:30:00Z");
        let stored = ScheduleStore::new(Db::new(pool.clone()))
            .create(&new, now)
            .await?;
        assert_eq!(stored.next_due_at, None, "a disabled schedule is not due");

        new.standing.enabled = true;
        let updated = ScheduleStore::new(Db::new(pool.clone()))
            .update(&stored.id, &new, now)
            .await?
            .expect("updated");
        assert_eq!(updated.next_due_at.as_deref(), Some("2026-08-23T13:00:00Z"));
        assert_eq!(updated.consecutive_failures, 0);

        let store = ScheduleStore::new(Db::new(pool.clone()));
        assert!(store.delete(&stored.id).await?);
        assert!(!store.delete(&stored.id).await?);
        assert!(store.get(&stored.id).await?.is_none());
        Ok(())
    }

    fn file_cursor() -> CursorSpec {
        CursorSpec::parse("rollup/STATE.json", None, Some("state/cursor.json")).expect("cursor")
    }

    /// A run-files bundle the way the loop's drop-box POSTs it: one gzipped tar keyed
    /// `<task>/<declared path>`.
    fn run_files(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, name, *body)
                .expect("append");
        }
        let tar = builder.into_inner().expect("tar");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut enc, &tar).expect("gzip");
        enc.finish().expect("gzip")
    }

    async fn refire(db: &Db, pool: &PgPool, id: &str) -> Result<String> {
        sqlx::query("UPDATE playbook_schedules SET next_due_at = $2 WHERE id = $1")
            .bind(id)
            .bind("2026-08-23T13:00:00Z")
            .execute(pool)
            .await?;
        let again = fire_due(
            db,
            ts("2026-08-23T13:00:30Z"),
            8,
            5,
            std::time::Duration::from_secs(3600),
        )
        .await?;
        Ok(again[0].clone())
    }

    /// A file cursor advances off the run's captured files, the event names the file by size and
    /// digest rather than printing it, and the next firing finds the body staged into its pack at
    /// the path the schedule named. A firing before any value landed stages nothing, so the
    /// pack's own file stands.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_finished_run_advances_a_file_cursor_and_the_next_firing_is_staged_with_it(
        pool: PgPool,
    ) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        let scheduled = schedule_with_cursor(&pool, &file_cursor(), None).await;
        assert_eq!(scheduled.cursor.as_ref(), Some(&file_cursor()));
        let fired = fire_due(
            &db,
            ts("2026-08-23T12:00:30Z"),
            8,
            5,
            std::time::Duration::from_secs(3600),
        )
        .await?;
        let launched =
            crate::launches::store::list_playbook_runs(&pool, Some(&fired[0]), 10).await?;
        assert_eq!(
            launched[0].params.get("since"),
            None,
            "a file cursor overlays no param"
        );

        let pack = tempfile::tempdir()?;
        let store = ScheduleStore::new(db.clone());
        assert_eq!(store.stage_cursor_file(&fired[0], pack.path()).await?, None);
        assert!(!pack.path().join("state/cursor.json").exists());

        let body = br#"{"seen": [{"pr": 12345, "head_sha": "ab12"}]}"#;
        crate::runs::blob_store::put_run_files(
            &pool,
            "run-1",
            run_files(&[("rollup/STATE.json", body), ("rollup/REPORT.md", b"# hi")]),
        )
        .await?;
        store
            .advance_cursor(&fired[0], "run-1", &serde_json::Map::new())
            .await?;
        let row = store.get(&scheduled.id).await?.expect("row");
        assert_eq!(
            row.cursor_value.as_deref(),
            Some(std::str::from_utf8(body)?)
        );
        assert!(row.cursor_updated_at.is_some());
        let events = events_for(&pool, &Trigger::Schedule.event_key(&scheduled.id)).await;
        let (_, to, note) = events.last().expect("event");
        assert_eq!(to, "advanced");
        let note = note.as_deref().expect("note");
        assert!(note.contains("rollup/STATE.json"), "{note}");
        assert!(note.contains(&format!("{} bytes", body.len())), "{note}");
        assert!(note.contains("sha256:"), "{note}");
        assert!(note.contains("state/cursor.json"), "{note}");
        assert!(
            !note.contains("seen"),
            "the note describes the file, it does not print it: {note}"
        );

        let key = refire(&db, &pool, &scheduled.id).await?;
        let pack = tempfile::tempdir()?;
        let written = store.stage_cursor_file(&key, pack.path()).await?;
        assert_eq!(written, Some(pack.path().join("state/cursor.json")));
        assert_eq!(std::fs::read(pack.path().join("state/cursor.json"))?, body);
        Ok(())
    }

    /// A run that captured no such file, one whose file is over the cap, and one whose file is
    /// not text all leave the stored value alone and say why.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_missing_oversize_or_binary_file_leaves_the_cursor_alone(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        let scheduled = schedule_with_cursor(&pool, &file_cursor(), Some("{\"seen\": []}")).await;
        let store = ScheduleStore::new(db.clone());
        let mut fired = fire_due(
            &db,
            ts("2026-08-23T12:00:30Z"),
            8,
            5,
            std::time::Duration::from_secs(3600),
        )
        .await?;
        let huge = vec![b'x'; CURSOR_FILE_MAX_BYTES + 1];
        let cases: Vec<(&str, Option<Vec<u8>>, &str)> = vec![
            ("run-none", None, "not among the run's captured files"),
            (
                "run-other",
                Some(run_files(&[("rollup/REPORT.md", b"# hi")])),
                "not among the run's captured files",
            ),
            (
                "run-huge",
                Some(run_files(&[("rollup/STATE.json", &huge)])),
                "over the 262144 byte cursor cap",
            ),
            (
                "run-binary",
                Some(run_files(&[("rollup/STATE.json", &[0xff, 0xfe, 0x00])])),
                "not UTF-8 text",
            ),
        ];
        for (run_id, bundle, expected) in cases {
            if let Some(bundle) = bundle {
                crate::runs::blob_store::put_run_files(&pool, run_id, bundle).await?;
            }
            let key = fired.remove(0);
            store
                .advance_cursor(&key, run_id, &serde_json::Map::new())
                .await?;
            let row = store.get(&scheduled.id).await?.expect("row");
            assert_eq!(
                row.cursor_value.as_deref(),
                Some("{\"seen\": []}"),
                "{run_id}"
            );
            let events = events_for(&pool, &Trigger::Schedule.event_key(&scheduled.id)).await;
            let (_, to, note) = events.last().expect("event");
            assert_eq!(to, "unchanged", "{run_id}");
            let note = note.as_deref().expect("note");
            assert!(note.contains(expected), "{run_id}: {note}");
            fired.push(refire(&db, &pool, &scheduled.id).await?);
        }
        Ok(())
    }

    /// The cursor file is a scheduled firing's input. A launch of the same schedule that arrived
    /// any other way is staged with nothing, exactly as it is passed no cursor param.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn only_a_scheduled_firing_is_staged_with_the_cursor_file(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        schedule_with_cursor(&pool, &file_cursor(), Some("{\"seen\": []}")).await;
        let fired = fire_due(
            &db,
            ts("2026-08-23T12:00:30Z"),
            8,
            5,
            std::time::Duration::from_secs(3600),
        )
        .await?;
        sqlx::query("UPDATE playbook_launches SET origin = 'manual' WHERE key = $1")
            .bind(&fired[0])
            .execute(&pool)
            .await?;
        let pack = tempfile::tempdir()?;
        let written = ScheduleStore::new(db)
            .stage_cursor_file(&fired[0], pack.path())
            .await?;
        assert_eq!(written, None);
        assert!(std::fs::read_dir(pack.path())?.next().is_none());
        Ok(())
    }

    /// Repointing a file cursor, to another file or to a param, drops the stored body the old one
    /// collected, the same rule a field cursor follows.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn repointing_a_file_cursor_drops_its_stored_body(pool: PgPool) -> Result<()> {
        register_row(&pool, "survey").await;
        let scheduled = schedule_with_cursor(&pool, &file_cursor(), Some("{\"seen\": []}")).await;
        let max_time = crate::model::MaxTime::parse("30m").expect("duration");
        let spec = CronSpec::parse("0 * * * *", "UTC").expect("expr");
        let store = ScheduleStore::new(Db::new(pool.clone()));
        let mut new = NewSchedule {
            standing: NewStanding {
                playbook: "survey",
                target_kind: "adopted",
                eligible_draft_version: None,
                params: &params(),
                schema_digest: "sha256:schema",
                max_cost: 3.5,
                max_time: &max_time,
                advance_dedupe: true,
                enabled: true,
                created_by: Some("wren"),
                owner_principal: None,
                owner_groups: None,
                dispatch_target: None,
                agent_provider: None,
                agent_model: None,
            },
            cursor: Some(&file_cursor()),
            spec: &spec,
        };
        let same = store
            .update(&scheduled.id, &new, Timestamp::now())
            .await?
            .expect("row");
        assert_eq!(same.cursor_value.as_deref(), Some("{\"seen\": []}"));

        let moved =
            CursorSpec::parse("rollup/STATE.json", None, Some("elsewhere.json")).expect("cursor");
        new.cursor = Some(&moved);
        let repointed = store
            .update(&scheduled.id, &new, Timestamp::now())
            .await?
            .expect("row");
        assert_eq!(repointed.cursor.as_ref(), Some(&moved));
        assert_eq!(repointed.cursor_value, None);
        assert_eq!(repointed.cursor_updated_at, None);
        Ok(())
    }
}
