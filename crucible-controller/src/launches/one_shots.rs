//! Deferred one-shots: a playbook launch authorized now and minted when its `fire_at` comes due.
//! A one-shot fires exactly once and completes — a recurring shape is a schedule, and the two
//! surfaces stay apart because their semantics do (frozen snapshots, relaunch-by-prefill, and no
//! dedupe advancement unless the launcher opted in). The authorization it fires with is its
//! [`crate::launches::standing`] row; the sidecar here is the instant, the status, and the launch it minted.
//!
//! The firing is [`OneShotTrigger`] on the generic sweep. The CAS `status = 'pending'` claim is
//! what makes it exactly-once: a second sweep (or a second process) finds the row already `fired`,
//! and the launch lands in the claim's transaction so a claim never outlives a launch that failed.

use crate::client::Db;
use crate::clock::stamp;
use crate::event_log::Event;
use crate::launches::model::OneShotStatus;
use crate::launches::standing::{
    self, Claim, Failed, LaunchTrigger, NewStanding, Recorded, Standing, SweepCfg, TriggerFuture,
};
use crate::model::Trigger;
use anyhow::{Context, Result};
use jiff::Timestamp;
use sqlx::PgPool;

/// How many due one-shots one sweep fires. A backlog this deep is a controller that was down for
/// a long while; the rest fire on the next tick rather than holding the discovery loop.
const FIRE_CAP: i64 = 32;

/// How many one-shots the list surface returns.
pub(crate) const LIST_LIMIT: i64 = 200;

/// What to defer: the same authorization a manual launch carries, plus the instant it fires.
#[derive(Debug, Clone)]
pub(crate) struct NewOneShot<'a> {
    pub standing: NewStanding<'a>,
    /// The schedule whose cursor this firing may advance. `None` advances nothing.
    pub dedupe_schedule: Option<&'a str>,
    /// Normalized UTC RFC3339 ([`normalize_fire_at`]).
    pub fire_at: &'a str,
}

/// One one-shot: its standing authorization and its instant, flattened.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OneShot {
    pub id: String,
    pub playbook: String,
    pub params: serde_json::Value,
    pub schema_digest: String,
    pub max_cost: f64,
    pub max_time: String,
    pub advance_dedupe: bool,
    pub dedupe_schedule: Option<String>,
    pub fire_at: String,
    pub status: OneShotStatus,
    /// The launch the firing minted; `None` until it fires.
    pub fired_key: Option<String>,
    pub fired_at: Option<String>,
    pub created_by: Option<String>,
    pub owner_principal: Option<String>,
    pub owner_signin_required: bool,
    pub dispatch_target: Option<String>,
    pub agent_provider: Option<String>,
    pub agent_model: Option<String>,
    pub created_at: String,
}

/// Why a cancel did nothing. A one-shot that already fired is a run in flight, not a pending
/// intent, so the CAS refuses rather than pretending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CancelOutcome {
    Canceled,
    NotPending(OneShotStatus),
    Unknown,
}

/// Re-serialize a caller's instant to the fixed `%Y-%m-%dT%H:%M:%SZ` UTC form every other stamp in
/// this schema uses. The sweep compares `fire_at <= now` on TEXT, so a raw client string (a local
/// offset, fractional seconds) would order wrong; normalizing here is what makes that compare
/// sound. `Err` carries the launcher-facing message.
pub(crate) fn normalize_fire_at(raw: &str) -> Result<String, String> {
    let ts: jiff::Timestamp = raw.trim().parse().map_err(|e| {
        format!("fire_at {raw:?} is not an RFC3339 instant (try `2026-08-23T14:00:00Z`): {e}")
    })?;
    Ok(stamp(ts))
}

/// The column set every read projects: the core row beside the sidecar.
#[derive(sqlx::FromRow)]
struct OneShotRow {
    #[sqlx(flatten)]
    core: Standing,
    dedupe_schedule: Option<String>,
    fire_at: String,
    status: String,
    fired_key: Option<String>,
    fired_at: Option<String>,
}

