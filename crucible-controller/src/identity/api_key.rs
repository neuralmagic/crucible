//! Opaque API keys: what one looks like, how it is minted, and how a presented one is checked.
//!
//! A key reads `crk_<id>_<secret>`. Both halves matter. The `id` is the row's primary key, so
//! checking a key is one indexed lookup and one comparison — not a scan that hashes the presented
//! secret against every key the deployment has ever issued. The `secret` is 32 bytes of CSPRNG
//! output, stored only as its SHA-256, and shown to its owner exactly once.
//!
//! No password KDF. A KDF earns its cost against secrets a human chose, where the search space is
//! small enough to enumerate; against 256 uniform bits there is nothing to enumerate, so argon2
//! here would buy no resistance and charge a hash to every request the key ever makes.
//!
//! A key carries its owner's identity and its owner's groups, which is why [`Authenticated`] holds
//! both. Groups are whatever the owner's last login stamped on their `users` row: a key is never
//! more powerful than the person it belongs to, and never fresher than their last sign-in.

use crate::clock::now_rfc3339;
use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

/// What every key starts with. Distinctive on purpose: the middleware refuses anything without it
/// before touching the database, and a secret scanner has something to match on.
pub const PREFIX: &str = "crk_";

/// Bytes of entropy in the secret half.
const SECRET_BYTES: usize = 32;

/// What a mint hands back: the row to list, and the one and only time the secret exists outside
/// its owner's hands.
#[derive(Debug)]
pub struct Minted {
    pub key: ApiKey,
    /// The full `crk_…` string. Never stored, never logged, never returned again.
    pub secret: String,
}

/// The public half of a key — everything a settings page may show.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct ApiKey {
    pub id: String,
    pub name: String,
    pub created_at: String,
    /// RFC3339, or null for a key that does not expire.
    pub expires_at: Option<String>,
    /// RFC3339 of the last request this key authenticated, or null if it never has.
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
}

/// Who a valid key names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authenticated {
    pub id: String,
    pub sub: String,
    pub login: String,
    /// The owner's groups as their last login stamped them.
    pub groups: Vec<String>,
}

/// Why a presented key does not authenticate. The four are kept apart because they need four
/// different things from the caller: fix the string, mint a key, mint a new one, ask for it back.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyRefusal {
    #[error("not an api key: an api key starts with `{PREFIX}`")]
    Malformed,
    #[error("this api key is not one this controller issued")]
    Unknown,
    #[error("this api key expired at {at}; mint a new one")]
    Expired { at: String },
    #[error("this api key was revoked at {at}")]
    Revoked { at: String },
}

/// Whether a bearer is even claiming to be an API key. The middleware asks this before the
/// database does anything, and [`crate::identity::auth::require_auth`] asks it to stay out of the way.
pub fn looks_like_key(presented: &str) -> bool {
    presented.starts_with(PREFIX)
}

/// Whether `value` carries a controller key anywhere inside it: the prefix at the start, or wrapped
/// in a header value, a JSON document, or any other envelope an agent could unwrap.
pub fn contains_key(value: &str) -> bool {
    value.contains(PREFIX)
}

/// Split `crk_<id>_<secret>` into its halves. Neither may be empty: `crk__` is malformed, not a
/// lookup for the empty id.
fn split(presented: &str) -> Option<(&str, &str)> {
    let rest = presented.strip_prefix(PREFIX)?;
    let (id, secret) = rest.split_once('_')?;
    (!id.is_empty() && !secret.is_empty()).then_some((id, secret))
}

/// SHA-256 of the secret half, base64url like the secret itself, which is what the row stores.
fn digest(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

/// Mint a key for `sub`. `expires_at` is RFC3339, or `None` for a key that outlives everything.
///
/// The subject must already have a `users` row — the foreign key says so, and that is the point:
/// a key names a person the controller has seen sign in, never a bare string.
pub async fn mint(
    pool: &PgPool,
    sub: &str,
    name: &str,
    expires_at: Option<&str>,
) -> Result<Minted> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let mut bytes = [0u8; SECRET_BYTES];
    rand::fill(&mut bytes);
    let secret = URL_SAFE_NO_PAD.encode(bytes);
    let hash = digest(&secret);
    let now = now_rfc3339();

    sqlx::query!(
        "INSERT INTO api_keys (id, sub, name, secret_hash, created_at, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
        id,
        sub,
        name,
        hash,
        now,
        expires_at,
    )
    .execute(pool)
    .await
    .context("recording the api key")?;

    Ok(Minted {
        key: ApiKey {
            id: id.clone(),
            name: name.to_string(),
            created_at: now,
            expires_at: expires_at.map(str::to_string),
            last_used_at: None,
            revoked_at: None,
        },
        secret: format!("{PREFIX}{id}_{secret}"),
    })
}

