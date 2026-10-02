//! The offline credential: one encrypted refresh token per user, and the refresh that spends it.
//!
//! ```text
//!   /auth/callback --------> upsert(sub, seal(refresh_token))   one row per user, latest wins
//!   schedule sweep --------> refresh(sub) ---------------------> fresh ID token -> live groups
//!                              pg_advisory_xact_lock(sub)        rotation persisted in the lock
//!   settings ---------------> revoke(sub) --------------------> row deleted, schedules park
//! ```
//!
//! The bytes are sealed with AES-256-GCM under a key the chart mounts, with the subject as
//! additional data. The database alone opens nothing.

#![allow(clippy::disallowed_macros)]

use anyhow::Context;
use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use base64::Engine;
use sqlx::{PgConnection, PgPool};
use std::sync::Arc;

use crate::identity::oidc::{OidcError, OidcProvider, VerifiedClaims};

/// The advisory-lock class every per-subject credential lock is taken in: the ASCII bytes of
/// `"cruc"`. Postgres keeps the two-argument key space disjoint from the single-argument one
/// [`crate::client::MAINTENANCE_ADVISORY_LOCK`] lives in.
const CREDENTIAL_LOCK_CLASS: i32 = 0x6372_7563;

/// How long a failed group refresh keeps its subject off the issuer.
const FAILED_REFRESH_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);

/// AES-256 key material length.
const KEY_LEN: usize = 32;

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// One mounted key: its material and the id a sealed row records so a later open knows which key
/// to try.
struct Key {
    id: String,
    key: LessSafeKey,
}

/// The chart-mounted key set. The first key seals; any key may open, so a rotation is "prepend the
/// new key" rather than "log everybody out".
pub struct CredentialKeys {
    keys: Vec<Key>,
}

