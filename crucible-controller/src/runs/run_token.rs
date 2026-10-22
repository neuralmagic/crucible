//! Run tokens: the bearer a run pod holds for its evidence (RFC-0004:C-RUN-EVIDENCE).
//!
//! A token reads `crr_<id>_<secret>` and is checked the way an api key is: one indexed lookup on
//! the id, then a constant-time compare against the stored SHA-256 of the secret. It names no
//! person. Its grants are exactly two, and [`Grant`] holds both: write the evidence of the run it
//! was minted for, and read the evidence of the run that run's launch reruns.

use crate::clock::{now_rfc3339, stamp};
use crate::identity::api_key::{constant_time_eq, digest, expired, fresh, split};
use anyhow::{Context, Result};
use sqlx::PgPool;
use std::time::Duration;

/// What every run token starts with, so the guards tell it from an api key before any lookup.
pub const PREFIX: &str = "crr_";

/// Whether a bearer claims to be a run token.
pub fn looks_like_token(presented: &str) -> bool {
    presented.starts_with(PREFIX)
}

/// What a valid run token may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// The run whose evidence the token writes.
    pub run_id: String,
    /// The run whose evidence the token reads besides its own: the parent its launch reruns.
    pub reads_run: Option<String>,
}

impl Grant {
    pub fn may_write(&self, run_id: &str) -> bool {
        self.run_id == run_id
    }

    pub fn may_read(&self, run_id: &str) -> bool {
        self.run_id == run_id || self.reads_run.as_deref() == Some(run_id)
    }
}

/// Why a presented run token does not authenticate.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TokenRefusal {
    #[error("not a run token: a run token starts with `{PREFIX}`")]
    Malformed,
    #[error("this run token is not one this controller issued")]
    Unknown,
    #[error("this run token expired at {at}")]
    Expired { at: String },
    #[error("this run token was revoked at {at}")]
    Revoked { at: String },
}

/// Mint a token for `run_id` that lives `ttl` from now, reading `reads_run` besides its own run.
/// Returns the full `crr_…` string, which is never stored.
pub async fn mint(
    pool: &PgPool,
    run_id: &str,
    reads_run: Option<&str>,
    ttl: Duration,
) -> Result<String> {
    let (id, secret) = fresh();
    let now = jiff::Timestamp::now();
    let ttl = jiff::SignedDuration::try_from(ttl).context("run token lifetime")?;
    let expires_at = stamp(now.checked_add(ttl).context("run token expiry")?);
    sqlx::query!(
        "INSERT INTO run_tokens (id, run_id, secret_hash, reads_run, created_at, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
        id,
        run_id,
        digest(&secret),
        reads_run,
        stamp(now),
        expires_at,
    )
    .execute(pool)
    .await
    .context("recording the run token")?;
    Ok(format!("{PREFIX}{id}_{secret}"))
}

/// Check a presented token and say what it grants. An unknown id and a wrong secret answer the
/// same [`TokenRefusal::Unknown`].
pub async fn verify(pool: &PgPool, presented: &str) -> Result<Grant, TokenRefusal> {
    let Some((id, secret)) = split(PREFIX, presented) else {
        return Err(TokenRefusal::Malformed);
    };
    let row = sqlx::query!(
        "SELECT run_id, secret_hash, reads_run, expires_at, revoked_at
         FROM run_tokens WHERE id = $1",
        id,
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        tracing::warn!(error = %e, "reading the run token");
        TokenRefusal::Unknown
    })?
    .ok_or(TokenRefusal::Unknown)?;
    if !constant_time_eq(digest(secret).as_bytes(), row.secret_hash.as_bytes()) {
        return Err(TokenRefusal::Unknown);
    }
    if let Some(at) = row.revoked_at {
        return Err(TokenRefusal::Revoked { at });
    }
    if expired(&row.expires_at) {
        return Err(TokenRefusal::Expired { at: row.expires_at });
    }
    Ok(Grant {
        run_id: row.run_id,
        reads_run: row.reads_run,
    })
}

/// Revoke every live token of `run_id`.
pub async fn revoke_run(pool: &PgPool, run_id: &str) -> Result<()> {
    sqlx::query!(
        "UPDATE run_tokens SET revoked_at = $2 WHERE run_id = $1 AND revoked_at IS NULL",
        run_id,
        now_rfc3339(),
    )
    .execute(pool)
    .await
    .context("revoking the run's tokens")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::runs::run_token::{Grant, PREFIX, TokenRefusal, mint, revoke_run, verify};
    use sqlx::PgPool;
    use std::time::Duration;

    const HOUR: Duration = Duration::from_secs(3600);

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_minted_token_grants_its_run_and_its_parent(pool: PgPool) {
        let token = mint(&pool, "run-b", Some("run-a"), HOUR).await.unwrap();
        assert!(token.starts_with("crr_"));
        let grant = verify(&pool, &token).await.unwrap();
        assert_eq!(
            grant,
            Grant {
                run_id: "run-b".to_string(),
                reads_run: Some("run-a".to_string()),
            }
        );
        assert!(grant.may_write("run-b"));
        assert!(!grant.may_write("run-a"), "the parent is read-only");
        assert!(grant.may_read("run-b"));
        assert!(grant.may_read("run-a"));
        assert!(!grant.may_read("run-c"));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_token_with_no_parent_reads_only_its_own_run(pool: PgPool) {
        let token = mint(&pool, "run-b", None, HOUR).await.unwrap();
        let grant = verify(&pool, &token).await.unwrap();
        assert!(grant.may_read("run-b"));
        assert!(!grant.may_read(""));
        assert!(!grant.may_read("run-a"));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_wrong_secret_and_an_unknown_id_are_both_unknown(pool: PgPool) {
        let token = mint(&pool, "run-b", None, HOUR).await.unwrap();
        let (id, _) = crate::identity::api_key::split(PREFIX, &token).unwrap();
        assert_eq!(
            verify(&pool, &format!("{PREFIX}{id}_wrong")).await,
            Err(TokenRefusal::Unknown)
        );
        assert_eq!(
            verify(&pool, "crr_nosuchid_secret").await,
            Err(TokenRefusal::Unknown)
        );
        assert_eq!(verify(&pool, "crr__").await, Err(TokenRefusal::Malformed));
        assert_eq!(verify(&pool, "crk_a_b").await, Err(TokenRefusal::Malformed));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_revoked_token_is_refused(pool: PgPool) {
        let token = mint(&pool, "run-b", None, HOUR).await.unwrap();
        let other = mint(&pool, "run-c", None, HOUR).await.unwrap();
        revoke_run(&pool, "run-b").await.unwrap();
        assert!(matches!(
            verify(&pool, &token).await,
            Err(TokenRefusal::Revoked { .. })
        ));
        assert!(verify(&pool, &other).await.is_ok(), "only run-b's tokens");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_expired_token_is_refused(pool: PgPool) {
        let token = mint(&pool, "run-b", None, Duration::ZERO).await.unwrap();
        assert!(matches!(
            verify(&pool, &token).await,
            Err(TokenRefusal::Expired { .. })
        ));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn only_the_secret_digest_is_stored(pool: PgPool) {
        let token = mint(&pool, "run-b", None, HOUR).await.unwrap();
        let (_, secret) = crate::identity::api_key::split(PREFIX, &token).unwrap();
        let stored: String = sqlx::query_scalar("SELECT secret_hash FROM run_tokens")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!stored.contains(secret));
        assert_eq!(stored, crate::identity::api_key::digest(secret));
    }
}
