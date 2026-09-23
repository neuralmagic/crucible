//! crucible-controller — the control plane's CLI: discovery (`triage`), the reconcile daemon
//! (`autopilot`), ledger maintenance (`db verify|rebuild|import-sqlite`), the grounded-vs-API ranking
//! comparison harness (`rank-compare`), and the OpenAPI spec print (`openapi`). Split out of the
//! public `crucible` engine binary so the engine ships without the control plane: the controller
//! shells the engine (`crucible rank-grounded`, `crucible deploy render`) as a subprocess and the
//! two meet only at the `crucible-contract` wire types.

#![allow(clippy::disallowed_macros)]

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// Ctrl-C flips this; a poller translates it into the daemon's `Notify`-based shutdown signal.
static STOP: AtomicBool = AtomicBool::new(false);

/// Top-level CLI: every subcommand is an outer-loop arm; there is no default run.
#[derive(Parser)]
#[command(
    about = "Crucible's outer-loop controller: issue discovery, the reconcile daemon, and ledger maintenance"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(clap::Subcommand)]
pub(crate) enum Cmd {
    /// Discovery (lane B): scan watched repos' issues (since each repo's watermark), rank them by
    /// judge-tier, and upsert them into the controller ledger.
    #[cfg(feature = "autoresearch")]
    Triage {
        #[command(flatten)]
        cfg: crucible_controller::ControllerCfg,
        /// Force a full resync: ignore each watched repo's stored watermark for this one sweep
        /// and re-fetch + re-upsert every issue, not just what changed. The backfill/repair path
        /// for columns added to `issues` after rows were first triaged (e.g. `title`/`author`),
        /// since a normal sweep only re-fetches issues whose upstream `updated_at` moved.
        #[arg(long)]
        full: bool,
    },
    /// Controller: reconcile the ledger — pick → scope → approve → launch → record — either once to
    /// quiescence (`--once`) or as a resident daemon.
    Autopilot {
        #[command(flatten)]
        cfg: crucible_controller::ControllerCfg,
        /// Enqueue all non-terminal rows, drain the queue, and exit (the dev/test/break-glass mode)
        /// instead of running as a resident daemon.
        #[arg(long)]
        once: bool,
    },
    /// Controller ledger maintenance: verify the schema, rebuild from evidence, or import a
    /// pre-Postgres sqlite ledger.
    Db {
        #[command(subcommand)]
        action: DbAction,
    },
    /// Print the controller API's OpenAPI spec as JSON (the input to the SPA's TypeScript
    /// codegen; no running server needed).
    Openapi,
    /// The spoke-side push agent: gather this cluster's GPU/Kueue snapshot with the ambient
    /// in-cluster identity and POST it to the hub's push endpoint. One shot — a CronJob is the
    /// scheduler. For spokes the hub cannot reach (corp-internal clusters push outbound).
    ClusterSnapshot {
        /// The cluster name to report as (must match the hub's CONTROLLER_PUSH_TOKENS entry).
        #[arg(long)]
        cluster: String,
        /// The hub's push endpoint, e.g. https://crucible-ext.example/api/clusters/push.
        #[arg(long)]
        push_url: String,
        /// File carrying the push bearer token (a mounted secret, never argv).
        #[arg(long)]
        token_file: PathBuf,
    },
    /// One-shot device-flow login that prints every claim the issuer hands this client, to
    /// confirm `groups` arrives before switching auth.mode to native.
    OidcClaims {
        #[arg(long, default_value = "openid email profile")]
        scopes: String,
    },
    /// Measurement harness: for each already-ranked issue in a repo, run the grounded ranker and emit
    /// a markdown report comparing the API tier against the grounded tier (agreement rate,
    /// disagreement matrix, cost, wall time) plus the raw rows as JSONL. Read-only over the ledger:
    /// it writes no tiers, only `rank-compare` ledger rows, and is resumable (skips issues already
    /// in the out JSONL).
    #[cfg(feature = "autoresearch")]
    RankCompare {
        #[command(flatten)]
        cfg: crucible_controller::ControllerCfg,
        /// The repo whose ranked issues to compare (owner/repo). Positional: the flattened
        /// controller config already owns the repeatable `--repo` flag, so a long flag here
        /// would collide and clap silently feeds every value to the flattened one.
        #[arg(value_name = "REPO")]
        repo: String,
        /// Cap the number of issues compared this run (0 = all).
        #[arg(long, default_value_t = 0)]
        limit: usize,
        /// Only compare issues the API ranker actually ranked (a non-null ranked-content hash),
        /// excluding any tier set by other means.
        #[arg(long)]
        only_ranked: bool,
        /// The markdown report to write (its `.jsonl` sidecar lands beside it).
        #[arg(long)]
        out: PathBuf,
        /// Override the grounded turn's agent with a `command`-backend script (the test seam,
        /// threaded down to each `rank-grounded` invocation).
        #[arg(long, hide = true)]
        agent_cmd: Option<String>,
    },
}

/// `crucible-controller db <verify|rebuild|import-sqlite>`: open/migrate the ledger and report
/// its schema version, re-derive it from evidence (lane H), or cut a sqlite-era ledger over.
#[derive(clap::Subcommand)]
pub(crate) enum DbAction {
    /// Verify the ledger. When the database named by `--db`/`DATABASE_URL` already exists,
    /// rebuilds it into a throwaway comparison database on the same server and reports the
    /// drift (periodic disaster-fallback check, lane H). Otherwise falls back to the original
    /// behavior: create a throwaway database, run migrations, and print the applied schema
    /// version — proving the migration set without touching real state.
    Verify {
        #[command(flatten)]
        cfg: crucible_controller::ControllerCfg,
    },
    /// Re-derive the ledger from evidence (GitHub issue state, re-ingestible session logs, the event
    /// log's transition history) into a fresh sibling database, then swap it in over the live one
    /// (`ALTER DATABASE … RENAME`) on full success (disaster-fallback rebuild, lane H). Run with
    /// the daemon stopped: the rename needs no live connections.
    Rebuild {
        #[command(flatten)]
        cfg: crucible_controller::ControllerCfg,
    },
    /// One-shot cutover: copy every row of a pre-Postgres `outer.sqlite` ledger file into the
    /// (empty) Postgres ledger at `--db`/`DATABASE_URL`. Run in-cluster with the daemon stopped —
    /// the pod is what can reach both the state PVC and the database server.
    ImportSqlite {
        /// The sqlite ledger file to read (typically `<state-dir>/outer.sqlite`).
        #[arg(long)]
        from: PathBuf,
        #[command(flatten)]
        cfg: crucible_controller::ControllerCfg,
    },
    /// One-shot cutover of a legacy state volume into the shared-state tables: drop-box evidence
    /// (digest-verified against `pod_artifacts`), run session logs (dispatched/adopted/external),
    /// packs (re-tarred, `STEER.md` steering appends split out), `controller-events.jsonl`, and
    /// `autopilot.json`. Idempotent, and refuses to run while the maintenance advisory lock is
    /// held. Run with the daemon stopped, `--state-dir` at the old volume.
    MigrateState {
        #[command(flatten)]
        cfg: crucible_controller::ControllerCfg,
        /// Import the parseable event lines even when some lines are unparseable (they are
        /// counted and reported instead of failing the run).
        #[arg(long)]
        skip_bad_lines: bool,
    },
    /// Export the events table as NDJSON in the frozen `controller-events.jsonl` line shape, to a
    /// file or stdout — the disaster-case export (the history survives losing the database) and
    /// the offline-analysis format.
    ExportEvents {
        #[command(flatten)]
        cfg: crucible_controller::ControllerCfg,
        /// The file to write; stdout when omitted.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Adopt a run launched outside the controller into the ledger from its published session log
    /// (`s3://` via `crucible fetch`, or a local path). Idempotent by run id: re-adopting updates
    /// the same row instead of duplicating candidates or re-booking cost.
    Adopt {
        #[command(flatten)]
        cfg: crucible_controller::ControllerCfg,
        /// s3://bucket/prefix/session.jsonl or a local session.jsonl path.
        #[arg(long)]
        session_uri: String,
        /// Attach the run to this tracked issue's latest scope (optional).
        #[arg(long)]
        issue: Option<String>,
        /// Reuse an id (re-adopt updates the same row); minted when omitted.
        #[arg(long)]
        run_id: Option<String>,
    },
}

fn main() -> Result<()> {
    crucible_controller::install_crypto_provider();
    match Cli::parse().command {
        #[cfg(feature = "autoresearch")]
        Cmd::Triage { cfg, full } => dispatch_triage(cfg, full),
        Cmd::Autopilot { cfg, once } => dispatch_autopilot(cfg, once),
        Cmd::Db { action } => dispatch_db(action),
        Cmd::Openapi => {
            println!("{}", crucible_controller::openapi_spec()?);
            Ok(())
        }
        Cmd::ClusterSnapshot {
            cluster,
            push_url,
            token_file,
        } => cli_runtime()?.block_on(push_cluster_snapshot(&cluster, &push_url, &token_file)),
        Cmd::OidcClaims { scopes } => cli_runtime()?.block_on(async {
            let cfg =
                crucible_controller::identity::oidc::claims_probe::ProbeCfg::from_env(scopes)?;
            crucible_controller::identity::oidc::claims_probe::run(&cfg, &mut std::io::stdout())
                .await
        }),
        #[cfg(feature = "autoresearch")]
        Cmd::RankCompare {
            cfg,
            repo,
            limit,
            only_ranked,
            out,
            agent_cmd,
        } => dispatch_rank_compare(cfg, repo, limit, only_ranked, out, agent_cmd),
    }
}

/// One multi-thread runtime for the finite CLI arms (triage / db / rank-compare): the controller
/// library is async, the CLI is not, so each arm block_ons to completion here. The autopilot
/// daemon deliberately builds its own runtime below — it has its own telemetry and shutdown model.
fn cli_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the tokio runtime")
}

