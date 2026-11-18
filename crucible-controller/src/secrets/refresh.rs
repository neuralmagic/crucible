//! Keeps a live run's minted GitHub App token fresh (ADR-0051). An installation token lasts an
//! hour and a run is minted one at dispatch, so once a token has less than half that left the
//! leader re-mints it for the running pod it recorded delivering it to, and merge-patches that
//! key of that pod's Secret alone. The token is mounted into `crucible-git` by directory, so the
//! kubelet carries the new bytes in, and the sidecar reads the file on every request.
//!
//! What to refresh comes only from `run_git_tokens`, written at dispatch. Each sweep first drops
//! the rows of pods no longer running and of runs whose binding is gone or now names another
//! secret, so revoking a binding stops the refresh at the next sweep rather than at the run's end.

use crate::runs::workpod::dispatcher::PodDispatcher;
use crate::secrets::deliver::GitToken;
use crate::secrets::provider::SecretProvider;
use anyhow::{Context, Result};
use jiff::{SignedDuration, Timestamp};
use std::sync::Arc;
use std::time::Duration;

/// How often the leader looks for tokens to re-mint.
pub const INTERVAL: Duration = Duration::from_secs(60);

const TOKEN_LIFETIME: SignedDuration = SignedDuration::from_mins(60);

const RENEW_WITH_LEFT: SignedDuration = SignedDuration::from_mins(30);

fn remaining(minted_at: Timestamp, now: Timestamp) -> SignedDuration {
    TOKEN_LIFETIME.saturating_sub(now.duration_since(minted_at))
}

fn due(minted_at: Option<Timestamp>, now: Timestamp) -> bool {
    minted_at.is_none_or(|minted_at| remaining(minted_at, now) < RENEW_WITH_LEFT)
}