fn decode(row: OneShotRow) -> Result<OneShot> {
    let c = row.core;
    Ok(OneShot {
        id: c.id,
        playbook: c.playbook,
        params: c.params,
        schema_digest: c.schema_digest,
        max_cost: c.max_cost,
        max_time: c.max_time,
        advance_dedupe: c.advance_dedupe,
        dedupe_schedule: row.dedupe_schedule,
        fire_at: row.fire_at,
        status: OneShotStatus::parse(&row.status)?,
        fired_key: row.fired_key,
        fired_at: row.fired_at,
        created_by: c.created_by,
        owner_principal: c.owner_principal,
        owner_signin_required: c.owner_signin_required,
        dispatch_target: c.dispatch_target,
        agent_provider: c.agent_provider,
        agent_model: c.agent_model,
        created_at: c.created_at,
    })
}

const SELECT: &str = "SELECT {COLUMNS}, o.dedupe_schedule, o.fire_at, o.status, o.fired_key, \
    o.fired_at FROM playbook_standing_launches c JOIN playbook_one_shots o USING (id)";

fn select() -> String {
    SELECT.replace("{COLUMNS}", standing::COLUMNS)
}

/// Store a deferred one-shot. The values were validated against the pack's stored schema and the
/// ceilings bounded by the admin caps at the endpoint, exactly as for an immediate launch.
///
/// `Ok(None)` when the playbook was deregistered between that authorization and this insert, which
/// the endpoint reports as a 404 rather than letting the constraint violation surface as a 500.
pub(crate) async fn create(pool: &PgPool, new: &NewOneShot<'_>) -> Result<Option<OneShot>> {
    let id = uuid::Uuid::now_v7().to_string();
    let now = crate::clock::now_rfc3339();
    let mut tx = pool.begin().await.context("create one-shot: begin")?;
    let registered: Option<String> = sqlx::query_scalar("SELECT id FROM playbooks WHERE id = $1")
        .bind(new.standing.playbook)
        .fetch_optional(&mut *tx)
        .await
        .context("create one-shot: registry")?;
    if registered.is_none() {
        return Ok(None);
    }
    standing::insert(&mut tx, &id, Trigger::Deferred, &new.standing, &now).await?;
    sqlx::query(
        "INSERT INTO playbook_one_shots (id, dedupe_schedule, fire_at) VALUES ($1, $2, $3)",
    )
    .bind(&id)
    .bind(new.dedupe_schedule)
    .bind(new.fire_at)
    .execute(&mut *tx)
    .await
    .context("create one-shot")?;
    tx.commit().await.context("create one-shot: commit")?;
    get(pool, &id).await
}

/// Every one-shot, soonest-due first among the pending ones.
pub(crate) async fn list(pool: &PgPool, limit: i64) -> Result<Vec<OneShot>> {
    let sql = format!(
        "{} ORDER BY (o.status = 'pending') DESC, o.fire_at, o.id LIMIT $1",
        select()
    );
    let rows = sqlx::query_as::<_, OneShotRow>(&sql)
        .bind(limit)
        .fetch_all(pool)
        .await
        .context("list one-shots")?;
    rows.into_iter().map(decode).collect()
}

/// One one-shot by id, or `None`.
pub(crate) async fn get(pool: &PgPool, id: &str) -> Result<Option<OneShot>> {
    let sql = format!("{} WHERE c.id = $1", select());
    let row = sqlx::query_as::<_, OneShotRow>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .context("get one-shot")?;
    row.map(decode).transpose()
}

/// Cancel a pending one-shot. A CAS on `status`, so a cancel that races the sweep's claim loses
/// and reports the row already fired.
pub(crate) async fn cancel(pool: &PgPool, id: &str) -> Result<CancelOutcome> {
    let updated = sqlx::query(
        "WITH off AS (
             UPDATE playbook_one_shots SET status = 'canceled'
             WHERE id = $1 AND status = 'pending' RETURNING id
         )
         UPDATE playbook_standing_launches c SET enabled = false FROM off WHERE c.id = off.id",
    )
    .bind(id)
    .execute(pool)
    .await
    .context("cancel one-shot")?;
    if updated.rows_affected() > 0 {
        return Ok(CancelOutcome::Canceled);
    }
    match get(pool, id).await? {
        Some(row) => Ok(CancelOutcome::NotPending(row.status)),
        None => Ok(CancelOutcome::Unknown),
    }
}

/// The event-log key a one-shot's own transitions are recorded under. Its firing is recorded under
/// the launch key it minted, like every other launch.
fn one_shot_key(id: &str) -> String {
    Trigger::Deferred.event_key(id)
}