/// `crucible-controller cluster-snapshot`: one gather + one POST. Exit code is the CronJob's
/// health signal, so any failure (gather, token read, non-2xx) is an error, never a shrug.
async fn push_cluster_snapshot(
    cluster: &str,
    push_url: &str,
    token_file: &std::path::Path,
) -> Result<()> {
    let token = std::fs::read_to_string(token_file)
        .with_context(|| format!("reading the push token at {}", token_file.display()))?;
    let token = token.trim();
    if token.is_empty() {
        anyhow::bail!("the push token file at {} is empty", token_file.display());
    }
    let clients = crucible_controller::runs::clusters::ClusterClients::new(None);
    let client = clients
        .client(crucible_controller::runs::clusters::HUB_CLUSTER)
        .await
        .context("building the ambient in-cluster kube client")?;
    let fetched = crucible_controller::runs::cluster_stats::gather(client, cluster)
        .await
        .context("gathering the cluster snapshot")?;
    let snapshot = crucible_controller::runs::cluster_stats::ClusterSnapshot {
        cluster: cluster.to_string(),
        reachable: true,
        age_secs: 0,
        pools: fetched.pools,
        kueue: fetched.kueue,
        error: None,
    };
    let res = reqwest::Client::new()
        .post(push_url)
        .bearer_auth(token)
        .json(&snapshot)
        .send()
        .await
        .context("posting the snapshot to the hub")?;
    let status = res.status();
    if !status.is_success() {
        let body = res.text().await.unwrap_or_default();
        anyhow::bail!("hub rejected the snapshot push: {status} {body}");
    }
    println!(
        "pushed {cluster}: {} pool(s), kueue={}",
        snapshot.pools.len(),
        snapshot.kueue.as_ref().map_or(0, |k| k.len())
    );
    Ok(())
}

/// `crucible-controller triage`: open the ledger, triage every watched repo (fetch changed issues,
/// rank by judge-tier, upsert), and print a summary table.
#[cfg(feature = "autoresearch")]
fn dispatch_triage(cfg: crucible_controller::ControllerCfg, full: bool) -> Result<()> {
    if cfg.repos.is_empty() {
        anyhow::bail!(
            "crucible-controller triage needs at least one watched repo: pass --repo owner/repo \
             (repeatable) or set CONTROLLER_WATCHED_REPOS"
        );
    }
    let rt = cli_runtime()?;
    let summaries = rt.block_on(async {
        let db = crucible_controller::Db::open(cfg.db_url()).await?;
        let mut summaries = Vec::with_capacity(cfg.repos.len());
        for repo in &cfg.repos {
            summaries.push(crucible_controller::triage_repo(&db, repo, full).await?);
        }
        Ok::<Vec<crucible_controller::TriageSummary>, anyhow::Error>(summaries)
    })?;

    println!(
        "{:<30} {:>5} {:>9} {:>8} {:>9} {:>8}",
        "repo", "new", "changed", "parked", "skipped", "retired"
    );
    for s in &summaries {
        println!(
            "{:<30} {:>5} {:>9} {:>8} {:>9} {:>8}",
            s.repo, s.new, s.changed, s.parked, s.skipped_closed, s.retired
        );
    }
    Ok(())
}

/// `crucible-controller rank-compare`: open the ledger, run the grounded ranker against each
/// already-ranked issue in `repo`, and emit the comparison report + JSONL.
#[cfg(feature = "autoresearch")]
fn dispatch_rank_compare(
    cfg: crucible_controller::ControllerCfg,
    repo: String,
    limit: usize,
    only_ranked: bool,
    out: PathBuf,
    agent_cmd: Option<String>,
) -> Result<()> {
    let rt = cli_runtime()?;
    let summary = rt.block_on(async {
        let db = crucible_controller::Db::open(cfg.db_url()).await?;
        crucible_controller::issues::compare::run_compare(
            &db,
            &cfg,
            &crucible_controller::issues::compare::CompareOpts {
                repo,
                limit,
                only_ranked,
                out,
                agent_cmd,
            },
        )
        .await
    })?;
    println!(
        "crucible-controller rank-compare: {} compared ({} new this run), {} agree — report at {}",
        summary.total,
        summary.newly_compared,
        summary.agreements,
        summary.report_path.display()
    );
    Ok(())
}

/// `crucible-controller db <verify|rebuild|import-sqlite>`.
fn dispatch_db(action: DbAction) -> Result<()> {
    match action {
        DbAction::Verify { cfg } => dispatch_db_verify(cfg),
        DbAction::Rebuild { cfg } => dispatch_db_rebuild(cfg),
        DbAction::ImportSqlite { from, cfg } => dispatch_db_import_sqlite(from, cfg),
        DbAction::MigrateState {
            cfg,
            skip_bad_lines,
        } => dispatch_db_migrate_state(cfg, skip_bad_lines),
        DbAction::ExportEvents { cfg, out } => dispatch_db_export_events(cfg, out),
        DbAction::Adopt {
            cfg,
            session_uri,
            issue,
            run_id,
        } => dispatch_db_adopt(cfg, session_uri, issue, run_id),
    }
}

