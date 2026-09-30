//! The webhook trigger (RFC-0003:C-WEBHOOK-SETTLEMENT): every recorded delivery settles, in the
//! order it was recorded, to exactly one outcome.
//!
//! ```text
//!   pending ──parse──filter──dedupe key──consumed?──rate──params──fire──▶ launched
//!              │       │         │           │        │      │      │
//!              ▼       ▼         ▼           ▼        ▼      ▼      ▼
//!            failed filtered   failed    duplicate throttled failed failed
//! ```

use crate::client::Db;
use crate::launches::standing::{
    Claim, Claimed, Failed, FailureCause, FireError, LaunchTrigger, Recorded, SweepCfg,
    TriggerFuture,
};
use crate::launches::webhooks::transform::{Input, Transform};
use crate::model::Trigger;
use anyhow::{Context, Result};
use jiff::{SignedDuration, Timestamp};

/// Deliveries one sweep settles per webhook; a deeper backlog drains on later ticks.
const SETTLE_CAP: i64 = 32;

const RATE_WINDOW: SignedDuration = SignedDuration::from_secs(3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Filtered,
    Duplicate,
    Throttled,
    Failed,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Filtered => "filtered",
            Outcome::Duplicate => "duplicate",
            Outcome::Throttled => "throttled",
            Outcome::Failed => "failed",
        }
    }
}

pub struct WebhookTrigger;

#[derive(Debug, thiserror::Error)]
#[error("delivery {0} settled before its launch committed")]
struct SettledElsewhere(String);

fn delivery_of(claim: &Claim) -> &str {
    claim
        .payload
        .get("delivery")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
}

async fn settle_as(
    ex: impl sqlx::PgExecutor<'_>,
    delivery: &str,
    outcome: Outcome,
    reason: &str,
    dedupe_key: Option<&str>,
    now: Timestamp,
) -> Result<()> {
    sqlx::query(
        "UPDATE playbook_webhook_deliveries
         SET outcome = $2, reason = $3, dedupe_key = $4, settled_at = $5
         WHERE id = $1 AND outcome IS NULL",
    )
    .bind(delivery)
    .bind(outcome.as_str())
    .bind(reason.replace('\0', "\u{fffd}"))
    .bind(dedupe_key)
    .bind(now.to_string())
    .execute(ex)
    .await
    .context("settle a webhook delivery")?;
    Ok(())
}

