//! The `users` table: the issuer's stable `sub` mapped to the login everything else is spelled in.
//!
//! A `user:` principal in the secrets registry is an SSO login. The cluster-token bearer path
//! resolves an OpenShift username that need not be one, so [`is_known_login`] is what decides
//! whether that credential may own anything.

use anyhow::Context;
use sqlx::PgPool;

/// Record a successful login. Latest login wins for the login/email a subject carries.
///
/// A login that has moved to a different subject (an account deleted and recreated in the IdP)
/// takes the name with it: the stale row is dropped in the same transaction, because two rows
/// holding one login would make `user:<login>` ambiguous.
pub async fn record_login(
    pool: &PgPool,
    sub: &str,
    login: &str,
    email: Option<&str>,
    now: jiff::Timestamp,
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await.context("opening the user upsert")?;
    record_login_on(&mut tx, sub, login, email, now).await?;
    tx.commit().await.context("committing the user upsert")?;
    Ok(())
}

/// The upsert itself, on the caller's connection, so the callback can write the user row and the
/// offline credential in one transaction.
pub async fn record_login_on(
    conn: &mut sqlx::PgConnection,
    sub: &str,
    login: &str,
    email: Option<&str>,
    now: jiff::Timestamp,
) -> anyhow::Result<()> {
    let now = now.to_string();
    sqlx::query!(
        "DELETE FROM users WHERE login = $1 AND sub <> $2",
        login,
        sub
    )
    .execute(&mut *conn)
    .await
    .context("releasing a login held by another subject")?;
    sqlx::query!(
        "INSERT INTO users (sub, login, email, last_login, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $4, $4)
         ON CONFLICT (sub) DO UPDATE
            SET login = EXCLUDED.login,
                email = EXCLUDED.email,
                last_login = EXCLUDED.last_login,
                updated_at = EXCLUDED.updated_at",
        sub,
        login,
        email,
        now
    )
    .execute(&mut *conn)
    .await
    .context("recording the login")?;
    Ok(())
}

/// Stamp the groups a login's claim carried, so a credential that carries no claim of its own —
/// an API key — can still answer for its owner's membership.
///
/// Separate from [`record_login_on`] rather than an argument to it, because the paths that write a
/// user row without seeing a claim (a schedule's fire-time backfill, a credential refresh) know
/// nothing about groups, and must leave the last real answer standing instead of clearing it.
pub async fn record_groups_on(
    conn: &mut sqlx::PgConnection,
    sub: &str,
    groups: &[String],
    now: jiff::Timestamp,
) -> anyhow::Result<()> {
    let groups = serde_json::to_value(groups).context("encoding the login groups")?;
    let now = now.to_string();
    sqlx::query!(
        "UPDATE users SET groups = $2, groups_at = $3, updated_at = $3 WHERE sub = $1",
        sub,
        groups,
        now,
    )
    .execute(&mut *conn)
    .await
    .context("recording the login groups")?;
    Ok(())
}

/// The subject a login currently belongs to, if any. The fire-time refresh starts from a
/// schedule's `user:<login>` snapshot and needs the subject its credential is keyed by.
pub async fn sub_for_login(pool: &PgPool, login: &str) -> anyhow::Result<Option<String>> {
    let login = login.trim().to_lowercase();
    sqlx::query_scalar!("SELECT sub FROM users WHERE login = $1", login)
        .fetch_optional(pool)
        .await
        .context("looking up a login's subject")
}

/// Whether a login belongs to somebody who has signed in through the issuer.
pub async fn is_known_login(pool: &PgPool, login: &str) -> anyhow::Result<bool> {
    let login = login.trim().to_lowercase();
    let found: Option<i32> = sqlx::query_scalar!("SELECT 1 FROM users WHERE login = $1", login)
        .fetch_optional(pool)
        .await
        .context("looking up a login")?
        .flatten();
    Ok(found.is_some())
}

/// The login a subject last signed in under, if any.
pub async fn login_for_sub(pool: &PgPool, sub: &str) -> anyhow::Result<Option<String>> {
    sqlx::query_scalar!("SELECT login FROM users WHERE sub = $1", sub)
        .fetch_optional(pool)
        .await
        .context("looking up a subject")
}