/// `crucible-controller db import-sqlite`: connect (creating + migrating the target if absent),
/// stream the sqlite file in, print the per-table receipt.
fn dispatch_db_import_sqlite(from: PathBuf, cfg: crucible_controller::ControllerCfg) -> Result<()> {
    let rt = cli_runtime()?;
    let report = rt.block_on(async {
        let pool = crucible_controller::connect(cfg.db_url()).await?;
        let lock = crucible_controller::try_maintenance_lock(&pool)
            .await?
            .context(
                "the maintenance advisory lock is held by another session (a running daemon or \
                 another maintenance command); stop it and retry",
            )?;
        let result = crucible_controller::daemon::import_sqlite::import_sqlite(&from, &pool).await;
        let _ = lock.release().await;
        pool.close().await;
        result
    })?;
    println!(
        "crucible-controller db import-sqlite: {} — {} row(s) across {} table(s)",
        from.display(),
        report.total(),
        report.tables.len()
    );
    for (table, n) in &report.tables {
        println!("  {table}: {n}");
    }
    Ok(())
}

/// `crucible-controller db migrate-state`: walk the legacy state volume into the shared-state
/// tables and print the per-category receipt. Any failed item exits nonzero after the table.
fn dispatch_db_migrate_state(
    cfg: crucible_controller::ControllerCfg,
    skip_bad_lines: bool,
) -> Result<()> {
    let rt = cli_runtime()?;
    let report = rt.block_on(async {
        let pool = crucible_controller::connect(cfg.db_url()).await?;
        let report = crucible_controller::runs::migrate_state::migrate_state(
            &pool,
            &cfg.state_dir,
            skip_bad_lines,
        )
        .await?;
        pool.close().await;
        Ok::<_, anyhow::Error>(report)
    })?;
    println!(
        "crucible-controller db migrate-state: {}",
        cfg.state_dir.display()
    );
    println!(
        "{:<14} {:>9} {:>17} {:>7}",
        "category", "migrated", "skipped-existing", "failed"
    );
    for (name, cat) in report.categories() {
        println!(
            "{:<14} {:>9} {:>17} {:>7}",
            name,
            cat.migrated,
            cat.skipped,
            cat.failed.len()
        );
    }
    if report.events.bad_lines > 0 && report.events.failed.is_empty() {
        println!(
            "events: {} unparseable line(s) skipped (--skip-bad-lines)",
            report.events.bad_lines
        );
    }
    let failures = report.failures();
    if !failures.is_empty() {
        for f in &failures {
            eprintln!("failed: {f}");
        }
        anyhow::bail!("{} item(s) failed to migrate", failures.len());
    }
    Ok(())
}

/// `crucible-controller db export-events`: dump the events table, oldest first, as NDJSON.
fn dispatch_db_export_events(
    cfg: crucible_controller::ControllerCfg,
    out: Option<PathBuf>,
) -> Result<()> {
    let rt = cli_runtime()?;
    let n = rt.block_on(async {
        let pool = crucible_controller::connect(cfg.db_url()).await?;
        let n = match &out {
            Some(path) => {
                let file = std::fs::File::create(path)
                    .with_context(|| format!("creating {}", path.display()))?;
                let mut w = std::io::BufWriter::new(file);
                let n = crucible_controller::event_log::export_ndjson(&pool, &mut w).await?;
                std::io::Write::flush(&mut w)
                    .with_context(|| format!("flushing {}", path.display()))?;
                n
            }
            None => {
                let stdout = std::io::stdout();
                let mut lock = stdout.lock();
                crucible_controller::event_log::export_ndjson(&pool, &mut lock).await?
            }
        };
        pool.close().await;
        Ok::<usize, anyhow::Error>(n)
    })?;
    if let Some(path) = &out {
        println!(
            "crucible-controller db export-events: {n} event(s) -> {}",
            path.display()
        );
    }
    Ok(())
}

/// `crucible-controller db adopt`: fold a foreign run's session log into the live ledger. The
/// stored `session_uri` is the original pointer (s3:// or local), never the downloaded temp path,
/// so the artifact proxy and Flow tab resolve evidence off it. Note: `db verify` gaps `s3://`
/// runs in its drift report (the comparison rebuild has no fetch bridge), so an adopted s3 run
/// showing there is expected noise, and the run's cost is booked against THIS controller's daily
/// caps — it is real spend.
fn dispatch_db_adopt(
    cfg: crucible_controller::ControllerCfg,
    session_uri: String,
    issue: Option<String>,
    run_id: Option<String>,
) -> Result<()> {
    let rt = cli_runtime()?;
    let (rid, outcome) = rt.block_on(async {
        let db = crucible_controller::Db::open(cfg.db_url()).await?;
        let scope_id = match &issue {
            Some(key) => {
                Some(crucible_controller::runs::ingest::adopt_scope_for_issue(&db, key).await?)
            }
            None => None,
        };
        // The temp-dir guard outlives the ingest read below.
        let (_guard, local_path) = if session_uri.starts_with("s3://") {
            let dir = tempfile::tempdir().context("adopt download scratch dir")?;
            let out = dir.path().join("session.jsonl");
            crucible_controller::runs::engine::fetch_object(&session_uri, &out).await?;
            (Some(dir), out)
        } else {
            let path = PathBuf::from(&session_uri);
            anyhow::ensure!(
                path.is_file(),
                "{} is not a file (pass an s3:// uri or a local session.jsonl path)",
                path.display()
            );
            (None, path)
        };
        let rid = match run_id {
            Some(id) => id,
            None => match &issue {
                Some(key) => crucible_controller::runs::model::new_run_id(key),
                None => format!("adopted-{}", jiff::Timestamp::now().as_second()),
            },
        };
        let outcome = crucible_controller::runs::ingest::adopt_session_file(
            &db,
            &rid,
            scope_id,
            &local_path,
            &session_uri,
        )
        .await?;
        Ok::<_, anyhow::Error>((rid, outcome))
    })?;
    println!(
        "crucible-controller db adopt: {rid} — status {}, best_score {}, cost {}, {} candidate(s){}",
        outcome.status,
        outcome
            .best_score
            .map_or_else(|| "-".to_string(), |s| format!("{s}")),
        outcome
            .cost_usd
            .map_or_else(|| "-".to_string(), |c| format!("${c:.2}")),
        outcome.candidates,
        if outcome.ledger_skipped {
            " (cost already ledgered; append skipped)"
        } else {
            ""
        }
    );
    Ok(())
}

/// `crucible-controller db verify`: when `cfg`'s resolved DB path already exists as a file, rebuild
/// it into a throwaway comparison database and print the drift (the periodic disaster-fallback
/// check). Otherwise — no existing DB at that path, and no reason to create one just to "verify"
/// it — fall back to the original behavior: open (creating if absent) a throwaway temp database,
/// run migrations, and print the applied schema version.
fn dispatch_db_verify(cfg: crucible_controller::ControllerCfg) -> Result<()> {
    use sqlx::migrate::MigrateDatabase;
    let rt = cli_runtime()?;
    let url = cfg.db_url().to_string();
    let exists = rt.block_on(sqlx::Postgres::database_exists(&url))?;
    if exists {
        let report = rt.block_on(async {
            let live = crucible_controller::Db::open(&url).await?;
            crucible_controller::verify(&live, &cfg).await
        })?;
        print_drift_report(crucible_controller::db_name(&url)?.as_str(), &report);
        return Ok(());
    }

    let scratch = crucible_controller::sibling_db_url(
        &url,
        &format!("crucible_db_verify_{}", std::process::id()),
    )?;
    let version = rt.block_on(async {
        let pool = crucible_controller::connect(&scratch).await?;
        let v = crucible_controller::schema_version(&pool).await?;
        pool.close().await;
        sqlx::Postgres::drop_database(&scratch).await?;
        Ok::<i64, anyhow::Error>(v)
    })?;
    println!("crucible-controller db verify: throwaway database — schema version {version}");
    Ok(())
}

