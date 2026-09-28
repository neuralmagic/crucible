//! [`Db`]: the controller's ledger handle — a Postgres pool, the event log, and an optional
//! metrics registry.

#![allow(clippy::disallowed_macros)]

use crate::event_log::EventLog;
use anyhow::{Context, Result};
use sqlx::PgExecutor;
use sqlx::PgPool;

/// Connect to the Postgres ledger at `url` (creating the database if absent, matching the old
/// SQLite create-if-missing behavior) and run all pending migrations before handing back the
/// pool. The URL never appears in the error chain — it can carry credentials.
pub async fn connect(url: &str) -> Result<PgPool> {
    use sqlx::migrate::MigrateDatabase;
    if !sqlx::Postgres::database_exists(url)
        .await
        .context("checking the postgres ledger database exists")?
    {
        sqlx::Postgres::create_database(url)
            .await
            .context("creating the postgres ledger database")?;
    }
    let pool = PgPool::connect(url)
        .await
        .context("opening postgres ledger")?;
    crate::MIGRATOR
        .run(&pool)
        .await
        .context("running controller migrations")?;
    Ok(pool)
}

/// Whether the ledger's database exists at all — the rebuild path's "from-nothing disaster"
/// check (the SQLite era asked `path.is_file()`).
pub(crate) async fn ledger_exists(url: &str) -> Result<bool> {
    use sqlx::migrate::MigrateDatabase;
    sqlx::Postgres::database_exists(url)
        .await
        .context("checking the postgres ledger database exists")
}

/// The session-scoped advisory-lock key every mutually-exclusive writer of the ledger takes: the
/// autopilot daemon for its lifetime, and the maintenance commands (`db rebuild`,
/// `db import-sqlite`, `db migrate-state`) for their run. The value is the ASCII bytes of
/// `"crucible"` as a big-endian i64.
pub const MAINTENANCE_ADVISORY_LOCK: i64 = 0x6372_7563_6962_6c65;

/// Holds [`MAINTENANCE_ADVISORY_LOCK`] on a dedicated connection detached from the pool. Released
/// by [`MaintenanceLock::release`], or by the server noticing the connection close on drop. The
/// lock is session-scoped, so a transaction-mode pooler (pgbouncer, RDS Proxy) between the
/// controller and the server would silently break it.
pub struct MaintenanceLock {
    conn: sqlx::postgres::PgConnection,
}

impl MaintenanceLock {
    /// One liveness probe of the lock's session. A session-scoped advisory lock lives exactly as
    /// long as this connection, so a failed ping means the fence is gone and the holder must
    /// step down.
    pub(crate) async fn ping(&mut self) -> Result<()> {
        sqlx::query("SELECT 1")
            .execute(&mut self.conn)
            .await
            .context("pinging the maintenance-lock session")?;
        Ok(())
    }

    /// Unlock and close the connection deterministically — for callers that take the lock again
    /// (or hand off to another maintenance command) in the same process.
    pub async fn release(self) -> Result<()> {
        let MaintenanceLock { mut conn } = self;
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(MAINTENANCE_ADVISORY_LOCK)
            .execute(&mut conn)
            .await
            .context("releasing the maintenance advisory lock")?;
        sqlx::Connection::close(conn)
            .await
            .context("closing the maintenance-lock connection")
    }
}

/// Try to take the maintenance advisory lock. `Ok(None)` means another session — a running
/// daemon, or another maintenance command — holds it.
pub async fn try_maintenance_lock(pool: &PgPool) -> Result<Option<MaintenanceLock>> {
    let mut conn = pool
        .acquire()
        .await
        .context("acquiring the maintenance-lock connection")?
        .detach();
    let held: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(MAINTENANCE_ADVISORY_LOCK)
        .fetch_one(&mut conn)
        .await
        .context("taking the maintenance advisory lock")?;
    if !held {
        let _ = sqlx::Connection::close(conn).await;
        return Ok(None);
    }
    Ok(Some(MaintenanceLock { conn }))
}

