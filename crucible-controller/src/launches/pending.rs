//! Launches minted but not yet dispatched. Any replica mints them; the leader's launch loop
//! enqueues them, then holds them until the hold runs out or they leave `new`.

use std::sync::Arc;

use anyhow::{Context, Result};
use jiff::Timestamp;
use sqlx::PgPool;

use crate::daemon::queue::{BoxFuture, DueSource, Enqueue, IssueKey, Pass};

pub struct PendingLaunches {
    pool: PgPool,
}

impl PendingLaunches {
    pub fn new(pool: PgPool) -> Self {
        PendingLaunches { pool }
    }
}

#[tracing::instrument(name = "db.pending_launch_keys", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn pending_launch_keys(pool: &PgPool, held: &[String]) -> Result<Vec<String>> {
    sqlx::query_scalar::<_, String>(
        "SELECT key FROM issues WHERE status = 'new' AND input_kind = 'playbook' \
         AND key <> ALL($1) ORDER BY updated_at",
    )
    .bind(held)
    .fetch_all(pool)
    .await
    .context("pending_launch_keys")
}

impl DueSource for PendingLaunches {
    fn next_due(&self, held: Vec<String>) -> BoxFuture<Result<Option<Timestamp>>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let earliest: Option<String> = sqlx::query_scalar(
                "SELECT MIN(updated_at) FROM issues WHERE status = 'new' \
                 AND input_kind = 'playbook' AND key <> ALL($1)",
            )
            .bind(&held)
            .fetch_one(&pool)
            .await
            .context("the oldest pending launch")?;
            crate::launches::standing::due_at(earliest)
        })
    }

    fn pass(&self, enqueue: Arc<dyn Enqueue>, held: Vec<String>) -> BoxFuture<Result<Pass>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let keys = pending_launch_keys(&pool, &held).await?;
            for key in &keys {
                enqueue.enqueue_urgent(IssueKey(key.clone()));
            }
            Ok(Pass {
                claimed: 0,
                held: keys,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::daemon::queue::{DueSource, Enqueue, IssueKey, Pass};
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
    async fn a_pass_enqueues_only_undispatched_playbook_launches_oldest_first(pool: PgPool) {
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

        let source = PendingLaunches::new(pool.clone());
        assert_eq!(
            source.next_due(vec![]).await.expect("probe"),
            Some(at("2026-09-30T00:00:01Z"))
        );
        let recorder = Arc::new(Recorder::default());
        let pass = source.pass(recorder.clone(), vec![]).await.expect("pass");

        let pending = vec!["playbook:a:1".to_string(), "playbook:b:2".to_string()];
        assert_eq!(*recorder.urgent.lock().expect("lock"), pending);
        assert!(recorder.plain.lock().expect("lock").is_empty());
        assert_eq!(
            pass,
            Pass {
                claimed: 0,
                held: pending
            },
            "enqueued launches are held, and holding claims nothing"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn held_launches_are_left_out_of_the_probe_and_the_pass(pool: PgPool) {
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
            "playbook:b:2",
            "new",
            "playbook",
            "2026-09-30T00:00:02Z",
        )
        .await;
        let source = PendingLaunches::new(pool.clone());
        let held = vec!["playbook:a:1".to_string()];

        assert_eq!(
            source.next_due(held.clone()).await.expect("probe"),
            Some(at("2026-09-30T00:00:02Z"))
        );
        let recorder = Arc::new(Recorder::default());
        source.pass(recorder.clone(), held).await.expect("pass");
        assert_eq!(
            *recorder.urgent.lock().expect("lock"),
            vec!["playbook:b:2".to_string()]
        );
        assert_eq!(
            source
                .next_due(vec!["playbook:a:1".into(), "playbook:b:2".into()])
                .await
                .expect("probe"),
            None
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn nothing_pending_is_never_due(pool: PgPool) {
        issue(
            &pool,
            "playbook:c:3",
            "running",
            "playbook",
            "2026-09-30T00:00:03Z",
        )
        .await;
        let source = PendingLaunches::new(pool.clone());
        assert_eq!(source.next_due(vec![]).await.expect("probe"), None);
        let recorder = Arc::new(Recorder::default());
        let pass = source.pass(recorder.clone(), vec![]).await.expect("pass");
        assert_eq!(pass, Pass::default());
        assert!(recorder.urgent.lock().expect("lock").is_empty());
        assert!(recorder.plain.lock().expect("lock").is_empty());
    }

    fn at(stamp: &str) -> jiff::Timestamp {
        stamp.parse().expect("stamp")
    }
}