fn print_drift_report(db_name: &str, report: &crucible_controller::DriftReport) {
    if report.is_clean() {
        println!("crucible-controller db verify: {db_name} — no drift");
    } else {
        println!(
            "crucible-controller db verify: {db_name} — {} drift line(s):",
            report.diffs.len()
        );
        for d in &report.diffs {
            println!(
                "  {}:{} {} live={:?} rebuilt={:?}",
                d.table, d.key, d.field, d.live, d.rebuilt
            );
        }
    }
    if !report.gaps.is_empty() {
        println!(
            "  {} known gap(s) in the comparison rebuild:",
            report.gaps.len()
        );
        for g in &report.gaps {
            println!("  {}:{} — {}", g.table, g.key, g.reason);
        }
    }
}

/// `crucible-controller db rebuild`: re-derive the ledger from evidence into a fresh sibling
/// database on the same server, then swap it in over the live one (`ALTER DATABASE … RENAME`)
/// only on full success. On any failure before the swap, the pre-existing live database is
/// untouched and the scratch database is dropped. The rename needs zero connections to either
/// database, so run this with the daemon stopped (it is the disaster-fallback path).
fn dispatch_db_rebuild(cfg: crucible_controller::ControllerCfg) -> Result<()> {
    use sqlx::migrate::MigrateDatabase;
    let live_url = cfg.db_url().to_string();
    let live_name = crucible_controller::db_name(&live_url)?;
    let tmp_name = format!("{live_name}_rebuild_tmp");
    let tmp_url = crucible_controller::sibling_db_url(&live_url, &tmp_name)?;
    let rt = cli_runtime()?;
    let result = rt.block_on(async {
        // A previous crashed rebuild may have left the scratch database behind; start clean.
        if sqlx::Postgres::database_exists(&tmp_url).await? {
            sqlx::Postgres::drop_database(&tmp_url).await?;
        }
        // Fence off a running daemon (or another maintenance command) for the whole read; the
        // swap below terminates the lock connection along with every other straggler.
        let live_lock = if sqlx::Postgres::database_exists(&live_url).await? {
            let live_pool = crucible_controller::connect(&live_url).await?;
            let lock = crucible_controller::try_maintenance_lock(&live_pool)
                .await?
                .context(
                    "the maintenance advisory lock is held by another session (a running daemon \
                     or another maintenance command); stop it and retry",
                )?;
            Some((live_pool, lock))
        } else {
            None
        };
        let tmp_db = crucible_controller::Db::open(&tmp_url).await?;
        let report = crucible_controller::rebuild(&tmp_db, &cfg).await;
        tmp_db.pool().close().await;
        if let Some((pool, lock)) = live_lock {
            let _ = lock.release().await;
            pool.close().await;
        }
        report
    });

    let report = match result {
        Ok(report) => report,
        Err(e) => {
            let _ = rt.block_on(sqlx::Postgres::drop_database(&tmp_url));
            return Err(
                e.context("crucible-controller db rebuild failed; the live ledger is untouched")
            );
        }
    };

    rt.block_on(swap_databases(&live_url, &live_name, &tmp_name))
        .context("swapping the rebuilt database into place")?;

    println!(
        "crucible-controller db rebuild: {live_name} — {} issue(s) upserted, {} run(s) ingested, {} candidate(s), {} event(s) replayed",
        report.issues_upserted,
        report.runs_ingested,
        report.candidates_ingested,
        report.events_replayed,
    );
    if !report.gaps.is_empty() {
        println!("  {} gap(s):", report.gaps.len());
        for g in &report.gaps {
            println!("  {}:{} — {}", g.table, g.key, g.reason);
        }
    }
    Ok(())
}

/// Swap `tmp_name` in over `live_name` via `ALTER DATABASE … RENAME` on an admin connection to
/// the server's `postgres` maintenance database (a rename can't run from inside either database).
/// Straggler connections are terminated first — this is the disaster-fallback path, run with the
/// daemon stopped. The old live database survives as `<live>_pre_rebuild` until the rename of the
/// scratch database succeeds, then it is dropped; a crash in between leaves both databases on the
/// server for hand recovery rather than a half-swapped state.
async fn swap_databases(live_url: &str, live_name: &str, tmp_name: &str) -> Result<()> {
    use sqlx::Connection;
    let admin_url = crucible_controller::sibling_db_url(live_url, "postgres")?;
    let mut conn = sqlx::postgres::PgConnection::connect(&admin_url)
        .await
        .context("connecting to the postgres maintenance database")?;
    let backup_name = format!("{live_name}_pre_rebuild");
    sqlx::query("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = ANY($1) AND pid <> pg_backend_pid()")
        .bind(vec![live_name.to_string(), tmp_name.to_string(), backup_name.clone()])
        .execute(&mut conn)
        .await
        .context("terminating straggler connections")?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS \"{backup_name}\""
    )))
    .execute(&mut conn)
    .await
    .context("dropping a leftover pre-rebuild backup")?;
    let live_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(live_name)
            .fetch_one(&mut conn)
            .await?;
    if live_exists {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER DATABASE \"{live_name}\" RENAME TO \"{backup_name}\""
        )))
        .execute(&mut conn)
        .await
        .context("setting the live database aside")?;
    }
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "ALTER DATABASE \"{tmp_name}\" RENAME TO \"{live_name}\""
    )))
    .execute(&mut conn)
    .await
    .context("renaming the rebuilt database into place")?;
    if live_exists {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE \"{backup_name}\""
        )))
        .execute(&mut conn)
        .await
        .context("dropping the pre-rebuild database")?;
    }
    Ok(())
}

