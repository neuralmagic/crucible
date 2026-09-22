//! Runtime-flippable autopilot enabled/disabled flag, backed by the `autopilot` ledger row
//! ([`crate::runs::blob_store::get_autopilot`] / [`crate::runs::blob_store::set_autopilot`]).
//!
//! Reads are cheap: an `AtomicBool` cache, updated on writes. The reconcile loop checks
//! `is_enabled()` per pass with zero I/O. The cache is per-process, so every replica also runs
//! [`AutopilotFlag::refresh_loop`]: with standbys serving the API (ADR-0049), a flip can land
//! through a replica that is not the reconciler, and the poll is what carries it across.

use anyhow::Result;
use sqlx::PgPool;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The flag's audit shape, served by `GET /api/autopilot`.
#[derive(Debug, Clone)]
pub struct AutopilotState {
    pub(crate) enabled: bool,
    pub(crate) changed_by: Option<String>,
    pub(crate) changed_at: Option<String>,
    pub(crate) reason: Option<String>,
}

impl Default for AutopilotState {
    fn default() -> Self {
        AutopilotState {
            enabled: true,
            changed_by: None,
            changed_at: None,
            reason: None,
        }
    }
}

impl From<crate::runs::blob_store::AutopilotRow> for AutopilotState {
    fn from(r: crate::runs::blob_store::AutopilotRow) -> Self {
        AutopilotState {
            enabled: r.enabled,
            changed_by: r.changed_by,
            changed_at: Some(r.updated_at),
            reason: r.reason,
        }
    }
}

/// The runtime handle: an atomic cache plus the pool the durable row lives in.
#[derive(Debug, Clone)]
pub struct AutopilotFlag {
    pool: PgPool,
    cache: Arc<AtomicBool>,
}

impl AutopilotFlag {
    /// Load the flag, seeding the cache from the row. No row (never set) means enabled.
    pub async fn load(pool: &PgPool) -> Result<Self> {
        let enabled = crate::runs::blob_store::get_autopilot(pool)
            .await?
            .map(|r| r.enabled)
            .unwrap_or(true);
        Ok(AutopilotFlag {
            pool: pool.clone(),
            cache: Arc::new(AtomicBool::new(enabled)),
        })
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.cache.load(Ordering::Relaxed)
    }

    pub(crate) async fn read(&self) -> Result<AutopilotState> {
        Ok(crate::runs::blob_store::get_autopilot(&self.pool)
            .await?
            .map(AutopilotState::from)
            .unwrap_or_default())
    }

    /// Poll the durable row into the cache so a flip written by another replica is visible
    /// here within `interval`. Read failures keep the last-known value; the task dies with the
    /// process.
    pub async fn refresh_loop(self, interval: std::time::Duration) {
        loop {
            tokio::time::sleep(interval).await;
            match crate::runs::blob_store::get_autopilot(&self.pool).await {
                Ok(row) => {
                    let enabled = row.map(|r| r.enabled).unwrap_or(true);
                    self.cache.store(enabled, Ordering::Relaxed);
                }
                Err(e) => {
                    tracing::debug!(error = %format!("{e:#}"), "autopilot flag refresh failed")
                }
            }
        }
    }

    pub(crate) async fn set(
        &self,
        enabled: bool,
        actor: Option<&str>,
        reason: &str,
    ) -> Result<AutopilotState> {
        let row = crate::runs::blob_store::set_autopilot(&self.pool, enabled, actor, Some(reason))
            .await?;
        self.cache.store(enabled, Ordering::Relaxed);
        Ok(row.into())
    }
}

#[cfg(test)]
impl AutopilotFlag {
    /// Cache-seeded handle for sync test constructors; the row is untouched until `set`.
    pub(crate) fn seeded(pool: PgPool, enabled: bool) -> Self {
        AutopilotFlag {
            pool,
            cache: Arc::new(AtomicBool::new(enabled)),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::daemon::autopilot_flag::AutopilotFlag;
    use sqlx::PgPool;

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn defaults_to_enabled_when_no_row(pool: PgPool) {
        let flag = AutopilotFlag::load(&pool).await.expect("load");
        assert!(flag.is_enabled());
        let state = flag.read().await.expect("read");
        assert!(state.enabled);
        assert!(state.changed_by.is_none());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn set_persists_and_updates_cache(pool: PgPool) {
        let flag = AutopilotFlag::load(&pool).await.expect("load");
        assert!(flag.is_enabled());

        let state = flag
            .set(false, Some("wren"), "cost runaway")
            .await
            .expect("set");
        assert!(!state.enabled);
        assert_eq!(state.changed_by.as_deref(), Some("wren"));
        assert_eq!(state.reason.as_deref(), Some("cost runaway"));
        assert!(state.changed_at.is_some());
        assert!(!flag.is_enabled());

        let flag2 = AutopilotFlag::load(&pool).await.expect("reload");
        assert!(!flag2.is_enabled());
        let state2 = flag2.read().await.expect("read");
        assert!(!state2.enabled);
        assert_eq!(state2.changed_by.as_deref(), Some("wren"));
    }
}