/// Record that `pod_name`'s Secret carries `token`, minted through its binding.
pub async fn record(
    pool: &sqlx::PgPool,
    pod_name: &str,
    cluster: &str,
    namespace: &str,
    token: &GitToken,
    minted_at: Timestamp,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO run_git_tokens
             (pod_name, cluster, namespace, binding_id, secret_id, secret_key, created_at,
              minted_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (pod_name) DO UPDATE SET binding_id = EXCLUDED.binding_id,
             secret_id = EXCLUDED.secret_id, secret_key = EXCLUDED.secret_key,
             minted_at = EXCLUDED.minted_at",
    )
    .bind(pod_name)
    .bind(cluster)
    .bind(namespace)
    .bind(&token.binding_id)
    .bind(&token.secret_id)
    .bind(&token.key)
    .bind(Timestamp::now().to_string())
    .bind(minted_at.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Live {
    pod_name: String,
    run_id: Option<String>,
    cluster: String,
    namespace: String,
    secret_id: String,
    secret_key: String,
    minted_at: Option<String>,
}

impl Live {
    fn minted_at(&self) -> Option<Timestamp> {
        self.minted_at.as_deref().and_then(|raw| raw.parse().ok())
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Swept {
    pub renewed: usize,
    pub failed: usize,
}

pub async fn watch_loop(
    db: crate::client::Db,
    dispatcher: Arc<dyn PodDispatcher>,
    provider: Arc<dyn SecretProvider>,
) {
    let mut timer = tokio::time::interval(INTERVAL);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        timer.tick().await;
        if let Err(e) = sweep(
            db.pool(),
            dispatcher.as_ref(),
            provider.as_ref(),
            Timestamp::now(),
        )
        .await
        {
            tracing::warn!(error = %format!("{e:#}"), "GitHub token refresh sweep failed");
        }
    }
}

/// Drop the rows of finished pods and of revoked or repointed bindings; the count dropped.
async fn prune(pool: &sqlx::PgPool) -> Result<u64> {
    let done = sqlx::query(
        "DELETE FROM run_git_tokens t
         WHERE NOT EXISTS (SELECT 1 FROM work_pods w
                           WHERE w.pod_name = t.pod_name AND w.state = 'running')
            OR NOT EXISTS (SELECT 1 FROM secret_bindings b
                           WHERE b.id = t.binding_id AND b.secret_id = t.secret_id)",
    )
    .execute(pool)
    .await
    .context("pruning GitHub token refresh rows")?;
    Ok(done.rows_affected())
}

pub async fn sweep(
    pool: &sqlx::PgPool,
    dispatcher: &dyn PodDispatcher,
    provider: &dyn SecretProvider,
    now: Timestamp,
) -> Result<Swept> {
    let dropped = prune(pool).await?;
    if dropped > 0 {
        tracing::info!(
            dropped,
            "stopped refreshing GitHub tokens of finished runs or revoked bindings"
        );
    }
    let live: Vec<Live> = sqlx::query_as(
        "SELECT t.pod_name, t.cluster, t.namespace, t.secret_id, t.secret_key, t.minted_at,
                (SELECT r.run_id FROM runs r WHERE r.pod = t.pod_name
                 ORDER BY r.seq DESC LIMIT 1) AS run_id
         FROM run_git_tokens t
         ORDER BY t.pod_name",
    )
    .fetch_all(pool)
    .await
    .context("reading the runs whose GitHub token to refresh")?;
    let mut swept = Swept::default();
    for run in live.iter().filter(|run| due(run.minted_at(), now)) {
        let run_id = run.run_id.as_deref().unwrap_or(&run.pod_name);
        let left = run.minted_at().map(|at| remaining(at, now).as_secs());
        match refresh(pool, dispatcher, provider, run, now).await {
            Ok(()) => {
                swept.renewed += 1;
                tracing::info!(
                    run_id,
                    pod = %run.pod_name,
                    cluster = %run.cluster,
                    "refreshed GitHub token"
                );
            }
            Err(e) => {
                swept.failed += 1;
                tracing::warn!(
                    run_id,
                    pod = %run.pod_name,
                    cluster = %run.cluster,
                    seconds_left = ?left,
                    retry_in_seconds = INTERVAL.as_secs(),
                    error = %format!("{e:#}"),
                    "GitHub token refresh failed"
                );
            }
        }
    }
    Ok(swept)
}

async fn refresh(
    pool: &sqlx::PgPool,
    dispatcher: &dyn PodDispatcher,
    provider: &dyn SecretProvider,
    run: &Live,
    now: Timestamp,
) -> Result<()> {
    let row = crate::secrets::store::get(pool, &run.secret_id)
        .await?
        .with_context(|| format!("registry secret {} is gone", run.secret_id))?;
    let token = provider.value_of(&row).await?;
    dispatcher
        .set_secret_value(
            &run.cluster,
            &run.namespace,
            &crate::secrets::deliver::secret_name(&run.pod_name),
            &run.secret_key,
            &token,
        )
        .await
        .context("rewriting the token in the run Secret")?;
    sqlx::query("UPDATE run_git_tokens SET minted_at = $1 WHERE pod_name = $2")
        .bind(now.to_string())
        .bind(&run.pod_name)
        .execute(pool)
        .await
        .context("recording the re-minted token's time")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::runs::workpod::dispatcher::{TerminalState, TurnPhase};
    use crate::secrets::github_app::GithubAppTokenSource;
    use crate::secrets::provider::RegistryProvider;
    use crate::secrets::refresh::*;
    use k8s_openapi::api::core::v1::Pod;
    use std::sync::Mutex;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn secret(pool: &sqlx::PgPool, id: &str) {
        secret_minted_by(pool, id, "mint://github-app").await;
    }

    async fn secret_minted_by(pool: &sqlx::PgPool, id: &str, minter: &str) {
        sqlx::query(
            "INSERT INTO secrets (id, name, owner, kind, visibility, consumer, mode, vault_path,
                                  created_at, updated_at)
             VALUES ($1, $1, 'user:u', 'opaque', 'broker_only', 'run', 'minted', $2, 'now', 'now')",
        )
        .bind(id)
        .bind(minter)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn work_pod(pool: &sqlx::PgPool, pod: &str, state: &str) {
        sqlx::query(
            "INSERT INTO work_pods (pod_name, kind, state, cost_tag, created_at, updated_at)
             VALUES ($1, 'run', $2, 'test', 'now', 'now')",
        )
        .bind(pod)
        .bind(state)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn binding(pool: &sqlx::PgPool, id: &str, secret_id: &str) {
        sqlx::query(
            "INSERT INTO secret_bindings (id, secret_id, scope_kind, scope_id, projection_kind,
                                          projection, declared_name, created_at)
             VALUES ($1, $2, 'playbook', $1, 'file', '/t', 'github-app', 'now')",
        )
        .bind(id)
        .bind(secret_id)
        .execute(pool)
        .await
        .unwrap();
    }

    fn token(binding_id: &str, secret_id: &str) -> GitToken {
        GitToken {
            key: "github-app".into(),
            binding_id: binding_id.into(),
            secret_id: secret_id.into(),
            minted: true,
        }
    }

    fn at(now: Timestamp, minutes_ago: i64) -> Timestamp {
        now - SignedDuration::from_mins(minutes_ago)
    }

    async fn live_run(pool: &sqlx::PgPool, pod: &str, now: Timestamp, minutes_ago: i64) {
        work_pod(pool, pod, "running").await;
        let binding_id = format!("b-{pod}");
        binding(pool, &binding_id, "s1").await;
        record(
            pool,
            pod,
            "hub",
            "ns",
            &token(&binding_id, "s1"),
            at(now, minutes_ago),
        )
        .await
        .unwrap();
    }

    async fn minted_at(pool: &sqlx::PgPool, pod: &str) -> Option<Timestamp> {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT minted_at FROM run_git_tokens WHERE pod_name = $1",
        )
        .bind(pod)
        .fetch_one(pool)
        .await
        .unwrap()
        .map(|raw| raw.parse().unwrap())
    }

    type Write = (String, String, String, String, String);

    #[derive(Default)]
    struct Secrets {
        writes: Mutex<Vec<Write>>,
        refuse: bool,
    }

    impl Secrets {
        fn writes(&self) -> Vec<Write> {
            self.writes.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl PodDispatcher for Secrets {
        async fn create(&self, _cluster: &str, _ns: &str, pod: Pod) -> Result<Pod> {
            Ok(pod)
        }
        async fn await_terminal(
            &self,
            _cluster: &str,
            _ns: &str,
            _name: &str,
            _timeout: Duration,
        ) -> Result<TerminalState> {
            Ok(TerminalState {
                phase: TurnPhase::Succeeded,
                message: None,
            })
        }
        async fn logs(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<String> {
            Ok(String::new())
        }
        async fn delete(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<()> {
            Ok(())
        }
        async fn set_secret_value(
            &self,
            cluster: &str,
            namespace: &str,
            secret: &str,
            key: &str,
            value: &str,
        ) -> Result<()> {
            if self.refuse {
                return Err(crate::runs::workpod::dispatcher::Unsupported::SecretValues.into());
            }
            self.writes.lock().unwrap().push((
                cluster.to_string(),
                namespace.to_string(),
                secret.to_string(),
                key.to_string(),
                value.to_string(),
            ));
            Ok(())
        }
    }

    fn exchange(token: &str) -> ResponseTemplate {
        ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "token": token,
            "expires_at": (Timestamp::now() + TOKEN_LIFETIME).to_string(),
        }))
    }

    fn app_provider(server: &MockServer, key_dir: &std::path::Path) -> Arc<dyn SecretProvider> {
        let app = GithubAppTokenSource::new(
            "4210340",
            "99",
            crate::secrets::github_app::testkey::write_test_key(key_dir),
            server.uri(),
        );
        RegistryProvider::installed(None, Some(app)).expect("an app is a backend")
    }

    #[test]
    fn a_token_is_re_minted_once_less_than_half_its_hour_is_left() {
        let now: Timestamp = "2026-11-12T17:00:00Z".parse().unwrap();
        let aged = |secs: i64| Some(now - SignedDuration::from_secs(secs));
        assert!(!due(aged(0), now));
        assert!(!due(aged(30 * 60), now));
        assert!(due(aged(30 * 60 + 1), now));
        assert!(due(aged(74 * 60), now));
        assert!(due(None, now));
        assert!(!due(aged(-120), now));
    }

    #[test]
    fn a_tokens_remaining_life_runs_out_an_hour_after_its_mint() {
        let now: Timestamp = "2026-11-12T17:00:00Z".parse().unwrap();
        assert_eq!(remaining(now, now), SignedDuration::from_mins(60));
        assert_eq!(remaining(at(now, 45), now), SignedDuration::from_mins(15));
        assert_eq!(remaining(at(now, 74), now), SignedDuration::from_mins(-14));
    }

    #[test]
    fn the_sweep_interval_leaves_many_retries_before_a_due_token_lapses() {
        let retries = RENEW_WITH_LEFT.as_secs() / INTERVAL.as_secs() as i64;
        assert!(retries >= 20, "{retries} retries");
        assert!(RENEW_WITH_LEFT < TOKEN_LIFETIME);
    }

    /// A row survives only while its pod runs and its binding still names the same secret.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn prune_keeps_live_bound_runs_and_drops_finished_or_revoked_ones(pool: sqlx::PgPool) {
        secret(&pool, "s1").await;
        secret(&pool, "s2").await;
        for (pod, state) in [
            ("live", "running"),
            ("done", "succeeded"),
            ("revoked", "running"),
            ("repointed", "running"),
        ] {
            work_pod(&pool, pod, state).await;
        }
        binding(&pool, "b-live", "s1").await;
        binding(&pool, "b-done", "s1").await;
        binding(&pool, "b-repointed", "s2").await;
        for (pod, binding_id) in [
            ("live", "b-live"),
            ("done", "b-done"),
            ("revoked", "b-gone"),
            ("repointed", "b-repointed"),
        ] {
            record(
                &pool,
                pod,
                "hub",
                "ns",
                &token(binding_id, "s1"),
                Timestamp::now(),
            )
            .await
            .unwrap();
        }

        assert_eq!(prune(&pool).await.unwrap(), 3);
        let left: Vec<String> = sqlx::query_scalar("SELECT pod_name FROM run_git_tokens")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(left, ["live"]);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_due_token_is_re_minted_into_its_runs_secret_and_a_fresh_one_is_left(
        pool: sqlx::PgPool,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/app/installations/99/access_tokens"))
            .respond_with(exchange("ghs_second_hour"))
            .expect(1)
            .mount(&server)
            .await;
        let provider = app_provider(&server, dir.path());
        let cluster = Secrets::default();
        let now = Timestamp::now();
        secret(&pool, "s1").await;
        live_run(&pool, "crucible-run-old", now, 31).await;
        live_run(&pool, "crucible-run-young", now, 5).await;

        let swept = sweep(&pool, &cluster, provider.as_ref(), now)
            .await
            .unwrap();

        assert_eq!(
            swept,
            Swept {
                renewed: 1,
                failed: 0
            }
        );
        assert_eq!(
            cluster.writes(),
            [(
                "hub".to_string(),
                "ns".to_string(),
                "crucible-run-old-secrets".to_string(),
                "github-app".to_string(),
                "ghs_second_hour".to_string(),
            )]
        );
        assert_eq!(minted_at(&pool, "crucible-run-old").await, Some(now));
        assert_eq!(
            minted_at(&pool, "crucible-run-young").await,
            Some(at(now, 5))
        );

        let again = sweep(&pool, &cluster, provider.as_ref(), now)
            .await
            .unwrap();
        assert_eq!(again, Swept::default());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_long_run_is_re_minted_again_once_the_new_token_is_half_spent(pool: sqlx::PgPool) {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/app/installations/99/access_tokens"))
            .respond_with(exchange("ghs_next"))
            .expect(2)
            .mount(&server)
            .await;
        let provider = app_provider(&server, dir.path());
        let cluster = Secrets::default();
        let start = Timestamp::now();
        secret(&pool, "s1").await;
        live_run(&pool, "crucible-run-long", start, 31).await;

        for later in [0, 20, 31, 40] {
            let now = start + SignedDuration::from_mins(later);
            sweep(&pool, &cluster, provider.as_ref(), now)
                .await
                .unwrap();
        }

        assert_eq!(cluster.writes().len(), 2);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_token_is_re_minted_through_the_installation_its_secret_names(pool: sqlx::PgPool) {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/app/installations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": 99, "account": {"login": "home"}},
                {"id": 4242, "account": {"login": "acme"}},
            ])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/app/installations/4242/access_tokens"))
            .respond_with(exchange("ghs_acme"))
            .expect(1)
            .mount(&server)
            .await;
        let provider = app_provider(&server, dir.path());
        let cluster = Secrets::default();
        let now = Timestamp::now();
        secret_minted_by(&pool, "s1", "mint://github-app/acme").await;
        live_run(&pool, "crucible-run-acme", now, 45).await;

        let swept = sweep(&pool, &cluster, provider.as_ref(), now)
            .await
            .unwrap();

        assert_eq!(swept.renewed, 1);
        assert_eq!(cluster.writes()[0].4, "ghs_acme");
        let requests = server.received_requests().await.unwrap();
        let mint = requests
            .iter()
            .find(|r| r.method.as_str() == "POST")
            .expect("one mint");
        assert!(mint.body.is_empty());
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_failed_re_mint_stays_due_and_the_next_sweep_retries_it(pool: sqlx::PgPool) {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/app/installations/99/access_tokens"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/app/installations/99/access_tokens"))
            .respond_with(exchange("ghs_after_retry"))
            .expect(1)
            .mount(&server)
            .await;
        let provider = app_provider(&server, dir.path());
        let cluster = Secrets::default();
        let now = Timestamp::now();
        secret(&pool, "s1").await;
        live_run(&pool, "crucible-run-a", now, 40).await;

        let first = sweep(&pool, &cluster, provider.as_ref(), now)
            .await
            .unwrap();
        assert_eq!(
            first,
            Swept {
                renewed: 0,
                failed: 1
            }
        );
        assert_eq!(cluster.writes(), []);
        assert_eq!(minted_at(&pool, "crucible-run-a").await, Some(at(now, 40)));

        let second = sweep(&pool, &cluster, provider.as_ref(), now)
            .await
            .unwrap();
        assert_eq!(
            second,
            Swept {
                renewed: 1,
                failed: 0
            }
        );
        assert_eq!(cluster.writes()[0].4, "ghs_after_retry");
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_secret_the_cluster_refuses_to_rewrite_stays_due(pool: sqlx::PgPool) {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/app/installations/99/access_tokens"))
            .respond_with(exchange("ghs_unwritten"))
            .expect(2)
            .mount(&server)
            .await;
        let provider = app_provider(&server, dir.path());
        let cluster = Secrets {
            refuse: true,
            ..Secrets::default()
        };
        let now = Timestamp::now();
        secret(&pool, "s1").await;
        live_run(&pool, "crucible-run-a", now, 40).await;
        live_run(&pool, "crucible-run-b", now, 50).await;

        let swept = sweep(&pool, &cluster, provider.as_ref(), now)
            .await
            .unwrap();

        assert_eq!(
            swept,
            Swept {
                renewed: 0,
                failed: 2
            }
        );
        assert_eq!(minted_at(&pool, "crucible-run-a").await, Some(at(now, 40)));
        assert_eq!(minted_at(&pool, "crucible-run-b").await, Some(at(now, 50)));
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_finished_runs_token_is_not_re_minted(pool: sqlx::PgPool) {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(exchange("ghs_never"))
            .expect(0)
            .mount(&server)
            .await;
        let provider = app_provider(&server, dir.path());
        let cluster = Secrets::default();
        let now = Timestamp::now();
        secret(&pool, "s1").await;
        live_run(&pool, "crucible-run-done", now, 55).await;
        sqlx::query("UPDATE work_pods SET state = 'succeeded'")
            .execute(&pool)
            .await
            .unwrap();

        let swept = sweep(&pool, &cluster, provider.as_ref(), now)
            .await
            .unwrap();

        assert_eq!(swept, Swept::default());
        assert_eq!(cluster.writes(), []);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM run_git_tokens")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_run_with_no_recorded_mint_time_is_re_minted_at_once(pool: sqlx::PgPool) {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/app/installations/99/access_tokens"))
            .respond_with(exchange("ghs_backfilled"))
            .expect(1)
            .mount(&server)
            .await;
        let provider = app_provider(&server, dir.path());
        let cluster = Secrets::default();
        let now = Timestamp::now();
        secret(&pool, "s1").await;
        live_run(&pool, "crucible-run-legacy", now, 0).await;
        sqlx::query("UPDATE run_git_tokens SET minted_at = NULL")
            .execute(&pool)
            .await
            .unwrap();

        let swept = sweep(&pool, &cluster, provider.as_ref(), now)
            .await
            .unwrap();

        assert_eq!(swept.renewed, 1);
        assert!(minted_at(&pool, "crucible-run-legacy").await.is_some());
    }
}