/// `crucible-controller autopilot [--once]`. `--once` runs the reconcile core to quiescence
/// (the dev/test/break-glass mode). The resident daemon opens the ledger, fetches the startup
/// re-enqueue set, and hands [`crucible_controller::daemon::run`] the full wiring: the reconcile
/// core with the human-override drain in front, the three approval polls + upstream watermark as
/// discovery, the kube pod-completion watch, the scheduled drift check, and the HTTP API/UI on
/// the same runtime. Owns a multi-thread runtime — the daemon's `select!` and the queue's
/// spawned backoff timers need more than one worker thread.
fn dispatch_autopilot(mut cfg: crucible_controller::ControllerCfg, once: bool) -> Result<()> {
    // The GitHub App pack-PR credential (CONTROLLER_GITHUB_APP_*), threaded here so the daemon
    // and `--once` share it; a partially-set config fails loudly at startup, never per-PR.
    cfg.github_app = crucible_controller::secrets::github_app::GithubAppTokenSource::from_env()
        .context("configuring the GitHub App pack-PR credential")?;
    // `--once` runs the reconcile core to quiescence (the dev/test/break-glass mode);
    // the no-flag path is the resident daemon.
    if once {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build tokio runtime for autopilot --once")?;
        return rt.block_on(async {
            // Install the tracing subscriber inside the runtime (an OTLP layer, if enabled, wants
            // one); the guard flushes any batched spans on drop.
            let _telemetry = crucible_controller::telemetry::init(cfg.db_url());
            let _embedded = embedded_database(&mut cfg).await?;
            let pool = crucible_controller::connect(cfg.db_url()).await?;
            let lock = crucible_controller::try_maintenance_lock(&pool)
                .await?
                .context(
                    "the maintenance advisory lock is held by another session (a running daemon \
                     or a maintenance command); stop it and retry",
                )?;
            let result = crucible_controller::daemon::autopilot::run_once(&cfg).await;
            let _ = lock.release().await;
            pool.close().await;
            result
        });
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for autopilot")?;
    rt.block_on(run_autopilot_daemon(cfg))
}

/// Start the embedded Postgres when `DATABASE_URL=embedded` and point `cfg` at it. The returned
/// guard stops the server when the daemon lets go of it.
#[cfg(feature = "embedded-db")]
async fn embedded_database(
    cfg: &mut crucible_controller::ControllerCfg,
) -> Result<Option<crucible_controller::embedded_db::EmbeddedDb>> {
    if !cfg.wants_embedded_db() {
        return Ok(None);
    }
    let (server, url) = crucible_controller::embedded_db::EmbeddedDb::start(&cfg.state_dir).await?;
    cfg.db = url;
    Ok(Some(server))
}

#[cfg(not(feature = "embedded-db"))]
async fn embedded_database(cfg: &mut crucible_controller::ControllerCfg) -> Result<Option<()>> {
    cfg.validate_database()?;
    Ok(None)
}

async fn run_autopilot_daemon(mut cfg: crucible_controller::ControllerCfg) -> Result<()> {
    // Structured logging for the daemon: RUST_LOG-driven (default info), stderr, plus an OTLP trace
    // layer only when OTEL_EXPORTER_OTLP_ENDPOINT is set. The guard flushes batched spans on exit.
    let _telemetry = crucible_controller::telemetry::init(cfg.db_url());
    let _embedded = embedded_database(&mut cfg).await?;

    // Fail loud at startup if the grounded executor is misconfigured (a missing profile/sandbox for
    // `pod` mode), never silently per verdict — the exact failure the old env gate had.
    cfg.validate_autoresearch()?;
    cfg.validate_grounded()
        .context("validating the grounded-rank executor configuration")?;
    cfg.validate_scope()
        .context("validating the scope executor configuration")?;
    tracing::info!(
        autoresearch = cfg.autoresearch_enabled(),
        "autopilot: autoresearch lane"
    );

    // One metrics registry for the process, attached to the ledger before it's cloned into the
    // reconcile wiring + the HTTP surface, so every choke point re-exports what it ingests and
    // `/metrics` renders it.
    let metrics =
        crucible_controller::Metrics::new().context("building the controller metrics registry")?;
    let db = crucible_controller::Db::open(cfg.db_url())
        .await
        .context("opening the controller ledger")?
        .with_metrics(metrics.clone());

    #[cfg(feature = "autoresearch")]
    if cfg.autoresearch_enabled() {
        // The autopilot flag: one instance shared between the HTTP surface (POST flips it) and the
        // reconcile wiring (reads the AtomicBool cache per pass). Loaded once, cloned for serve().
        let autopilot = crucible_controller::AutopilotFlag::load(db.pool())
            .await
            .context("loading the autopilot flag")?;
        cfg.autopilot = Some(autopilot.clone());
        // A flip written through any replica reaches this process's AtomicBool within the poll
        // interval; the task dies with the process.
        tokio::spawn(
            autopilot
                .clone()
                .refresh_loop(std::time::Duration::from_secs(5)),
        );
    }

    // The runtime override store (Lane O2): defaults from the parsed cfg, live overrides from a
    // watched ConfigMap. Create-if-missing + an initial load so the first reconcile sees any
    // standing overrides, then thread the handle onto cfg (every clone shares the one cache) and
    // spawn the re-list poll loop.
    let config_store = crucible_controller::ConfigStore::new(
        cfg.base_config(),
        cfg.overrides_configmap.clone(),
        cfg.overrides_namespace(),
        std::sync::Arc::new(crucible_controller::KubeConfigMapApi),
        Some(metrics),
        Some(db.events().clone()),
    );
    let overrides_watched = kube::Config::infer().await.is_ok();
    if overrides_watched {
        if let Err(e) = config_store.ensure_configmap().await {
            tracing::warn!(error = %format!("{e:#}"), "autopilot: overrides create-if-missing failed");
        }
        config_store.reload_once().await;
    } else {
        tracing::info!(
            "no Kubernetes config to infer; runtime overrides are off and the parsed config stands"
        );
    }
    cfg.overrides = Some(config_store.clone());

    // The WorkPod dispatcher (grounded-rank turns + loop runs + future kinds): the real kube
    // dispatcher creates/watches/GCs the pod over the kube API. Installed process-wide because the
    // dispatch fires inside the pure reconcile step (a turn) and the reconcile-awaiting launch (a
    // run). Then sweep the ledger against the cluster so a restart wastes no paid work and leaks no
    // pod (adopt live pods, ingest completed-but-uncollected turns, converge failed runs, sweep
    // orphans). The sweep runs unconditionally — loop runs flow through the primitive regardless of
    // the grounded-rank executor mode.
    // One shared per-cluster client registry: the hub is the ambient in-cluster identity, every
    // spoke a kubeconfig under `clusters_dir`.
    let serve_vault = crucible_controller::secrets::vault::VaultClient::from_env()
        .context("building the hub's vault client")?
        .map(std::sync::Arc::new);
    if serve_vault.is_none() {
        tracing::info!("VAULT_ADDR unset — the secrets registry answers 503 on its write routes");
    }
    // The same client the registry writes through, plus the App a minted secret is issued from,
    // installed as the provider a dispatch reads the bound values with. Without either a scope
    // that binds nothing still launches, and one that binds something is refused rather than
    // started without it.
    cfg.secret_provider = crucible_controller::secrets::provider::RegistryProvider::installed(
        serve_vault.clone(),
        cfg.github_app.clone(),
    );
    let clusters =
        crucible_controller::runs::clusters::ClusterClients::new(cfg.clusters_dir.clone());
    // Personal dispatch targets are kubeconfig secrets, so they resolve only where the registry
    // has a Vault to read from. Without one they cannot be registered either, and the eligible set
    // is the shared clusters alone.
    let clusters = std::sync::Arc::new(match serve_vault.clone() {
        Some(vault) => clusters.with_personal(std::sync::Arc::new(
            crucible_controller::runs::dispatch_target::RegistryKubeconfigs::new(
                db.pool().clone(),
                vault,
            ),
        )),
        None => clusters,
    });

    let dispatcher = std::sync::Arc::new(crucible_controller::KubePodDispatcher::new(
        clusters.clone(),
    ));
    crucible_controller::install_dispatcher(dispatcher.clone());
    // The contract check only refuses launches, never serving, so it runs in the background: an
    // unreachable registry must not hold the API listener shut past the liveness probe's patience.
    // Whatever this sweep does not reach, the first dispatch that needs it reads lazily.
    let contracts = std::sync::Arc::new(
        crucible_controller::runs::contract::ContractRegistry::new(std::sync::Arc::new(
            crucible_controller::runs::contract::LiveContractReader::new(
                cfg.registry_authfile.clone(),
            ),
        )),
    );
    crucible_controller::install_contracts(contracts.clone());
    {
        let contracts = contracts.clone();
        let targets = crucible_controller::runs::contract::configured_targets(
            &cfg,
            crucible_controller::runs::engine::resolve_bin(),
        );
        tokio::spawn(async move {
            contracts.check_all(targets).await;
        });
    }
    #[cfg(feature = "autoresearch")]
    if cfg.autoresearch_enabled() {
        // The build backend, when `--build-backends` is set with creds: routes a declared build to the
        // forge cluster/github dispatcher. Off ⇒ the not-installed stub parks a `[build]` pack with a
        // clear reason rather than dispatching a build no one configured creds for.
        if let Some(backend) = crucible_controller::builds::lifecycle::forge_backend_from_cfg(&cfg)
        {
            tracing::info!("autopilot: build backends installed (cluster + github-actions)");
            crucible_controller::builds::lifecycle::install_build_backend(std::sync::Arc::new(
                backend,
            ));
        }
    }

    // The drift check runs far less often than discovery (a rebuild-and-diff is expensive) and only
    // needs to catch slow divergence; a fixed multiple of the discovery cadence keeps it a single
    // knob with no new profile field.
    let verify_interval = cfg
        .profile
        .discovery_interval()
        .saturating_mul(DRIFT_CHECK_CADENCE_MULTIPLE);
    // The discovery cadence re-reads the effective config each tick, so an override to
    // `discovery_secs` retunes the timer without a restart.
    let cadence_store = config_store.clone();
    // The manual reconcile trigger: `POST /api/reconcile` fires it, the daemon's select loop runs
    // a discovery sweep + full non-terminal re-enqueue.
    let reconcile_now = std::sync::Arc::new(tokio::sync::Notify::new());
    let daemon_cfg = crucible_controller::daemon::DaemonConfig {
        queue: crucible_controller::QueueConfig::default(),
        discovery_interval: cfg.effective().discovery_interval(),
        discovery_interval_fn: Some(std::sync::Arc::new(move || {
            cadence_store.effective().discovery_interval()
        })),
        verify_interval,
        reconcile_now: Some(reconcile_now.clone()),
    };

    // Ctrl-C flips the STOP flag; a poller translates it into the daemon's `Notify`-based shutdown
    // signal (no `signal` tokio feature needed for one flag).
    let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
    let shutdown_poller = shutdown.clone();
    tokio::spawn(async move {
        while !STOP.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        shutdown_poller.notify_waiters();
    });
    ctrlc::set_handler(|| STOP.store(true, Ordering::SeqCst))
        .context("installing the autopilot ctrl-c handler")?;

    // The overrides watch: re-list the ConfigMap on a fixed cadence, swap on a valid change, keep
    // last-good otherwise. Shares the daemon's shutdown signal so it stops cleanly.
    if overrides_watched {
        tokio::spawn(crucible_controller::daemon::overrides_store::watch_loop(
            config_store.clone(),
            std::time::Duration::from_secs(30),
            shutdown.clone(),
        ));
    }

    // One work queue shared between the worker and the override sink (human park/unpark/bump lands
    // in the same FIFO as the discovery/approval/pod-watch sources); the store carries the intent
    // the key alone can't.
    let override_store = std::sync::Arc::new(crucible_controller::OverrideStore::new());
    let queue = crucible_controller::WorkQueue::new();

    // The HTTP API + debug UI, mounted on the same runtime. Binds loopback without
    // CONTROLLER_API_TOKEN (the frozen default); a rendered Deployment sets the token + a real bind.
    let serve_sink: std::sync::Arc<dyn crucible_controller::OverrideSink> = std::sync::Arc::new(
        crucible_controller::QueueOverrideSink::new(override_store.clone(), queue.clone()),
    );
    let serve_addr = controller_api_addr();
    let serve_shutdown = shutdown.clone();
    let serve_turn_accounts = cfg
        .turn_accounts()
        .context("validating the per-cluster turn service accounts")?;
    crucible_controller::runs::dispatch_target::cluster_policy(&cfg)
        .context("validating the per-cluster dispatch policy")?;
    cfg.contract_clusters()
        .context("validating the per-contract dispatch routing")?;
    let serve_secure_cookies = cfg.session_secure_cookies;
    match crucible_controller::authz::bootstrap::seed_platform_administrators(
        db.pool(),
        &cfg.admins,
    )
    .await
    .context("seeding the platform administrators team")?
    {
        crucible_controller::authz::bootstrap::SeedOutcome::Reachable => {}
        crucible_controller::authz::bootstrap::SeedOutcome::Seeded { added } => {
            tracing::info!(
                added,
                "platform administrators team seeded from CONTROLLER_ADMINS"
            )
        }
        crucible_controller::authz::bootstrap::SeedOutcome::Unreachable => tracing::warn!(
            "the platform administrators team reaches no user and CONTROLLER_ADMINS names nobody"
        ),
    }
    match crucible_controller::authz::bootstrap::seed_platform_operators(
        db.pool(),
        &cfg.operators,
        &cfg.operator_groups,
    )
    .await
    .context("seeding the platform operators team")?
    {
        crucible_controller::authz::bootstrap::SeedOutcome::Reachable
        | crucible_controller::authz::bootstrap::SeedOutcome::Unreachable => {}
        crucible_controller::authz::bootstrap::SeedOutcome::Seeded { added } => {
            tracing::info!(
                added,
                "platform operators team seeded from CONTROLLER_OPERATORS"
            )
        }
    }
    match crucible_controller::authz::bootstrap::migrate_group_owners(db.pool()).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(count = n, "group-owned resources moved to their teams"),
        Err(e) => tracing::error!(
            error = %format!("{e:#}"),
            "the group owner migration failed; group owners stay until the next start"
        ),
    }
    let policy = crucible_controller::authz::policy::ActivePolicy::default_set()
        .context("loading the shipped default policy set")?;
    match crucible_controller::authz::bootstrap::load_active_policy(db.pool(), &policy)
        .await
        .context("loading the active policy set")?
    {
        crucible_controller::authz::bootstrap::PolicyOutcome::Stored { digest } => {
            tracing::info!(digest, "policy set in force")
        }
        crucible_controller::authz::bootstrap::PolicyOutcome::DefaultStored { digest } => {
            tracing::info!(digest, "default policy set stored and activated")
        }
        crucible_controller::authz::bootstrap::PolicyOutcome::DefaultUpgraded { from, digest } => {
            tracing::info!(
                from,
                digest,
                "earlier default policy set replaced by the shipped one"
            )
        }
        crucible_controller::authz::bootstrap::PolicyOutcome::Fallback { digest, error } => {
            tracing::error!(
                digest,
                error,
                "the active policy set no longer loads; the shipped default is in force"
            )
        }
    }
    let daemon_policy = policy.clone();
    let serve_state = crucible_controller::api::state::ApiState::new(
        db.clone(),
        serve_sink,
        std::sync::Arc::new(queue.clone()),
        clusters.clone(),
        Some(config_store.clone()),
        reconcile_now.clone(),
        contracts.clone(),
        policy,
        &cfg,
    )
    .with_vault(serve_vault);
    let images_refresh = serve_state.images_refresh();
    tokio::spawn(async move {
        tokio::select! {
            r = crucible_controller::serve(serve_state, serve_addr, serve_turn_accounts, serve_secure_cookies) => {
                if let Err(e) = r {
                    tracing::error!(error = %format!("{e:#}"), "autopilot: http surface exited");
                }
            }
            _ = serve_shutdown.notified() => {}
        }
    });

    // ADR-0049 §1: everything below writes the ledger, so it waits for leadership. A standby
    // stops here — serving the API and SPA above off the shared tables — until it wins the
    // lease and the advisory-lock fence. Shutdown while standing by is a clean exit.
    let Some(leadership) = crucible_controller::daemon::leader::campaign(
        crucible_controller::daemon::leader::LeaderConfig::from_env(),
        db.pool(),
        shutdown.clone(),
    )
    .await?
    else {
        return Ok(());
    };

    // Re-extract the params schema of every registered playbook whose stored engine pin is not
    // this binary's. Startup, not a discovery tick: the trigger is a new binary. The maintenance
    // advisory lock above is what keeps it single-writer.
    match crucible_controller::playbooks::registry::rederive_stale(db.pool()).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            count = n,
            "autopilot: playbook schemas re-derived for this engine revision"
        ),
        Err(e) => tracing::warn!(
            error = %format!("{e:#}"),
            "autopilot: playbook schema re-derivation failed; stored schemas stand"
        ),
    }
    // Fill in the agent substrate of any stored pack that predates the columns recording it, so a
    // launch reads what the pack declares rather than refusing for want of a stamp.
    match crucible_controller::playbooks::dispatch::backfill_pack_agents(db.pool()).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(count = n, "autopilot: pack agent substrate backfilled"),
        Err(e) => tracing::warn!(
            error = %format!("{e:#}"),
            "autopilot: pack agent backfill failed; unstamped packs cannot launch"
        ),
    }
    #[cfg(feature = "autoresearch")]
    if cfg.autoresearch_enabled() {
        // Seed the runtime repo watch-set (Lane O3) from the boot-time env config: idempotent, and
        // never un-watches or overwrites a row an admin already added/paused/unwatched. Discovery
        // itself never reads `cfg.repos` again after this — see `Db::watched_repos`.
        crucible_controller::issues::repo_watch::seed_watched_repos(db.pool(), &cfg.repos)
            .await
            .context("seeding the watched-repo set from CONTROLLER_WATCHED_REPOS")?;
    }
    // Recover the dispatch location of runs written before `runs.cluster` existed, so the live
    // relay stops asking the hub about pods that only ever existed on a spoke. Needs the cluster
    // registry above to ask each spoke for its namespace, so it runs here rather than beside the
    // other startup backfills; the maintenance lock held for the daemon's lifetime still covers it.
    match crucible_controller::playbooks::dispatch::backfill_run_locations(
        db.pool(),
        &clusters,
        &cfg.pod_namespace,
    )
    .await
    {
        Ok(0) => {}
        Ok(n) => tracing::info!(count = n, "autopilot: run dispatch locations backfilled"),
        Err(e) => tracing::warn!(
            error = %format!("{e:#}"),
            "autopilot: run location backfill failed; those runs still read as hub"
        ),
    }
    // Unparking writes the ledger, so it re-checks after the sweep under the fence.
    {
        let contracts = contracts.clone();
        let db = db.clone();
        tokio::spawn(async move {
            match crucible_controller::runs::contract::release_parked(&db, &contracts).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(
                    released = n,
                    "contract check: unparked issues whose image now matches"
                ),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "contract check: releasing parked issues failed")
                }
            }
        });
    }
    crucible_controller::reconcile_on_startup(
        &db,
        dispatcher.as_ref(),
        &cfg.pod_namespace,
        cfg.effective().failed_pod_keep,
    )
    .await;
    #[cfg(feature = "autoresearch")]
    if cfg.autoresearch_enabled() {
        // Re-adopt in-flight builds by listing Jobs/runs (never from memory): a build whose Job/run
        // vanished during downtime resolves from the registry in one pass instead of waiting out its
        // full timeout. Best-effort — a failed sweep only delays that resolution to the next poll.
        if let Err(e) = crucible_controller::builds::lifecycle::adopt_builds(&db, &cfg).await {
            tracing::warn!(
                error = %format!("{e:#}"),
                "autopilot: build startup adoption failed, the next reconcile poll retries"
            );
        }
    }
    // Settle the local-mode runs a restart orphaned before anything re-drives them: the
    // supervisor died with the last process, so their launches would sit at `running` forever.
    if let Err(e) = crucible_controller::runs::local_run::adopt_orphans(&db, &cfg).await {
        tracing::warn!(
            error = %format!("{e:#}"),
            "autopilot: local run startup adoption failed"
        );
    }
    let keys = crucible_controller::issues::store::non_terminal_keys(db.pool()).await?;
    tracing::info!(
        count = keys.len(),
        "autopilot: non-terminal issues re-enqueued at startup"
    );
    #[cfg(feature = "autoresearch")]
    if cfg.autoresearch_enabled() {
        // Backfill `upstream_updated_at` for rows ingested before the column existed, so the rank
        // horizon can gate on it. Spawned (a rate-limited GitHub must not stall boot) and repeated by
        // the discovery cycle while NULL rows remain, so a failure here only delays the stamp.
        let backfill_db = db.clone();
        tokio::spawn(async move {
            if let Err(e) =
                crucible_controller::issues::triage::backfill_upstream_updated_at(&backfill_db)
                    .await
            {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "autopilot: upstream_updated_at backfill failed, the next discovery cycle retries"
                );
            }
        });
    }
    // The MLflow exporter: a controller-only background task that pushes each folded run's traces
    // and metrics to a per-deployment MLflow, off unless CONTROLLER_MLFLOW_TRACKING_URI is set.
    // Post-fold, allow-failure — a failed export marks its bookkeeping row and retries.
    if let Some(mlflow_cfg) = crucible_controller::runs::mlflow::MlflowConfig::from_env() {
        tokio::spawn(crucible_controller::runs::mlflow::export_loop(
            db.clone(),
            mlflow_cfg,
            shutdown.clone(),
        ));
    }
    // The image catalog watcher: sweeps the configured repositories on an interval and on
    // `POST /api/images/refresh`; off when no repository is configured.
    if !cfg.image_catalog_repos.is_empty() {
        let reader = std::sync::Arc::new(
            crucible_controller::images::registry::LiveRegistryReader::new(
                cfg.registry_authfile.clone(),
            ),
        );
        tokio::spawn(crucible_controller::images::sweep::watch_loop(
            db.clone(),
            reader,
            crucible_controller::images::sweep::CatalogConfig {
                repositories: cfg.image_catalog_repos.clone(),
                interval: std::time::Duration::from_secs(cfg.image_catalog_interval_secs),
            },
            images_refresh.clone(),
            shutdown.clone(),
        ));
    }
    // The pod-completion edge: a namespaced watch over the managed-by selector on every connected
    // cluster, mapped to the finished pods' issue keys. Enqueued like any other source; reconcile of
    // a `running` row folds the run in.
    let completions =
        crucible_controller::daemon::kube_completion_stream(clusters.clone(), &cfg.pod_namespace);

    // Everything else — reconcile core + override drain, machine park, the approval polls, the drift
    // check — is the library's one production assembly (shared with the integration harness).
    let wiring = crucible_controller::daemon::assemble(
        &db,
        &cfg,
        queue,
        override_store,
        completions,
        daemon_policy,
    );
    crucible_controller::daemon::run_led(keys, daemon_cfg, wiring, shutdown, None, Some(leadership))
        .await
}