impl std::fmt::Debug for CredentialKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialKeys")
            .field(
                "key_ids",
                &self.keys.iter().map(|k| &k.id).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// The id a key is recorded under: a truncated digest of the material, so a key never has to be
/// named by hand and two deployments cannot disagree about which id means which bytes.
fn key_id(material: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(material);
    digest
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

impl CredentialKeys {
    /// Build from raw key material, newest first.
    pub fn new(materials: Vec<Vec<u8>>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !materials.is_empty(),
            "a credential key set needs at least one key"
        );
        let mut keys = Vec::with_capacity(materials.len());
        for material in materials {
            anyhow::ensure!(
                material.len() == KEY_LEN,
                "a credential key must be {KEY_LEN} bytes, got {}",
                material.len()
            );
            let unbound = UnboundKey::new(&AES_256_GCM, &material)
                .map_err(|_| anyhow::anyhow!("the credential key was refused by AES-256-GCM"))?;
            keys.push(Key {
                id: key_id(&material),
                key: LessSafeKey::new(unbound),
            });
        }
        Ok(CredentialKeys { keys })
    }

    /// Read the mounted key set: `CONTROLLER_CREDENTIAL_KEY_FILE` (a mounted file, one base64 key
    /// per non-empty line, newest first) or `CONTROLLER_CREDENTIAL_KEY` (the same, comma- or
    /// whitespace-separated). `None` when neither is configured, which turns the offline credential
    /// off and leaves scheduled launches on the schedule-row snapshot.
    pub fn from_env() -> anyhow::Result<Option<Arc<Self>>> {
        Self::from_sources(
            "credential key",
            env_value("CONTROLLER_CREDENTIAL_KEY_FILE"),
            env_value("CONTROLLER_CREDENTIAL_KEY"),
        )
    }

    /// The key webhook secrets are sealed under: `CONTROLLER_WEBHOOK_KEY_FILE` or
    /// `CONTROLLER_WEBHOOK_KEY`, in the same format as the credential key and independent of it.
    /// `None` refuses `hmac_sha256` webhooks.
    pub fn webhook_from_env() -> anyhow::Result<Option<Arc<Self>>> {
        Self::from_sources(
            "webhook key",
            env_value("CONTROLLER_WEBHOOK_KEY_FILE"),
            env_value("CONTROLLER_WEBHOOK_KEY"),
        )
    }

    /// Keys from a key file path or inline material: base64 keys, newest first, one per line or
    /// separated by commas or whitespace. The file wins. `None` when neither is given.
    pub fn from_sources(
        noun: &str,
        file: Option<String>,
        inline: Option<String>,
    ) -> anyhow::Result<Option<Arc<Self>>> {
        let raw = match (file, inline) {
            (Some(path), _) => std::fs::read_to_string(&path)
                .with_context(|| format!("reading the {noun} file {path}"))?,
            (None, Some(inline)) => inline,
            (None, None) => return Ok(None),
        };
        let materials = raw
            .split(['\n', '\r', ',', ' ', '\t'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                b64()
                    .decode(s)
                    .with_context(|| format!("a {noun} is not valid base64"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        anyhow::ensure!(
            !materials.is_empty(),
            "the configured {noun} material is empty"
        );
        CredentialKeys::new(materials).map(|k| Some(Arc::new(k)))
    }

    /// Seal a secret under the newest key, bound to `sub`. The stored form is
    /// `base64(nonce || ciphertext || tag)`.
    pub(crate) fn seal(&self, sub: &str, plaintext: &str) -> anyhow::Result<(String, String)> {
        let key = self
            .keys
            .first()
            .ok_or_else(|| anyhow::anyhow!("no credential key is mounted"))?;
        let mut nonce_bytes = [0u8; NONCE_LEN];
        aws_lc_rs::rand::fill(&mut nonce_bytes)
            .map_err(|_| anyhow::anyhow!("the system random source refused a nonce"))?;
        let mut sealed = plaintext.as_bytes().to_vec();
        key.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::from(sub.as_bytes()),
                &mut sealed,
            )
            .map_err(|_| anyhow::anyhow!("sealing the offline credential failed"))?;
        let mut framed = nonce_bytes.to_vec();
        framed.extend_from_slice(&sealed);
        Ok((b64().encode(framed), key.id.clone()))
    }

    /// Open a sealed secret. `key_id` picks the key; a row sealed under a key this deployment no
    /// longer mounts cannot be opened and its owner signs in again.
    pub(crate) fn open(&self, sub: &str, key_id: &str, sealed: &str) -> anyhow::Result<String> {
        let key = self
            .keys
            .iter()
            .find(|k| k.id == key_id)
            .ok_or_else(|| anyhow::anyhow!("no mounted credential key has id {key_id}"))?;
        let framed = b64()
            .decode(sealed)
            .context("the stored credential is not valid base64")?;
        anyhow::ensure!(
            framed.len() > NONCE_LEN,
            "the stored credential is too short to carry a nonce"
        );
        let (nonce_bytes, body) = framed.split_at(NONCE_LEN);
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(nonce_bytes);
        let mut body = body.to_vec();
        let opened = key
            .key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(sub.as_bytes()),
                &mut body,
            )
            .map_err(|_| anyhow::anyhow!("the stored credential did not authenticate"))?;
        String::from_utf8(opened.to_vec()).context("the stored credential is not utf-8")
    }
}

/// A set, non-blank environment value, trimmed.
fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Store (or replace) a subject's offline credential. Runs on the caller's connection so the
/// callback can persist it in the same transaction the `users` row is written in.
pub async fn upsert(
    conn: &mut PgConnection,
    keys: &CredentialKeys,
    sub: &str,
    refresh_token: &str,
    now: jiff::Timestamp,
) -> anyhow::Result<()> {
    let (cipher, key_id) = keys.seal(sub, refresh_token)?;
    let now = now.to_string();
    sqlx::query!(
        "INSERT INTO user_credentials (sub, token_cipher, key_id, failures, created_at, updated_at)
         VALUES ($1, $2, $3, 0, $4, $4)
         ON CONFLICT (sub) DO UPDATE
            SET token_cipher = EXCLUDED.token_cipher,
                key_id = EXCLUDED.key_id,
                last_error = NULL,
                failures = 0,
                updated_at = EXCLUDED.updated_at",
        sub,
        cipher,
        key_id,
        now,
    )
    .execute(conn)
    .await
    .context("storing the offline credential")?;
    Ok(())
}

/// What a settings page needs to say about a user's credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialStatus {
    pub refreshed_at: Option<String>,
    pub last_error: Option<String>,
    pub failures: i64,
}

pub async fn status(pool: &PgPool, sub: &str) -> anyhow::Result<Option<CredentialStatus>> {
    let row = sqlx::query!(
        r#"SELECT refreshed_at, last_error, failures AS "failures!"
           FROM user_credentials WHERE sub = $1"#,
        sub
    )
    .fetch_optional(pool)
    .await
    .context("reading the offline credential status")?;
    Ok(row.map(|r| CredentialStatus {
        refreshed_at: r.refreshed_at,
        last_error: r.last_error,
        failures: r.failures,
    }))
}

/// Drop a subject's credential and the groups it answered for. Returns the token it held, so the
/// caller can tell the issuer too.
pub async fn take(
    pool: &PgPool,
    keys: &CredentialKeys,
    sub: &str,
) -> anyhow::Result<Option<String>> {
    let mut tx = pool
        .begin()
        .await
        .context("opening the credential revoke")?;
    lock(&mut tx, sub)
        .await
        .context("taking the credential lock")?;
    let row = sqlx::query!(
        r#"DELETE FROM user_credentials WHERE sub = $1
           RETURNING token_cipher AS "token_cipher!", key_id AS "key_id!""#,
        sub
    )
    .fetch_optional(&mut *tx)
    .await
    .context("deleting the offline credential")?;
    crate::identity::oidc::users::clear_groups_on(&mut tx, sub, jiff::Timestamp::now()).await?;
    tx.commit()
        .await
        .context("committing the credential revoke")?;
    let Some(row) = row else { return Ok(None) };
    // A row sealed under a key this deploy no longer mounts is still gone; only the issuer-side
    // revoke is lost.
    Ok(keys.open(sub, &row.key_id, &row.token_cipher).ok())
}

/// The refresher: the pool the credential lives in, the issuer it is spent against, and the keys
/// that open it.
pub struct OwnerRefresh {
    pool: PgPool,
    provider: Arc<OidcProvider>,
    keys: Arc<CredentialKeys>,
    failed: FailedRefreshes,
}

/// The subjects whose last group refresh failed, and when. Expired entries are pruned on every
/// insert, so the map holds only subjects that failed within the backoff.
struct FailedRefreshes {
    backoff: std::time::Duration,
    at: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

impl FailedRefreshes {
    fn new(backoff: std::time::Duration) -> Self {
        FailedRefreshes {
            backoff,
            at: std::sync::Mutex::default(),
        }
    }

    fn holds(&self, sub: &str, now: std::time::Instant) -> bool {
        let at = self
            .at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        at.get(sub)
            .is_some_and(|failed| now.duration_since(*failed) < self.backoff)
    }

    fn note(&self, sub: &str, now: std::time::Instant) {
        let mut at = self
            .at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        at.retain(|_, failed| now.duration_since(*failed) < self.backoff);
        at.insert(sub.to_string(), now);
    }
}

/// What one refresh attempt produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// The issuer minted a fresh ID token; these are the claims it carried.
    Claims(VerifiedClaims),
    /// This subject has no stored credential: never signed in since the credential shipped, the
    /// realm granted no `offline_access`, or the user revoked it.
    Absent,
}

impl std::fmt::Debug for OwnerRefresh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnerRefresh")
            .field("issuer", &self.provider.cfg().issuer)
            .field("keys", &self.keys)
            .finish_non_exhaustive()
    }
}

impl OwnerRefresh {
    pub fn new(pool: PgPool, provider: Arc<OidcProvider>, keys: Arc<CredentialKeys>) -> Self {
        OwnerRefresh {
            pool,
            provider,
            keys,
            failed: FailedRefreshes::new(FAILED_REFRESH_BACKOFF),
        }
    }

    /// Build one when the deployment configured both an issuer and a key. `None` otherwise, which
    /// is what keeps a proxy-mode or key-less deploy on the schedule-row snapshot alone.
    pub fn from_parts(
        pool: PgPool,
        provider: Option<Arc<OidcProvider>>,
        keys: Option<Arc<CredentialKeys>>,
    ) -> Option<Arc<Self>> {
        Some(Arc::new(OwnerRefresh::new(pool, provider?, keys?)))
    }