/// Settle every pending delivery of every disabled webhook failed, and prune settled deliveries
/// past their webhook's retention.
async fn housekeep(db: &Db, now: Timestamp) -> Result<()> {
    sqlx::query(
        "UPDATE playbook_webhook_deliveries d
         SET outcome = 'failed', reason = 'the webhook is disabled', settled_at = $1
         FROM playbook_standing_launches c
         WHERE d.webhook_id = c.id AND d.outcome IS NULL AND NOT c.enabled",
    )
    .bind(now.to_string())
    .execute(db.pool())
    .await
    .context("settle the deliveries of disabled webhooks")?;
    let retentions: Vec<(String, i32)> =
        sqlx::query_as("SELECT id, retention_days FROM playbook_webhooks")
            .fetch_all(db.pool())
            .await
            .context("webhook retentions")?;
    for (id, days) in retentions {
        let cutoff = now - SignedDuration::from_hours(24 * i64::from(days));
        sqlx::query(
            "DELETE FROM playbook_webhook_deliveries
             WHERE webhook_id = $1 AND outcome IS NOT NULL AND settled_at < $2",
        )
        .bind(&id)
        .bind(cutoff.to_string())
        .execute(db.pool())
        .await
        .context("prune settled deliveries")?;
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Locked {
    enabled: bool,
    filter: String,
    dedupe: String,
    derive: serde_json::Value,
    max_launches_per_hour: i32,
}

#[derive(sqlx::FromRow)]
struct Pending {
    id: String,
    received_at: String,
    headers: serde_json::Value,
    body: Vec<u8>,
}

/// Claim one delivery on the sweep's transaction: settle it here, or hand the core a launch.
async fn claim_delivery(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    claim: &mut Claim,
    now: Timestamp,
) -> Result<Claimed> {
    let locked: Option<Locked> = sqlx::query_as(
        "SELECT c.enabled, w.filter, w.dedupe, w.derive, w.max_launches_per_hour
         FROM playbook_webhooks w JOIN playbook_standing_launches c USING (id)
         WHERE w.id = $1 FOR UPDATE OF w, c",
    )
    .bind(&claim.id)
    .fetch_optional(&mut **tx)
    .await
    .context("lock the webhook")?;
    let Some(webhook) = locked else {
        return Ok(Claimed::Lost);
    };
    let delivery = delivery_of(claim).to_string();
    let oldest: Option<Pending> = sqlx::query_as(
        "SELECT id, received_at, headers, body FROM playbook_webhook_deliveries
         WHERE webhook_id = $1 AND outcome IS NULL ORDER BY id LIMIT 1",
    )
    .bind(&claim.id)
    .fetch_optional(&mut **tx)
    .await
    .context("the oldest pending delivery")?;
    let Some(pending) = oldest.filter(|p| p.id == delivery) else {
        return Ok(Claimed::Lost);
    };
    let verdict = 'settle: {
        if !webhook.enabled {
            break 'settle (Outcome::Failed, "the webhook is disabled".into(), None);
        }
        let body: serde_json::Value = match serde_json::from_slice(&pending.body) {
            Ok(body) => body,
            Err(e) => break 'settle (Outcome::Failed, format!("the body is not JSON: {e}"), None),
        };
        let derive = webhook.derive.as_object().cloned().unwrap_or_default();
        let transform = match Transform::compile(&webhook.filter, &webhook.dedupe, &derive) {
            Ok(t) => t,
            Err(refused) => {
                let detail = refused
                    .iter()
                    .map(|f| format!("{}: {}", f.field, f.message))
                    .collect::<Vec<_>>()
                    .join("; ");
                break 'settle (
                    Outcome::Failed,
                    format!("the stored transform no longer compiles: {detail}"),
                    None,
                );
            }
        };
        let headers = pending.headers.as_object().cloned().unwrap_or_default();
        let evaluated = transform.evaluate(&Input {
            delivery: &pending.id,
            body: &body,
            headers: &headers,
            received_at: &pending.received_at,
        });
        match evaluated.filter {
            Err(e) => break 'settle (Outcome::Failed, format!("filter: {e}"), None),
            Ok(false) => {
                break 'settle (Outcome::Filtered, "the filter did not match".into(), None);
            }
            Ok(true) => {}
        }
        let key = match evaluated.dedupe {
            Ok(key) => key,
            Err(e) => break 'settle (Outcome::Failed, format!("dedupe: {e}"), None),
        };
        let consumed: Option<String> = sqlx::query_scalar(
            "SELECT launch_key FROM playbook_webhook_keys WHERE webhook_id = $1 AND dedupe_key = $2",
        )
        .bind(&claim.id)
        .bind(&key)
        .fetch_optional(&mut **tx)
        .await
        .context("a consumed key")?;
        if let Some(launch) = consumed {
            break 'settle (
                Outcome::Duplicate,
                format!("key {key:?} already launched {launch}"),
                Some(key),
            );
        }
        let launched: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM playbook_webhook_keys WHERE webhook_id = $1 AND consumed_at > $2",
        )
        .bind(&claim.id)
        .bind((now - RATE_WINDOW).to_string())
        .fetch_one(&mut **tx)
        .await
        .context("launches in the rate window")?;
        if launched >= i64::from(webhook.max_launches_per_hour) {
            break 'settle (
                Outcome::Throttled,
                format!(
                    "{launched} launches in the last hour; the webhook allows {}",
                    webhook.max_launches_per_hour
                ),
                Some(key),
            );
        }
        match evaluated.params {
            Ok(overlay) => {
                claim.overlay = overlay;
                claim.payload["dedupe_key"] = serde_json::Value::String(key);
                return Ok(Claimed::Taken);
            }
            Err(e) => (Outcome::Failed, e, Some(key)),
        }
    };
    let (outcome, reason, key) = verdict;
    settle_as(&mut **tx, &delivery, outcome, &reason, key.as_deref(), now).await?;
    Ok(Claimed::Settled(Vec::new()))
}

