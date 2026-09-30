//! Launches minted but not yet dispatched.

use std::sync::Arc;

use anyhow::{Context, Result};
use sqlx::PgPool;

use crate::daemon::queue::{BoxFuture, DiscoverySource, Enqueue, IssueKey};

pub struct PendingLaunches {
    pool: PgPool,
}

impl PendingLaunches {
    pub fn new(pool: PgPool) -> Self {
        PendingLaunches { pool }
    }
}

#[tracing::instrument(name = "db.pending_launch_keys", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn pending_launch_keys(pool: &PgPool) -> Result<Vec<String>> {
    sqlx::query_scalar::<_, String>(
        "SELECT key FROM issues WHERE status = 'new' AND input_kind = 'playbook' \
         ORDER BY updated_at",
    )
    .fetch_all(pool)
    .await
    .context("pending_launch_keys")
}

impl DiscoverySource for PendingLaunches {
    fn poll(&self, enqueue: Arc<dyn Enqueue>) -> BoxFuture<Result<()>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            for key in pending_launch_keys(&pool).await? {
                enqueue.enqueue_urgent(IssueKey(key));
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::daemon::queue::{DiscoverySource, Enqueue, IssueKey};
    use crate::launches::pending::PendingLaunches;
    use sqlx::PgPool;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Recorder {
        plain: Mutex<Vec<String>>,
        urgent: Mutex<Vec<String>>,
    }

    impl Enqueue for Recorder {
        fn enqueue(&self, key: IssueKey) {
            self.plain.lock().expect("lock").push(key.0);
        }

        fn enqueue_urgent(&self, key: IssueKey) {
            self.urgent.lock().expect("lock").push(key.0);
        }
    }

    async fn issue(pool: &PgPool, key: &str, status: &str, kind: &str, updated_at: &str) {
        sqlx::query(
            "INSERT INTO issues (key, repo, tier, status, priority, input_kind, title, updated_at) \
             VALUES ($1, 'o/r', 'T1', $2, 0, $3, 't', $4)",
        )
        .bind(key)
        .bind(status)
        .bind(kind)
        .bind(updated_at)
        .execute(pool)
        .await
        .expect("insert issue");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn the_poll_enqueues_only_undispatched_playbook_launches_oldest_first(pool: PgPool) {
        issue(
            &pool,
            "playbook:b:2",
            "new",
            "playbook",
            "2026-09-30T00:00:02Z",
        )
        .await;
        issue(
            &pool,
            "playbook:a:1",
            "new",
            "playbook",
            "2026-09-30T00:00:01Z",
        )
        .await;
        issue(
            &pool,
            "playbook:c:3",
            "running",
            "playbook",
            "2026-09-30T00:00:03Z",
        )
        .await;
        issue(
            &pool,
            "playbook:d:4",
            "parked",
            "playbook",
            "2026-09-30T00:00:04Z",
        )
        .await;
        issue(
            &pool,
            "playbook:e:5",
            "done",
            "playbook",
            "2026-09-30T00:00:05Z",
        )
        .await;
        issue(&pool, "o/r#6", "new", "github", "2026-09-30T00:00:06Z").await;
        issue(
            &pool,
            "scenario:7",
            "new",
            "scenario",
            "2026-09-30T00:00:07Z",
        )
        .await;

        let recorder = Arc::new(Recorder::default());
        PendingLaunches::new(pool.clone())
            .poll(recorder.clone())
            .await
            .expect("poll");

        assert_eq!(
            *recorder.urgent.lock().expect("lock"),
            vec!["playbook:a:1".to_string(), "playbook:b:2".to_string()]
        );
        assert!(recorder.plain.lock().expect("lock").is_empty());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn the_poll_is_a_no_op_with_nothing_pending(pool: PgPool) {
        issue(
            &pool,
            "playbook:c:3",
            "running",
            "playbook",
            "2026-09-30T00:00:03Z",
        )
        .await;
        let recorder = Arc::new(Recorder::default());
        PendingLaunches::new(pool.clone())
            .poll(recorder.clone())
            .await
            .expect("poll");
        assert!(recorder.urgent.lock().expect("lock").is_empty());
        assert!(recorder.plain.lock().expect("lock").is_empty());
    }
}