    /// The same, off the environment.
    pub fn from_env(pool: PgPool) -> anyhow::Result<Option<Arc<Self>>> {
        let provider = OidcProvider::from_env().context("reading the oidc registration")?;
        let keys = CredentialKeys::from_env().context("reading the offline credential key")?;
        Ok(OwnerRefresh::from_parts(pool, provider, keys))
    }

    /// Spend `sub`'s credential and hand back the claims the issuer answered with.
    ///
    /// The whole exchange runs inside one transaction holding `pg_advisory_xact_lock` keyed by the
    /// subject, so two schedules of one owner firing in the same sweep serialize: the second waits
    /// for the first to commit the rotated token instead of spending the one the first just
    /// invalidated.
    ///
    /// A definitive refusal, including an answer for another subject, is recorded on the row and
    /// clears the owner's stored groups, as does a credential that is gone before or after the
    /// exchange. An unreachable issuer rolls the transaction back and counts nothing.
    pub async fn refresh(&self, sub: &str) -> Result<RefreshOutcome, OidcError> {
        let tx = self.locked(sub).await?;
        self.exchange(tx, sub).await
    }

    /// The groups `sub` holds now, for a caller that found their stamp older than `window`. Another
    /// caller may have restamped it while this one waited on the credential lock, and then the
    /// stored groups stand; otherwise the issuer answers. A failed exchange holds no groups and
    /// keeps `sub` off the issuer for [`FAILED_REFRESH_BACKOFF`], including callers already queued
    /// on the lock.
    pub async fn current_groups(&self, sub: &str, window: std::time::Duration) -> Vec<String> {
        match self.groups_unless_fresh(sub, window).await {
            Ok(groups) => groups,
            Err(OidcError::Rejected(why)) => {
                tracing::info!(sub, reason = %why, "group refresh refused; no groups");
                Vec::new()
            }
            Err(OidcError::Unavailable(why)) => {
                tracing::warn!(sub, error = %why, "group refresh failed; no groups");
                Vec::new()
            }
        }
    }

    async fn groups_unless_fresh(
        &self,
        sub: &str,
        window: std::time::Duration,
    ) -> Result<Vec<String>, OidcError> {
        if self.failed.holds(sub, std::time::Instant::now()) {
            return Ok(Vec::new());
        }
        let mut tx = self.locked(sub).await?;
        let stamp = crate::identity::oidc::users::stamp_on(&mut tx, sub)
            .await
            .map_err(|e| OidcError::Unavailable(format!("{e:#}")))?;
        if let Some((groups, Some(at))) = stamp
            && !crate::identity::oidc::users::stale(&at, jiff::Timestamp::now(), window)
        {
            return Ok(groups);
        }
        if self.failed.holds(sub, std::time::Instant::now()) {
            return Ok(Vec::new());
        }
        match self.exchange(tx, sub).await {
            Ok(RefreshOutcome::Claims(claims)) => Ok(claims.groups),
            Ok(RefreshOutcome::Absent) => Ok(Vec::new()),
            Err(e) => {
                self.failed.note(sub, std::time::Instant::now());
                Err(e)
            }
        }
    }

    /// A transaction holding `sub`'s credential lock.
    async fn locked(
        &self,
        sub: &str,
    ) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, OidcError> {
        let mut tx =
            self.pool.begin().await.map_err(|e| {
                OidcError::Unavailable(format!("opening the refresh transaction: {e}"))
            })?;
        lock(&mut tx, sub)
            .await
            .map_err(|e| OidcError::Unavailable(format!("taking the credential lock: {e}")))?;
        Ok(tx)
    }

    /// Spend `sub`'s credential inside `tx`, which holds its lock.
    async fn exchange(
        &self,
        mut tx: sqlx::Transaction<'static, sqlx::Postgres>,
        sub: &str,
    ) -> Result<RefreshOutcome, OidcError> {
        let row = sqlx::query!(
            r#"SELECT token_cipher AS "token_cipher!", key_id AS "key_id!"
               FROM user_credentials WHERE sub = $1"#,
            sub
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| OidcError::Unavailable(format!("reading the offline credential: {e}")))?;
        let Some(row) = row else {
            return absent(tx, sub).await;
        };
        let token = match self.keys.open(sub, &row.key_id, &row.token_cipher) {
            Ok(token) => token,
            Err(e) => {
                let why = format!("{e:#}");
                record_refusal(tx, sub, &why).await?;
                return Err(OidcError::Rejected(why));
            }
        };

        let refreshed = match self.provider.refresh(&token).await {
            Ok(refreshed) => refreshed,
            Err(OidcError::Unavailable(why)) => {
                // Transient: nothing is recorded, and the credential is exactly as it was.
                return Err(OidcError::Unavailable(why));
            }
            Err(OidcError::Rejected(why)) => {
                record_refusal(tx, sub, &why).await?;
                return Err(OidcError::Rejected(why));
            }
        };

        if let Err(why) = answers_for(sub, &refreshed.claims) {
            record_refusal(tx, sub, &why).await?;
            return Err(OidcError::Rejected(why));
        }

        let stamped = jiff::Timestamp::now();
        let now = stamped.to_string();
        let updated = if let Some(rotated) = refreshed.refresh_token.as_deref() {
            let (cipher, key_id) = self
                .keys
                .seal(sub, rotated)
                .map_err(|e| OidcError::Rejected(format!("{e:#}")))?;
            sqlx::query!(
                "UPDATE user_credentials
                 SET token_cipher = $2, key_id = $3, refreshed_at = $4, updated_at = $4,
                     last_error = NULL, failures = 0
                 WHERE sub = $1",
                sub,
                cipher,
                key_id,
                now,
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| OidcError::Unavailable(format!("persisting the rotated token: {e}")))?
        } else {
            sqlx::query!(
                "UPDATE user_credentials
                 SET refreshed_at = $2, updated_at = $2, last_error = NULL, failures = 0
                 WHERE sub = $1",
                sub,
                now,
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| OidcError::Unavailable(format!("stamping the refresh: {e}")))?
        };
        if updated.rows_affected() == 0 {
            return absent(tx, sub).await;
        }
        crate::identity::oidc::users::record_groups_on(
            &mut tx,
            sub,
            &refreshed.claims.groups,
            stamped,
        )
        .await
        .map_err(|e| OidcError::Unavailable(format!("{e:#}")))?;
        tx.commit()
            .await
            .map_err(|e| OidcError::Unavailable(format!("committing the refresh: {e}")))?;
        Ok(RefreshOutcome::Claims(refreshed.claims))
    }
}

