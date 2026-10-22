//! Pod tokens: the bearer a run pod or a scope turn's pod holds for its evidence
//! (RFC-0004:C-RUN-EVIDENCE).
//!
//! A token reads `crr_<id>_<secret>` and is checked the way an api key is: one indexed lookup on
//! the id, then a constant-time compare against the stored SHA-256 of the secret. It names no
//! person, only its [`Subject`]. A run's token writes that run's evidence and reads the run its
//! launch reruns; a turn's token writes that turn pod's pack and transcript.

use crate::clock::{now_rfc3339, stamp};
use crate::identity::api_key::{constant_time_eq, digest, expired, fresh, split};
use anyhow::{Context, Result};
use sqlx::PgPool;
use std::time::Duration;

/// What every pod token starts with, so the guards tell it from an api key before any lookup.
pub const PREFIX: &str = "crr_";

/// Whether a bearer claims to be a pod token.
pub fn looks_like_token(presented: &str) -> bool {
    presented.starts_with(PREFIX)
}

/// The one thing a token was minted for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    /// A run, by id.
    Run(String),
    /// A scope turn, by its pod's name.
    Turn(String),
}

impl Subject {
    fn columns(&self) -> (Option<&str>, Option<&str>) {
        match self {
            Subject::Run(run) => (Some(run), None),
            Subject::Turn(pod) => (None, Some(pod)),
        }
    }
}

/// What a valid pod token may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub subject: Subject,
    /// The run whose evidence a run's token reads besides its own: the parent its launch reruns.
    pub reads_run: Option<String>,
}

impl Grant {
    pub fn may_write_run(&self, run_id: &str) -> bool {
        self.subject == Subject::Run(run_id.to_string())
    }

    pub fn may_read_run(&self, run_id: &str) -> bool {
        self.may_write_run(run_id) || self.reads_run.as_deref() == Some(run_id)
    }

    pub fn may_write_turn(&self, pod: &str) -> bool {
        self.subject == Subject::Turn(pod.to_string())
    }
}

/// Why a presented pod token does not authenticate.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TokenRefusal {
    #[error("not a pod token: a pod token starts with `{PREFIX}`")]
    Malformed,
    #[error("this pod token is not one this controller issued")]
    Unknown,
    #[error("this pod token expired at {at}")]
    Expired { at: String },
    #[error("this pod token was revoked at {at}")]
    Revoked { at: String },
}

/// Mint a token for `subject` that lives `ttl` from now. `reads_run` is the parent a run's
/// launch reruns, and is refused for a turn. Returns the full `crr_…` string, which is never
/// stored.
pub async fn mint(
    pool: &PgPool,
    subject: &Subject,
    reads_run: Option<&str>,
    ttl: Duration,
) -> Result<String> {
    if matches!(subject, Subject::Turn(_)) && reads_run.is_some() {
        return Err(TurnReads.into());
    }
    let (id, secret) = fresh();
    let now = jiff::Timestamp::now();
    let ttl = jiff::SignedDuration::try_from(ttl).context("pod token lifetime")?;
    let expires_at = stamp(now.checked_add(ttl).context("pod token expiry")?);
    let (run_id, turn_pod) = subject.columns();
    sqlx::query!(
        "INSERT INTO pod_tokens (id, run_id, turn_pod, secret_hash, reads_run, created_at,
                                 expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
        id,
        run_id,
        turn_pod,
        digest(&secret),
        reads_run,
        stamp(now),
        expires_at,
    )
    .execute(pool)
    .await
    .context("recording the pod token")?;
    Ok(format!("{PREFIX}{id}_{secret}"))
}

#[derive(Debug, thiserror::Error)]
#[error("a turn's token reads no run")]
struct TurnReads;

