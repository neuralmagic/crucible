//! Webhook triggers (ADR-0055): a standing launch whose firings a sender outside the controller
//! requests. [`receive`] verifies and records a delivery; the sidecar row here holds the verifier,
//! the secret, the transform, and the launch rate bound.

pub mod presets;
pub mod receive;
pub mod transform;
pub mod trigger;
pub mod verify;

use crate::identity::oidc::credentials::CredentialKeys;
use crate::launches::standing::{self, NewStanding, Standing};
use crate::launches::webhooks::verify::{MintError, Stored, Verifier, VerifierKind};
use crate::model::Trigger;
use anyhow::{Context, Result};
use sqlx::PgPool;

/// What to create: the authorization every launch carries, the verifier, and the transform.
#[derive(Debug, Clone)]
pub(crate) struct NewWebhook<'a> {
    pub standing: NewStanding<'a>,
    pub verifier: &'a Verifier,
    pub filter: &'a str,
    pub dedupe: &'a str,
    /// Playbook param name to the CEL expression that derives it.
    pub derive: &'a serde_json::Map<String, serde_json::Value>,
    pub max_launches_per_hour: i32,
    pub retention_days: i32,
}

/// One webhook: its standing authorization and its sidecar.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub(crate) struct Webhook {
    #[sqlx(flatten)]
    pub core: Standing,
    pub verifier: VerifierKind,
    pub header: Option<String>,
    token_digest: Option<String>,
    secret_sealed: Option<String>,
    secret_key_id: Option<String>,
    pub filter: String,
    pub dedupe: String,
    pub derive: serde_json::Value,
    pub max_launches_per_hour: i32,
    pub retention_days: i32,
    pub withheld_headers: sqlx::types::Json<Vec<String>>,
    pub last_delivery_at: Option<String>,
}

impl Webhook {
    pub(crate) fn verifier(&self) -> Verifier {
        Verifier {
            kind: self.verifier,
            header: self.header.clone(),
        }
    }

    pub(crate) fn stored(&self) -> Option<Stored> {
        match (&self.token_digest, &self.secret_sealed, &self.secret_key_id) {
            (Some(digest), None, None) => Some(Stored::Digest(digest.clone())),
            (None, Some(sealed), Some(key_id)) => Some(Stored::Sealed {
                sealed: sealed.clone(),
                key_id: key_id.clone(),
            }),
            _ => None,
        }
    }
}

const SELECT: &str = const_format::concatcp!(
    "SELECT ",
    standing::COLUMNS,
    ", w.verifier, w.header, w.token_digest, w.secret_sealed, w.secret_key_id, w.filter, \
     w.dedupe, w.derive, w.max_launches_per_hour, w.retention_days, w.withheld_headers, w.last_delivery_at \
     FROM playbook_standing_launches c JOIN playbook_webhooks w USING (id)"
);