/// Take the per-subject credential lock for the rest of `tx`.
pub(crate) async fn lock(tx: &mut PgConnection, sub: &str) -> sqlx::Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(CREDENTIAL_LOCK_CLASS)
        .bind(sub)
        .execute(tx)
        .await
        .map(|_| ())
}

/// No credential to spend: clear the owner's stored groups, as a session is downgraded, and commit.
async fn absent(
    mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
    sub: &str,
) -> Result<RefreshOutcome, OidcError> {
    crate::identity::oidc::users::clear_groups_on(&mut tx, sub, jiff::Timestamp::now())
        .await
        .map_err(|e| OidcError::Unavailable(format!("{e:#}")))?;
    tx.commit()
        .await
        .map_err(|e| OidcError::Unavailable(format!("committing the absent credential: {e}")))?;
    Ok(RefreshOutcome::Absent)
}

/// Whether a refresh's claims are about the subject whose credential was spent.
fn answers_for(sub: &str, claims: &VerifiedClaims) -> Result<(), String> {
    if claims.sub == sub {
        Ok(())
    } else {
        Err(format!(
            "the refreshed ID token names subject {}, not {sub}",
            claims.sub
        ))
    }
}

/// Record a definitive refusal against the credential, clear the owner's stored groups, and
/// COMMIT: the refusal has to survive the error the caller is about to be handed, or the next tick
/// spends the dead token again. The row stays so its owner can read why they have to sign in again.
async fn record_refusal(
    mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
    sub: &str,
    why: &str,
) -> Result<(), OidcError> {
    let stamped = jiff::Timestamp::now();
    let now = stamped.to_string();
    sqlx::query!(
        "UPDATE user_credentials SET last_error = $2, failures = failures + 1, updated_at = $3
         WHERE sub = $1",
        sub,
        why,
        now,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| OidcError::Unavailable(format!("recording the refresh refusal: {e}")))?;
    crate::identity::oidc::users::clear_groups_on(&mut tx, sub, stamped)
        .await
        .map_err(|e| OidcError::Unavailable(format!("{e:#}")))?;
    tx.commit()
        .await
        .map_err(|e| OidcError::Unavailable(format!("committing the refresh refusal: {e}")))
}