/// Check a presented token and say what it grants. An unknown id and a wrong secret answer the
/// same [`TokenRefusal::Unknown`].
pub async fn verify(pool: &PgPool, presented: &str) -> Result<Grant, TokenRefusal> {
    let Some((id, secret)) = split(PREFIX, presented) else {
        return Err(TokenRefusal::Malformed);
    };
    let row = sqlx::query!(
        "SELECT run_id, turn_pod, secret_hash, reads_run, expires_at, revoked_at
         FROM pod_tokens WHERE id = $1",
        id,
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        tracing::warn!(error = %e, "reading the pod token");
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
    let subject = match (row.run_id, row.turn_pod) {
        (Some(run), None) => Subject::Run(run),
        (None, Some(pod)) => Subject::Turn(pod),
        _ => return Err(TokenRefusal::Unknown),
    };
    Ok(Grant {
        subject,
        reads_run: row.reads_run,
    })
}

/// Revoke every live token of `subject`.
pub async fn revoke(pool: &PgPool, subject: &Subject) -> Result<()> {
    let (run_id, turn_pod) = subject.columns();
    sqlx::query!(
        "UPDATE pod_tokens SET revoked_at = $3
         WHERE (run_id = $1 OR turn_pod = $2) AND revoked_at IS NULL",
        run_id,
        turn_pod,
        now_rfc3339(),
    )
    .execute(pool)
    .await
    .context("revoking the pod's tokens")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::runs::pod_token::{Grant, PREFIX, Subject, TokenRefusal, mint, revoke, verify};
    use sqlx::PgPool;
    use std::time::Duration;

    const HOUR: Duration = Duration::from_secs(3600);

    fn run(id: &str) -> Subject {
        Subject::Run(id.to_string())
    }

    fn turn(pod: &str) -> Subject {
        Subject::Turn(pod.to_string())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_run_token_grants_its_run_and_its_parent(pool: PgPool) {
        let token = mint(&pool, &run("run-b"), Some("run-a"), HOUR)
            .await
            .unwrap();
        assert!(token.starts_with("crr_"));
        let grant = verify(&pool, &token).await.unwrap();
        assert_eq!(
            grant,
            Grant {
                subject: run("run-b"),
                reads_run: Some("run-a".to_string()),
            }
        );
        assert!(grant.may_write_run("run-b"));
        assert!(!grant.may_write_run("run-a"), "the parent is read-only");
        assert!(grant.may_read_run("run-b"));
        assert!(grant.may_read_run("run-a"));
        assert!(!grant.may_read_run("run-c"));
        assert!(
            !grant.may_write_turn("run-b"),
            "a run's token writes no turn"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_run_token_with_no_parent_reads_only_its_own_run(pool: PgPool) {
        let token = mint(&pool, &run("run-b"), None, HOUR).await.unwrap();
        let grant = verify(&pool, &token).await.unwrap();
        assert!(grant.may_read_run("run-b"));
        assert!(!grant.may_read_run(""));
        assert!(!grant.may_read_run("run-a"));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_turn_token_writes_only_its_pod_and_reads_no_run(pool: PgPool) {
        let token = mint(&pool, &turn("crucible-turn-a"), None, HOUR)
            .await
            .unwrap();
        let grant = verify(&pool, &token).await.unwrap();
        assert!(grant.may_write_turn("crucible-turn-a"));
        assert!(!grant.may_write_turn("crucible-turn-b"));
        assert!(
            !grant.may_write_run("crucible-turn-a"),
            "a turn writes no run"
        );
        assert!(!grant.may_read_run("crucible-turn-a"));
        assert!(
            mint(&pool, &turn("crucible-turn-a"), Some("run-a"), HOUR)
                .await
                .is_err(),
            "a turn's token reads no run"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_wrong_secret_and_an_unknown_id_are_both_unknown(pool: PgPool) {
        let token = mint(&pool, &run("run-b"), None, HOUR).await.unwrap();
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
    async fn a_revoked_token_is_refused_and_only_its_subject_is_revoked(pool: PgPool) {
        let token = mint(&pool, &run("run-b"), None, HOUR).await.unwrap();
        let other = mint(&pool, &run("run-c"), None, HOUR).await.unwrap();
        let turn_token = mint(&pool, &turn("pod-b"), None, HOUR).await.unwrap();
        revoke(&pool, &run("run-b")).await.unwrap();
        assert!(matches!(
            verify(&pool, &token).await,
            Err(TokenRefusal::Revoked { .. })
        ));
        assert!(verify(&pool, &other).await.is_ok(), "only run-b's tokens");
        assert!(verify(&pool, &turn_token).await.is_ok());
        revoke(&pool, &turn("pod-b")).await.unwrap();
        assert!(matches!(
            verify(&pool, &turn_token).await,
            Err(TokenRefusal::Revoked { .. })
        ));
        assert!(verify(&pool, &other).await.is_ok());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_expired_token_is_refused(pool: PgPool) {
        let token = mint(&pool, &run("run-b"), None, Duration::ZERO)
            .await
            .unwrap();
        assert!(matches!(
            verify(&pool, &token).await,
            Err(TokenRefusal::Expired { .. })
        ));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn only_the_secret_digest_is_stored(pool: PgPool) {
        let token = mint(&pool, &run("run-b"), None, HOUR).await.unwrap();
        let (_, secret) = crate::identity::api_key::split(PREFIX, &token).unwrap();
        let stored: String = sqlx::query_scalar("SELECT secret_hash FROM pod_tokens")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!stored.contains(secret));
        assert_eq!(stored, crate::identity::api_key::digest(secret));
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_row_naming_both_or_neither_subject_is_refused_by_the_schema(pool: PgPool) {
        for (run_id, turn_pod) in [(Some("r"), Some("p")), (None, None)] {
            let insert = sqlx::query(
                "INSERT INTO pod_tokens (id, run_id, turn_pod, secret_hash, created_at, expires_at)
                 VALUES ('x', $1, $2, 'h', 'now', 'later')",
            )
            .bind(run_id)
            .bind(turn_pod)
            .execute(&pool)
            .await;
            assert!(insert.is_err(), "{run_id:?} {turn_pod:?}");
        }
    }
}
