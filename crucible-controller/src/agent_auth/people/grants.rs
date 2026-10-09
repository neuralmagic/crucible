//! People's authorization codes and refresh tokens in Postgres, so every replica honours what any
//! replica issued and a restart signs nobody out.
//!
//! A code starts a grant family; each refresh spends the family's live token and issues the next.
//! Presenting a spent token again revokes the whole family (RFC 9700 §4.14.2): one of the two
//! holders is not the client the person approved.

use crate::agent_auth::people::{Approved, CodeGrant};
use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

/// A refresh token's family dies when no token was issued in this long.
pub const REFRESH_IDLE_SECS: u64 = 14 * 24 * 3600;
/// A family dies this long after the approval, however often it was refreshed.
pub const REFRESH_ABSOLUTE_SECS: u64 = 90 * 24 * 3600;
/// How often expired codes and families are deleted.
pub const SWEEP_PERIOD: std::time::Duration = std::time::Duration::from_secs(3600);
const SECRET_BYTES: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum GrantError {
    #[error("the code or refresh token is unknown or already used")]
    Unknown,
    #[error("the code or refresh token expired")]
    Expired,
    #[error("the refresh token was already used; every token of its approval is revoked")]
    Reused,
    #[error("the approval behind this refresh token was revoked")]
    Revoked,
    #[error("drawing random bytes failed")]
    Random,
    #[error("timestamp {0} does not fit the store")]
    Clock(u64),
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

/// What a family was approved for, as a refresh finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Family {
    pub id: String,
    pub sub: String,
    pub resource: String,
    pub cluster: String,
    pub scopes: Vec<String>,
}

/// One of a person's live approvals, for Settings to list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub resource: String,
    pub cluster: String,
    pub approved_at: u64,
    pub last_used_at: u64,
}

fn new_secret() -> Result<(String, [u8; 32]), GrantError> {
    let mut bytes = [0u8; SECRET_BYTES];
    aws_lc_rs::rand::fill(&mut bytes).map_err(|_| GrantError::Random)?;
    let secret = URL_SAFE_NO_PAD.encode(bytes);
    Ok((secret.clone(), key_of(&secret)))
}