/// A stored webhook and its secret, which is never readable again.
pub(crate) struct Created {
    pub webhook: Webhook,
    pub secret: String,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SaveError {
    #[error(transparent)]
    Mint(#[from] MintError),
    #[error("{0:#}")]
    Internal(#[from] anyhow::Error),
}

fn secret_columns(stored: &Stored) -> (Option<&str>, Option<&str>, Option<&str>) {
    match stored {
        Stored::Digest(digest) => (Some(digest), None, None),
        Stored::Sealed { sealed, key_id } => (None, Some(sealed), Some(key_id)),
    }
}

/// Store a webhook and mint its secret.
pub(crate) async fn create(
    pool: &PgPool,
    keys: Option<&CredentialKeys>,
    new: &NewWebhook<'_>,
) -> Result<Created, SaveError> {
    let id = uuid::Uuid::now_v7().to_string();
    let minted = verify::mint(new.verifier.kind, &id, keys)?;
    let (digest, sealed, key_id) = secret_columns(&minted.stored);
    let now = crate::clock::now_rfc3339();
    let mut tx = pool.begin().await.context("create webhook: begin")?;
    standing::insert(&mut tx, &id, Trigger::Webhook, &new.standing, &now).await?;
    sqlx::query(
        "INSERT INTO playbook_webhooks (id, verifier, header, token_digest, secret_sealed,
             secret_key_id, filter, dedupe, derive, max_launches_per_hour, retention_days)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(&id)
    .bind(new.verifier.kind)
    .bind(new.verifier.header.as_deref())
    .bind(digest)
    .bind(sealed)
    .bind(key_id)
    .bind(new.filter)
    .bind(new.dedupe)
    .bind(serde_json::Value::Object(new.derive.clone()))
    .bind(new.max_launches_per_hour)
    .bind(new.retention_days)
    .execute(&mut *tx)
    .await
    .context("create webhook")?;
    tx.commit().await.context("create webhook: commit")?;
    let webhook = get(pool, &id)
        .await?
        .context("the webhook just stored is gone")?;
    Ok(Created {
        webhook,
        secret: minted.shown,
    })
}

pub(crate) async fn get(pool: &PgPool, id: &str) -> Result<Option<Webhook>> {
    sqlx::query_as::<_, Webhook>(const_format::concatcp!(SELECT, " WHERE c.id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
        .context("read webhook")
}

/// A webhook that may record deliveries: it exists and its standing launch is enabled.
pub(crate) async fn receivable(pool: &PgPool, id: &str) -> Result<Option<Webhook>> {
    sqlx::query_as::<_, Webhook>(const_format::concatcp!(
        SELECT,
        " WHERE c.id = $1 AND c.enabled"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("read a receivable webhook")
}

/// Replace a webhook's secret, keeping its verifier. `None` when there is no webhook under `id`.
pub(crate) async fn rotate_secret(
    pool: &PgPool,
    keys: Option<&CredentialKeys>,
    id: &str,
) -> Result<Option<String>, SaveError> {
    let Some(webhook) = get(pool, id).await? else {
        return Ok(None);
    };
    let minted = verify::mint(webhook.verifier, id, keys)?;
    let (digest, sealed, key_id) = secret_columns(&minted.stored);
    sqlx::query(
        "UPDATE playbook_webhooks SET token_digest = $2, secret_sealed = $3, secret_key_id = $4
         WHERE id = $1",
    )
    .bind(id)
    .bind(digest)
    .bind(sealed)
    .bind(key_id)
    .execute(pool)
    .await
    .context("rotate webhook secret")?;
    Ok(Some(minted.shown))
}

/// Record a verified delivery. Returns its id.
pub(crate) async fn record_delivery(
    pool: &PgPool,
    webhook_id: &str,
    headers: &serde_json::Map<String, serde_json::Value>,
    body: &[u8],
) -> Result<String> {
    let id = uuid::Uuid::now_v7().to_string();
    let now = crate::clock::now_rfc3339();
    let mut tx = pool.begin().await.context("record delivery: begin")?;
    sqlx::query(
        "INSERT INTO playbook_webhook_deliveries (id, webhook_id, received_at, headers, body)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&id)
    .bind(webhook_id)
    .bind(&now)
    .bind(serde_json::Value::Object(headers.clone()))
    .bind(body)
    .execute(&mut *tx)
    .await
    .context("record delivery")?;
    sqlx::query("UPDATE playbook_webhooks SET last_delivery_at = $2 WHERE id = $1")
        .bind(webhook_id)
        .bind(&now)
        .execute(&mut *tx)
        .await
        .context("stamp the last delivery")?;
    tx.commit().await.context("record delivery: commit")?;
    Ok(id)
}

/// How many webhooks and deliveries a list returns.
pub(crate) const LIST_LIMIT: i64 = 200;

pub(crate) async fn list(pool: &PgPool, limit: i64) -> Result<Vec<Webhook>> {
    sqlx::query_as::<_, Webhook>(const_format::concatcp!(
        SELECT,
        " ORDER BY c.enabled DESC, c.created_at DESC LIMIT $1"
    ))
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("list webhooks")
}

/// Replace a webhook's authorization and transform. A new verifier kind mints a new secret, which
/// is returned; otherwise the secret stays. `None` when there is no webhook under `id`.
pub(crate) async fn update(
    pool: &PgPool,
    keys: Option<&CredentialKeys>,
    id: &str,
    new: &NewWebhook<'_>,
) -> Result<Option<(Webhook, Option<String>)>, SaveError> {
    let Some(prior) = get(pool, id).await? else {
        return Ok(None);
    };
    let minted = (prior.verifier != new.verifier.kind)
        .then(|| verify::mint(new.verifier.kind, id, keys))
        .transpose()?;
    let mut withheld = prior.withheld_headers.0.clone();
    if let Some(old) = prior.header.as_ref()
        && new.verifier.header.as_ref() != Some(old)
        && !withheld.contains(old)
    {
        withheld.push(old.clone());
    }
    let now = crate::clock::now_rfc3339();
    let mut tx = pool.begin().await.context("update webhook: begin")?;
    if !standing::replace(&mut tx, id, &new.standing, &now).await? {
        return Ok(None);
    }
    sqlx::query(
        "UPDATE playbook_webhooks SET verifier = $2, header = $3, filter = $4, dedupe = $5,
             derive = $6, max_launches_per_hour = $7, retention_days = $8, withheld_headers = $9
         WHERE id = $1",
    )
    .bind(id)
    .bind(new.verifier.kind)
    .bind(new.verifier.header.as_deref())
    .bind(new.filter)
    .bind(new.dedupe)
    .bind(serde_json::Value::Object(new.derive.clone()))
    .bind(new.max_launches_per_hour)
    .bind(new.retention_days)
    .bind(sqlx::types::Json(&withheld))
    .execute(&mut *tx)
    .await
    .context("update webhook")?;
    if let Some(minted) = &minted {
        let (digest, sealed, key_id) = secret_columns(&minted.stored);
        sqlx::query(
            "UPDATE playbook_webhooks SET token_digest = $2, secret_sealed = $3, secret_key_id = $4
             WHERE id = $1",
        )
        .bind(id)
        .bind(digest)
        .bind(sealed)
        .bind(key_id)
        .execute(&mut *tx)
        .await
        .context("replace the webhook secret")?;
    }
    tx.commit().await.context("update webhook: commit")?;
    let webhook = get(pool, id)
        .await?
        .context("the webhook just updated is gone")?;
    Ok(Some((webhook, minted.map(|m| m.shown))))
}

pub(crate) async fn expire_stale_owner_parks(pool: &PgPool, id: &str) -> Result<Vec<String>> {
    standing::expire_stale_owner_parks(pool, Trigger::Webhook, id).await
}

/// One recorded delivery and how it settled; `outcome` is `None` while it is pending.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub(crate) struct Delivery {
    pub id: String,
    pub received_at: String,
    pub headers: serde_json::Value,
    pub body: Vec<u8>,
    pub outcome: Option<String>,
    pub reason: Option<String>,
    pub dedupe_key: Option<String>,
    pub launch_key: Option<String>,
    pub settled_at: Option<String>,
}

/// A webhook's recorded deliveries, newest first, before delivery `before` when given.
pub(crate) async fn deliveries(
    pool: &PgPool,
    id: &str,
    before: Option<&str>,
    limit: i64,
) -> Result<Vec<Delivery>> {
    sqlx::query_as(
        "SELECT id, received_at, headers, body, outcome, reason, dedupe_key, launch_key, settled_at
         FROM playbook_webhook_deliveries
         WHERE webhook_id = $1 AND ($2::text IS NULL OR id < $2)
         ORDER BY id DESC LIMIT $3",
    )
    .bind(id)
    .bind(before)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("list webhook deliveries")
}

/// Settle every pending delivery of webhook `id` failed with `reason`.
pub(crate) async fn settle_pending(pool: &PgPool, id: &str, reason: &str) -> Result<u64> {
    let settled = sqlx::query(
        "UPDATE playbook_webhook_deliveries SET outcome = 'failed', reason = $2, settled_at = $3
         WHERE webhook_id = $1 AND outcome IS NULL",
    )
    .bind(id)
    .bind(reason)
    .bind(crate::clock::now_rfc3339())
    .execute(pool)
    .await
    .context("settle pending deliveries")?;
    Ok(settled.rows_affected())
}