/// `url` with its database name replaced by `name` — the scratch databases `db verify`/`db
/// rebuild` use live on the same server as the ledger.
pub fn sibling_db_url(url: &str, name: &str) -> Result<String> {
    let mut parsed = url::Url::parse(url).context("parsing the ledger URL")?;
    parsed.set_path(&format!("/{name}"));
    Ok(parsed.into())
}

/// The ledger URL's database name (scratch names are derived from it).
pub fn db_name(url: &str) -> Result<String> {
    let parsed = url::Url::parse(url).context("parsing the ledger URL")?;
    let name = parsed.path().trim_start_matches('/');
    if name.is_empty() {
        anyhow::bail!("the ledger URL names no database");
    }
    Ok(name.to_string())
}

/// The controller's ledger handle: a Postgres pool plus the event log every transition appends to,
/// and an optional metrics registry. The registry is `None` for the many test constructions and
/// attached once at startup ([`Db::with_metrics`]) so the choke points re-export what they ingest;
/// it's a cheap `Arc` clone, so every `Db` clone shares the one registry.
#[derive(Clone)]
pub struct Db {
    pool: PgPool,
    events: EventLog,
    metrics: Option<crate::metrics::Metrics>,
}

impl Db {
    /// Wrap an already-open pool; the event log rides the same pool. No metrics until
    /// [`Db::with_metrics`] attaches them.
    pub(crate) fn new(pool: PgPool) -> Self {
        let events = EventLog::new(pool.clone());
        Db {
            pool,
            events,
            metrics: None,
        }
    }