/// The deferred trigger: a pending row whose instant has passed is a firing. The claim is the
/// `pending → fired` CAS; a row the sweep cannot launch parks as `failed` so the earliest-first
/// order does not re-pick it on every tick and starve the rows behind it.
pub struct OneShotTrigger;

impl LaunchTrigger for OneShotTrigger {
    fn trigger(&self) -> Trigger {
        Trigger::Deferred
    }

    fn due<'a>(
        &'a self,
        db: &'a Db,
        _cfg: SweepCfg,
        now: Timestamp,
    ) -> TriggerFuture<'a, Result<Vec<Claim>>> {
        let db = db.clone();
        Box::pin(async move {
            let rows = sqlx::query_as::<_, (String, Option<String>)>(
                "SELECT o.id, o.dedupe_schedule FROM playbook_one_shots o
                 JOIN playbook_standing_launches c USING (id)
                 WHERE o.status = 'pending' AND c.enabled AND o.fire_at <= $1
                 ORDER BY o.fire_at, o.id LIMIT $2",
            )
            .bind(stamp(now))
            .bind(FIRE_CAP)
            .fetch_all(db.pool())
            .await
            .context("the due one-shots")?;
            Ok(rows
                .into_iter()
                .map(|(id, dedupe_schedule)| {
                    let mut claim = Claim::new(&id, "deferred one-shot fired".to_string());
                    claim.dedupe_schedule = dedupe_schedule;
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
        Box::pin(async move {
            let claimed = sqlx::query(
                "UPDATE playbook_one_shots SET status = 'fired', fired_at = $2
                 WHERE id = $1 AND status = 'pending'",
            )
            .bind(&claim.id)
            .bind(stamp(now))
            .execute(&mut **tx)
            .await
            .context("claim due one-shot")?;
            Ok(claimed.rows_affected() > 0)
        })
    }

    fn settle<'a, 'c>(
        &'a self,
        tx: &'a mut sqlx::Transaction<'c, sqlx::Postgres>,
        claim: &'a Claim,
        key: &'a str,
        _now: Timestamp,
    ) -> TriggerFuture<'a, Result<Vec<Recorded>>> {
        Box::pin(async move {
            sqlx::query("UPDATE playbook_one_shots SET fired_key = $2 WHERE id = $1")
                .bind(&claim.id)
                .bind(key)
                .execute(&mut **tx)
                .await
                .context("record the one-shot's launch")?;
            // Fired: the core row has nothing left to fire.
            sqlx::query("UPDATE playbook_standing_launches SET enabled = false WHERE id = $1")
                .bind(&claim.id)
                .execute(&mut **tx)
                .await
                .context("retire the fired one-shot")?;
            Ok(Vec::new())
        })
    }

    fn fail<'a>(
        &'a self,
        db: &'a Db,
        claim: &'a Claim,
        message: &'a str,
        _now: Timestamp,
    ) -> TriggerFuture<'a, Result<Failed>> {
        Box::pin(async move {
            let updated = sqlx::query(
                "UPDATE playbook_one_shots SET status = 'failed' WHERE id = $1 AND status = 'pending'",
            )
            .bind(&claim.id)
            .execute(db.pool())
            .await
            .context("park a failed one-shot")?;
            if updated.rows_affected() > 0 {
                let key = one_shot_key(&claim.id);
                let event = Event::now(&key, "pending", "failed", Some(message), Some(&claim.id));
                crate::event_log::insert(db.pool(), &event).await?;
                db.events().publish(&event);
            }
            Ok(Failed {
                force_disable: true,
                announced: true,
            })
        })
    }
}