/// Check a presented key and say who it names.
///
/// An unknown id and a wrong secret answer the same [`KeyRefusal::Unknown`]: telling the two apart
/// would confirm which ids exist. Expiry and revocation are told apart from both, because their
/// owner is entitled to know a key of theirs died and how.
pub async fn verify(pool: &PgPool, presented: &str) -> Result<Authenticated, KeyRefusal> {
    let Some((id, secret)) = split(presented) else {
        return Err(KeyRefusal::Malformed);
    };
    let row = sqlx::query!(
        r#"SELECT k.secret_hash AS "secret_hash!", k.expires_at, k.revoked_at,
                  u.sub AS "sub!", u.login AS "login!", u.groups AS "groups!"
           FROM api_keys k JOIN users u ON u.sub = k.sub
           WHERE k.id = $1"#,
        id,
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        tracing::warn!(error = %e, "reading the api key");
        KeyRefusal::Unknown
    })?;
    let Some(row) = row else {
        return Err(KeyRefusal::Unknown);
    };

    // Constant time, so the comparison says nothing about how much of the digest matched.
    let presented_hash = digest(secret);
    if !constant_time_eq(presented_hash.as_bytes(), row.secret_hash.as_bytes()) {
        return Err(KeyRefusal::Unknown);
    }
    if let Some(at) = row.revoked_at {
        return Err(KeyRefusal::Revoked { at });
    }
    if let Some(at) = row.expires_at
        && expired(&at)
    {
        return Err(KeyRefusal::Expired { at });
    }

    let groups = serde_json::from_value::<Vec<String>>(row.groups).unwrap_or_else(|e| {
        // A groups column that will not parse is a bug in what wrote it, not grounds to hand the
        // caller somebody's authority; the key still names them, with nothing claimed.
        tracing::error!(login = %row.login, error = %e, "unreadable stored groups; treating the key as group-less");
        Vec::new()
    });
    Ok(Authenticated {
        id: id.to_string(),
        sub: row.sub,
        login: row.login,
        groups,
    })
}

/// Whether an RFC3339 expiry has passed. An unparseable stamp is expired: a key whose lifetime
/// cannot be read is not one to keep honoring.
fn expired(at: &str) -> bool {
    match at.parse::<jiff::Timestamp>() {
        Ok(at) => at <= jiff::Timestamp::now(),
        Err(_) => true,
    }
}

/// Record that a key authenticated a request. Best effort and deliberately not awaited on the
/// request path's critical section: a write failure here must never 500 a request the key was
/// entitled to make.
pub async fn touch(pool: &PgPool, id: &str) {
    let now = now_rfc3339();
    if let Err(e) = sqlx::query!(
        "UPDATE api_keys SET last_used_at = $2 WHERE id = $1",
        id,
        now
    )
    .execute(pool)
    .await
    {
        tracing::warn!(error = %e, "stamping api key last use");
    }
}

/// Every key `sub` holds, newest first, revoked ones included so their owner can see them.
pub async fn list(pool: &PgPool, sub: &str) -> Result<Vec<ApiKey>> {
    let rows = sqlx::query!(
        r#"SELECT id AS "id!", name AS "name!", created_at AS "created_at!",
                  expires_at, last_used_at, revoked_at
           FROM api_keys WHERE sub = $1 ORDER BY created_at DESC"#,
        sub,
    )
    .fetch_all(pool)
    .await
    .context("listing the api keys")?;
    Ok(rows
        .into_iter()
        .map(|r| ApiKey {
            id: r.id,
            name: r.name,
            created_at: r.created_at,
            expires_at: r.expires_at,
            last_used_at: r.last_used_at,
            revoked_at: r.revoked_at,
        })
        .collect())
}

/// Revoke one of `sub`'s keys. Scoped to the owner in the statement, so a caller cannot revoke
/// somebody else's key by guessing its id. `false` means no live key of theirs has that id.
pub async fn revoke(pool: &PgPool, sub: &str, id: &str) -> Result<bool> {
    let now = now_rfc3339();
    let done = sqlx::query!(
        "UPDATE api_keys SET revoked_at = $3
         WHERE id = $1 AND sub = $2 AND revoked_at IS NULL",
        id,
        sub,
        now,
    )
    .execute(pool)
    .await
    .context("revoking the api key")?;
    Ok(done.rows_affected() > 0)
}

/// Length-gated constant-time byte compare, so the token check leaks no early-exit timing.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
