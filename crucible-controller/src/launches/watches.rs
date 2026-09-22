//! Tracker watches: a query in a tracker's native language, swept on the discovery cadence, that
//! launches a playbook once per matching item. The authorization every launch carries is the
//! watch's [`crate::launches::standing`] row; the sidecar here is the tracker, the query, the param the
//! item's identifier is passed as, and the watermark the next sweep searches from.
//!
//! **One automatic launch per watch and item, ever.** A pipeline's own report is typically a
//! comment on the driving item, which bumps its update time, so a relaunch-on-update sweep would
//! feed on its own output forever. The `playbook_watch_hits` row is the guard, inserted in the
//! same transaction as the launch; a re-run is an explicit launch through the ordinary playbook
//! form, or a per-item [`reset_hit`] that lets the next sweep see the item again.
//!
//! The watermark starts at the watch's creation (or an explicit earlier bound) and only ever moves
//! forward to the newest item launched, so a new watch does not backfill a board, and an item that
//! raced the tracker's watermark slack is re-seen and idempotently skipped.

use crate::client::Db;
use crate::launches::standing::{
    self, Claim, Failed, LaunchTrigger, NewStanding, Recorded, Standing, SweepCfg, TriggerFuture,
};
use crate::launches::tracker::{TrackerKind, Trackers};
use crate::model::Trigger;
use anyhow::{Context, Result};
use jiff::Timestamp;
use sqlx::PgPool;

/// Launches one sweep may mint per watch. A deeper backlog drains on later ticks (hits arrive
/// oldest-first) rather than holding the discovery loop.
const LAUNCH_CAP: usize = 8;

/// How many watches the list surface returns.
pub(crate) const LIST_LIMIT: i64 = 200;

/// What to watch: the authorization every launch carries, plus the query that triggers one.
#[derive(Debug, Clone)]
pub(crate) struct NewWatch<'a> {
    pub standing: NewStanding<'a>,
    pub tracker: TrackerKind,
    pub query: &'a str,
    /// The playbook param the matching item's identifier is passed as.
    pub key_param: &'a str,
    /// An explicit watermark (RFC 3339). `None` keeps the stored one on an update and starts at
    /// now on a create.
    pub since: Option<&'a str>,
}

/// One watch: its standing authorization and its trigger, flattened.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Watch {
    pub id: String,
    pub playbook: String,
    pub adopted_repo: Option<String>,
    pub adopted_path: Option<String>,
    pub adopted_rev: Option<String>,
    pub params: serde_json::Value,
    pub schema_digest: String,
    pub max_cost: f64,
    pub max_time: String,
    pub tracker: String,
    pub query: String,
    pub key_param: String,
    /// The update time the next sweep searches from, RFC 3339.
    pub watermark: String,
    pub enabled: bool,
    pub last_swept_at: Option<String>,
    pub last_launched_at: Option<String>,
    pub consecutive_failures: i64,
    pub created_by: Option<String>,
    pub owner_principal: Option<String>,
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

#[derive(sqlx::FromRow)]
struct WatchRow {
    #[sqlx(flatten)]
    core: Standing,
    tracker: String,
    query: String,
    key_param: String,
    watermark: String,
    last_swept_at: Option<String>,
    last_launched_at: Option<String>,
}