/// Drift check cadence as a multiple of the discovery interval — the rebuild-and-diff is expensive
/// and only needs to catch slow divergence, so it runs an order of magnitude less often.
const DRIFT_CHECK_CADENCE_MULTIPLE: u32 = 12;

/// The controller HTTP surface's bind address: `CONTROLLER_API_ADDR` (host:port) if set, else
/// loopback on the default port. `serve` forces loopback anyway when no API token is configured.
fn controller_api_addr() -> std::net::SocketAddr {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    std::env::var("CONTROLLER_API_ADDR")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), CONTROLLER_API_PORT))
}

/// The default controller HTTP surface port (the rendered Service targets it cluster-internally).
const CONTROLLER_API_PORT: u16 = 8870;

#[cfg(test)]
mod tests {
    #[cfg(feature = "autoresearch")]
    use crate::dispatch_triage;
    #[cfg(feature = "autoresearch")]
    use crate::{Cli, dispatch_db_rebuild};
    #[cfg(feature = "autoresearch")]
    use clap::Parser;
    #[cfg(feature = "autoresearch")]
    use std::fs;
    #[cfg(feature = "autoresearch")]
    use std::path::PathBuf;

    /// Serializes the tests below that point the process-global `GITHUB_API_URL` at a local
    /// listener — two such tests would otherwise race on the one environ.
    #[cfg(feature = "autoresearch")]
    fn github_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(feature = "autoresearch")]
    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crucible-controller-bin-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).expect("mkdir tmp");
        dir
    }

    #[cfg(feature = "autoresearch")]
    fn controller_cfg_from(argv: &[&str]) -> crucible_controller::ControllerCfg {
        #[derive(clap::Parser)]
        struct Harness {
            #[command(flatten)]
            cfg: crucible_controller::ControllerCfg,
        }
        Harness::parse_from(argv).cfg
    }

    /// A unique ledger URL on the test server, so tests never write into the `DATABASE_URL`
    /// database itself (that's the sqlx-prepare schema database).
    #[cfg(feature = "autoresearch")]
    fn test_ledger_url(name: &str) -> String {
        let base =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must point at the test server");
        crucible_controller::sibling_db_url(
            &base,
            &format!("crucible_bin_{name}_{}", std::process::id()),
        )
        .expect("deriving a test ledger URL")
    }

    /// `crucible-controller triage --full` parses to `Cmd::Triage { full: true, .. }` (the
    /// backfill/repair flag threaded from clap into [`dispatch_triage`]); omitting it defaults to
    /// `false` — a normal sweep never accidentally pays for a full resync.
    #[cfg(feature = "autoresearch")]
    #[test]
    fn triage_full_flag_parses_and_defaults_to_false() {
        let cli = Cli::parse_from([
            "crucible-controller",
            "triage",
            "--repo",
            "owner/repo",
            "--full",
        ]);
        match cli.command {
            crate::Cmd::Triage { full, .. } => assert!(full),
            _ => panic!("expected Cmd::Triage"),
        }

        let cli = Cli::parse_from(["crucible-controller", "triage", "--repo", "owner/repo"]);
        match cli.command {
            crate::Cmd::Triage { full, .. } => {
                assert!(!full, "--full must default to false")
            }
            _ => panic!("expected Cmd::Triage"),
        }
    }

    #[cfg(feature = "autoresearch")]
    #[test]
    fn dispatch_triage_requires_at_least_one_watched_repo() {
        let dir = tempdir("triage-no-repos");
        let cfg = controller_cfg_from(&["ctl", "--state-dir", &dir.to_string_lossy()]);
        let err = dispatch_triage(cfg, false)
            .expect_err("no --repo must be an error, not a silent no-op");
        assert!(err.to_string().contains("watched repo"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A one-shot listener standing in for api.github.com: drains the request headers and answers
    /// `200 OK` with `body`.
    #[cfg(feature = "autoresearch")]
    fn stub_github(body: &'static str) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let mut read = 0;
            while !buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf[read..]).expect("read request");
                if n == 0 {
                    break;
                }
                read += n;
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).expect("write response");
        });
        (addr, server)
    }

    /// `crucible-controller triage` end to end against a real listener standing in for
    /// api.github.com: fetch, upsert, and confirm the row lands in the ledger this CLI path opens —
    /// with no tier (triage is pure discovery, tiering is the ranker's job alone at reconcile time).
    #[cfg(feature = "autoresearch")]
    #[test]
    fn dispatch_triage_fetches_and_upserts_into_the_ledger() {
        let _guard = github_env_lock();

        let dir = tempdir("triage-e2e");
        let (addr, server) = stub_github(
            r#"[{"number": 9, "title": "panic on empty header", "body": "steps to reproduce: send an empty header", "labels": [{"name": "bug"}], "html_url": "https://github.com/owner/repo/issues/9", "updated_at": "2026-07-01T00:00:00Z", "state": "open"}]"#,
        );

        unsafe {
            std::env::set_var("GITHUB_API_URL", format!("http://{addr}"));
        }
        let db_url = test_ledger_url("triage_e2e");
        let cfg = controller_cfg_from(&[
            "ctl",
            "--state-dir",
            &dir.to_string_lossy(),
            "--repo",
            "owner/repo",
            "--db",
            &db_url,
        ]);
        dispatch_triage(cfg, false).expect("triage ok");
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }
        server.join().expect("server thread");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let issue = rt
            .block_on(async {
                let db = crucible_controller::Db::open(&db_url).await?;
                crucible_controller::issues::store::get_issue(db.pool(), "owner/repo#9").await
            })
            .expect("db query ok")
            .expect("issue tracked");
        assert!(
            issue.tier.is_none(),
            "triage is pure discovery: no tier guess"
        );
        assert_eq!(issue.status, crucible_controller::Status::New);

        let _ = fs::remove_dir_all(&dir);
    }

    /// `crucible-controller db rebuild`: a GitHub fetch that fails outright must abort the
    /// whole rebuild before ever touching the live ledger. Forces the failure with a listener that
    /// answers 200 with an undecodable body (no retries — an immediate hard error, unlike a
    /// per-run evidence gap), then asserts the live database's rows are untouched and no
    /// `_rebuild_tmp` scratch database is left behind.
    #[cfg(feature = "autoresearch")]
    #[test]
    fn dispatch_db_rebuild_leaves_the_live_ledger_untouched_on_failure() {
        let _guard = github_env_lock();

        let dir = tempdir("rebuild-atomic-failure");
        let db_url = test_ledger_url("rebuild_atomic");
        let cfg = controller_cfg_from(&[
            "ctl",
            "--state-dir",
            &dir.to_string_lossy(),
            "--repo",
            "owner/repo",
            "--db",
            &db_url,
        ]);

        // Seed a real live ledger with one row.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let before = rt
            .block_on(async {
                let db = crucible_controller::Db::open(cfg.db_url()).await?;
                // rebuild reads the live ledger's watch-set (Lane O3), not `cfg.repos` — seed it
                // the way a real boot would.
                crucible_controller::issues::repo_watch::seed_watched_repos(db.pool(), &cfg.repos)
                    .await?;
                crucible_controller::issues::store::upsert_issue(
                    db.pool(),
                    &crucible_controller::NewIssue {
                        key: "owner/repo#1".to_string(),
                        repo: "owner/repo".to_string(),
                        priority: 0,
                        evidence_url: None,
                        title: None,
                        author: None,
                        body: None,
                        labels: Vec::new(),
                        upstream_updated_at: None,
                    },
                )
                .await?;
                let issue =
                    crucible_controller::issues::store::get_issue(db.pool(), "owner/repo#1")
                        .await?
                        .expect("seeded issue");
                db.pool().close().await;
                anyhow::Ok(issue)
            })
            .expect("seed live ledger");

        let (addr, server) = stub_github("not json");
        unsafe {
            std::env::set_var("GITHUB_API_URL", format!("http://{addr}"));
        }

        let err = dispatch_db_rebuild(cfg.clone()).expect_err("a GitHub decode failure must abort");
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }
        server.join().expect("server thread");
        assert!(
            err.to_string().contains("untouched"),
            "error should say the live ledger was left alone: {err:#}"
        );

        let live_name = crucible_controller::db_name(&db_url).expect("db name");
        let tmp_url =
            crucible_controller::sibling_db_url(&db_url, &format!("{live_name}_rebuild_tmp"))
                .expect("tmp url");
        rt.block_on(async {
            use sqlx::migrate::MigrateDatabase;
            let db = crucible_controller::Db::open(&db_url).await?;
            let after = crucible_controller::issues::store::get_issue(db.pool(), "owner/repo#1")
                .await?
                .expect("issue still present");
            assert_eq!(before, after, "the live ledger's row must be untouched");
            db.pool().close().await;
            assert!(
                !sqlx::Postgres::database_exists(&tmp_url).await?,
                "no leftover rebuild scratch database"
            );
            sqlx::Postgres::drop_database(&db_url).await?;
            anyhow::Ok(())
        })
        .expect("post-rebuild assertions");

        let _ = fs::remove_dir_all(&dir);
    }
}