/// The groups `sub` holds now: the stored ones while their stamp is younger than the window a
/// session re-reads its own groups at, and past it whatever a refresh of the owner's offline
/// credential answers. A refusal, no credential, no refresher, or an unreachable issuer is none.
pub async fn current_groups(
    refresh: Option<&OwnerRefresh>,
    sub: &str,
    stored: Vec<String>,
    groups_at: Option<&str>,
) -> Vec<String> {
    let window = crate::identity::oidc::users::session_group_refresh_interval();
    if groups_at
        .is_some_and(|at| !crate::identity::oidc::users::stale(at, jiff::Timestamp::now(), window))
    {
        return stored;
    }
    match refresh {
        Some(refresh) => refresh.current_groups(sub, window).await,
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use crate::identity::oidc::OidcCfg;
    use crate::identity::oidc::credentials::*;

    /// An issuer nothing is listening on: a real unreachable endpoint, not a stubbed one, so the
    /// transport failure the refresh has to treat as transient is the genuine article.
    const DEAD_ISSUER: &str = "http://127.0.0.1:1/realms/nobody";

    fn dead_provider() -> Arc<OidcProvider> {
        Arc::new(
            OidcProvider::new(OidcCfg {
                issuer: DEAD_ISSUER.to_string(),
                client_id: "rp".to_string(),
                client_secret: None,
                redirect_url: "http://localhost/auth/callback".to_string(),
                scopes: vec!["openid".to_string()],
                device_client_id: None,
                post_logout_redirect: None,
            })
            .expect("provider"),
        )
    }

    async fn seed_user(pool: &PgPool, sub: &str, login: &str) {
        crate::identity::oidc::users::record_login(pool, sub, login, None, jiff::Timestamp::now())
            .await
            .expect("record login");
    }

    async fn seed_groups(pool: &PgPool, sub: &str, groups: &[&str]) {
        let groups = groups.iter().map(|g| g.to_string()).collect::<Vec<_>>();
        let mut conn = pool.acquire().await.expect("conn");
        crate::identity::oidc::users::record_groups_on(
            &mut conn,
            sub,
            &groups,
            jiff::Timestamp::now(),
        )
        .await
        .expect("record groups");
    }

    async fn seed_credential(pool: &PgPool, keys: &CredentialKeys, sub: &str) {
        let mut conn = pool.acquire().await.expect("conn");
        upsert(&mut conn, keys, sub, "offline", jiff::Timestamp::now())
            .await
            .expect("store");
    }

    /// The groups and stamp a subject's `users` row holds.
    async fn stored_groups(pool: &PgPool, sub: &str) -> (Vec<String>, Option<String>) {
        let (groups, at): (sqlx::types::Json<Vec<String>>, Option<String>) =
            sqlx::query_as("SELECT groups, groups_at FROM users WHERE sub = $1")
                .bind(sub)
                .fetch_one(pool)
                .await
                .expect("users row");
        (groups.0, at)
    }

    /// The subject's row holds no groups, stamped as the issuer's current answer.
    async fn assert_cleared(pool: &PgPool, sub: &str) {
        let (groups, at) = stored_groups(pool, sub).await;
        assert!(groups.is_empty(), "{groups:?}");
        assert!(at.is_some(), "an empty answer is stamped like any other");
    }

    /// A successful refresh stamps the groups the issuer's signed ID token carried on the owner's
    /// row, replacing what was there, and stores the rotated token.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_refresh_stamps_the_issuers_groups_on_the_owners_row(pool: PgPool) {
        let keys = Arc::new(keys(1));
        seed_user(&pool, "sub-alice", "alice").await;
        seed_groups(&pool, "sub-alice", &["/groups/stale"]).await;
        seed_credential(&pool, &keys, "sub-alice").await;

        let (_server, provider) =
            crate::identity::oidc::tests::issuer_signing_for("sub-alice", &["/Groups/Team-X"])
                .await;
        let refresh = OwnerRefresh::new(pool.clone(), provider, keys.clone());
        let RefreshOutcome::Claims(claims) = refresh.refresh("sub-alice").await.expect("refresh")
        else {
            panic!("the credential is stored");
        };
        assert_eq!(claims.groups, vec!["/groups/team-x".to_string()]);
        let (groups, at) = stored_groups(&pool, "sub-alice").await;
        assert_eq!(groups, vec!["/groups/team-x".to_string()]);
        assert!(at.is_some(), "the stamp records when the issuer answered");
        assert_eq!(
            take(&pool, &keys, "sub-alice").await.expect("take"),
            Some("rotated".to_string()),
            "the rotated token replaced the spent one"
        );
    }

    /// An issuer answering a credential with an ID token for another subject is a refusal: no
    /// groups are written to either row, the owner's are cleared, and the token is not rotated.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_refresh_answering_for_another_subject_is_refused(pool: PgPool) {
        let keys = Arc::new(keys(1));
        seed_user(&pool, "sub-alice", "alice").await;
        seed_groups(&pool, "sub-alice", &["/groups/alice"]).await;
        seed_user(&pool, "sub-mallory", "mallory").await;
        seed_groups(&pool, "sub-mallory", &["/groups/mallory"]).await;
        seed_credential(&pool, &keys, "sub-alice").await;

        let (_server, provider) =
            crate::identity::oidc::tests::issuer_signing_for("sub-mallory", &["/groups/admins"])
                .await;
        let refresh = OwnerRefresh::new(pool.clone(), provider, keys.clone());
        let err = refresh.refresh("sub-alice").await.expect_err("mismatch");
        assert!(
            matches!(&err, OidcError::Rejected(why) if why.contains("sub-mallory")),
            "{err:?}"
        );
        assert_cleared(&pool, "sub-alice").await;
        assert_eq!(
            stored_groups(&pool, "sub-mallory").await.0,
            vec!["/groups/mallory".to_string()],
            "the other subject's row is untouched"
        );
        let status = status(&pool, "sub-alice")
            .await
            .expect("status")
            .expect("the row stays so its owner can read why");
        assert_eq!(status.failures, 1);
        assert_eq!(
            take(&pool, &keys, "sub-alice").await.expect("take"),
            Some("offline".to_string()),
            "the rotated token from the wrong answer is not stored"
        );
    }

    #[test]
    fn claims_answer_only_for_their_own_subject() {
        let claims = |sub: &str| VerifiedClaims {
            sub: sub.to_string(),
            login: "alice".to_string(),
            email: None,
            groups: vec!["/groups/a".to_string()],
        };
        assert!(answers_for("sub-alice", &claims("sub-alice")).is_ok());
        let why = answers_for("sub-alice", &claims("sub-mallory")).expect_err("mismatch");
        assert!(
            why.contains("sub-mallory") && why.contains("sub-alice"),
            "{why}"
        );
        assert!(answers_for("sub-alice", &claims("SUB-ALICE")).is_err());
    }

    /// A revoke drops the credential and the groups it answered for in one write.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_revoke_clears_the_owners_groups(pool: PgPool) {
        let keys = keys(1);
        seed_user(&pool, "sub-alice", "alice").await;
        seed_groups(&pool, "sub-alice", &["/groups/alice"]).await;
        seed_user(&pool, "sub-bob", "bob").await;
        seed_groups(&pool, "sub-bob", &["/groups/bob"]).await;
        seed_credential(&pool, &keys, "sub-alice").await;

        assert_eq!(
            take(&pool, &keys, "sub-alice").await.expect("take"),
            Some("offline".to_string())
        );
        assert_cleared(&pool, "sub-alice").await;
        assert_eq!(
            stored_groups(&pool, "sub-bob").await.0,
            vec!["/groups/bob".to_string()]
        );
    }

    /// The callback's write: one row per subject, latest login wins, and the stored bytes are not
    /// the token.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_login_stores_one_row_per_subject_and_the_latest_wins(pool: PgPool) {
        let keys = keys(1);
        seed_user(&pool, "sub-alice", "alice").await;
        let mut conn = pool.acquire().await.expect("conn");
        upsert(
            &mut conn,
            &keys,
            "sub-alice",
            "first",
            jiff::Timestamp::now(),
        )
        .await
        .expect("first");
        upsert(
            &mut conn,
            &keys,
            "sub-alice",
            "second",
            jiff::Timestamp::now(),
        )
        .await
        .expect("second");
        drop(conn);

        let rows: i64 = sqlx::query_scalar("select count(*) from user_credentials")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(rows, 1, "one row per user, latest login wins");
        let stored: String = sqlx::query_scalar("select token_cipher from user_credentials")
            .fetch_one(&pool)
            .await
            .expect("cipher");
        assert!(
            !stored.contains("second") && !stored.contains("first"),
            "the token must not be readable in the row"
        );
        assert_eq!(
            take(&pool, &keys, "sub-alice").await.expect("take"),
            Some("second".to_string())
        );
        assert!(
            take(&pool, &keys, "sub-alice")
                .await
                .expect("second take")
                .is_none(),
            "a revoke is final"
        );
    }

    /// A refresh against an issuer that cannot be reached is transient: nothing is recorded, the
    /// credential is exactly as it was, and the caller is told to retry.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_unreachable_issuer_leaves_the_credential_untouched(pool: PgPool) {
        let keys = Arc::new(keys(1));
        seed_user(&pool, "sub-alice", "alice").await;
        seed_groups(&pool, "sub-alice", &["/groups/alice"]).await;
        let mut conn = pool.acquire().await.expect("conn");
        upsert(
            &mut conn,
            &keys,
            "sub-alice",
            "offline",
            jiff::Timestamp::now(),
        )
        .await
        .expect("store");
        drop(conn);

        let refresh = OwnerRefresh::new(pool.clone(), dead_provider(), keys.clone());
        let err = refresh.refresh("sub-alice").await.expect_err("unreachable");
        assert!(
            matches!(err, OidcError::Unavailable(_)),
            "a transport failure must not read as a refusal: {err:?}"
        );
        let status = status(&pool, "sub-alice")
            .await
            .expect("status")
            .expect("still stored");
        assert_eq!(status.failures, 0, "a transient failure counts nothing");
        assert_eq!(status.last_error, None);
        assert_eq!(
            stored_groups(&pool, "sub-alice").await.0,
            vec!["/groups/alice".to_string()],
            "an outage keeps the last answer"
        );
        assert_eq!(
            take(&pool, &keys, "sub-alice").await.expect("take"),
            Some("offline".to_string()),
            "the stored token is unchanged"
        );
    }

    /// The deployment shape a transport failure never covers: the issuer is behind a router, the
    /// pods are restarting, and the router answers 503 with an HTML page. That is still the issuer
    /// being unreachable, so the credential must come out of it untouched.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_issuer_behind_a_failing_route_leaves_the_credential_untouched(pool: PgPool) {
        let keys = Arc::new(keys(1));
        seed_user(&pool, "sub-alice", "alice").await;
        let mut conn = pool.acquire().await.expect("conn");
        upsert(
            &mut conn,
            &keys,
            "sub-alice",
            "offline",
            jiff::Timestamp::now(),
        )
        .await
        .expect("store");
        drop(conn);

        let (_server, provider) = crate::identity::oidc::tests::issuer_answering(
            wiremock::ResponseTemplate::new(503)
                .set_body_string("<html><body>Application is not available</body></html>"),
        )
        .await;
        let refresh = OwnerRefresh::new(pool.clone(), provider, keys.clone());
        let err = refresh.refresh("sub-alice").await.expect_err("unreachable");
        assert!(
            matches!(err, OidcError::Unavailable(_)),
            "a router answering for a restarting issuer must not read as a refusal: {err:?}"
        );
        let status = status(&pool, "sub-alice")
            .await
            .expect("status")
            .expect("still stored");
        assert_eq!(status.failures, 0, "an outage counts nothing");
        assert_eq!(status.last_error, None);
        assert_eq!(
            take(&pool, &keys, "sub-alice").await.expect("take"),
            Some("offline".to_string()),
            "the stored token is unchanged"
        );
    }

    /// A row sealed under a key the deploy no longer mounts is a definitive refusal, recorded on
    /// the row so the next tick does not try it again for free.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_credential_sealed_under_a_dropped_key_is_a_refusal(pool: PgPool) {
        let old = CredentialKeys::new(vec![vec![7u8; KEY_LEN]]).expect("old");
        seed_user(&pool, "sub-alice", "alice").await;
        seed_groups(&pool, "sub-alice", &["/groups/alice"]).await;
        let mut conn = pool.acquire().await.expect("conn");
        upsert(
            &mut conn,
            &old,
            "sub-alice",
            "offline",
            jiff::Timestamp::now(),
        )
        .await
        .expect("store");
        drop(conn);

        let rotated = Arc::new(CredentialKeys::new(vec![vec![8u8; KEY_LEN]]).expect("rotated"));
        let refresh = OwnerRefresh::new(pool.clone(), dead_provider(), rotated);
        let err = refresh.refresh("sub-alice").await.expect_err("unopenable");
        assert!(matches!(err, OidcError::Rejected(_)), "{err:?}");
        let status = status(&pool, "sub-alice")
            .await
            .expect("status")
            .expect("the row stays so its owner can read why");
        assert_eq!(status.failures, 1);
        assert!(status.last_error.is_some());
        assert_cleared(&pool, "sub-alice").await;
    }

    /// No credential at all is neither an error nor a refusal: it is the shape a deployment whose
    /// realm grants no `offline_access` runs in. The owner's stored groups are cleared, as a
    /// session's are.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_subject_with_no_credential_is_absent(pool: PgPool) {
        let keys = Arc::new(keys(1));
        seed_user(&pool, "sub-alice", "alice").await;
        seed_groups(&pool, "sub-alice", &["/groups/team-x"]).await;
        let refresh = OwnerRefresh::new(pool.clone(), dead_provider(), keys);
        assert_eq!(
            refresh.refresh("sub-alice").await.expect("absent"),
            RefreshOutcome::Absent
        );
        assert_eq!(status(&pool, "sub-alice").await.expect("status"), None);
        assert_cleared(&pool, "sub-alice").await;
    }

    /// Wait until `count` backends in this database are blocked on `wait_event`, running
    /// `query_prefix`.
    async fn blocked_on(pool: &PgPool, wait_event: &str, query_prefix: &str, count: i64) {
        for _ in 0..500 {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'
                   AND wait_event = $1 AND query LIKE $2 || '%'",
            )
            .bind(wait_event)
            .bind(query_prefix)
            .fetch_one(pool)
            .await
            .expect("pg_stat_activity");
            if waiting >= count {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("nothing blocked on {wait_event} running {query_prefix}");
    }

    /// A credential deleted while its refresh is in flight takes the groups with it: the refresh's
    /// credential update matches nothing, so the issuer's answer is never stamped on the owner.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_refresh_whose_credential_is_deleted_mid_flight_writes_no_groups(pool: PgPool) {
        let keys = Arc::new(keys(1));
        seed_user(&pool, "sub-alice", "alice").await;
        seed_groups(&pool, "sub-alice", &["/groups/stale"]).await;
        seed_credential(&pool, &keys, "sub-alice").await;
        let (_server, provider) =
            crate::identity::oidc::tests::issuer_signing_for("sub-alice", &["/groups/team-x"])
                .await;

        let mut deleter = pool.begin().await.expect("deleter");
        sqlx::query("DELETE FROM user_credentials WHERE sub = 'sub-alice'")
            .execute(&mut *deleter)
            .await
            .expect("delete");
        let refresh = OwnerRefresh::new(pool.clone(), provider, keys);
        let flight = tokio::spawn(async move { refresh.refresh("sub-alice").await });
        blocked_on(&pool, "transactionid", "UPDATE user_credentials", 1).await;
        deleter.commit().await.expect("commit the delete");

        assert_eq!(
            flight.await.expect("join").expect("refresh"),
            RefreshOutcome::Absent
        );
        assert_cleared(&pool, "sub-alice").await;
    }

    /// A revoke waits for a refresh holding the subject's credential lock, so the two never
    /// interleave.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_revoke_waits_for_the_credential_lock(pool: PgPool) {
        let keys = Arc::new(keys(1));
        seed_user(&pool, "sub-alice", "alice").await;
        seed_credential(&pool, &keys, "sub-alice").await;

        let mut holder = pool.begin().await.expect("holder");
        lock(&mut holder, "sub-alice").await.expect("lock");
        let revoke = {
            let pool = pool.clone();
            let keys = keys.clone();
            tokio::spawn(async move { take(&pool, &keys, "sub-alice").await })
        };
        blocked_on(&pool, "advisory", "SELECT pg_advisory_xact_lock", 1).await;
        assert!(!revoke.is_finished());
        holder.commit().await.expect("release");
        assert_eq!(
            revoke.await.expect("join").expect("take"),
            Some("offline".to_string())
        );
    }

    /// Stamp `sub`'s stored groups two hours ago, past any session refresh window.
    async fn seed_stale_groups(pool: &PgPool, sub: &str, groups: &[&str]) {
        let groups = groups.iter().map(|g| g.to_string()).collect::<Vec<_>>();
        let aged = jiff::Timestamp::now()
            .checked_sub(jiff::SignedDuration::from_hours(2))
            .expect("aged");
        let mut conn = pool.acquire().await.expect("conn");
        crate::identity::oidc::users::record_groups_on(&mut conn, sub, &groups, aged)
            .await
            .expect("record groups");
    }

    /// How many token requests the local issuer has answered.
    async fn token_requests(server: &wiremock::MockServer) -> usize {
        server
            .received_requests()
            .await
            .expect("recording is on")
            .iter()
            .filter(|r| r.url.path() == "/token")
            .count()
    }

    const WINDOW: std::time::Duration = std::time::Duration::from_secs(600);

    /// Two callers that both found the stamp stale queue on the credential lock. The first spends
    /// the credential and restamps the row; the second finds the fresh stamp and takes it without
    /// asking the issuer again.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn concurrent_stale_callers_refresh_once(pool: PgPool) {
        let keys = Arc::new(keys(1));
        seed_user(&pool, "sub-alice", "alice").await;
        seed_stale_groups(&pool, "sub-alice", &["/groups/stale"]).await;
        seed_credential(&pool, &keys, "sub-alice").await;
        let (server, provider) =
            crate::identity::oidc::tests::issuer_signing_for("sub-alice", &["/groups/team-x"])
                .await;
        let refresh = Arc::new(OwnerRefresh::new(pool.clone(), provider, keys.clone()));

        let mut holder = pool.begin().await.expect("holder");
        lock(&mut holder, "sub-alice").await.expect("lock");
        let callers = [(); 2].map(|()| {
            let refresh = refresh.clone();
            tokio::spawn(async move { refresh.current_groups("sub-alice", WINDOW).await })
        });
        blocked_on(&pool, "advisory", "SELECT pg_advisory_xact_lock", 2).await;
        holder.commit().await.expect("release");
        for caller in callers {
            assert_eq!(
                caller.await.expect("join"),
                vec!["/groups/team-x".to_string()]
            );
        }
        assert_eq!(token_requests(&server).await, 1, "one exchange for both");
        assert_eq!(
            take(&pool, &keys, "sub-alice").await.expect("take"),
            Some("rotated".to_string()),
            "the credential rotated once and the rotation stands"
        );
    }

    /// A failed refresh holds no groups, and the next caller inside the backoff gets none without
    /// reaching the issuer.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_failed_refresh_keeps_the_subject_off_the_issuer(pool: PgPool) {
        let keys = Arc::new(keys(1));
        seed_user(&pool, "sub-alice", "alice").await;
        seed_stale_groups(&pool, "sub-alice", &["/groups/alice"]).await;
        seed_credential(&pool, &keys, "sub-alice").await;
        let (server, provider) = crate::identity::oidc::tests::issuer_answering(
            wiremock::ResponseTemplate::new(503).set_body_string("unavailable"),
        )
        .await;
        let refresh = OwnerRefresh::new(pool.clone(), provider, keys);

        assert!(refresh.current_groups("sub-alice", WINDOW).await.is_empty());
        let attempted = server.received_requests().await.expect("recording").len();
        assert_eq!(token_requests(&server).await, 1);
        assert!(refresh.current_groups("sub-alice", WINDOW).await.is_empty());
        assert_eq!(
            server.received_requests().await.expect("recording").len(),
            attempted,
            "the second caller made no request"
        );
        assert_eq!(
            stored_groups(&pool, "sub-alice").await.0,
            vec!["/groups/alice".to_string()],
            "an outage leaves the stored groups alone"
        );
    }

    /// Callers queued on the lock behind a failed exchange take the backoff instead of each
    /// spending their own exchange against the issuer.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn queued_callers_behind_a_failed_refresh_skip_the_issuer(pool: PgPool) {
        let keys = Arc::new(keys(1));
        seed_user(&pool, "sub-alice", "alice").await;
        seed_stale_groups(&pool, "sub-alice", &["/groups/alice"]).await;
        seed_credential(&pool, &keys, "sub-alice").await;
        let (server, provider) = crate::identity::oidc::tests::issuer_answering(
            wiremock::ResponseTemplate::new(503).set_body_string("unavailable"),
        )
        .await;
        let refresh = Arc::new(OwnerRefresh::new(pool.clone(), provider, keys));

        let mut holder = pool.begin().await.expect("holder");
        lock(&mut holder, "sub-alice").await.expect("lock");
        let callers = [(); 3].map(|()| {
            let refresh = refresh.clone();
            tokio::spawn(async move { refresh.current_groups("sub-alice", WINDOW).await })
        });
        blocked_on(&pool, "advisory", "SELECT pg_advisory_xact_lock", 3).await;
        holder.commit().await.expect("release");
        for caller in callers {
            assert!(caller.await.expect("join").is_empty());
        }
        assert_eq!(
            token_requests(&server).await,
            1,
            "one exchange for all three"
        );
    }

    /// A database failure before the exchange does not put a healthy subject in the backoff.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_database_failure_leaves_the_subject_out_of_the_backoff(pool: PgPool) {
        let keys = Arc::new(keys(1));
        seed_user(&pool, "sub-alice", "alice").await;
        seed_stale_groups(&pool, "sub-alice", &["/groups/alice"]).await;
        seed_credential(&pool, &keys, "sub-alice").await;
        let (_server, provider) =
            crate::identity::oidc::tests::issuer_signing_for("sub-alice", &["/groups/team-x"])
                .await;
        let refresh = OwnerRefresh::new(pool.clone(), provider, keys);

        pool.close().await;
        assert!(refresh.current_groups("sub-alice", WINDOW).await.is_empty());
        assert!(!refresh.failed.holds("sub-alice", std::time::Instant::now()));
    }

    #[test]
    fn a_failed_refresh_expires_after_the_backoff_and_is_pruned() {
        let backoff = std::time::Duration::from_secs(30);
        let failed = FailedRefreshes::new(backoff);
        let t0 = std::time::Instant::now();
        let later = |secs| {
            t0.checked_add(std::time::Duration::from_secs(secs))
                .expect("instant")
        };
        assert!(!failed.holds("sub-alice", t0));
        failed.note("sub-alice", t0);
        assert!(failed.holds("sub-alice", t0));
        assert!(failed.holds("sub-alice", later(29)));
        assert!(!failed.holds("sub-bob", later(29)));
        assert!(!failed.holds("sub-alice", later(30)));
        failed.note("sub-bob", later(31));
        let held = failed.at.lock().expect("lock");
        assert_eq!(
            held.keys().collect::<Vec<_>>(),
            vec!["sub-bob"],
            "an expired entry is pruned on the next insert"
        );
    }

    fn keys(count: usize) -> CredentialKeys {
        let materials = (0..count)
            .map(|n| vec![n as u8 + 1; KEY_LEN])
            .collect::<Vec<_>>();
        CredentialKeys::new(materials).expect("keys")
    }

    #[test]
    fn a_sealed_credential_opens_only_under_its_own_subject() {
        let keys = keys(1);
        let (sealed, id) = keys.seal("sub-alice", "offline-token").expect("seal");
        assert_eq!(
            keys.open("sub-alice", &id, &sealed).expect("open"),
            "offline-token"
        );
        assert!(
            keys.open("sub-mallory", &id, &sealed).is_err(),
            "the subject is additional data, so another subject's row must not open"
        );
    }

    #[test]
    fn two_seals_of_one_secret_differ() {
        let keys = keys(1);
        let (a, _) = keys.seal("sub", "same").expect("seal");
        let (b, _) = keys.seal("sub", "same").expect("seal");
        assert_ne!(a, b, "a fresh nonce per seal, or GCM is broken");
    }

    /// Rotation: the newest key seals, every mounted key still opens, and a key that is no longer
    /// mounted opens nothing.
    #[test]
    fn an_older_key_still_opens_and_a_dropped_one_does_not() {
        let old = CredentialKeys::new(vec![vec![2u8; KEY_LEN]]).expect("old");
        let (sealed, id) = old.seal("sub", "token").expect("seal");

        let rotated =
            CredentialKeys::new(vec![vec![9u8; KEY_LEN], vec![2u8; KEY_LEN]]).expect("rotated");
        assert_eq!(rotated.open("sub", &id, &sealed).expect("open"), "token");
        let (fresh, fresh_id) = rotated.seal("sub", "token").expect("seal");
        assert_ne!(fresh_id, id, "the newest key seals");

        let dropped = CredentialKeys::new(vec![vec![9u8; KEY_LEN]]).expect("dropped");
        assert!(dropped.open("sub", &id, &sealed).is_err());
        assert!(dropped.open("sub", &fresh_id, &fresh).is_ok());
    }

    #[test]
    fn a_tampered_ciphertext_never_opens() {
        let keys = keys(1);
        let (sealed, id) = keys.seal("sub", "token").expect("seal");
        let mut raw = b64().decode(&sealed).expect("decode");
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
        assert!(keys.open("sub", &id, &b64().encode(raw)).is_err());
    }

    #[test]
    fn a_key_of_the_wrong_length_is_refused() {
        assert!(CredentialKeys::new(vec![vec![0u8; 16]]).is_err());
        assert!(CredentialKeys::new(vec![]).is_err());
    }

    #[test]
    fn keys_load_from_a_file_or_inline_and_the_file_wins() {
        use base64::Engine;
        let a = base64::engine::general_purpose::STANDARD.encode([1u8; 32]);
        let b = base64::engine::general_purpose::STANDARD.encode([2u8; 32]);
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("webhook.key");
        std::fs::write(&file, format!("{a}\n\n{b}\n")).expect("write");
        let path = file.display().to_string();

        let from_file = crate::identity::oidc::credentials::CredentialKeys::from_sources(
            "webhook key",
            Some(path),
            Some("not base64!".to_string()),
        )
        .expect("file keys")
        .expect("some");
        let (sealed, _) = from_file.seal("webhook:w1", "s3cret").expect("seal");
        let inline = crate::identity::oidc::credentials::CredentialKeys::from_sources(
            "webhook key",
            None,
            Some(format!("{a}, {b}")),
        )
        .expect("inline keys")
        .expect("some");
        let key_id = from_file.seal("webhook:w1", "x").expect("seal").1;
        assert_eq!(
            inline.open("webhook:w1", &key_id, &sealed).expect("open"),
            "s3cret",
            "the newest key seals, and the same material opens it however it was given"
        );
        assert!(
            crate::identity::oidc::credentials::CredentialKeys::from_sources(
                "webhook key",
                None,
                None
            )
            .expect("none")
            .is_none()
        );
        let refused = crate::identity::oidc::credentials::CredentialKeys::from_sources(
            "webhook key",
            None,
            Some("not base64!".to_string()),
        )
        .map(|_| ())
        .expect_err("refused");
        assert!(refused.to_string().contains("webhook key"), "{refused}");
    }
}