impl LaunchTrigger for WebhookTrigger {
    fn trigger(&self) -> Trigger {
        Trigger::Webhook
    }

    fn due<'a>(
        &'a self,
        db: &'a Db,
        _cfg: SweepCfg,
        now: Timestamp,
    ) -> TriggerFuture<'a, Result<Vec<Claim>>> {
        Box::pin(async move {
            housekeep(db, now).await?;
            let pending: Vec<(String, String)> = sqlx::query_as(
                "SELECT webhook_id, id FROM (
                     SELECT d.webhook_id, d.id,
                            row_number() OVER (PARTITION BY d.webhook_id ORDER BY d.id) AS n
                     FROM playbook_webhook_deliveries d
                     JOIN playbook_standing_launches c ON c.id = d.webhook_id
                     WHERE d.outcome IS NULL AND c.enabled
                 ) ranked
                 WHERE n <= $1 ORDER BY webhook_id, id",
            )
            .bind(SETTLE_CAP)
            .fetch_all(db.pool())
            .await
            .context("pending webhook deliveries")?;
            Ok(pending
                .into_iter()
                .map(|(webhook, delivery)| {
                    let mut claim =
                        Claim::new(&webhook, format!("webhook {webhook} delivery {delivery}"));
                    claim.reference = Some(delivery.clone());
                    claim.payload = serde_json::json!({ "delivery": delivery });
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
    ) -> TriggerFuture<'a, Result<Claimed>> {
        Box::pin(claim_delivery(tx, claim, now))
    }

    fn settle<'a, 'c>(
        &'a self,
        tx: &'a mut sqlx::Transaction<'c, sqlx::Postgres>,
        claim: &'a Claim,
        key: &'a str,
        now: Timestamp,
    ) -> TriggerFuture<'a, Result<Vec<Recorded>>> {
        Box::pin(async move {
            let delivery = delivery_of(claim);
            let dedupe_key = claim
                .payload
                .get("dedupe_key")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let settled = sqlx::query(
                "UPDATE playbook_webhook_deliveries
                 SET outcome = 'launched', reason = NULL, dedupe_key = $2, launch_key = $3,
                     settled_at = $4
                 WHERE id = $1 AND outcome IS NULL",
            )
            .bind(delivery)
            .bind(dedupe_key)
            .bind(key)
            .bind(now.to_string())
            .execute(&mut **tx)
            .await
            .context("settle the launched delivery")?;
            if settled.rows_affected() != 1 {
                return Err(SettledElsewhere(delivery.to_string()).into());
            }
            sqlx::query(
                "INSERT INTO playbook_webhook_keys
                     (webhook_id, dedupe_key, delivery_id, launch_key, consumed_at)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(&claim.id)
            .bind(dedupe_key)
            .bind(delivery)
            .bind(key)
            .bind(now.to_string())
            .execute(&mut **tx)
            .await
            .context("consume the dedupe key")?;
            Ok(Vec::new())
        })
    }

    fn retire<'a>(&'a self, db: &'a Db, _claim: &'a Claim) -> TriggerFuture<'a, Result<()>> {
        Box::pin(async move { housekeep(db, Timestamp::now()).await })
    }

    fn fail<'a>(
        &'a self,
        db: &'a Db,
        claim: &'a Claim,
        error: &'a FireError,
        now: Timestamp,
    ) -> TriggerFuture<'a, Result<Failed>> {
        Box::pin(async move {
            let key = claim.payload.get("dedupe_key").and_then(|v| v.as_str());
            settle_as(
                db.pool(),
                delivery_of(claim),
                Outcome::Failed,
                &error.message,
                key,
                now,
            )
            .await?;
            Ok(Failed {
                counts: error.cause == FailureCause::Firing,
                force_disable: false,
                announced: false,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::client::Db;
    use crate::launches::standing::{self, NewStanding, SweepCfg};
    use crate::launches::webhooks::trigger::WebhookTrigger;
    use crate::launches::webhooks::verify::{Verifier, VerifierKind};
    use crate::launches::webhooks::{self as store, NewWebhook};
    use crate::model::MaxTime;
    use jiff::Timestamp;
    use serde_json::json;
    use sqlx::{PgPool, Row};

    const FILTER: &str = r#""latest" in body.updated_tags"#;
    const DEDUPE: &str = "body.manifest_digests[0]";

    async fn register(pool: &PgPool) {
        sqlx::query(
            r#"INSERT INTO playbooks (id, description, repo, git_ref, rev, path, tar_gz,
                                      tar_digest, tar_bytes, params_schema, schema_digest,
                                      core_rev, created_by, created_at, updated_at)
               VALUES ('rebuild', 'Rebuild on push', 'neuralmagic/crucible', 'main', 'abc123',
                       'domains/rebuild', $1, 'sha256:tar', 3, $2::jsonb, 'sha256:schema',
                       'core1', 'tms', '2026-09-29T00:00:00Z', '2026-09-29T00:00:00Z')"#,
        )
        .bind(vec![1u8, 2, 3])
        .bind(
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "image": {"type": "string", "pattern": "^quay\\.io/"},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "release": {"type": "string"}
                }
            })
            .to_string(),
        )
        .execute(pool)
        .await
        .expect("register");
    }

    async fn webhook(pool: &PgPool, per_hour: i32, image: &str) -> String {
        let max_time = MaxTime::parse("1h").expect("duration");
        let params = json!({"release": "3.2"});
        let derive = json!({"image": image, "tags": "body.updated_tags"});
        store::create(
            pool,
            None,
            &NewWebhook {
                standing: NewStanding {
                    playbook: "rebuild",
                    target_kind: "adopted",
                    eligible_draft_version: None,
                    params: &params,
                    schema_digest: "sha256:schema",
                    max_cost: 5.0,
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
                verifier: &Verifier {
                    kind: VerifierKind::PathToken,
                    header: None,
                },
                filter: FILTER,
                dedupe: DEDUPE,
                derive: derive.as_object().expect("object"),
                max_launches_per_hour: per_hour,
                retention_days: 7,
            },
        )
        .await
        .expect("create")
        .webhook
        .core
        .id
    }

    fn push(tag: &str, digest: &str) -> String {
        json!({
            "docker_url": "quay.io/org/img",
            "updated_tags": [tag],
            "manifest_digests": [digest],
        })
        .to_string()
    }

    async fn deliver(pool: &PgPool, webhook: &str, body: &str) -> String {
        let headers = json!({"content-type": "application/json"});
        store::record_delivery(
            pool,
            webhook,
            headers.as_object().expect("object"),
            body.as_bytes(),
        )
        .await
        .expect("record")
    }

    fn cfg() -> SweepCfg {
        SweepCfg {
            auto_disable_after: 2,
            owner_ttl: std::time::Duration::from_secs(3600),
        }
    }

    async fn sweep(pool: &PgPool) -> Vec<String> {
        standing::sweep(
            &Db::new(pool.clone()),
            &WebhookTrigger,
            cfg(),
            Timestamp::now(),
            None,
            None,
        )
        .await
        .expect("sweep")
    }

    async fn outcomes(pool: &PgPool) -> Vec<(String, Option<String>, Option<String>)> {
        sqlx::query(
            "SELECT outcome, reason, dedupe_key FROM playbook_webhook_deliveries ORDER BY id",
        )
        .fetch_all(pool)
        .await
        .expect("deliveries")
        .into_iter()
        .map(|r| {
            (
                r.get::<Option<String>, _>("outcome").unwrap_or_default(),
                r.get("reason"),
                r.get("dedupe_key"),
            )
        })
        .collect()
    }

    fn kinds(rows: &[(String, Option<String>, Option<String>)]) -> Vec<&str> {
        rows.iter().map(|(o, _, _)| o.as_str()).collect()
    }

    async fn failures(pool: &PgPool, id: &str) -> (i64, bool) {
        let row = sqlx::query(
            "SELECT consecutive_failures, enabled FROM playbook_standing_launches WHERE id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("core");
        (row.get("consecutive_failures"), row.get("enabled"))
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_recorded_delivery_wakes_the_listener_before_the_next_tick(pool: PgPool) {
        use futures_util::StreamExt;
        let wait = std::time::Duration::from_secs(10);
        register(&pool).await;
        let id = webhook(&pool, 10, "body.docker_url").await;
        let mut wakes = store::delivery_wakes(pool.clone());
        tokio::time::timeout(wait, wakes.next())
            .await
            .expect("a wake once listening")
            .expect("the stream stays open");

        deliver(&pool, &id, &push("latest", "sha256:a")).await;
        tokio::time::timeout(wait, wakes.next())
            .await
            .expect("a wake for the delivery")
            .expect("the stream stays open");

        assert_eq!(sweep(&pool).await.len(), 1);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_delivery_launches_with_derived_params_and_a_replay_is_a_duplicate(pool: PgPool) {
        register(&pool).await;
        let id = webhook(&pool, 10, "body.docker_url").await;
        let first = deliver(&pool, &id, &push("latest", "sha256:a")).await;
        deliver(&pool, &id, &push("latest", "sha256:a")).await;

        let minted = sweep(&pool).await;

        assert_eq!(minted.len(), 1);
        let launch = sqlx::query("SELECT key, origin, params FROM playbook_launches")
            .fetch_one(&pool)
            .await
            .expect("launch");
        assert_eq!(launch.get::<String, _>("origin"), "webhook");
        assert_eq!(
            launch.get::<serde_json::Value, _>("params"),
            json!({"image": "quay.io/org/img", "tags": "[\"latest\"]", "release": "3.2"}),
            "derived params overlay the stored ones, a list in its JSON form"
        );
        let rows = outcomes(&pool).await;
        assert_eq!(kinds(&rows), vec!["launched", "duplicate"]);
        assert_eq!(rows[1].2.as_deref(), Some("sha256:a"));
        let key: (String, String) =
            sqlx::query_as("SELECT delivery_id, launch_key FROM playbook_webhook_keys")
                .fetch_one(&pool)
                .await
                .expect("consumed key");
        assert_eq!(key.0, first);
        assert_eq!(key.1, launch.get::<String, _>("key"));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn each_step_settles_in_recorded_order(pool: PgPool) {
        register(&pool).await;
        let id = webhook(&pool, 1, "body.docker_url").await;
        deliver(&pool, &id, "not json").await;
        deliver(&pool, &id, &push("dev", "sha256:a")).await;
        deliver(&pool, &id, &json!({"updated_tags": ["latest"]}).to_string()).await;
        deliver(&pool, &id, &push("latest", "sha256:a")).await;
        deliver(&pool, &id, &push("latest", "sha256:b")).await;

        let minted = sweep(&pool).await;

        assert_eq!(minted.len(), 1);
        let rows = outcomes(&pool).await;
        assert_eq!(
            kinds(&rows),
            vec!["failed", "filtered", "failed", "launched", "throttled"]
        );
        assert!(rows[0].1.as_deref().is_some_and(|r| r.contains("not JSON")));
        assert!(
            rows[2]
                .1
                .as_deref()
                .is_some_and(|r| r.starts_with("dedupe:"))
        );
        assert_eq!(
            failures(&pool, &id).await,
            (0, true),
            "nothing a sender caused counts toward auto-disable"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_derived_value_the_schema_refuses_fails_the_delivery_alone(pool: PgPool) {
        register(&pool).await;
        let id = webhook(&pool, 10, r#""docker.io/" + body.docker_url"#).await;
        for digest in ["sha256:a", "sha256:b", "sha256:c"] {
            deliver(&pool, &id, &push("latest", digest)).await;
        }

        assert!(sweep(&pool).await.is_empty());

        let rows = outcomes(&pool).await;
        assert_eq!(kinds(&rows), vec!["failed", "failed", "failed"]);
        assert!(rows[0].1.as_deref().is_some_and(|r| r.contains("image")));
        assert_eq!(failures(&pool, &id).await, (0, true));
        let consumed: i64 = sqlx::query_scalar("SELECT count(*) FROM playbook_webhook_keys")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(consumed, 0, "only a launch consumes a key");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_firing_failure_counts_and_disabling_settles_the_backlog(pool: PgPool) {
        register(&pool).await;
        let id = webhook(&pool, 10, "body.docker_url").await;
        sqlx::query("UPDATE playbook_standing_launches SET params = $2 WHERE id = $1")
            .bind(&id)
            .bind(json!({"retired": "x"}))
            .execute(&pool)
            .await
            .expect("drift the stored params");
        for digest in ["sha256:a", "sha256:b", "sha256:c", "sha256:d"] {
            deliver(&pool, &id, &push("latest", digest)).await;
        }

        sweep(&pool).await;
        assert_eq!(kinds(&outcomes(&pool).await)[0], "failed");
        assert_eq!(failures(&pool, &id).await, (1, true));
        sweep(&pool).await;

        assert_eq!(
            failures(&pool, &id).await,
            (2, false),
            "stored-param drift is the row's failure and auto-disables it"
        );
        let rows = outcomes(&pool).await;
        assert_eq!(kinds(&rows), vec!["failed", "failed", "failed", "failed"]);
        assert_eq!(rows[3].1.as_deref(), Some("the webhook is disabled"));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn settled_deliveries_past_retention_are_pruned_and_keys_are_kept(pool: PgPool) {
        register(&pool).await;
        let id = webhook(&pool, 10, "body.docker_url").await;
        deliver(&pool, &id, &push("latest", "sha256:a")).await;
        sweep(&pool).await;
        sqlx::query("UPDATE playbook_webhook_deliveries SET settled_at = '2020-01-01T00:00:00Z'")
            .execute(&pool)
            .await
            .expect("age");
        deliver(&pool, &id, &push("latest", "sha256:a")).await;

        sweep(&pool).await;

        let rows = outcomes(&pool).await;
        assert_eq!(
            kinds(&rows),
            vec!["duplicate"],
            "the old record is gone and its key still dedupes"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_delivery_the_database_cannot_key_fails_alone_and_the_queue_moves_on(pool: PgPool) {
        register(&pool).await;
        let id = webhook(&pool, 10, "body.docker_url").await;
        deliver(&pool, &id, &push("latest", "sha256:\u{0}")).await;
        deliver(&pool, &id, &push("latest", &"x".repeat(600))).await;
        deliver(&pool, &id, &push("latest", "sha256:ok")).await;

        let minted = sweep(&pool).await;

        assert_eq!(minted.len(), 1);
        assert_eq!(
            kinds(&outcomes(&pool).await),
            vec!["failed", "failed", "launched"]
        );
        assert_eq!(failures(&pool, &id).await, (0, true));
    }
}