impl From<WatchRow> for Watch {
    fn from(row: WatchRow) -> Watch {
        let c = row.core;
        Watch {
            id: c.id,
            playbook: c.playbook,
            adopted_repo: c.adopted_repo,
            adopted_path: c.adopted_path,
            adopted_rev: c.adopted_rev,
            params: c.params,
            schema_digest: c.schema_digest,
            max_cost: c.max_cost,
            max_time: c.max_time,
            tracker: row.tracker,
            query: row.query,
            key_param: row.key_param,
            watermark: row.watermark,
            enabled: c.enabled,
            last_swept_at: row.last_swept_at,
            last_launched_at: row.last_launched_at,
            consecutive_failures: c.consecutive_failures,
            created_by: c.created_by,
            owner_principal: c.owner_principal,
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

const SELECT: &str = "SELECT {COLUMNS}, w.tracker, w.query, w.key_param, w.watermark, \
    w.last_swept_at, w.last_launched_at \
    FROM playbook_standing_launches c JOIN playbook_watches w USING (id)";

fn select() -> String {
    SELECT.replace("{COLUMNS}", standing::COLUMNS)
}

/// One item a watch launched.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub(crate) struct Hit {
    pub item_id: String,
    pub item_updated_at: String,
    pub launch_key: String,
    pub launched_at: String,
}

/// Store a watch. The values were validated against the pack's stored schema, the ceilings
/// bounded by the admin caps, and the query checked by the tracker at the endpoint.
pub(crate) async fn create(pool: &PgPool, new: &NewWatch<'_>, now: Timestamp) -> Result<Watch> {
    let id = uuid::Uuid::now_v7().to_string();
    let created_at = crate::clock::now_rfc3339();
    let mut tx = pool.begin().await.context("create watch: begin")?;
    standing::insert(&mut tx, &id, Trigger::Watch, &new.standing, &created_at).await?;
    sqlx::query(
        "INSERT INTO playbook_watches (id, tracker, query, key_param, watermark)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&id)
    .bind(new.tracker.as_str())
    .bind(new.query)
    .bind(new.key_param)
    .bind(
        new.since
            .map(str::to_string)
            .unwrap_or_else(|| now.to_string()),
    )
    .execute(&mut *tx)
    .await
    .context("create watch")?;
    tx.commit().await.context("create watch: commit")?;
    get(pool, &id)
        .await?
        .context("the watch just stored is gone")
}

/// Replace a watch's authorization and trigger. The failure count starts over; the watermark
/// stays where the sweep left it unless the save names an explicit bound.
pub(crate) async fn update(pool: &PgPool, id: &str, new: &NewWatch<'_>) -> Result<Option<Watch>> {
    let updated_at = crate::clock::now_rfc3339();
    let mut tx = pool.begin().await.context("update watch: begin")?;
    if !standing::replace(&mut tx, id, &new.standing, &updated_at).await? {
        return Ok(None);
    }
    sqlx::query(
        "UPDATE playbook_watches
         SET tracker = $2, query = $3, key_param = $4, watermark = COALESCE($5, watermark)
         WHERE id = $1",
    )
    .bind(id)
    .bind(new.tracker.as_str())
    .bind(new.query)
    .bind(new.key_param)
    .bind(new.since)
    .execute(&mut *tx)
    .await
    .context("update watch")?;
    tx.commit().await.context("update watch: commit")?;
    get(pool, id).await
}

/// Every watch, enabled first.
pub(crate) async fn list(pool: &PgPool, limit: i64) -> Result<Vec<Watch>> {
    let sql = format!(
        "{} ORDER BY c.enabled DESC, c.created_at, c.id LIMIT $1",
        select()
    );
    let rows = sqlx::query_as::<_, WatchRow>(&sql)
        .bind(limit)
        .fetch_all(pool)
        .await
        .context("list watches")?;
    Ok(rows.into_iter().map(Watch::from).collect())
}

/// One watch by id, or `None`.
pub(crate) async fn get(pool: &PgPool, id: &str) -> Result<Option<Watch>> {
    let sql = format!("{} WHERE c.id = $1", select());
    let row = sqlx::query_as::<_, WatchRow>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .context("get watch")?;
    Ok(row.map(Watch::from))
}

/// Delete a watch. `false` when there was none under that id. Its hits go with it; the launches
/// they recorded are ordinary runs and stay.
pub(crate) async fn delete(pool: &PgPool, id: &str) -> Result<bool> {
    standing::delete(pool, id).await
}

/// Flip a watch on or off. `None` when there is no watch under that id; otherwise the prior state.
pub(crate) async fn set_enabled(pool: &PgPool, id: &str, enabled: bool) -> Result<Option<bool>> {
    let exists: Option<String> =
        sqlx::query_scalar("SELECT id FROM playbook_watches WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await
            .context("set watch enabled")?;
    if exists.is_none() {
        return Ok(None);
    }
    standing::set_enabled(pool, id, enabled).await
}

/// The items a watch has launched, newest first.
pub(crate) async fn hits(pool: &PgPool, id: &str, limit: i64) -> Result<Vec<Hit>> {
    sqlx::query_as::<_, Hit>(
        "SELECT item_id, item_updated_at, launch_key, launched_at FROM playbook_watch_hits
         WHERE watch_id = $1 ORDER BY launched_at DESC, item_id LIMIT $2",
    )
    .bind(id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("list watch hits")
}

/// Forget that a watch launched an item, so the next sweep that finds it launches it again.
/// `false` when the watch never launched that item.
pub(crate) async fn reset_hit(pool: &PgPool, id: &str, item_id: &str) -> Result<bool> {
    let deleted =
        sqlx::query("DELETE FROM playbook_watch_hits WHERE watch_id = $1 AND item_id = $2")
            .bind(id)
            .bind(item_id)
            .execute(pool)
            .await
            .context("reset watch hit")?;
    Ok(deleted.rows_affected() > 0)
}

/// The launches a watch parked for a stale owner snapshot, closed. Returns the keys closed.
pub(crate) async fn expire_stale_owner_parks(pool: &PgPool, id: &str) -> Result<Vec<String>> {
    standing::expire_stale_owner_parks(pool, Trigger::Watch, id).await
}

/// A tracker's update-time string as the RFC 3339 UTC instant the watermark stores and compares.
/// Jira's `updated` carries an offset and milliseconds; a stored watermark is already RFC 3339.
fn normalize_updated(raw: &str) -> Result<String> {
    let ts = Timestamp::strptime("%Y-%m-%dT%H:%M:%S%.f%z", raw)
        .or_else(|_| raw.parse::<Timestamp>())
        .with_context(|| format!("unparseable tracker update time {raw:?}"))?;
    Ok(ts.to_string())
}

/// The trigger row the sweep reads per enabled watch.
#[derive(sqlx::FromRow)]
struct Due {
    id: String,
    tracker: String,
    query: String,
    key_param: String,
    watermark: String,
}

async fn due(pool: &PgPool) -> Result<Vec<Due>> {
    sqlx::query_as::<_, Due>(
        "SELECT w.id, w.tracker, w.query, w.key_param, w.watermark
         FROM playbook_watches w JOIN playbook_standing_launches c USING (id)
         WHERE c.enabled ORDER BY c.created_at, w.id",
    )
    .fetch_all(pool)
    .await
    .context("read the enabled watches")
}

/// The tracker-query trigger: every unseen hit of an enabled watch is a firing. The claim is the
/// hit row, so a second sweep (or the tracker's watermark slack) finds the item already launched.
pub struct WatchTrigger {
    trackers: Trackers,
}

impl WatchTrigger {
    pub fn new(trackers: Trackers) -> Self {
        WatchTrigger { trackers }
    }

    /// One watch's unseen hits as claims (up to the cap), or the message its search failed with.
    async fn hits_of(&self, watch: &Due, seen: &[String]) -> Result<Vec<Claim>, String> {
        let kind = TrackerKind::parse(&watch.tracker)
            .map_err(|e| format!("watch {} tracker: {e}", watch.id))?;
        let tracker = self.trackers.get(kind).ok_or_else(|| {
            format!(
                "watch {} names tracker {}, which this controller has no credentials for",
                watch.id, watch.tracker
            )
        })?;
        let hits = tracker
            .search(&watch.query, Some(&watch.watermark))
            .await
            .map_err(|e| format!("watch {} search: {e:#}", watch.id))?;
        let mut claims = Vec::new();
        for hit in hits {
            if claims.len() >= LAUNCH_CAP {
                break;
            }
            let updated = normalize_updated(&hit.updated_at)
                .map_err(|e| format!("watch {} hit {}: {e:#}", watch.id, hit.id))?;
            // The tracker's watermark slack reaches back before the bound; the bound is the contract.
            if updated < watch.watermark || seen.iter().any(|s| s == &hit.id) {
                continue;
            }
            let mut claim = Claim::new(&watch.id, format!("watch {} matched {}", watch.id, hit.id));
            claim.overlay = Some((watch.key_param.clone(), hit.id.clone()));
            claim.subject = Some(hit.id.clone());
            claim.reference = Some(hit.id.clone());
            claim.payload = serde_json::json!({"item": hit.id, "updated": updated});
            claims.push(claim);
        }
        Ok(claims)
    }
}

fn item_of(claim: &Claim) -> (&str, &str) {
    (
        claim
            .payload
            .get("item")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
        claim
            .payload
            .get("updated")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
    )
}

impl LaunchTrigger for WatchTrigger {
    fn trigger(&self) -> Trigger {
        Trigger::Watch
    }

    fn due<'a>(
        &'a self,
        db: &'a Db,
        cfg: SweepCfg,
        now: Timestamp,
    ) -> TriggerFuture<'a, Result<Vec<Claim>>> {
        let db = db.clone();
        Box::pin(async move {
            let mut claims = Vec::new();
            for watch in due(db.pool()).await? {
                let seen: Vec<String> = sqlx::query_scalar(
                    "SELECT item_id FROM playbook_watch_hits WHERE watch_id = $1",
                )
                .bind(&watch.id)
                .fetch_all(db.pool())
                .await
                .context("the items already launched")?;
                match self.hits_of(&watch, &seen).await {
                    Ok(hits) => claims.extend(hits),
                    // A search that fails is a failure of that watch, not of the sweep.
                    Err(message) => {
                        standing::record_failure(
                            &db,
                            Trigger::Watch,
                            &watch.id,
                            &message,
                            cfg.auto_disable_after,
                            false,
                            true,
                        )
                        .await?;
                    }
                }
                sqlx::query("UPDATE playbook_watches SET last_swept_at = $2 WHERE id = $1")
                    .bind(&watch.id)
                    .bind(now.to_string())
                    .execute(db.pool())
                    .await
                    .context("stamp the sweep")?;
            }
            Ok(claims)
        })
    }

    fn claim<'a, 'c>(
        &'a self,
        tx: &'a mut sqlx::Transaction<'c, sqlx::Postgres>,
        claim: &'a mut Claim,
        now: Timestamp,
    ) -> TriggerFuture<'a, Result<bool>> {
        Box::pin(async move {
            let (item, updated) = item_of(claim);
            // The launch key is filled in at settle; the row is the claim.
            let seen = sqlx::query(
                "INSERT INTO playbook_watch_hits (watch_id, item_id, item_updated_at, launch_key, launched_at)
                 VALUES ($1, $2, $3, '', $4) ON CONFLICT (watch_id, item_id) DO NOTHING",
            )
            .bind(&claim.id)
            .bind(item)
            .bind(updated)
            .bind(now.to_string())
            .execute(&mut **tx)
            .await
            .context("record the hit")?;
            Ok(seen.rows_affected() > 0)
        })
    }

    fn settle<'a, 'c>(
        &'a self,
        tx: &'a mut sqlx::Transaction<'c, sqlx::Postgres>,
        claim: &'a Claim,
        key: &'a str,
        now: Timestamp,
    ) -> TriggerFuture<'a, Result<Vec<Recorded>>> {
        Box::pin(async move {
            let (item, updated) = item_of(claim);
            sqlx::query(
                "UPDATE playbook_watch_hits SET launch_key = $3 WHERE watch_id = $1 AND item_id = $2",
            )
            .bind(&claim.id)
            .bind(item)
            .bind(key)
            .execute(&mut **tx)
            .await
            .context("record the hit's launch")?;
            sqlx::query(
                "UPDATE playbook_watches SET watermark = GREATEST(watermark, $2), last_launched_at = $3
                 WHERE id = $1",
            )
            .bind(&claim.id)
            .bind(updated)
            .bind(now.to_string())
            .execute(&mut **tx)
            .await
            .context("advance the watermark")?;
            Ok(Vec::new())
        })
    }

    fn fail<'a>(
        &'a self,
        _db: &'a Db,
        _claim: &'a Claim,
        _message: &'a str,
        _now: Timestamp,
    ) -> TriggerFuture<'a, Result<Failed>> {
        Box::pin(async move {
            Ok(Failed {
                force_disable: false,
                announced: false,
            })
        })
    }
}

