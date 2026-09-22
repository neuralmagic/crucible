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
        let raw = match std::env::var("CONTROLLER_CREDENTIAL_KEY_FILE")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
        {
            Some(path) => std::fs::read_to_string(&path)
                .with_context(|| format!("reading the credential key file {path}"))?,
            None => match std::env::var("CONTROLLER_CREDENTIAL_KEY")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
            {
                Some(inline) => inline,
                None => return Ok(None),
            },
        };
        let materials = raw
            .split(['\n', '\r', ',', ' ', '\t'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                b64()
                    .decode(s)
                    .context("a credential key is not valid base64")
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        anyhow::ensure!(
            !materials.is_empty(),
            "the configured credential key material is empty"
        );
        CredentialKeys::new(materials).map(|k| Some(Arc::new(k)))
    }

    /// Seal a secret under the newest key, bound to `sub`. The stored form is
    /// `base64(nonce || ciphertext || tag)`.
    fn seal(&self, sub: &str, plaintext: &str) -> anyhow::Result<(String, String)> {
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
    fn open(&self, sub: &str, key_id: &str, sealed: &str) -> anyhow::Result<String> {
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

/// Drop a subject's credential. Returns the token it held, so the caller can tell the issuer too.
pub async fn take(
    pool: &PgPool,
    keys: &CredentialKeys,
    sub: &str,
) -> anyhow::Result<Option<String>> {
    let row = sqlx::query!(
        r#"DELETE FROM user_credentials WHERE sub = $1
           RETURNING token_cipher AS "token_cipher!", key_id AS "key_id!""#,
        sub
    )
    .fetch_optional(pool)
    .await
    .context("deleting the offline credential")?;
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

impl OwnerRefresh {
    pub fn new(pool: PgPool, provider: Arc<OidcProvider>, keys: Arc<CredentialKeys>) -> Self {
        OwnerRefresh {
            pool,
            provider,
            keys,
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
    /// A definitive refusal is recorded on the row and the credential is dropped — nothing about it
    /// will work again. An unreachable issuer rolls the transaction back and counts nothing.
    pub async fn refresh(&self, sub: &str) -> Result<RefreshOutcome, OidcError> {
        let mut tx =
            self.pool.begin().await.map_err(|e| {
                OidcError::Unavailable(format!("opening the refresh transaction: {e}"))
            })?;
        sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
            .bind(CREDENTIAL_LOCK_CLASS)
            .bind(sub)
            .execute(&mut *tx)
            .await
            .map_err(|e| OidcError::Unavailable(format!("taking the credential lock: {e}")))?;

        let row = sqlx::query!(
            r#"SELECT token_cipher AS "token_cipher!", key_id AS "key_id!"
               FROM user_credentials WHERE sub = $1"#,
            sub
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| OidcError::Unavailable(format!("reading the offline credential: {e}")))?;
        let Some(row) = row else {
            return Ok(RefreshOutcome::Absent);
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

        let now = jiff::Timestamp::now().to_string();
        if let Some(rotated) = refreshed.refresh_token.as_deref() {
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
            .map_err(|e| OidcError::Unavailable(format!("persisting the rotated token: {e}")))?;
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
            .map_err(|e| OidcError::Unavailable(format!("stamping the refresh: {e}")))?;
        }
        tx.commit()
            .await
            .map_err(|e| OidcError::Unavailable(format!("committing the refresh: {e}")))?;
        Ok(RefreshOutcome::Claims(refreshed.claims))
    }
}

/// Record a definitive refusal against the credential and COMMIT: the refusal has to survive the
/// error the caller is about to be handed, or the next tick spends the dead token again. The row
/// stays so its owner can read why they have to sign in again.
async fn record_refusal(
    mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
    sub: &str,
    why: &str,
) -> Result<(), OidcError> {
    let now = jiff::Timestamp::now().to_string();
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
    tx.commit()
        .await
        .map_err(|e| OidcError::Unavailable(format!("committing the refresh refusal: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::oidc::OidcCfg;

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
    }

    /// No credential at all is neither an error nor a refusal: it is the shape a deployment whose
    /// realm grants no `offline_access` runs in.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_subject_with_no_credential_is_absent(pool: PgPool) {
        let keys = Arc::new(keys(1));
        seed_user(&pool, "sub-alice", "alice").await;
        let refresh = OwnerRefresh::new(pool.clone(), dead_provider(), keys);
        assert_eq!(
            refresh.refresh("sub-alice").await.expect("absent"),
            RefreshOutcome::Absent
        );
        assert_eq!(status(&pool, "sub-alice").await.expect("status"), None);
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
}