fn key_of(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

fn at(secs: u64) -> Result<i64, GrantError> {
    i64::try_from(secs).map_err(|_| GrantError::Clock(secs))
}

fn secs(stored: i64) -> u64 {
    u64::try_from(stored).unwrap_or(0)
}

#[derive(Clone)]
pub struct Grants {
    pool: PgPool,
}

impl Grants {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn issue_code(&self, grant: &CodeGrant) -> Result<String, GrantError> {
        let (code, key) = new_secret()?;
        let json = serde_json::to_value(grant).context("encoding the code grant")?;
        sqlx::query(
            "INSERT INTO people_codes (key, sub, grant_json, expires_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(&key[..])
        .bind(&grant.approved.sub)
        .bind(json)
        .bind(at(grant.expires_at)?)
        .execute(&self.pool)
        .await
        .context("storing an authorization code")?;
        Ok(code)
    }

    /// The code's grant, deleted whether or not it is still valid: a code works at most once.
    pub async fn take_code(&self, code: &str, now: u64) -> Result<CodeGrant, GrantError> {
        let row: Option<(serde_json::Value, i64)> = sqlx::query_as(
            "DELETE FROM people_codes WHERE key = $1 RETURNING grant_json, expires_at",
        )
        .bind(&key_of(code)[..])
        .fetch_optional(&self.pool)
        .await
        .context("redeeming an authorization code")?;
        let (json, expires_at) = row.ok_or(GrantError::Unknown)?;
        if secs(expires_at) <= now {
            return Err(GrantError::Expired);
        }
        Ok(serde_json::from_value(json).context("decoding a stored code grant")?)
    }

    /// Start a family for `approved` and return its first refresh token.
    pub async fn start_family(&self, approved: &Approved, now: u64) -> Result<String, GrantError> {
        let (id, _) = new_secret()?;
        let mut tx = self.pool.begin().await.context("opening the grant write")?;
        sqlx::query(
            "INSERT INTO people_grants
               (id, sub, resource, cluster, scopes, created_at, last_used_at)
             VALUES ($1, $2, $3, $4, $5, $6, $6)",
        )
        .bind(&id)
        .bind(&approved.sub)
        .bind(&approved.resource)
        .bind(&approved.cluster)
        .bind(serde_json::json!(approved.scopes))
        .bind(at(now)?)
        .execute(&mut *tx)
        .await
        .context("storing an approval")?;
        let token = issue_token(&mut tx, &id, now).await?;
        tx.commit().await.context("committing the approval")?;
        Ok(token)
    }

    /// Spend `token` and issue its family's next one. A token already spent revokes the family.
    pub async fn rotate(&self, token: &str, now: u64) -> Result<(Family, String), GrantError> {
        let key = key_of(token);
        let mut tx = self.pool.begin().await.context("opening the refresh")?;
        let spent: Option<(String, i64)> = sqlx::query_as(
            "UPDATE people_refresh_tokens SET used_at = $2
              WHERE key = $1 AND used_at IS NULL
          RETURNING grant_id, issued_at",
        )
        .bind(&key[..])
        .bind(at(now)?)
        .fetch_optional(&mut *tx)
        .await
        .context("spending a refresh token")?;
        let Some((grant_id, issued_at)) = spent else {
            let reused: Option<String> =
                sqlx::query_scalar("SELECT grant_id FROM people_refresh_tokens WHERE key = $1")
                    .bind(&key[..])
                    .fetch_optional(&mut *tx)
                    .await
                    .context("looking up a spent refresh token")?;
            let Some(grant_id) = reused else {
                return Err(GrantError::Unknown);
            };
            revoke_family(&mut tx, &grant_id, now).await?;
            tx.commit().await.context("committing the revocation")?;
            tracing::warn!(grant = grant_id, "refresh token reused; approval revoked");
            return Err(GrantError::Reused);
        };
        let row: (String, String, String, serde_json::Value, i64, Option<i64>) = sqlx::query_as(
            "SELECT sub, resource, cluster, scopes, created_at, revoked_at
               FROM people_grants WHERE id = $1 FOR UPDATE",
        )
        .bind(&grant_id)
        .fetch_one(&mut *tx)
        .await
        .context("reading the approval")?;
        let (sub, resource, cluster, scopes, created_at, revoked_at) = row;
        if revoked_at.is_some() {
            return Err(GrantError::Revoked);
        }
        if secs(issued_at).saturating_add(REFRESH_IDLE_SECS) <= now
            || secs(created_at).saturating_add(REFRESH_ABSOLUTE_SECS) <= now
        {
            return Err(GrantError::Expired);
        }
        sqlx::query("UPDATE people_grants SET last_used_at = $2 WHERE id = $1")
            .bind(&grant_id)
            .bind(at(now)?)
            .execute(&mut *tx)
            .await
            .context("stamping the approval's use")?;
        let next = issue_token(&mut tx, &grant_id, now).await?;
        tx.commit().await.context("committing the refresh")?;
        let scopes: Vec<String> =
            serde_json::from_value(scopes).context("decoding the approval's scopes")?;
        Ok((
            Family {
                id: grant_id,
                sub,
                resource,
                cluster,
                scopes,
            },
            next,
        ))
    }

    /// Revoke one family, as a refresh that fails its person checks does.
    pub async fn revoke(&self, grant_id: &str, now: u64) -> Result<(), GrantError> {
        let mut tx = self.pool.begin().await.context("opening the revocation")?;
        revoke_family(&mut tx, grant_id, now).await?;
        tx.commit().await.context("committing the revocation")?;
        Ok(())
    }

    /// Revoke every approval `sub` holds and their outstanding codes. Returns how many approvals.
    pub async fn revoke_all(&self, sub: &str, now: u64) -> Result<u64, GrantError> {
        let mut tx = self.pool.begin().await.context("opening the sign-out")?;
        sqlx::query("DELETE FROM people_codes WHERE sub = $1")
            .bind(sub)
            .execute(&mut *tx)
            .await
            .context("dropping outstanding codes")?;
        let ids: Vec<String> = sqlx::query_scalar(
            "UPDATE people_grants SET revoked_at = $2
              WHERE sub = $1 AND revoked_at IS NULL RETURNING id",
        )
        .bind(sub)
        .bind(at(now)?)
        .fetch_all(&mut *tx)
        .await
        .context("revoking approvals")?;
        sqlx::query(
            "DELETE FROM people_refresh_tokens t USING people_grants g
              WHERE t.grant_id = g.id AND g.sub = $1 AND t.used_at IS NULL",
        )
        .bind(sub)
        .execute(&mut *tx)
        .await
        .context("dropping live refresh tokens")?;
        tx.commit().await.context("committing the sign-out")?;
        Ok(u64::try_from(ids.len()).unwrap_or(u64::MAX))
    }

    /// `sub`'s live approvals, newest first.
    pub async fn live(&self, sub: &str, now: u64) -> Result<Vec<Summary>, GrantError> {
        let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(
            "SELECT g.resource, g.cluster, g.created_at, g.last_used_at FROM people_grants g
              WHERE g.sub = $1 AND g.revoked_at IS NULL AND g.created_at > $3
                AND EXISTS (SELECT 1 FROM people_refresh_tokens t
                             WHERE t.grant_id = g.id AND t.used_at IS NULL AND t.issued_at > $2)
              ORDER BY g.created_at DESC",
        )
        .bind(sub)
        .bind(at(now.saturating_sub(REFRESH_IDLE_SECS))?)
        .bind(at(now.saturating_sub(REFRESH_ABSOLUTE_SECS))?)
        .fetch_all(&self.pool)
        .await
        .context("listing approvals")?;
        Ok(rows
            .into_iter()
            .map(|(resource, cluster, created_at, last_used_at)| Summary {
                resource,
                cluster,
                approved_at: secs(created_at),
                last_used_at: secs(last_used_at),
            })
            .collect())
    }

    /// Delete expired codes, and families that are revoked, past their absolute lifetime, or idle.
    /// Returns how many rows went.
    pub async fn sweep(&self, now: u64) -> Result<u64, GrantError> {
        let codes = sqlx::query("DELETE FROM people_codes WHERE expires_at <= $1")
            .bind(at(now)?)
            .execute(&self.pool)
            .await
            .context("sweeping codes")?
            .rows_affected();
        let families = sqlx::query(
            "DELETE FROM people_grants g
              WHERE g.revoked_at IS NOT NULL OR g.created_at <= $2
                 OR NOT EXISTS (SELECT 1 FROM people_refresh_tokens t
                                 WHERE t.grant_id = g.id AND t.used_at IS NULL AND t.issued_at > $1)",
        )
        .bind(at(now.saturating_sub(REFRESH_IDLE_SECS))?)
        .bind(at(now.saturating_sub(REFRESH_ABSOLUTE_SECS))?)
        .execute(&self.pool)
        .await
        .context("sweeping approvals")?
        .rows_affected();
        Ok(codes.saturating_add(families))
    }
}

/// Delete expired grants every [`SWEEP_PERIOD`]. A failed sweep is logged and retried next period.
pub fn spawn_sweep(grants: Grants) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_PERIOD);
        loop {
            tick.tick().await;
            match grants.sweep(crate::clock::unix_secs()).await {
                Ok(0) => {}
                Ok(gone) => tracing::info!(gone, "people grants swept"),
                Err(e) => tracing::warn!(error = %e, "sweeping people grants failed"),
            }
        }
    });
}