/// Sweep every enabled watch through the generic sweep. Returns the keys minted for the queue.
#[cfg(test)]
pub(crate) async fn sweep(
    db: &Db,
    trackers: &Trackers,
    cfg: SweepCfg,
    now: Timestamp,
    refresh: Option<&crate::identity::oidc::credentials::OwnerRefresh>,
) -> Result<Vec<String>> {
    standing::sweep(
        db,
        &WatchTrigger::new(trackers.clone()),
        cfg,
        now,
        refresh,
        None,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launches::jira::{JiraConfig, JiraTracker};
    use crate::model::MaxTime;
    use sqlx::Row;
    use std::sync::Arc;

    /// Serve canned JSON bodies over real HTTP, one per connection: the reqwest path is exercised
    /// for real, no mocks.
    fn spawn_server(bodies: Vec<String>) -> std::net::SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for body in bodies {
                let (mut socket, _) = listener.accept().expect("accept");
                let mut buf = [0u8; 8192];
                let _ = socket.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes());
            }
        });
        addr
    }

    fn trackers_for(addr: std::net::SocketAddr) -> Trackers {
        let tracker = JiraTracker::new(
            JiraConfig::from_parts(
                Some(format!("http://{addr}")),
                Some("watch@example.com".into()),
                Some("token".into()),
            )
            .expect("config"),
        )
        .expect("tracker");
        Trackers::default().with(TrackerKind::Jira, Arc::new(tracker))
    }

    fn search_body(keys: &[(&str, &str)]) -> String {
        let issues: Vec<serde_json::Value> = keys
            .iter()
            .map(|(k, updated)| serde_json::json!({"key": k, "fields": {"updated": updated}}))
            .collect();
        serde_json::json!({"issues": issues}).to_string()
    }

    const UPDATED: &str = "2026-08-26T12:00:00.000+0000";

    fn all_at(keys: &[&str]) -> String {
        let pairs: Vec<(&str, &str)> = keys.iter().map(|k| (*k, UPDATED)).collect();
        search_body(&pairs)
    }

    /// A registered backport-shaped pack: the launch path only reads repo, description, schema,
    /// and the stored tarball.
    async fn register_backport(pool: &PgPool) {
        sqlx::query(
            r#"INSERT INTO playbooks (id, description, repo, git_ref, rev, path, tar_gz,
                                      tar_digest, tar_bytes, params_schema, schema_digest,
                                      core_rev, created_by, created_at, updated_at)
               VALUES ('backport', 'Evidence-gated backport', 'neuralmagic/crucible',
                       'main', 'abc123', 'domains/backport', $1, 'sha256:tar', 3,
                       $2::jsonb, 'sha256:schema', 'core1', 'tms',
                       '2026-08-26T00:00:00Z', '2026-08-26T00:00:00Z')"#,
        )
        .bind(vec![1u8, 2, 3])
        .bind(
            serde_json::json!({
                "type": "object",
                "required": ["jira_key"],
                "additionalProperties": false,
                "properties": {
                    "jira_key": {"type": "string", "pattern": "^[A-Z][A-Z0-9]*-[0-9]+$"},
                    "release": {"type": "string"}
                }
            })
            .to_string(),
        )
        .execute(pool)
        .await
        .expect("register");
    }

    fn cfg() -> SweepCfg {
        SweepCfg {
            auto_disable_after: 5,
            owner_ttl: std::time::Duration::from_secs(3600),
        }
    }

    async fn watch_since(pool: &PgPool, since: &str) -> Watch {
        let max_time = MaxTime::parse("4h").expect("duration");
        let params = serde_json::json!({"release": "3.2"});
        create(
            pool,
            &NewWatch {
                standing: NewStanding {
                    playbook: "backport",
                    target_kind: "adopted",
                    eligible_draft_version: None,
                    params: &params,
                    schema_digest: "sha256:schema",
                    max_cost: 25.0,
                    max_time: &max_time,
                    advance_dedupe: false,
                    enabled: true,
                    created_by: Some("tms"),
                    owner_principal: None,
                    owner_groups: None,
                    dispatch_target: None,
                    agent_provider: None,
                    agent_model: None,
                },
                tracker: TrackerKind::Jira,
                query: "project = ACME AND labels = backport-request",
                key_param: "jira_key",
                since: Some(since),
            },
            Timestamp::now(),
        )
        .await
        .expect("create")
    }

    async fn launch_rows(pool: &PgPool) -> Vec<(String, String, serde_json::Value)> {
        sqlx::query("SELECT key, origin, params FROM playbook_launches ORDER BY created_at, key")
            .fetch_all(pool)
            .await
            .expect("launch rows")
            .into_iter()
            .map(|r| (r.get("key"), r.get("origin"), r.get("params")))
            .collect()
    }

    async fn hit_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM playbook_watch_hits")
            .fetch_one(pool)
            .await
            .expect("count")
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn first_sight_launches_and_a_resweep_mints_nothing(pool: PgPool) {
        register_backport(&pool).await;
        let db = Db::new(pool.clone());
        let watch = watch_since(&pool, "2026-08-01T00:00:00Z").await;
        let addr = spawn_server(vec![
            all_at(&["ACME-10192", "ACME-10300"]),
            all_at(&["ACME-10192", "ACME-10300"]),
        ]);
        let trackers = trackers_for(addr);

        let minted = sweep(&db, &trackers, cfg(), Timestamp::now(), None)
            .await
            .expect("sweep");
        assert_eq!(minted.len(), 2);
        let rows = launch_rows(&pool).await;
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|(_, origin, _)| origin == "watch"));
        assert_eq!(
            rows[0].2,
            serde_json::json!({"jira_key": "ACME-10192", "release": "3.2"}),
            "the key overlays the stored params"
        );
        let stored = get(&pool, &watch.id).await.expect("get").expect("row");
        assert_eq!(stored.watermark, "2026-08-26T12:00:00Z");
        assert!(stored.last_swept_at.is_some());
        assert!(stored.last_launched_at.is_some());
        let title: String = sqlx::query_scalar("SELECT title FROM issues WHERE key = $1")
            .bind(&rows[0].0)
            .fetch_one(&pool)
            .await
            .expect("title");
        assert_eq!(title, "Evidence-gated backport: ACME-10192");

        let again = sweep(&db, &trackers, cfg(), Timestamp::now(), None)
            .await
            .expect("resweep");
        assert!(again.is_empty(), "{again:?}");
        assert_eq!(launch_rows(&pool).await.len(), 2);
        let hits = hits(&pool, &watch.id, 10).await.expect("hits");
        let mut recorded: Vec<&str> = hits.iter().map(|h| h.launch_key.as_str()).collect();
        recorded.sort_unstable();
        let mut launched: Vec<&str> = rows.iter().map(|(k, _, _)| k.as_str()).collect();
        launched.sort_unstable();
        assert_eq!(recorded, launched, "every launch is a recorded hit");
    }

    /// Two sweeps over the same board at once: the hit row is the claim, so one of them launches
    /// and the other finds the item already taken. No advisory lock, no second launch.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn concurrent_sweeps_launch_an_item_once(pool: PgPool) {
        register_backport(&pool).await;
        watch_since(&pool, "2026-08-01T00:00:00Z").await;
        let addr = spawn_server(vec![all_at(&["ACME-42"]), all_at(&["ACME-42"])]);
        let trackers = trackers_for(addr);
        let a = (Db::new(pool.clone()), trackers.clone());
        let b = (Db::new(pool.clone()), trackers);
        let (first, second) = tokio::join!(
            tokio::spawn(async move { sweep(&a.0, &a.1, cfg(), Timestamp::now(), None).await }),
            tokio::spawn(async move { sweep(&b.0, &b.1, cfg(), Timestamp::now(), None).await }),
        );
        let minted = first.expect("join").expect("sweep").len()
            + second.expect("join").expect("sweep").len();
        assert_eq!(minted, 1, "one sweep wins the hit");
        assert_eq!(launch_rows(&pool).await.len(), 1);
        assert_eq!(hit_count(&pool).await, 1);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn the_cap_bounds_one_sweep_and_the_rest_drain_next_tick(pool: PgPool) {
        register_backport(&pool).await;
        let db = Db::new(pool.clone());
        watch_since(&pool, "2026-08-01T00:00:00Z").await;
        let keys: Vec<String> = (0..12).map(|n| format!("ACME-{}", 100 + n)).collect();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let addr = spawn_server(vec![all_at(&refs), all_at(&refs)]);
        let trackers = trackers_for(addr);
        let first = sweep(&db, &trackers, cfg(), Timestamp::now(), None)
            .await
            .expect("sweep");
        assert_eq!(first.len(), LAUNCH_CAP);
        let second = sweep(&db, &trackers, cfg(), Timestamp::now(), None)
            .await
            .expect("sweep");
        assert_eq!(second.len(), 12 - LAUNCH_CAP);
        assert_eq!(launch_rows(&pool).await.len(), 12);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn items_updated_before_the_watermark_never_launch(pool: PgPool) {
        register_backport(&pool).await;
        let db = Db::new(pool.clone());
        let watch = watch_since(&pool, "2026-08-27T00:00:00Z").await;
        let addr = spawn_server(vec![search_body(&[
            ("ACME-1", "2026-08-26T12:00:00.000+0000"),
            ("ACME-2", "2026-08-28T09:30:00.000+0200"),
        ])]);
        let minted = sweep(&db, &trackers_for(addr), cfg(), Timestamp::now(), None)
            .await
            .expect("sweep");
        assert_eq!(minted.len(), 1);
        let rows = launch_rows(&pool).await;
        assert_eq!(rows[0].2["jira_key"], "ACME-2");
        let stored = get(&pool, &watch.id).await.expect("get").expect("row");
        assert_eq!(
            stored.watermark, "2026-08-28T07:30:00Z",
            "normalized to UTC"
        );
        assert_eq!(hit_count(&pool).await, 1);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_new_watch_starts_at_its_creation(pool: PgPool) {
        register_backport(&pool).await;
        let max_time = MaxTime::parse("4h").expect("duration");
        let params = serde_json::json!({});
        let before = Timestamp::now();
        let watch = create(
            &pool,
            &NewWatch {
                standing: NewStanding {
                    playbook: "backport",
                    target_kind: "adopted",
                    eligible_draft_version: None,
                    params: &params,
                    schema_digest: "sha256:schema",
                    max_cost: 25.0,
                    max_time: &max_time,
                    advance_dedupe: false,
                    enabled: true,
                    created_by: Some("tms"),
                    owner_principal: None,
                    owner_groups: None,
                    dispatch_target: None,
                    agent_provider: None,
                    agent_model: None,
                },
                tracker: TrackerKind::Jira,
                query: "labels = backport-request",
                key_param: "jira_key",
                since: None,
            },
            before,
        )
        .await
        .expect("create");
        assert_eq!(watch.watermark, before.to_string());
        assert_eq!(watch.tracker, "jira");
        assert_eq!(watch.key_param, "jira_key");
        assert!(watch.enabled);
        assert_eq!(watch.consecutive_failures, 0);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_schema_refusal_is_a_failure_that_leaves_the_item_unseen(pool: PgPool) {
        register_backport(&pool).await;
        let db = Db::new(pool.clone());
        let watch = watch_since(&pool, "2026-08-01T00:00:00Z").await;
        // lowercase key: the pack's pattern refuses it.
        let addr = spawn_server(vec![all_at(&["bogus-1"])]);
        let minted = sweep(&db, &trackers_for(addr), cfg(), Timestamp::now(), None)
            .await
            .expect("sweep");
        assert!(minted.is_empty());
        assert!(launch_rows(&pool).await.is_empty());
        assert_eq!(
            hit_count(&pool).await,
            0,
            "the tx rolls the hit back with the launch"
        );
        let stored = get(&pool, &watch.id).await.expect("get").expect("row");
        assert_eq!(stored.consecutive_failures, 1);
        assert!(stored.enabled);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn consecutive_failures_auto_disable_the_watch(pool: PgPool) {
        register_backport(&pool).await;
        let db = Db::new(pool.clone());
        let watch = watch_since(&pool, "2026-08-01T00:00:00Z").await;
        sqlx::query("DELETE FROM playbooks WHERE id = 'backport'")
            .execute(&pool)
            .await
            .expect("unregister");
        let addr = spawn_server(vec![all_at(&["ACME-1"]), all_at(&["ACME-1"])]);
        let trackers = trackers_for(addr);
        let cfg = SweepCfg {
            auto_disable_after: 2,
            ..cfg()
        };
        sweep(&db, &trackers, cfg, Timestamp::now(), None)
            .await
            .expect("first");
        assert!(
            get(&pool, &watch.id)
                .await
                .expect("get")
                .expect("row")
                .enabled
        );
        sweep(&db, &trackers, cfg, Timestamp::now(), None)
            .await
            .expect("second");
        let stored = get(&pool, &watch.id).await.expect("get").expect("row");
        assert!(!stored.enabled, "disabled at the threshold");
        assert_eq!(stored.consecutive_failures, 2);
        let reason: Option<String> = sqlx::query_scalar(
            "SELECT reason FROM events WHERE key = $1 AND to_status = 'disabled'",
        )
        .bind(Trigger::Watch.event_key(&watch.id))
        .fetch_one(&pool)
        .await
        .expect("event");
        assert!(
            reason
                .as_deref()
                .is_some_and(|r| r.contains("no longer registered")),
            "{reason:?}"
        );
        // Disabled: the sweep reads nothing, so a third body would go unconsumed.
        let third = sweep(&db, &trackers, cfg, Timestamp::now(), None)
            .await
            .expect("third");
        assert!(third.is_empty());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_reset_item_launches_again(pool: PgPool) {
        register_backport(&pool).await;
        let db = Db::new(pool.clone());
        let watch = watch_since(&pool, "2026-08-01T00:00:00Z").await;
        let addr = spawn_server(vec![all_at(&["ACME-7"]), all_at(&["ACME-7"])]);
        let trackers = trackers_for(addr);
        assert_eq!(
            sweep(&db, &trackers, cfg(), Timestamp::now(), None)
                .await
                .expect("sweep")
                .len(),
            1
        );
        assert!(reset_hit(&pool, &watch.id, "ACME-7").await.expect("reset"));
        assert!(!reset_hit(&pool, &watch.id, "ACME-7").await.expect("reset"));
        assert_eq!(
            sweep(&db, &trackers, cfg(), Timestamp::now(), None)
                .await
                .expect("sweep")
                .len(),
            1
        );
        assert_eq!(launch_rows(&pool).await.len(), 2);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_tracker_without_credentials_fails_the_watch(pool: PgPool) {
        register_backport(&pool).await;
        let db = Db::new(pool.clone());
        let watch = watch_since(&pool, "2026-08-01T00:00:00Z").await;
        let minted = sweep(&db, &Trackers::default(), cfg(), Timestamp::now(), None)
            .await
            .expect("sweep");
        assert!(minted.is_empty());
        let stored = get(&pool, &watch.id).await.expect("get").expect("row");
        assert_eq!(stored.consecutive_failures, 1);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_watch_on_a_bound_scope_parks_without_an_owner_snapshot(pool: PgPool) {
        use crate::authz::model::Principal;
        use crate::secrets::store::{NewBinding, NewSecret};
        use crate::secrets::{
            ConsumerClass, ProjectionKind, ScopeKind, SecretKind, SecretMode, SecretName,
            Visibility,
        };
        register_backport(&pool).await;
        let owner = Principal::parse("user:tms").expect("owner");
        let name = SecretName::parse("jira_token").expect("name");
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
                vault_path: "user:tms/jira-token",
                current_version: Some(1),
                created_by: Some("tms"),
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
                scope_id: "backport",
                projection_kind: ProjectionKind::Env,
                projection: "JIRA_TOKEN",
                declared_name: &name,
                pack_rev: None,
                schema_digest: None,
                created_by: Some("tms"),
            },
        )
        .await
        .expect("bind");
        drop(conn);
        let db = Db::new(pool.clone());
        let watch = watch_since(&pool, "2026-08-01T00:00:00Z").await;
        let addr = spawn_server(vec![all_at(&["ACME-9"])]);
        let minted = sweep(&db, &trackers_for(addr), cfg(), Timestamp::now(), None)
            .await
            .expect("sweep");
        assert!(minted.is_empty(), "a parked launch is not enqueued");
        let rows = launch_rows(&pool).await;
        assert_eq!(
            rows.len(),
            1,
            "the launch row exists so the history is honest"
        );
        let (status, reason): (String, Option<String>) =
            sqlx::query_as("SELECT status, parked_reason FROM issues WHERE key = $1")
                .bind(&rows[0].0)
                .fetch_one(&pool)
                .await
                .expect("issue");
        assert_eq!(status, "parked");
        assert!(
            reason.as_deref().is_some_and(
                |r| r.starts_with(&format!("watch {} owner snapshot is stale", watch.id))
            ),
            "{reason:?}"
        );
        let expired = expire_stale_owner_parks(&pool, &watch.id)
            .await
            .expect("expire");
        assert_eq!(expired, vec![rows[0].0.clone()]);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn crud_round_trips_and_enable_resets_failures(pool: PgPool) {
        register_backport(&pool).await;
        let watch = watch_since(&pool, "2026-08-01T00:00:00Z").await;
        assert_eq!(list(&pool, 10).await.expect("list").len(), 1);
        sqlx::query("UPDATE playbook_standing_launches SET consecutive_failures = 3 WHERE id = $1")
            .bind(&watch.id)
            .execute(&pool)
            .await
            .expect("fail");
        assert_eq!(
            set_enabled(&pool, &watch.id, false).await.expect("disable"),
            Some(true)
        );
        assert_eq!(
            set_enabled(&pool, &watch.id, true).await.expect("enable"),
            Some(false)
        );
        let stored = get(&pool, &watch.id).await.expect("get").expect("row");
        assert!(stored.enabled);
        assert_eq!(
            stored.consecutive_failures, 0,
            "enabling starts the count over"
        );
        assert_eq!(
            set_enabled(&pool, "nope", true).await.expect("missing"),
            None
        );

        let max_time = MaxTime::parse("1h").expect("duration");
        let params = serde_json::json!({"release": "3.3"});
        let edited = update(
            &pool,
            &watch.id,
            &NewWatch {
                standing: NewStanding {
                    playbook: "backport",
                    target_kind: "adopted",
                    eligible_draft_version: None,
                    params: &params,
                    schema_digest: "sha256:schema",
                    max_cost: 10.0,
                    max_time: &max_time,
                    advance_dedupe: false,
                    enabled: true,
                    created_by: Some("tms"),
                    owner_principal: Some("user:tms"),
                    owner_groups: Some(&serde_json::json!(["/groups/team"])),
                    dispatch_target: None,
                    agent_provider: None,
                    agent_model: None,
                },
                tracker: TrackerKind::Jira,
                query: "labels = backport-request",
                key_param: "jira_key",
                since: None,
            },
        )
        .await
        .expect("update")
        .expect("row");
        assert_eq!(edited.query, "labels = backport-request");
        assert_eq!(edited.max_cost, 10.0);
        assert_eq!(
            edited.watermark, "2026-08-01T00:00:00Z",
            "kept without since"
        );
        assert_eq!(edited.owner_principal.as_deref(), Some("user:tms"));
        assert!(edited.owner_groups_at.is_some());
        assert!(delete(&pool, &watch.id).await.expect("delete"));
        assert!(get(&pool, &watch.id).await.expect("get").is_none());
        assert_eq!(hit_count(&pool).await, 0);
    }
}
