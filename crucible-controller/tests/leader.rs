//! Leader election end to end at the fence layer (ADR-0049 §1), against a real Postgres: two
//! campaigners, exactly one leads; killing the leader's lock session steps it down and the
//! standby takes over. The lease layer has its own kind-gated test in `src/leader.rs`.

use anyhow::Result;
use crucible_controller::MAINTENANCE_ADVISORY_LOCK;
use crucible_controller::daemon::leader::campaign;
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use std::time::Duration;

async fn test_pool(name: &str) -> Result<sqlx::PgPool> {
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must point at the test server");
    let url = crucible_controller::sibling_db_url(&base, name)?;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&base)
        .await?;
    // Advisory locks are per-database: a sibling database isolates this test's fence from every
    // other test binary sharing the server.
    let _ = sqlx::query(sqlx::AssertSqlSafe(format!(r#"CREATE DATABASE "{name}""#)))
        .execute(&admin)
        .await;
    Ok(PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await?)
}

/// Two fence-only campaigns: one leads, the other waits; dropping the leader hands over.
#[tokio::test]
async fn fence_admits_one_leader_and_fails_over_on_drop() -> Result<()> {
    let pool = test_pool("leader_fence_drop").await?;
    let shutdown = Arc::new(tokio::sync::Notify::new());

    let a = campaign(None, &pool, shutdown.clone())
        .await?
        .expect("first campaigner leads");

    // The standby: campaigns but must not complete while A holds the fence.
    let pool_b = pool.clone();
    let shutdown_b = shutdown.clone();
    let b = tokio::spawn(async move { campaign(None, &pool_b, shutdown_b).await });
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        !b.is_finished(),
        "standby must wait while the leader holds the fence"
    );

    drop(a);
    let b = tokio::time::timeout(Duration::from_secs(15), b)
        .await
        .expect("standby takes over after the leader drops")??
        .expect("standby leads");
    drop(b);
    Ok(())
}

/// Killing the leader's lock session server-side fires its step-down signal, and the standby's
/// campaign then completes: the fence, not the process, is what keeps writers single.
#[tokio::test]
async fn killed_fence_session_steps_the_leader_down() -> Result<()> {
    let pool = test_pool("leader_fence_kill").await?;
    let shutdown = Arc::new(tokio::sync::Notify::new());

    let mut a = campaign(None, &pool, shutdown.clone())
        .await?
        .expect("first campaigner leads");

    // Find and kill the fence session by its advisory-lock key halves.
    let classid = (MAINTENANCE_ADVISORY_LOCK >> 32) as i32;
    let objid = (MAINTENANCE_ADVISORY_LOCK & 0xffff_ffff) as i32;
    let killed: bool = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_locks
         WHERE locktype = 'advisory' AND classid = $1::oid AND objid = $2::oid
           AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
         LIMIT 1",
    )
    .bind(classid)
    .bind(objid)
    .fetch_one(&pool)
    .await?;
    assert!(killed, "the fence session was found and terminated");

    let reason = tokio::time::timeout(Duration::from_secs(15), a.lost())
        .await
        .expect("the leader notices the dead fence within its ping interval");
    assert_eq!(reason, "advisory-lock fence lost");

    // With the old session gone the fence is free: a new campaign leads.
    let b = tokio::time::timeout(
        Duration::from_secs(15),
        campaign(None, &pool, shutdown.clone()),
    )
    .await
    .expect("standby campaign completes")?
    .expect("standby leads");
    drop(b);
    Ok(())
}