async fn issue_token(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    grant_id: &str,
    now: u64,
) -> Result<String, GrantError> {
    let (token, key) = new_secret()?;
    sqlx::query("INSERT INTO people_refresh_tokens (key, grant_id, issued_at) VALUES ($1, $2, $3)")
        .bind(&key[..])
        .bind(grant_id)
        .bind(at(now)?)
        .execute(&mut **tx)
        .await
        .context("storing a refresh token")?;
    Ok(token)
}

async fn revoke_family(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    grant_id: &str,
    now: u64,
) -> Result<(), GrantError> {
    sqlx::query("UPDATE people_grants SET revoked_at = $2 WHERE id = $1 AND revoked_at IS NULL")
        .bind(grant_id)
        .bind(at(now)?)
        .execute(&mut **tx)
        .await
        .context("revoking an approval")?;
    sqlx::query("DELETE FROM people_refresh_tokens WHERE grant_id = $1 AND used_at IS NULL")
        .bind(grant_id)
        .execute(&mut **tx)
        .await
        .context("dropping the approval's live token")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::agent_auth::people::grants::*;
    use crate::agent_auth::people::{CodeChallenge, LoopbackRedirect};

    const SUB: &str = "sub-alice";
    const T0: u64 = 1_800_000_000;

    async fn signed_in(pool: &PgPool) {
        crate::identity::oidc::users::record_login(
            pool,
            SUB,
            "alice",
            None,
            jiff::Timestamp::now(),
        )
        .await
        .unwrap();
    }

    fn approved() -> Approved {
        Approved {
            sub: SUB.into(),
            login: "alice".into(),
            email: None,
            resource: "https://kueue.example/mcp".into(),
            cluster: "pirate".into(),
            scopes: vec!["kueue.read".into()],
        }
    }

    fn code_grant(expires_at: u64) -> CodeGrant {
        CodeGrant {
            approved: approved(),
            redirect: LoopbackRedirect::parse("http://127.0.0.1:8765/callback").unwrap(),
            challenge: CodeChallenge::parse(&"a".repeat(43)).unwrap(),
            expires_at,
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_code_works_once_and_not_after_it_expires(pool: PgPool) {
        signed_in(&pool).await;
        let grants = Grants::new(pool.clone());
        let code = grants.issue_code(&code_grant(T0 + 60)).await.unwrap();
        assert!(code.len() >= 43);
        assert_eq!(
            grants.take_code(&code, T0).await.unwrap().approved,
            approved()
        );
        assert!(matches!(
            grants.take_code(&code, T0).await,
            Err(GrantError::Unknown)
        ));
        let code = grants.issue_code(&code_grant(T0 + 60)).await.unwrap();
        assert!(matches!(
            grants.take_code(&code, T0 + 60).await,
            Err(GrantError::Expired)
        ));
        assert!(
            matches!(grants.take_code(&code, T0).await, Err(GrantError::Unknown)),
            "an expired code is spent too"
        );
        let stored: Vec<Vec<u8>> = sqlx::query_scalar("SELECT key FROM people_codes")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(stored.is_empty());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn no_secret_is_stored_in_the_clear(pool: PgPool) {
        signed_in(&pool).await;
        let grants = Grants::new(pool.clone());
        let code = grants.issue_code(&code_grant(T0 + 60)).await.unwrap();
        let refresh = grants.start_family(&approved(), T0).await.unwrap();
        let dump: Vec<String> = sqlx::query_scalar(
            "SELECT encode(key, 'escape') || grant_json::text FROM people_codes
             UNION ALL SELECT encode(key, 'escape') FROM people_refresh_tokens",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(
            dump.iter()
                .all(|row| !row.contains(&code) && !row.contains(&refresh)),
            "{dump:?}"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn each_refresh_slides_the_idle_window_until_the_absolute_limit(pool: PgPool) {
        signed_in(&pool).await;
        let grants = Grants::new(pool);
        let mut token = grants.start_family(&approved(), T0).await.unwrap();
        let mut now = T0;
        while now + REFRESH_IDLE_SECS - 1 < T0 + REFRESH_ABSOLUTE_SECS {
            now += REFRESH_IDLE_SECS - 1;
            let (family, next) = grants.rotate(&token, now).await.unwrap();
            assert_eq!(family.resource, "https://kueue.example/mcp");
            assert_eq!(family.scopes, ["kueue.read"]);
            token = next;
        }
        assert!(matches!(
            grants.rotate(&token, T0 + REFRESH_ABSOLUTE_SECS).await,
            Err(GrantError::Expired)
        ));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_token_idle_for_fourteen_days_is_expired(pool: PgPool) {
        signed_in(&pool).await;
        let grants = Grants::new(pool);
        let token = grants.start_family(&approved(), T0).await.unwrap();
        assert!(matches!(
            grants.rotate(&token, T0 + REFRESH_IDLE_SECS).await,
            Err(GrantError::Expired)
        ));
        let fresh = grants.start_family(&approved(), T0).await.unwrap();
        assert!(
            grants
                .rotate(&fresh, T0 + REFRESH_IDLE_SECS - 1)
                .await
                .is_ok()
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn reuse_revokes_the_family_and_only_that_family(pool: PgPool) {
        signed_in(&pool).await;
        let grants = Grants::new(pool);
        let first = grants.start_family(&approved(), T0).await.unwrap();
        let other = grants.start_family(&approved(), T0).await.unwrap();
        let (_, second) = grants.rotate(&first, T0 + 1).await.unwrap();
        assert!(matches!(
            grants.rotate(&first, T0 + 2).await,
            Err(GrantError::Reused)
        ));
        assert!(matches!(
            grants.rotate(&second, T0 + 3).await,
            Err(GrantError::Unknown)
        ));
        assert!(grants.rotate(&other, T0 + 3).await.is_ok());
        assert!(matches!(
            grants.rotate("never-issued", T0).await,
            Err(GrantError::Unknown)
        ));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn signing_out_revokes_every_approval_and_code(pool: PgPool) {
        signed_in(&pool).await;
        let grants = Grants::new(pool);
        let a = grants.start_family(&approved(), T0).await.unwrap();
        let b = grants.start_family(&approved(), T0).await.unwrap();
        let code = grants.issue_code(&code_grant(T0 + 60)).await.unwrap();
        assert_eq!(grants.live(SUB, T0).await.unwrap().len(), 2);
        assert_eq!(grants.revoke_all(SUB, T0).await.unwrap(), 2);
        assert!(grants.live(SUB, T0).await.unwrap().is_empty());
        for token in [&a, &b] {
            assert!(grants.rotate(token, T0 + 1).await.is_err());
        }
        assert!(matches!(
            grants.take_code(&code, T0).await,
            Err(GrantError::Unknown)
        ));
        assert_eq!(grants.revoke_all(SUB, T0).await.unwrap(), 0);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn the_sweep_deletes_expired_codes_and_dead_families_only(pool: PgPool) {
        signed_in(&pool).await;
        let grants = Grants::new(pool.clone());
        let now = T0 + REFRESH_IDLE_SECS + 10;
        grants.issue_code(&code_grant(T0 + 60)).await.unwrap();
        let idle = grants.start_family(&approved(), T0).await.unwrap();
        let live = grants.start_family(&approved(), now - 100).await.unwrap();
        let revoked = grants.start_family(&approved(), now - 100).await.unwrap();
        grants.rotate(&revoked, now - 90).await.unwrap();
        assert!(matches!(
            grants.rotate(&revoked, now - 80).await,
            Err(GrantError::Reused)
        ));
        assert_eq!(
            grants.sweep(now).await.unwrap(),
            3,
            "the expired code, the idle family and the revoked one"
        );
        let families: i64 = sqlx::query_scalar("SELECT count(*) FROM people_grants")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(families, 1);
        assert!(grants.rotate(&live, now).await.is_ok());
        assert!(matches!(
            grants.rotate(&idle, now).await,
            Err(GrantError::Unknown)
        ));
    }
}