    /// Attach the process's metrics registry (the entrypoint, once, before the ledger is cloned
    /// into the reconcile wiring + the HTTP surface). A builder so the daemon can chain it onto
    /// [`Db::open`]; tests opt in the same way.
    pub fn with_metrics(mut self, metrics: crate::metrics::Metrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// The attached metrics registry, if any — the choke points move a family through this, and
    /// the `/metrics` handler renders it.
    pub(crate) fn metrics(&self) -> Option<&crate::metrics::Metrics> {
        self.metrics.as_ref()
    }

    /// Open the ledger at `db_url`.
    pub async fn open(db_url: &str) -> Result<Self> {
        let pool = connect(db_url).await?;
        Ok(Db::new(pool))
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn events(&self) -> &EventLog {
        &self.events
    }

    /// Append one admission-ledger entry. Stamps `ts`. Kept on `Db` because it composes SQL +
    /// stamping + a real side effect — re-exporting the spend into the metrics registry — in one
    /// place; splitting it would force every one of its call sites to remember the metrics step by
    /// hand, and a dropped spend fails silently (metrics aren't asserted in most tests).
    pub(crate) async fn ledger_append(
        &self,
        run_id: Option<&str>,
        kind: &str,
        cost_usd: f64,
    ) -> Result<()> {
        crate::ledger::ledger_append(
            &self.pool,
            &crate::clock::now_rfc3339(),
            run_id,
            kind,
            cost_usd,
        )
        .await?;
        // Re-export the spend the moment it's booked: `kind` is the cost_tag (a bounded set —
        // rank/rank-grounded/scope/run/capped), so this stays low-cardinality.
        if let Some(m) = &self.metrics {
            m.record_spend(kind, cost_usd);
        }
        Ok(())
    }

    /// Whether `day`'s spend has reached `ceiling`; when it has, book the `capped` row too.
    pub(crate) async fn decline_if_over_ceiling(&self, day: &str, ceiling: f64) -> Result<bool> {
        if crate::ledger::ledger_day_total(&self.pool, day).await? < ceiling {
            return Ok(false);
        }
        self.ledger_append(None, "capped", 0.0).await?;
        Ok(true)
    }
}

/// The highest applied migration version (the schema version), or 0 on a fresh/empty ledger.
#[tracing::instrument(name = "db.schema_version", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn schema_version(ex: impl PgExecutor<'_>) -> Result<i64> {
    let row = sqlx::query!(r#"SELECT MAX(version) AS "version" FROM _sqlx_migrations"#)
        .fetch_one(ex)
        .await
        .context("schema_version")?;
    Ok(row.version.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issues::model::NewIssue;
    use crate::model::{ParkedBy, Status};
    use crate::runs::model::{NewCandidate, NewRun};

    /// Shipped migrations are immutable: sqlx checksums their bytes, so editing one — even a
    /// comment — makes every existing database fail to open ("migration N was previously
    /// applied but has been modified"). CI can't catch that with fresh per-test DBs, so this
    /// pins the exact bytes. A new migration must be added to this list; a shipped one must
    /// never change — put schema changes in a NEW migration instead.
    #[test]
    fn shipped_migrations_are_byte_frozen() {
        use sha2::{Digest, Sha256};
        let pinned = [
            (
                "0001_baseline.sql",
                "eb354c13c81699ff4e4b4c4cf778c502ae87792f2aefe3c2396613d8e4d29a58",
            ),
            (
                "0002_sessions.sql",
                "fdd2031a9044c8e54fa8ad202959b73d7a28be6cca0361f36cfb297eb48ae0c3",
            ),
            (
                "0003_issue_affinity.sql",
                "b7c766cdb4a382900342184e03dd7e3386b4ba363b84f029950233ad4b5ab25c",
            ),
            (
                "0004_shared_state.sql",
                "d4bc4ce89666f427def82c8b52c5034379fc5d007ae6a47007fbc7d9f7e63762",
            ),
            (
                "0005_drop_pod_artifact_path.sql",
                "7177753047a6925974728d8e9000fd25359ed8317f1b6afda8f79bca7ce44f84",
            ),
            (
                "0006_playbooks.sql",
                "c47d88cbd6b5f0e447498875dd3a84b5fa3898fc9b85f7dad2b8981b05390ee4",
            ),
            (
                "0007_playbook_launches.sql",
                "34ed3260e4fa15b805b9512a81ec93ba328ee69a7024f05f33793d66874eda48",
            ),
            (
                "0008_playbook_one_shots.sql",
                "ed08adf0fcd3b211b2efce76fc96c472caed061b80a0d7788ba4c89c924023d8",
            ),
            (
                "0009_playbook_schedules.sql",
                "2fa31a4ec6d0cb8add3669da2eea3268c38395f0fba5a44e8209114e34cb5621",
            ),
            (
                "0010_playbook_cursor.sql",
                "e3beba3f305c88a1767092bd7b6adf5440109d6714782a5be72e00f9b8c67d06",
            ),
            (
                "0011_one_shot_failed.sql",
                "acbcd5e028f7970c20c792b9f3f218df8dd2830b4526dfc6d14e620b77da3bdc",
            ),
            (
                "0012_playbook_drafts.sql",
                "58538f82e4ef8831c0af0031269715a3dcbcd219d2f5436ecf9eb2b400d58820",
            ),
            (
                "0013_pack_imports.sql",
                "d18bfb1bacd0dc18904c349da5588f59dd61f34c09d52aaf89719355f8ad75f8",
            ),
            (
                "0014_dispatch_capability.sql",
                "a4c80ee0a2a775b5ca953c8d39d0e2a8977cfdba837b2ff2a39a36199b62b3d1",
            ),
            (
                "0015_user_prefs.sql",
                "4eeaf502822e1e4b2fd2cc8e09cc5c6dfb32033616e91645da906984ddea33e0",
            ),
            (
                "0016_draft_origin.sql",
                "a6ad091af7e025d63dd9b0ab75168e545c671cc6f96fbca71737d682c2107fe0",
            ),
            (
                "0017_runs_issue.sql",
                "aefae26a62805749fcccde3341d647dede7436e1ea9333c4c86aa958b195dcfd",
            ),
            (
                "0018_secrets_registry.sql",
                "50fe2a5d6fe5cfe62d71db8e2384e1b0ba230f2f232959f448919e76fdb70c20",
            ),
            (
                "0019_dispatch_grants.sql",
                "a7af089ba647a7bf3cfba2b9caa0f808f42e4120eadce4a1b3f671c8d7b8733f",
            ),
            (
                "0020_users.sql",
                "f67972d1117fc8963c87d211399c3091902f105776331c3696d8652b453343ec",
            ),
            (
                "0021_user_credentials.sql",
                "dec77ea26d495fba5bd8cc1063ce82675d28d35f2571dc8374d5293442937859",
            ),
            (
                "0022_import_declared_secrets.sql",
                "474e91e07ccafd4453756a22e0d380d67b053c5be06318fb0d67ab3ec2d84502",
            ),
            (
                "0023_scenario_pack_path.sql",
                "6f288abe6ee34b6977bd673bccfb39c2917d7b8c08c8c3b19f354fd94a29be01",
            ),
            (
                "0024_run_dispatch_location.sql",
                "b3dfb136d9913f0b6741c4e1db4a863fcc209c218d0146e832280a443f075a54",
            ),
            (
                "0025_issue_dispatch_target.sql",
                "ce5edcd34322dc64e6ab157232005e860e25c905549c40e97bdaa415936f6140",
            ),
            (
                "0026_api_keys.sql",
                "0e3e19997e910a4184df034a04021cb1447a009aaaf6e23b894a00484b71aa84",
            ),
            (
                "0027_schedule_pack_adoption.sql",
                "78c527775e779b47937848945d7de342af542316812d4c53a503f8bbc3aa0682",
            ),
            (
                "0028_minted_secret_mode.sql",
                "b3d5fd82702fdf7a67b7bf56ac906089b9874b0852bbea3b76ec39ebe3f62ba2",
            ),
            (
                "0029_model_providers.sql",
                "11725430061abc5bd4ab78a05c1440f0f20871c5f2bc457b0e9da1e37504b69c",
            ),
            (
                "0030_inference_api_key_secret_kind.sql",
                "bbcbd2aea7acbb544c72bc11c8b68ced468c8e30e53b421265cc04d509472601",
            ),
            (
                "0031_secret_transfer_audit.sql",
                "029a3ae4ff7d3a2bb8b2a936f60a224fcfbd204a7048ee8b2bdb4cee2ace2ada",
            ),
            (
                "0032_provider_endpoint.sql",
                "6f0733a61a2d04d616f5c3c0c5f021e5d8f12672f0f25ab7c6fa33717073556c",
            ),
            (
                "0033_standing_launches.sql",
                "ebfcc969ddf077e889c95aef31fa181a8ff9093816bddff2da64c28aff396ceb",
            ),
            (
                "0034_pack_exposure.sql",
                "f6c6c248e4a093fd71de065cad6abf58ded54e72ad04cc84faa9dbaffe07e7b4",
            ),
            (
                "0035_task_blocked.sql",
                "5563c73846dd1b4068265e151ffd3509673afd5b63d5135ce75c4eda25b04272",
            ),
            (
                "0036_playbook_cursor_file.sql",
                "4935c34c548b9d029639294eab889321a44a1fb7e089ebff8ee5124e3bc81464",
            ),
            (
                "0037_image_catalog.sql",
                "1d67605c015461506810d1572764f8bdad7d273f4a25f631b887732ee3718acf",
            ),
            (
                "0038_agent_requires.sql",
                "c114acb3f989fb4f616a8ce7f616f146c33baf769bbfadb612ab4a6fd8d26d7d",
            ),
            (
                "0039_teams.sql",
                "8913812ef82eb101f95d743c1435dd2b2cf0bf072b9d8bffdb8e6fff93f8f0b2",
            ),
            (
                "0040_policy_sets.sql",
                "bb41406a6ff415c313d60d941f4a62bd808f36069388d1ad898a43cb6233e8d9",
            ),
            (
                "0041_owners.sql",
                "4c5367dbdab45b7137b1d8d4b33733c5a1437827289b3113a478359042aa8695",
            ),
            (
                "0042_resource_shares.sql",
                "66286f6d5c06668cade6b7e273b7278d7e186e18a0ca9212236274abccc3e34d",
            ),
            (
                "0043_provider_harness.sql",
                "74d09ae8de4a6e790de667fe47156631615325fd720615c502d545e68a3822d0",
            ),
            (
                "0044_playbook_draft_source.sql",
                "8572e7673871be111108b92be1e74a48a6619699a8d9cc7e266d79ade3f9ce65",
            ),
        ];
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        for (name, want) in pinned {
            let bytes = std::fs::read(dir.join(name)).expect(name);
            let got = format!("{:x}", Sha256::digest(&bytes));
            assert_eq!(
                got, want,
                "{name} was modified after shipping — restore its exact bytes; schema changes go in a NEW migration"
            );
        }
        let on_disk = std::fs::read_dir(&dir)
            .expect("migrations dir")
            .filter(|e| {
                e.as_ref()
                    .is_ok_and(|e| e.path().extension().is_some_and(|x| x == "sql"))
            })
            .count();
        assert_eq!(
            on_disk,
            pinned.len(),
            "a migration exists that isn't pinned here — add its hash to this list when shipping it"
        );
    }

    fn sample_issue(key: &str) -> NewIssue {
        NewIssue {
            key: key.to_string(),
            repo: "owner/repo".to_string(),
            priority: 5,
            evidence_url: Some("https://github.com/owner/repo/issues/1".to_string()),
            title: Some("a sample issue".to_string()),
            author: Some("octocat".to_string()),
            body: None,
            labels: Vec::new(),
            upstream_updated_at: None,
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn upsert_then_get_round_trips(pool: PgPool) -> Result<()> {
        let db = Db::new(pool);
        crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;

        let got = crate::issues::store::get_issue(db.pool(), "owner/repo#1")
            .await?
            .expect("issue exists");
        assert_eq!(got.key, "owner/repo#1");
        assert_eq!(got.repo, "owner/repo");
        assert!(
            got.tier.is_none(),
            "triage is pure discovery: no tier guess"
        );
        assert_eq!(got.status, Status::New, "enters at new");
        assert_eq!(got.priority, 5);
        assert!(got.parked_by.is_none());
        assert!(
            crate::issues::store::get_issue(db.pool(), "nope#9")
                .await?
                .is_none()
        );
        Ok(())
    }

    #[cfg(feature = "autoresearch")]
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn upsert_never_overwrites_a_ranked_tier(pool: PgPool) -> Result<()> {
        let db = Db::new(pool);
        crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
        assert!(
            crate::issues::store::claim_issue(
                db.pool(),
                "owner/repo#1",
                Status::New,
                Status::Scoped
            )
            .await?
        );
        crate::issues::store::set_ranked_tier(db.pool(), "owner/repo#1", "T2", "perf", "somehash")
            .await?;

        // A re-triage sweep (same tracked issue): status, tier, and priority must all survive.
        let mut re = sample_issue("owner/repo#1");
        re.priority = 9;
        crate::issues::store::upsert_issue(db.pool(), &re).await?;

        let got = crate::issues::store::get_issue(db.pool(), "owner/repo#1")
            .await?
            .expect("issue");
        assert_eq!(
            got.status,
            Status::Scoped,
            "re-triage must not reset status"
        );
        assert_eq!(
            got.tier.as_deref(),
            Some("T2"),
            "re-triage must not clobber a tier the ranker already set"
        );
        assert_eq!(
            got.priority, 5,
            "re-triage must not clobber a priority the human set"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn upsert_refreshes_body(pool: PgPool) -> Result<()> {
        let db = Db::new(pool);
        let mut iss = sample_issue("owner/repo#1");
        iss.body = Some("original body".to_string());
        crate::issues::store::upsert_issue(db.pool(), &iss).await?;
        assert_eq!(
            crate::issues::store::get_issue(db.pool(), "owner/repo#1")
                .await?
                .expect("issue")
                .body
                .as_deref(),
            Some("original body")
        );

        // A re-sweep with an edited body refreshes it, like title/author/labels.
        iss.body = Some("edited body".to_string());
        crate::issues::store::upsert_issue(db.pool(), &iss).await?;
        assert_eq!(
            crate::issues::store::get_issue(db.pool(), "owner/repo#1")
                .await?
                .expect("issue")
                .body
                .as_deref(),
            Some("edited body")
        );
        Ok(())
    }

    #[cfg(feature = "autoresearch")]
    fn comment(id: i64, key: &str, body: &str) -> crate::issues::model::IssueComment {
        crate::issues::model::IssueComment {
            id,
            issue_key: key.to_string(),
            author: Some("octocat".to_string()),
            created_at: format!("2026-07-01T00:00:{:02}Z", id),
            updated_at: format!("2026-07-01T00:00:{:02}Z", id),
            body: body.to_string(),
        }
    }

    #[cfg(feature = "autoresearch")]
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn issue_comments_upsert_edit_and_delete_vanished(pool: PgPool) -> Result<()> {
        let db = Db::new(pool);
        crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;

        // First sweep: two comments land, oldest first on read-back.
        crate::issues::store::replace_issue_comments(
            db.pool(),
            "owner/repo#1",
            &[
                comment(2, "owner/repo#1", "second"),
                comment(1, "owner/repo#1", "first"),
            ],
        )
        .await?;
        let got = crate::issues::store::list_issue_comments(db.pool(), "owner/repo#1").await?;
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, 1);
        assert_eq!(got[0].body, "first");
        assert_eq!(got[1].id, 2);

        // Second sweep: #1 edited, #2 vanished upstream, #3 new.
        let mut edited = comment(1, "owner/repo#1", "first (edited)");
        edited.updated_at = "2026-07-02T00:00:00Z".to_string();
        crate::issues::store::replace_issue_comments(
            db.pool(),
            "owner/repo#1",
            &[edited, comment(3, "owner/repo#1", "third")],
        )
        .await?;
        let got = crate::issues::store::list_issue_comments(db.pool(), "owner/repo#1").await?;
        assert_eq!(
            got.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![1, 3],
            "the vanished comment's row is deleted"
        );
        assert_eq!(got[0].body, "first (edited)");
        assert_eq!(got[0].updated_at, "2026-07-02T00:00:00Z");

        // A sweep seeing zero comments clears the set.
        crate::issues::store::replace_issue_comments(db.pool(), "owner/repo#1", &[]).await?;
        assert!(
            crate::issues::store::list_issue_comments(db.pool(), "owner/repo#1")
                .await?
                .is_empty()
        );
        Ok(())
    }

    #[cfg(feature = "autoresearch")]
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn issue_comments_are_scoped_per_issue(pool: PgPool) -> Result<()> {
        let db = Db::new(pool);
        crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
        crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#2")).await?;
        crate::issues::store::replace_issue_comments(
            db.pool(),
            "owner/repo#1",
            &[comment(1, "owner/repo#1", "on #1")],
        )
        .await?;
        crate::issues::store::replace_issue_comments(
            db.pool(),
            "owner/repo#2",
            &[comment(2, "owner/repo#2", "on #2")],
        )
        .await?;

        // Replacing #1's set never touches #2's rows.
        crate::issues::store::replace_issue_comments(db.pool(), "owner/repo#1", &[]).await?;
        assert!(
            crate::issues::store::list_issue_comments(db.pool(), "owner/repo#1")
                .await?
                .is_empty()
        );
        let other = crate::issues::store::list_issue_comments(db.pool(), "owner/repo#2").await?;
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].body, "on #2");
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn claim_flips_only_from_the_expected_status(pool: PgPool) -> Result<()> {
        let db = Db::new(pool);
        crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;

        // Wrong `from` loses.
        assert!(
            !crate::issues::store::claim_issue(
                db.pool(),
                "owner/repo#1",
                Status::Scoped,
                Status::Running
            )
            .await?
        );
        // Right `from` wins.
        assert!(
            crate::issues::store::claim_issue(
                db.pool(),
                "owner/repo#1",
                Status::New,
                Status::Scoped
            )
            .await?
        );
        // The same claim can't win twice.
        assert!(
            !crate::issues::store::claim_issue(
                db.pool(),
                "owner/repo#1",
                Status::New,
                Status::Scoped
            )
            .await?
        );
        assert_eq!(
            crate::issues::store::get_issue(db.pool(), "owner/repo#1")
                .await?
                .unwrap()
                .status,
            Status::Scoped
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn claim_is_won_by_exactly_one_racer(pool: PgPool) -> Result<()> {
        let db = Db::new(pool);
        crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;

        let a = {
            let pool = db.pool().clone();
            tokio::spawn(async move {
                crate::issues::store::claim_issue(
                    &pool,
                    "owner/repo#1",
                    Status::New,
                    Status::Running,
                )
                .await
            })
        };
        let b = {
            let pool = db.pool().clone();
            tokio::spawn(async move {
                crate::issues::store::claim_issue(
                    &pool,
                    "owner/repo#1",
                    Status::New,
                    Status::Running,
                )
                .await
            })
        };
        let (ra, rb) = tokio::join!(a, b);
        let won = [ra??, rb??];
        assert_eq!(
            won.iter().filter(|w| **w).count(),
            1,
            "exactly one racer claims the issue"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn park_sets_reason_and_authority(pool: PgPool) -> Result<()> {
        let db = Db::new(pool);
        crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
        crate::issues::transitions::park_and_purge(
            db.pool(),
            "owner/repo#1",
            "no repro",
            ParkedBy::Machine,
        )
        .await?;

        let got = crate::issues::store::get_issue(db.pool(), "owner/repo#1")
            .await?
            .expect("issue");
        assert_eq!(got.status, Status::Parked);
        assert_eq!(got.parked_reason.as_deref(), Some("no repro"));
        assert_eq!(got.parked_by, Some(ParkedBy::Machine));
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn transition_claims_and_logs_once(pool: PgPool) -> Result<()> {
        let db = Db::new(pool);
        crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;

        let won = crate::issues::transitions::transition(
            db.pool(),
            db.events(),
            "owner/repo#1",
            Status::New,
            Status::Scoped,
            Some("check passed"),
            Some("s3://run/1"),
        )
        .await?;
        assert!(won);
        // A losing transition writes no line.
        let again = crate::issues::transitions::transition(
            db.pool(),
            db.events(),
            "owner/repo#1",
            Status::New,
            Status::Scoped,
            None,
            None,
        )
        .await?;
        assert!(!again);

        let body = crate::event_log::export_string(db.pool()).await?;
        assert_eq!(
            body.lines().count(),
            1,
            "one line for the one won transition"
        );
        let line: serde_json::Value = serde_json::from_str(body.lines().next().unwrap())?;
        assert_eq!(line["from"], "new");
        assert_eq!(line["to"], "scoped");
        assert_eq!(line["reason"], "check passed");
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn record_run_and_candidate_round_trip(pool: PgPool) -> Result<()> {
        let db = Db::new(pool);
        crate::runs::store::insert_run(
            db.pool(),
            &NewRun {
                run_id: "run-1".to_string(),
                scope: None,
                issue: None,
                identity_digest: Some("deadbeef".to_string()),
                status: "running".to_string(),
                pod: Some("loop-abc".to_string()),
                session_uri: Some("s3://run/1/session.jsonl".to_string()),
                best_score: Some(234.0),
                cost_usd: Some(1.25),
            },
        )
        .await?;
        crate::runs::store::insert_candidate(
            db.pool(),
            &NewCandidate {
                run_id: "run-1".to_string(),
                kind: Some("wide".to_string()),
                lane: Some(0),
                iter: Some(1),
                score: Some(240.0),
                decision: Some("keep".to_string()),
                worktree: Some("/w/lane-0".to_string()),
                sandbox: Some("sbx-0".to_string()),
                pr_url: Some("https://github.com/owner/repo/pull/9".to_string()),
                branch: Some("autoresearch/run-1/0".to_string()),
            },
        )
        .await?;

        // Read back through raw queries (no read verb for these tables).
        let run = sqlx::query!("SELECT status, best_score, pod FROM runs WHERE run_id = 'run-1'")
            .fetch_one(db.pool())
            .await?;
        assert_eq!(run.status, "running");
        assert_eq!(run.best_score, Some(234.0));
        assert_eq!(run.pod.as_deref(), Some("loop-abc"));

        let cand = sqlx::query!(
            "SELECT kind, decision, score, branch FROM candidates WHERE run_id = 'run-1'"
        )
        .fetch_one(db.pool())
        .await?;
        assert_eq!(cand.kind.as_deref(), Some("wide"));
        assert_eq!(cand.decision.as_deref(), Some("keep"));
        assert_eq!(cand.score, Some(240.0));
        assert_eq!(cand.branch.as_deref(), Some("autoresearch/run-1/0"));
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn ledger_append_and_day_total(pool: PgPool) -> Result<()> {
        let db = Db::new(pool);
        db.ledger_append(Some("run-1"), "scope", 0.50).await?;
        db.ledger_append(Some("run-1"), "run", 1.25).await?;
        db.ledger_append(None, "discovery", 0.05).await?;

        let today = jiff::Timestamp::now().strftime("%Y-%m-%d").to_string();
        let total = crate::ledger::ledger_day_total(db.pool(), &today).await?;
        assert!(
            (total - 1.80).abs() < 1e-9,
            "summed the day's cost: {total}"
        );
        // A day with no entries totals zero, not an error.
        assert_eq!(
            crate::ledger::ledger_day_total(db.pool(), "1999-01-01").await?,
            0.0
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn non_terminal_keys_excludes_done(pool: PgPool) -> Result<()> {
        let db = Db::new(pool);
        for k in ["owner/repo#1", "owner/repo#2", "owner/repo#3"] {
            crate::issues::store::upsert_issue(db.pool(), &sample_issue(k)).await?;
        }
        // Drive #2 to done; it must drop out of the re-enqueue set.
        assert!(
            crate::issues::store::claim_issue(db.pool(), "owner/repo#2", Status::New, Status::Done)
                .await?
        );

        let keys = crate::issues::store::non_terminal_keys(db.pool()).await?;
        assert_eq!(keys, vec!["owner/repo#1", "owner/repo#3"]);
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn schema_version_is_current(pool: PgPool) -> Result<()> {
        let db = Db::new(pool);
        assert!(
            crate::client::schema_version(db.pool()).await? >= 1,
            "migration 0001 applied"
        );
        Ok(())
    }

    /// The `crucible-controller db verify` path: `connect` creates + migrates a fresh file on open (no
    /// pre-existing DB, no `sqlx::test` fixture), and the schema version reads back as the latest
    /// migration.
    #[tokio::test]
    async fn connect_creates_and_migrates_a_fresh_db() -> Result<()> {
        use sqlx::migrate::MigrateDatabase;
        let url = crate::test_ledger_url();
        assert!(
            !sqlx::Postgres::database_exists(&url).await?,
            "precondition: no db yet"
        );

        let pool = connect(&url).await?;
        assert!(
            sqlx::Postgres::database_exists(&url).await?,
            "connect creates the database"
        );
        assert_eq!(crate::schema_version(&pool).await?, 44);

        // Idempotent: re-opening an already-migrated DB is a no-op, not an error.
        let pool2 = connect(&url).await?;
        assert_eq!(crate::schema_version(&pool2).await?, 44);
        pool.close().await;
        pool2.close().await;
        let _ = sqlx::Postgres::drop_database(&url).await;
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn maintenance_lock_is_exclusive_and_releasable(pool: PgPool) -> Result<()> {
        let first = try_maintenance_lock(&pool)
            .await?
            .expect("first taker wins");
        assert!(
            try_maintenance_lock(&pool).await?.is_none(),
            "a second taker is refused while the lock is held"
        );
        first.release().await?;
        let again = try_maintenance_lock(&pool)
            .await?
            .expect("released lock is takable again");
        again.release().await?;
        Ok(())
    }
}