/// Claim every one-shot due at `now` and mint its launch, up to `cap` per sweep, through the
/// generic sweep. Returns the keys the launches landed under.
#[cfg(test)]
pub(crate) async fn fire_due(db: &Db, now: &str, cap: usize) -> Result<Vec<String>> {
    let now: Timestamp = now
        .parse()
        .with_context(|| format!("fire_due: now {now:?}"))?;
    let cfg = SweepCfg {
        auto_disable_after: 1,
        owner_ttl: std::time::Duration::from_secs(3600),
    };
    let mut keys = standing::sweep(db, &OneShotTrigger, cfg, now, None, None).await?;
    keys.truncate(cap);
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::queue::DiscoverySource;
    use crate::daemon::queue::{Enqueue, IssueKey};
    use crate::model::LaunchOrigin;
    use crate::model::Status;
    use std::sync::Arc;

    /// A registry row without the clone-and-extract dance: the sweep only reads `repo`,
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

    async fn defer(pool: &PgPool, fire_at: &str) -> OneShot {
        let max_time = crate::model::MaxTime::parse("30m").expect("duration");
        create(
            pool,
            &NewOneShot {
                standing: NewStanding {
                    playbook: "survey",
                    target_kind: "adopted",
                    eligible_draft_version: None,
                    params: &params(),
                    schema_digest: "sha256:schema",
                    max_cost: 3.5,
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
                dedupe_schedule: None,
                fire_at,
            },
        )
        .await
        .expect("create")
        .expect("the playbook is registered")
    }

    #[test]
    fn fire_at_normalizes_to_the_schema_stamp_format() {
        assert_eq!(
            normalize_fire_at("2026-08-23T02:00:00+02:00").expect("offset"),
            "2026-08-23T00:00:00Z"
        );
        assert_eq!(
            normalize_fire_at(" 2026-08-23T00:00:00.750Z ").expect("fractional"),
            "2026-08-23T00:00:00Z"
        );
        for bad in ["", "tomorrow", "2026-13-01T00:00:00Z", "2026-08-23"] {
            assert!(normalize_fire_at(bad).is_err(), "{bad:?} is not an instant");
        }
    }

    /// The whole point of the CAS claim: a due one-shot mints exactly one launch, however many
    /// sweeps run over it.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_due_one_shot_fires_exactly_once(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        let deferred = defer(&pool, "2026-08-23T12:00:00Z").await;

        let first = fire_due(&db, "2026-08-23T12:00:00Z", 8).await?;
        let second = fire_due(&db, "2026-08-23T13:00:00Z", 8).await?;
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(
            second.is_empty(),
            "a second sweep finds nothing: {second:?}"
        );

        let fired = get(&pool, &deferred.id).await?.expect("row");
        assert_eq!(fired.status, OneShotStatus::Fired);
        assert_eq!(fired.fired_key.as_deref(), Some(first[0].as_str()));
        assert_eq!(fired.fired_at.as_deref(), Some("2026-08-23T12:00:00Z"));

        let launched = crate::launches::store::list_playbook_runs(&pool, None, 10).await?;
        assert_eq!(launched.len(), 1, "one launch, not two");
        assert_eq!(launched[0].key, first[0]);
        assert_eq!(launched[0].origin, LaunchOrigin::Deferred);
        assert_eq!(launched[0].params, params(), "the frozen values launched");
        assert!(!launched[0].advance_dedupe);
        assert_eq!(launched[0].status, Status::New);
        assert_eq!(launched[0].created_by.as_deref(), Some("wren"));

        // The launch owns its copy of the registered pack, like every other launch.
        let slug = crate::model::sanitize_key(&first[0]);
        let copied: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM pack_tarballs WHERE issue_slug = $1")
                .bind(&slug)
                .fetch_one(&pool)
                .await?;
        assert_eq!(copied, 1, "the firing copied the pack it was authorized on");
        Ok(())
    }

    /// Two sweeps running at once: `FOR UPDATE SKIP LOCKED` plus the CAS claim means one of them
    /// mints the launch and the other finds nothing, with no deadlock between them.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn concurrent_sweeps_mint_one_launch(pool: PgPool) -> Result<()> {
        register_row(&pool, "survey").await;
        defer(&pool, "2026-08-23T12:00:00Z").await;

        let a = Db::new(pool.clone());
        let b = Db::new(pool.clone());
        let (first, second) = tokio::join!(
            tokio::spawn(async move { fire_due(&a, "2026-08-23T12:00:00Z", 8).await }),
            tokio::spawn(async move { fire_due(&b, "2026-08-23T12:00:00Z", 8).await }),
        );
        let fired = first??.len() + second??.len();
        assert_eq!(fired, 1, "one sweep wins the claim");
        assert_eq!(
            crate::launches::store::list_playbook_runs(&pool, None, 10)
                .await?
                .len(),
            1
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_one_shot_fires_only_once_its_time_has_come(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        let deferred = defer(&pool, "2026-08-23T12:00:00Z").await;

        assert!(
            fire_due(&db, "2026-08-23T11:59:59Z", 8).await?.is_empty(),
            "a second early is not due"
        );
        assert_eq!(
            get(&pool, &deferred.id).await?.expect("row").status,
            OneShotStatus::Pending
        );
        assert_eq!(fire_due(&db, "2026-08-23T12:00:00Z", 8).await?.len(), 1);
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_canceled_one_shot_never_fires(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        let deferred = defer(&pool, "2026-08-23T12:00:00Z").await;

        assert_eq!(cancel(&pool, &deferred.id).await?, CancelOutcome::Canceled);
        assert_eq!(
            cancel(&pool, &deferred.id).await?,
            CancelOutcome::NotPending(OneShotStatus::Canceled)
        );
        assert_eq!(cancel(&pool, "nope").await?, CancelOutcome::Unknown);
        assert!(fire_due(&db, "2026-08-23T12:00:00Z", 8).await?.is_empty());
        assert!(
            crate::launches::store::list_playbook_runs(&pool, None, 10)
                .await?
                .is_empty()
        );
        Ok(())
    }

    /// A ceiling no launch can render. The failure lands after the claim UPDATE, which is exactly
    /// the window the claim's transaction has to cover.
    async fn poison(pool: &PgPool, id: &str) -> Result<()> {
        sqlx::query("UPDATE playbook_standing_launches SET max_time = 'forever' WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await?;
        Ok(())
    }

    /// A one-shot the sweep cannot launch mints nothing and parks as `failed`: the claim and the
    /// launch share one transaction, so there is no claimed-but-unlaunched orphan to hand-repair.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_failed_launch_rolls_the_claim_back(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        let deferred = defer(&pool, "2026-08-23T12:00:00Z").await;
        poison(&pool, &deferred.id).await?;

        assert!(fire_due(&db, "2026-08-23T12:00:00Z", 8).await?.is_empty());

        let row = get(&pool, &deferred.id).await?.expect("row");
        assert_eq!(row.status, OneShotStatus::Failed);
        assert!(row.fired_key.is_none());
        assert!(
            crate::launches::store::list_playbook_runs(&pool, None, 10)
                .await?
                .is_empty(),
            "no half-written launch"
        );

        let events = db
            .events()
            .read_for_key(&one_shot_key(&deferred.id))
            .await?;
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].to, "failed");
        assert!(
            events[0]
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("max_time")),
            "{:?}",
            events[0].reason
        );
        Ok(())
    }

    /// The claim always takes the earliest pending `fire_at`, so a row that cannot launch has to
    /// leave the pending set or it starves every one-shot behind it.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_poisoned_row_does_not_block_the_ones_behind_it(pool: PgPool) -> Result<()> {
        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        let bad = defer(&pool, "2026-08-23T11:00:00Z").await;
        let good = defer(&pool, "2026-08-23T12:00:00Z").await;
        poison(&pool, &bad.id).await?;

        let fired = fire_due(&db, "2026-08-23T12:00:00Z", 8).await?;
        assert_eq!(fired.len(), 1, "the later one-shot fired: {fired:?}");

        assert_eq!(
            get(&pool, &bad.id).await?.expect("row").status,
            OneShotStatus::Failed
        );
        let fired_row = get(&pool, &good.id).await?.expect("row");
        assert_eq!(fired_row.status, OneShotStatus::Fired);
        assert_eq!(fired_row.fired_key.as_deref(), Some(fired[0].as_str()));

        assert!(
            fire_due(&db, "2026-08-23T13:00:00Z", 8).await?.is_empty(),
            "the parked row is not re-claimed"
        );
        Ok(())
    }

    /// The sweep is a discovery source: it enqueues what it launched, so the ordinary reconcile
    /// dispatches a deferred run exactly like a manual one.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn the_poll_enqueues_every_launch_it_fires(pool: PgPool) -> Result<()> {
        #[derive(Default)]
        struct Recorder(std::sync::Mutex<Vec<String>>);
        impl Enqueue for Recorder {
            fn enqueue(&self, key: IssueKey) {
                self.0.lock().expect("lock").push(key.0);
            }
        }

        let db = Db::new(pool.clone());
        register_row(&pool, "survey").await;
        defer(&pool, "2020-01-01T00:00:00Z").await;
        defer(&pool, "2020-01-02T00:00:00Z").await;

        let recorder = Arc::new(Recorder::default());
        standing::TriggerSweep::new(
            db,
            vec![Arc::new(OneShotTrigger)],
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
}
