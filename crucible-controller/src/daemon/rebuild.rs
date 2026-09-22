//! rebuild-from-evidence path: `crucible-controller db rebuild` re-derives the ledger from GitHub
//! issue state, ingestible session logs, and the event log's transition history — and the periodic
//! drift check ([`verify`]) rebuilds into a throwaway database and diffs it against the live one,
//! so the "the DB is a rebuildable index" claim is exercised, not just asserted.
//!
//! ## What v1 can and can't reconstruct (read before trusting a `RebuildReport`)
//!
//! - **GitHub issue state** is reconstructable *except its tier*: [`triage::list_changed_issues`]
//!   with `since: None`, upserted the same way `triage_repo` does — tier is never among it,
//!   because tiering is the ranker's job alone, and replaying it would mean
//!   re-spending on every rebuilt issue (or worse, guessing). A rebuilt row's tier is `NULL`;
//!   [`issue_fields`] excludes `tier` from the live/rebuilt drift diff for exactly this reason —
//!   it is a known, permanent gap, not a finding. A repo this crate can't reach is a hard failure
//!   for the whole rebuild — without it there is nothing to build an issue's row on, unlike a
//!   single run's missing session log.
//! - **Scopes are not reconstructable in v1.** The `scopes` table has no on-disk path column —
//!   only `pack_digest` / `check_outcome` / the approval-PR fields — so there is nothing on disk
//!   this crate can point at to redraft a `scopes` row from scratch. Any issue the *live* ledger
//!   shows past `new` (which only a scope turn could have produced) becomes a
//!   [`GapReason::ScopePackPathUnavailable`] gap instead of a fabricated row.
//! - **Runs with a stored (`db://run-session/…`) or local `session_uri`** are re-ingested through
//!   [`ingest::ingest_session`]
//!   exactly as the pull-ingest path would. **Runs with an `s3://` `session_uri` are gapped**
//!   ([`GapReason::S3EvidenceUnavailable`]): this crate has no S3 client, and adding one would
//!   cross the async-boundary-stops-here line the work plan draws — S3 fetch lives in the engine
//!   binary's `publish.rs`, reached only via subprocess. A future iteration can bridge that gap;
//!   v1 just names it instead of quietly fabricating a row or aborting the whole rebuild.
//! - **Everything else** (`awaiting-approval`, `running`, `pr-open`, `done`, `parked` — and a
//!   park's reason) comes from replaying the live ledger's `events` table. `parked_by`
//!   isn't part of the event row's frozen shape, so a replayed park always lands `machine`; see
//!   the module-level note on [`replay_events`].
//!
//! Because "runs" and "scopes" gaps are read off the *live* ledger (the one place that still
//! remembers a run/scope existed at all), rebuild takes `cfg` — which carries the live DB's own
//! URL — and opens it **read-only in spirit** (never written to) alongside the fresh `into`
//! database it is populating. A from-nothing disaster recovery (the live database itself is
//! gone from the server) can only ever recover the GitHub-derived + event-log-derived slice;
//! that is the real limit of "rebuildable index" today, not a bug in this module.

use crate::client::Db;
use crate::config::ControllerCfg;
use crate::event_log::{Event, EventLog};
use crate::issues::model::{Issue, NewIssue};
use crate::issues::triage;
use crate::model::{ParkReason, ParkedBy, Status};
use crate::runs::ingest;
use crate::runs::model::{Candidate, Run};
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::Path;

/// Why one row (or one table, wholesale) couldn't be reconstructed from evidence. A strongly-typed
/// tag rather than a bare string, per the crate's house style — see the module doc for what each
/// reason means and why it isn't a hard failure.
#[derive(Debug, Clone, PartialEq)]
pub enum GapReason {
    /// This issue's status implies a scope turn happened, but `scopes` has no on-disk pointer to
    /// redraft the row from (see the module doc).
    ScopePackPathUnavailable,
    /// The run's evidence is an `s3://` URI; this crate has no S3 client (async-boundary rule).
    S3EvidenceUnavailable,
    /// The run has no `session_uri` at all to re-ingest from.
    NoSessionEvidence,
    /// A local `session_uri` doesn't resolve to a readable file, or failed to parse. Carries the
    /// underlying detail for triage.
    SessionLogUnreadable(String),
}

impl std::fmt::Display for GapReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GapReason::ScopePackPathUnavailable => {
                write!(f, "scope pack has no on-disk pointer to reconstruct from")
            }
            GapReason::S3EvidenceUnavailable => {
                write!(f, "s3:// session evidence unreachable from this crate")
            }
            GapReason::NoSessionEvidence => write!(f, "no session_uri recorded for this run"),
            GapReason::SessionLogUnreadable(detail) => {
                write!(f, "session log unreadable: {detail}")
            }
        }
    }
}

/// One row (or row-shaped table entry) rebuild could not honestly reconstruct.
#[derive(Debug, Clone, PartialEq)]
pub struct Gap {
    pub table: &'static str,
    pub key: String,
    pub reason: GapReason,
}

/// The outcome of one [`rebuild`] pass: how much evidence was folded back in, and what it couldn't
/// reach.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RebuildReport {
    pub issues_upserted: usize,
    pub runs_ingested: usize,
    pub candidates_ingested: usize,
    pub events_replayed: usize,
    pub gaps: Vec<Gap>,
}

/// One live-vs-rebuilt disagreement: `table`+`key` name the row, `field` names the column (or
/// `<row>` when the whole row is present on only one side), and `live`/`rebuilt` carry the two
/// values as strings (`None` when the row is absent on that side).
#[derive(Debug, Clone, PartialEq)]
pub struct Drift {
    pub table: &'static str,
    pub key: String,
    pub field: String,
    pub live: Option<String>,
    pub rebuilt: Option<String>,
}

/// The outcome of one [`verify`] pass: every field-level disagreement found, plus the gaps the
/// comparison rebuild itself hit (a gapped row is reported here, not manufactured as spurious
/// "missing" drift — see [`verify`]'s doc).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DriftReport {
    pub diffs: Vec<Drift>,
    pub gaps: Vec<Gap>,
}

impl DriftReport {
    pub fn is_clean(&self) -> bool {
        self.diffs.is_empty()
    }
}

/// What the rebuilt database is for, which decides whether the artifact payloads and the verbatim
/// event history travel into it. A [`RebuildMode::Swap`] rebuild replaces the live database, so
/// everything must come along or the swap destroys it; a [`RebuildMode::Verify`] rebuild is a
/// throwaway comparison whose drift diff only reads row metadata — copying gigabytes of chunks and
/// history into it on every scheduled verify tick would be pure I/O cost.
#[derive(Clone, Copy, PartialEq)]
enum RebuildMode {
    Swap,
    Verify,
}

/// Re-derive `into` from evidence: GitHub issue state, re-ingestible session logs, and the event
/// log's transition history. See the module doc for exactly what each source can and can't
/// reconstruct. A GitHub fetch failure aborts the whole rebuild (there is no issue row to build
/// anything else on top of); a single run's unreachable evidence does not — it becomes a [`Gap`]
/// and the rest of the rebuild continues. This is the swap-bound rebuild (`db rebuild`); the
/// scheduled drift check goes through [`verify`], whose comparison rebuild skips the payload and
/// history copies.
pub async fn rebuild(into: &Db, cfg: &ControllerCfg) -> Result<RebuildReport> {
    rebuild_mode(into, cfg, RebuildMode::Swap).await
}

async fn rebuild_mode(into: &Db, cfg: &ControllerCfg, mode: RebuildMode) -> Result<RebuildReport> {
    let mut report = RebuildReport::default();

    // Which repos to rebuild: the live ledger's watch-set (Lane O3 — the same DB the "scopes +
    // runs" pass below reads its evidence pointers from), so a repo added via `POST /api/repos`
    // since the last boot is still rebuilt. Only a from-nothing disaster (the live database itself
    // gone) falls back to `cfg.repos`, the boot-seed list — the sole surviving hint of which repos
    // mattered.
    //
    // Triage never ingests a closed-and-untracked issue, so rebuild mustn't either — otherwise
    // every rebuilt DB grows the repo's closed history back and the drift check cries wolf. The
    // live ledger's key set is the "tracked" side of that rule; a from-nothing disaster (no live
    // DB) skips closed issues wholesale, losing only their retired-park rows.
    let (repos, live_keys): (Vec<String>, Option<BTreeSet<String>>) =
        if crate::client::ledger_exists(cfg.db_url()).await? {
            let live_pool = crate::client::connect(cfg.db_url())
                .await
                .context("rebuild: opening the live ledger to read its watch-set and issue keys")?;
            let repos = crate::issues::repo_watch::watched_repos(&live_pool).await?;
            let keys = crate::daemon::store::list_all_issues(&live_pool)
                .await?
                .into_iter()
                .map(|i| i.key)
                .collect();
            live_pool.close().await;
            (repos, Some(keys))
        } else {
            (cfg.repos.clone(), None)
        };

    // --- GitHub issue state -------------------------------------------------------------------
    for repo in &repos {
        let issues = triage::list_changed_issues(repo, None)
            .await
            .with_context(|| format!("rebuild: fetching {repo}'s issues from GitHub"))?;
        let mut newest: Option<String> =
            crate::issues::store::get_watermark(into.pool(), repo).await?;
        for issue in &issues {
            let key = format!("{repo}#{}", issue.number);
            if !issue.state.eq_ignore_ascii_case("open")
                && !live_keys.as_ref().is_some_and(|k| k.contains(&key))
            {
                // The watermark still advances past skipped issues, same as triage.
                if newest
                    .as_deref()
                    .is_none_or(|n| issue.updated_at.as_str() > n)
                {
                    newest = Some(issue.updated_at.clone());
                }
                continue;
            }
            crate::issues::store::upsert_issue(
                into.pool(),
                &NewIssue {
                    key,
                    repo: repo.clone(),
                    priority: 0,
                    evidence_url: Some(issue.html_url.clone()),
                    title: Some(issue.title.clone()),
                    author: issue.author.clone(),
                    body: issue.body.clone(),
                    labels: issue.labels.clone(),
                    upstream_updated_at: Some(issue.updated_at.clone()),
                },
            )
            .await?;
            report.issues_upserted += 1;
            if newest
                .as_deref()
                .is_none_or(|n| issue.updated_at.as_str() > n)
            {
                newest = Some(issue.updated_at.clone());
            }
        }
        if let Some(w) = newest {
            crate::issues::store::set_watermark(into.pool(), repo, &w).await?;
        }
    }

    // --- Scopes + runs, read off the live ledger's own pointers -----------------------------
    // The live DB is the only place that still remembers a scope/run existed; rebuild reads it
    // (never writes it) purely as a work list of evidence pointers to re-derive from scratch. A
    // from-nothing disaster (the live database itself gone) can only ever recover the GitHub- and
    // event-log-derived slice above and below — see the module doc.
    if crate::client::ledger_exists(cfg.db_url()).await? {
        let live_pool = crate::client::connect(cfg.db_url())
            .await
            .context("rebuild: opening the live ledger to read its evidence pointers")?;

        for issue in crate::daemon::store::list_all_issues(&live_pool).await? {
            if issue.status != Status::New {
                report.gaps.push(Gap {
                    table: "scopes",
                    key: issue.key.clone(),
                    reason: GapReason::ScopePackPathUnavailable,
                });
            }
        }

        for run in crate::daemon::store::list_all_runs(&live_pool).await? {
            match ingest_one_run(into, &live_pool, &run).await {
                Ok(Some(parsed_candidates)) => {
                    report.runs_ingested += 1;
                    report.candidates_ingested += parsed_candidates;
                }
                Ok(None) => {} // gap already pushed by ingest_one_run
                Err(reason) => report.gaps.push(Gap {
                    table: "runs",
                    key: run.run_id.clone(),
                    reason,
                }),
            }
        }
        // The artifact store IS the evidence now; `db rebuild` swaps the rebuilt database in over
        // the live one, so the payloads have to travel with it or the swap destroys them — the
        // same verbatim-copy rule the events table follows. A verify rebuild is thrown away and
        // its diff never reads the payloads, so it skips the copy.
        if mode == RebuildMode::Swap {
            copy_artifact_store(&live_pool, into)
                .await
                .context("rebuild: copying the artifact store")?;
        }
        live_pool.close().await;
    }

    // --- Event log replay ----------------------------------------------------------------------
    report.events_replayed = replay_events(into, cfg, mode).await?;

    Ok(report)
}

/// Copy the artifact payloads and their pointer rows verbatim into the rebuilt DB: `artifacts` +
/// `artifact_chunks` (ids preserved, chunks moved one row at a time so a 128 MiB session never
/// materializes whole), `pod_artifacts`, `pack_tarballs`, and `pack_steering`. Runs against a
/// freshly-migrated empty target.
async fn copy_artifact_store(live: &sqlx::PgPool, into: &Db) -> Result<()> {
    use sqlx::Row as _;
    let arts = sqlx::query(
        "SELECT id, owner_kind, owner_id, kind, digest, bytes, created_at \
         FROM artifacts ORDER BY id",
    )
    .fetch_all(live)
    .await?;
    for a in &arts {
        let id: i64 = a.get("id");
        sqlx::query(
            "INSERT INTO artifacts (id, owner_kind, owner_id, kind, digest, bytes, created_at) \
             OVERRIDING SYSTEM VALUE VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id)
        .bind(a.get::<String, _>("owner_kind"))
        .bind(a.get::<String, _>("owner_id"))
        .bind(a.get::<String, _>("kind"))
        .bind(a.get::<String, _>("digest"))
        .bind(a.get::<i64, _>("bytes"))
        .bind(a.get::<String, _>("created_at"))
        .execute(into.pool())
        .await?;
        let mut seq: i64 = 0;
        loop {
            let chunk =
                sqlx::query("SELECT data FROM artifact_chunks WHERE artifact_id = $1 AND seq = $2")
                    .bind(id)
                    .bind(seq)
                    .fetch_optional(live)
                    .await?;
            let Some(chunk) = chunk else { break };
            sqlx::query("INSERT INTO artifact_chunks (artifact_id, seq, data) VALUES ($1, $2, $3)")
                .bind(id)
                .bind(seq)
                .bind(chunk.get::<Vec<u8>, _>("data"))
                .execute(into.pool())
                .await?;
            seq += 1;
        }
    }
    sqlx::query(
        "SELECT setval(pg_get_serial_sequence('artifacts', 'id'), \
                COALESCE(MAX(id), 1), MAX(id) IS NOT NULL) FROM artifacts",
    )
    .execute(into.pool())
    .await?;
    let pointers = sqlx::query("SELECT pod, kind, digest, bytes, created_at FROM pod_artifacts")
        .fetch_all(live)
        .await?;
    for p in &pointers {
        sqlx::query(
            "INSERT INTO pod_artifacts (pod, kind, digest, bytes, created_at) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (pod, kind) DO NOTHING",
        )
        .bind(p.get::<String, _>("pod"))
        .bind(p.get::<String, _>("kind"))
        .bind(p.get::<String, _>("digest"))
        .bind(p.get::<i64, _>("bytes"))
        .bind(p.get::<String, _>("created_at"))
        .execute(into.pool())
        .await?;
    }
    let packs =
        sqlx::query("SELECT issue_slug, tar_gz, digest, bytes, created_at FROM pack_tarballs")
            .fetch_all(live)
            .await?;
    for p in &packs {
        sqlx::query(
            "INSERT INTO pack_tarballs (issue_slug, tar_gz, digest, bytes, created_at) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (issue_slug) DO NOTHING",
        )
        .bind(p.get::<String, _>("issue_slug"))
        .bind(p.get::<Vec<u8>, _>("tar_gz"))
        .bind(p.get::<String, _>("digest"))
        .bind(p.get::<i64, _>("bytes"))
        .bind(p.get::<String, _>("created_at"))
        .execute(into.pool())
        .await?;
    }
    let steering =
        sqlx::query("SELECT issue_slug, seq, body_md, author, created_at FROM pack_steering")
            .fetch_all(live)
            .await?;
    for s in &steering {
        sqlx::query(
            "INSERT INTO pack_steering (issue_slug, seq, body_md, author, created_at) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (issue_slug, seq) DO NOTHING",
        )
        .bind(s.get::<String, _>("issue_slug"))
        .bind(s.get::<i64, _>("seq"))
        .bind(s.get::<String, _>("body_md"))
        .bind(s.get::<Option<String>, _>("author"))
        .bind(s.get::<String, _>("created_at"))
        .execute(into.pool())
        .await?;
    }
    Ok(())
}

/// Re-ingest one `runs` row's session log. `Ok(Some(n))` on success (`n` candidates folded);
/// `Ok(None)` when the row was gapped (pushed straight onto the caller's report — no `scope`
/// pointer is ever passed through, since scopes aren't reconstructed in v1); `Err(reason)` when the
/// caller should record the gap itself (kept separate so the caller can attach `run.run_id`).
async fn ingest_one_run(
    into: &Db,
    live: &sqlx::PgPool,
    run: &Run,
) -> Result<Option<usize>, GapReason> {
    let Some(uri) = &run.session_uri else {
        return Err(GapReason::NoSessionEvidence);
    };
    if uri.starts_with("s3://") {
        return Err(GapReason::S3EvidenceUnavailable);
    }
    let content = if let Some(run_id) = crate::runs::blob_store::run_id_of_session_uri(uri) {
        crate::runs::blob_store::get_run_session(live, run_id)
            .await
            .map_err(|e| GapReason::SessionLogUnreadable(format!("{e:#}")))?
            .ok_or_else(|| {
                GapReason::SessionLogUnreadable(format!("{uri} names no stored artifact"))
            })?
    } else {
        let path = Path::new(uri);
        if !path.is_file() {
            return Err(GapReason::SessionLogUnreadable(format!(
                "{} does not exist",
                path.display()
            )));
        }
        std::fs::read_to_string(path)
            .map_err(|e| GapReason::SessionLogUnreadable(format!("{}: {e}", path.display())))?
    };
    let parsed = ingest::ingest_session(
        into,
        &ingest::IngestTarget {
            run_id: &run.run_id,
            scope_id: None,
            issue: run.issue.as_deref(),
            pod: run.pod.as_deref(),
            session_uri: uri,
        },
        &content,
    )
    .await
    .map_err(|e| GapReason::SessionLogUnreadable(format!("{e:#}")))?;
    Ok(Some(parsed.candidates.len()))
}

/// Replay the live ledger's `events` table onto `into`: in [`RebuildMode::Swap`] every row is
/// first copied verbatim (`db rebuild` swaps the rebuilt database in over the live one, so the
/// history has to travel with it or the swap destroys the evidence — a verify rebuild skips the
/// copy), then applied as a transition when its `to` status
/// isn't otherwise recoverable from GitHub or session-log evidence (i.e. everything past `new`).
/// Rows are read in `id` order (the table's own chronology), so the same compare-and-set claim
/// each transition originally won replays cleanly in sequence. Rows whose `from`/`to` aren't
/// issue statuses (audit events, drift markers) are copied but not applied, same as always.
///
/// The event row's frozen shape carries no `parked_by`, so a replayed park always lands `machine`
/// — the one place this rebuild path is provably lossier than the original write. See the module
/// doc. A from-nothing disaster (no live database) replays nothing; the NDJSON export
/// (`db export-events`) is the compensating control for that case.
async fn replay_events(into: &Db, cfg: &ControllerCfg, mode: RebuildMode) -> Result<usize> {
    use sqlx::Row;
    if !crate::client::ledger_exists(cfg.db_url()).await? {
        return Ok(0);
    }
    let live_pool = crate::client::connect(cfg.db_url())
        .await
        .context("rebuild: opening the live ledger to read its event history")?;
    let rows = sqlx::query(
        "SELECT v, ts, key, from_status, to_status, reason, evidence, actor \
         FROM events ORDER BY id",
    )
    .fetch_all(&live_pool)
    .await
    .context("rebuild: reading the live events table")?;
    live_pool.close().await;

    let mut applied = 0usize;
    for row in &rows {
        let key: String = row.get("key");
        let from_s: String = row.get("from_status");
        let to_s: String = row.get("to_status");
        let reason: Option<String> = row.get("reason");
        let evidence: Option<String> = row.get("evidence");
        let actor: Option<String> = row.get("actor");
        if mode == RebuildMode::Swap {
            crate::event_log::insert(
                into.pool(),
                &Event {
                    v: row.get("v"),
                    ts: row.get("ts"),
                    key: &key,
                    from: &from_s,
                    to: &to_s,
                    reason: reason.as_deref(),
                    evidence: evidence.as_deref(),
                    actor: actor.as_deref(),
                },
            )
            .await?;
        }
        let (Ok(from), Ok(to)) = (Status::parse(&from_s), Status::parse(&to_s)) else {
            continue;
        };
        let won = if to == Status::Parked {
            // `ParkReason::parse` reads back whatever the original park rendered, and an
            // unrecognized (or legacy) event row reads back as `Legacy`, verbatim — so this
            // replay reproduces the original persisted text exactly either way. The raw
            // `claim_park` CAS (not `operations::park`) so the history above stays the one copy.
            let raw = reason.unwrap_or_else(|| "replayed from event log".to_string());
            let rendered = ParkReason::parse(&raw).to_string();
            let now = crate::clock::now_rfc3339();
            crate::issues::store::claim_park(
                into.pool(),
                &key,
                from,
                &rendered,
                ParkedBy::Machine,
                &now,
            )
            .await?
        } else {
            crate::issues::store::claim_issue(into.pool(), &key, from, to).await?
        };
        if won {
            applied += 1;
        }
    }
    Ok(applied)
}

/// Rebuild into a throwaway database and diff it, table by table, against `live`. `issues` are
/// compared key-by-key on both sides (a row present only on one side is real drift); `runs` and
/// `candidates` are compared only where the run survived rebuild on both sides — a run gapped by
/// [`rebuild`] (an `s3://` URI, a missing local file) is *reported as that gap*, not manufactured
/// into a spurious "row went missing" drift line. `scopes` aren't compared at all in v1 (nothing to
/// diff against — see the module doc); every issue the live ledger shows past `new` shows up as a
/// [`GapReason::ScopePackPathUnavailable`] gap instead. `ledger` is compared as daily sums
/// (it's append-only and high-volume; row-by-row would be both slow and noisy).
///
/// The comparison result is appended to `live`'s event log (see [`append_drift_to_event_log`] for
/// the line-shape convention) whether or not anything drifted.
///
/// The comparison rebuild runs in [`RebuildMode::Verify`]: it replays the history and re-ingests
/// sessions to reconstruct the rows the diff reads, but copies neither the artifact payloads nor
/// the verbatim event history into the throwaway — the diff never looks at them, and on a real
/// evidence volume that copy is gigabytes of I/O per scheduled tick.
pub async fn verify(live: &Db, cfg: &ControllerCfg) -> Result<DriftReport> {
    use sqlx::migrate::MigrateDatabase;
    let stamp = format!(
        "{}_{}",
        std::process::id(),
        jiff::Timestamp::now().as_nanosecond()
    );
    // The throwaway comparison database is a sibling on the ledger's own server (a temp file
    // has no Postgres analog).
    let tmp_db_url = crate::client::sibling_db_url(
        cfg.db_url(),
        &format!("{}_verify_{stamp}", crate::client::db_name(cfg.db_url())?),
    )?;

    let result = run_verify(live, cfg, &tmp_db_url).await;

    let _ = sqlx::Postgres::drop_database(&tmp_db_url).await;

    result
}

async fn run_verify(live: &Db, cfg: &ControllerCfg, tmp_db_url: &str) -> Result<DriftReport> {
    let tmp_db = Db::open(tmp_db_url)
        .await
        .context("verify: opening the throwaway comparison database")?;
    let rebuild_report = rebuild_mode(&tmp_db, cfg, RebuildMode::Verify).await?;

    let mut diffs = Vec::new();
    diffs.extend(diff_issues(
        &crate::daemon::store::list_all_issues(live.pool()).await?,
        &crate::daemon::store::list_all_issues(tmp_db.pool()).await?,
    ));
    diffs.extend(diff_runs(
        &crate::daemon::store::list_all_runs(live.pool()).await?,
        &crate::daemon::store::list_all_runs(tmp_db.pool()).await?,
    ));
    diffs.extend(diff_candidates(
        &crate::daemon::store::list_all_candidates(live.pool()).await?,
        &crate::daemon::store::list_all_candidates(tmp_db.pool()).await?,
    ));
    diffs.extend(diff_ledger_daily_sums(live, &tmp_db).await?);

    tmp_db.pool().close().await;

    let report = DriftReport {
        diffs,
        gaps: rebuild_report.gaps,
    };
    append_drift_to_event_log(live.events(), &report, tmp_db_url).await?;
    Ok(report)
}

/// Append a [`DriftReport`] to the event log using the frozen `Event` shape (no generic "kind" tag
/// exists). Convention: one row per differing field, keyed
/// `"<table>:<row-key>"` with `from`/`to` holding the live/rebuilt values as strings, `reason` =
/// `"drift"`, and `evidence` naming the differing field; a clean report (no diffs) appends a single
/// `"_drift_check"` marker row with `reason = "drift: clean"` and `evidence` = the temp DB path
/// used for the comparison, so "we checked and found nothing" is as visible in the history as a
/// real finding.
async fn append_drift_to_event_log(
    events: &EventLog,
    report: &DriftReport,
    tmp_db_url: &str,
) -> Result<()> {
    if report.diffs.is_empty() {
        events
            .append(&Event::now(
                "_drift_check",
                "",
                "",
                Some("drift: clean"),
                Some(tmp_db_url),
            ))
            .await?;
        return Ok(());
    }
    for d in &report.diffs {
        let key = format!("{}:{}", d.table, d.key);
        let from = d.live.clone().unwrap_or_default();
        let to = d.rebuilt.clone().unwrap_or_default();
        events
            .append(&Event::now(&key, &from, &to, Some("drift"), Some(&d.field)))
            .await?;
    }
    Ok(())
}

/// `tier` and `priority` are deliberately excluded: rebuild never reconstructs either (see the
/// module doc's "What v1 can and can't reconstruct"). Tiering is the ranker's job alone; priority
/// is a purely-human knob (bumps are event-logged but the event line stores the value in the
/// reason string, not as a structured field, so replay can't reconstruct it without parsing free
/// text). Both would drift-flag on every touched issue otherwise.
fn issue_fields(i: &Issue) -> Vec<(&'static str, String)> {
    vec![
        ("repo", i.repo.clone()),
        ("status", i.status.as_str().to_string()),
        ("evidence_url", i.evidence_url.clone().unwrap_or_default()),
        ("parked_reason", i.parked_reason.clone().unwrap_or_default()),
        (
            "parked_by",
            i.parked_by
                .map(|p| p.as_str().to_string())
                .unwrap_or_default(),
        ),
    ]
}

fn diff_issues(live: &[Issue], rebuilt: &[Issue]) -> Vec<Drift> {
    diff_rows(
        "issues",
        live,
        rebuilt,
        |i| i.key.clone(),
        issue_fields,
        OneSided::Report,
    )
}

fn run_fields(r: &Run) -> Vec<(&'static str, String)> {
    vec![
        ("scope", r.scope.map(|s| s.to_string()).unwrap_or_default()),
        (
            "identity_digest",
            r.identity_digest.clone().unwrap_or_default(),
        ),
        ("status", r.status.clone()),
        ("pod", r.pod.clone().unwrap_or_default()),
        ("session_uri", r.session_uri.clone().unwrap_or_default()),
        (
            "best_score",
            r.best_score.map(|s| s.to_string()).unwrap_or_default(),
        ),
        (
            "cost_usd",
            r.cost_usd.map(|c| c.to_string()).unwrap_or_default(),
        ),
    ]
}

fn diff_runs(live: &[Run], rebuilt: &[Run]) -> Vec<Drift> {
    diff_rows(
        "runs",
        live,
        rebuilt,
        |r| r.run_id.clone(),
        run_fields,
        OneSided::Skip,
    )
}

fn candidate_key(c: &Candidate) -> String {
    format!(
        "{}:{}:{:?}:{:?}",
        c.run_id,
        c.kind.as_deref().unwrap_or(""),
        c.lane,
        c.iter
    )
}

fn candidate_fields(c: &Candidate) -> Vec<(&'static str, String)> {
    vec![
        ("score", c.score.map(|s| s.to_string()).unwrap_or_default()),
        ("decision", c.decision.clone().unwrap_or_default()),
        ("worktree", c.worktree.clone().unwrap_or_default()),
        ("sandbox", c.sandbox.clone().unwrap_or_default()),
        ("pr_url", c.pr_url.clone().unwrap_or_default()),
        ("branch", c.branch.clone().unwrap_or_default()),
    ]
}

fn diff_candidates(live: &[Candidate], rebuilt: &[Candidate]) -> Vec<Drift> {
    diff_rows(
        "candidates",
        live,
        rebuilt,
        candidate_key,
        candidate_fields,
        OneSided::Skip,
    )
}

/// What [`diff_rows`] does with a key present on only one side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OneSided {
    /// Skipped (used for `runs`/`candidates`, where a one-sided row is an already reported
    /// [`Gap`], not new drift).
    Skip,
    /// Becomes a `<row>`-field drift line (used for `issues`, where every key should exist on
    /// both sides after a healthy rebuild).
    Report,
}

/// Field-level diff of two row sets keyed by `key_of`.
fn diff_rows<T>(
    table: &'static str,
    live_rows: &[T],
    rebuilt_rows: &[T],
    key_of: impl Fn(&T) -> String,
    fields_of: impl Fn(&T) -> Vec<(&'static str, String)>,
    one_sided: OneSided,
) -> Vec<Drift> {
    use std::collections::HashMap;
    let live_map: HashMap<String, &T> = live_rows.iter().map(|r| (key_of(r), r)).collect();
    let rebuilt_map: HashMap<String, &T> = rebuilt_rows.iter().map(|r| (key_of(r), r)).collect();

    let mut keys: BTreeSet<String> = BTreeSet::new();
    match one_sided {
        OneSided::Skip => keys.extend(
            live_map
                .keys()
                .filter(|k| rebuilt_map.contains_key(*k))
                .cloned(),
        ),
        OneSided::Report => {
            keys.extend(live_map.keys().cloned());
            keys.extend(rebuilt_map.keys().cloned());
        }
    }

    let mut out = Vec::new();
    for key in keys {
        match (live_map.get(&key), rebuilt_map.get(&key)) {
            (Some(l), Some(r)) => {
                for ((field, lv), (_, rv)) in fields_of(l).into_iter().zip(fields_of(r)) {
                    if lv != rv {
                        out.push(Drift {
                            table,
                            key: key.clone(),
                            field: field.to_string(),
                            live: Some(lv),
                            rebuilt: Some(rv),
                        });
                    }
                }
            }
            (Some(_), None) => out.push(Drift {
                table,
                key: key.clone(),
                field: "<row>".to_string(),
                live: Some("present".to_string()),
                rebuilt: None,
            }),
            (None, Some(_)) => out.push(Drift {
                table,
                key: key.clone(),
                field: "<row>".to_string(),
                live: None,
                rebuilt: Some("present".to_string()),
            }),
            (None, None) => {}
        }
    }
    out
}

async fn diff_ledger_daily_sums(live: &Db, rebuilt: &Db) -> Result<Vec<Drift>> {
    let mut days: BTreeSet<String> = BTreeSet::new();
    days.extend(crate::daemon::store::list_ledger_days(live.pool()).await?);
    days.extend(crate::daemon::store::list_ledger_days(rebuilt.pool()).await?);

    let mut out = Vec::new();
    for day in days {
        let l = crate::ledger::ledger_day_total(live.pool(), &day).await?;
        let r = crate::ledger::ledger_day_total(rebuilt.pool(), &day).await?;
        if (l - r).abs() > 1e-9 {
            out.push(Drift {
                table: "ledger",
                key: day,
                field: "day_total_usd".to_string(),
                live: Some(format!("{l:.4}")),
                rebuilt: Some(format!("{r:.4}")),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issues::model::NewIssue;
    use crate::runs::model::NewRun;
    use sqlx::PgPool;
    use wiremock::matchers::{method, path as wpath};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn db_with(pool: PgPool) -> Db {
        Db::new(pool)
    }

    fn raw_issue(number: u64, title: &str, labels: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "number": number,
            "title": title,
            "body": "",
            "labels": labels.iter().map(|l| serde_json::json!({"name": l})).collect::<Vec<_>>(),
            "html_url": format!("https://github.com/owner/repo/issues/{number}"),
            "updated_at": "2026-07-01T00:00:00Z",
            "state": "open",
        })
    }

    fn sample_log() -> String {
        [
            r#"{"v":1,"kind":"row","row":{"iter":0,"decision":"baseline","score":200.0}}"#,
            r#"{"v":1,"kind":"row","row":{"iter":1,"decision":"keep","score":230.0}}"#,
            r#"{"v":1,"kind":"budget","spent":1.1,"elapsed_secs":120}"#,
            r#"{"v":1,"kind":"summary","rows":[],"gate":"bench","best_score":230.0}"#,
            r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":"done"}"#,
        ]
        .join("\n")
    }

    // --- 1. Rebuild reproduces rows -------------------------------------------------------------

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn rebuild_reproduces_issue_and_run_rows(pool: PgPool) -> Result<()> {
        let _guard = crate::ENV_LOCK.lock().await;
        let root = tempfile::tempdir()?;
        let cfg = crate::testing::controller_cfg(root.path(), vec!["owner/repo".to_string()]);

        // A "live" ledger with one run whose session log lives on disk locally. Seed its
        // watch-set the way a real boot would (`operations::seed_watched_repos`) — rebuild reads the
        // live ledger's watch-set, not `cfg.repos`, for which repos to rebuild.
        let live_pool = crate::client::connect(cfg.db_url()).await?;
        let live = db_with(live_pool);
        crate::issues::repo_watch::seed_watched_repos(live.pool(), &cfg.repos).await?;
        let session_path = root.path().join("session.jsonl");
        std::fs::write(&session_path, sample_log())?;
        crate::runs::store::insert_run(
            live.pool(),
            &NewRun {
                run_id: "run-1".to_string(),
                scope: None,
                issue: None,
                identity_digest: Some("v1:abc".to_string()),
                status: "running".to_string(),
                pod: Some("loop-1".to_string()),
                session_uri: Some(session_path.display().to_string()),
                best_score: None,
                cost_usd: None,
            },
        )
        .await?;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wpath("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![raw_issue(
                1,
                "a bug with steps to reproduce",
                &["bug"],
            )]))
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("GITHUB_API_URL", server.uri());
        }

        let into = db_with(pool);
        let report = rebuild(&into, &cfg).await?;
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }

        assert_eq!(report.issues_upserted, 1);
        assert_eq!(report.runs_ingested, 1);
        assert_eq!(report.candidates_ingested, 2, "baseline + keep rows");

        let issue = crate::issues::store::get_issue(into.pool(), "owner/repo#1")
            .await?
            .expect("issue row");
        assert!(
            issue.tier.is_none(),
            "rebuild never reconstructs tier — that's the ranker's job alone"
        );
        assert_eq!(issue.status, Status::New);

        let run = sqlx::query!(
            r#"SELECT status, best_score, cost_usd, pod FROM runs WHERE run_id = 'run-1'"#
        )
        .fetch_one(into.pool())
        .await?;
        assert_eq!(run.status, "finished");
        assert_eq!(run.best_score, Some(230.0));
        assert_eq!(run.pod.as_deref(), Some("loop-1"));

        live.pool().close().await;
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn rebuild_replays_the_event_log_to_advance_status(pool: PgPool) -> Result<()> {
        let _guard = crate::ENV_LOCK.lock().await;
        let root = tempfile::tempdir()?;
        let cfg = crate::testing::controller_cfg(root.path(), vec!["owner/repo".to_string()]);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wpath("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![raw_issue(
                2,
                "reduce p99 latency",
                &["performance"],
            )]))
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("GITHUB_API_URL", server.uri());
        }

        // Hand-write a transition history into the live ledger: new -> scoped -> parked, the
        // shape reconcile itself would have appended.
        let live = db_with(crate::client::connect(cfg.db_url()).await?);
        crate::issues::repo_watch::seed_watched_repos(live.pool(), &cfg.repos).await?;
        live.events()
            .append(&Event::now(
                "owner/repo#2",
                "new",
                "scoped",
                Some("proposed pack passed check"),
                None,
            ))
            .await?;
        live.events()
            .append(&Event::now(
                "owner/repo#2",
                "scoped",
                "parked",
                Some("stale scope"),
                None,
            ))
            .await?;

        let into = db_with(pool);
        let report = rebuild(&into, &cfg).await?;
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }

        assert_eq!(report.events_replayed, 2);
        // The history itself travels into the rebuilt database (a `db rebuild` swap would
        // otherwise destroy it), verbatim and without extra rows from the replay's own parks.
        let copied = into.events().read_all().await?;
        assert_eq!(copied.len(), 2);
        assert_eq!(
            copied[0].reason.as_deref(),
            Some("proposed pack passed check")
        );
        assert_eq!(copied[1].to, "parked");
        let issue = crate::issues::store::get_issue(into.pool(), "owner/repo#2")
            .await?
            .expect("issue row");
        assert_eq!(issue.status, Status::Parked);
        assert_eq!(issue.parked_reason.as_deref(), Some("stale scope"));
        assert_eq!(
            issue.parked_by,
            Some(ParkedBy::Machine),
            "the event line carries no parked_by; replay's documented default"
        );
        live.pool().close().await;
        Ok(())
    }

    /// The verify-mode rebuild reconstructs the rows the drift diff reads (statuses via replay)
    /// without copying the artifact payloads or the verbatim event history into the throwaway.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn verify_mode_rebuild_applies_history_without_copying_payloads(
        pool: PgPool,
    ) -> Result<()> {
        let _guard = crate::ENV_LOCK.lock().await;
        let root = tempfile::tempdir()?;
        let cfg = crate::testing::controller_cfg(root.path(), vec!["owner/repo".to_string()]);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wpath("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![raw_issue(
                2,
                "reduce p99 latency",
                &["performance"],
            )]))
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("GITHUB_API_URL", server.uri());
        }

        let live = db_with(crate::client::connect(cfg.db_url()).await?);
        crate::issues::repo_watch::seed_watched_repos(live.pool(), &cfg.repos).await?;
        live.events()
            .append(&Event::now("owner/repo#2", "new", "scoped", None, None))
            .await?;
        live.events()
            .append(&Event::now(
                "owner/repo#2",
                "scoped",
                "parked",
                Some("stale scope"),
                None,
            ))
            .await?;
        crate::runs::blob_store::put_pack_tarball(live.pool(), "owner_repo_2", b"tarball bytes")
            .await?;
        crate::runs::blob_store::put_run_session(live.pool(), "run-1", b"{\"v\":1}\n").await?;

        let into = db_with(pool);
        let report = rebuild_mode(&into, &cfg, RebuildMode::Verify).await?;
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }

        assert_eq!(report.events_replayed, 2, "history still applies");
        let issue = crate::issues::store::get_issue(into.pool(), "owner/repo#2")
            .await?
            .expect("issue row");
        assert_eq!(issue.status, Status::Parked);
        assert!(
            into.events().read_all().await?.is_empty(),
            "the verify rebuild carries no verbatim history"
        );
        let artifacts: i64 = sqlx::query_scalar("SELECT count(*) FROM artifacts")
            .fetch_one(into.pool())
            .await?;
        assert_eq!(artifacts, 0, "no artifact payloads copied");
        let packs: i64 = sqlx::query_scalar("SELECT count(*) FROM pack_tarballs")
            .fetch_one(into.pool())
            .await?;
        assert_eq!(packs, 0, "no pack tarballs copied");
        live.pool().close().await;
        Ok(())
    }

    // --- 2. Drift test -----------------------------------------------------------------------

    // No `#[sqlx::test]` pool here: `verify` needs `live` at a real path (`rebuild` re-opens
    // `cfg.db_path()` internally), the same reason `client::tests::connect_creates_and_migrates_a_fresh_db`
    // uses a plain `#[tokio::test]`.
    #[tokio::test]
    async fn verify_catches_a_hand_edited_row() -> Result<()> {
        let _guard = crate::ENV_LOCK.lock().await;
        let root = tempfile::tempdir()?;
        let cfg = crate::testing::controller_cfg(root.path(), vec!["owner/repo".to_string()]);

        let live_pool = crate::client::connect(cfg.db_url()).await?;
        let live = db_with(live_pool);
        crate::issues::repo_watch::seed_watched_repos(live.pool(), &cfg.repos).await?;
        crate::issues::store::upsert_issue(
            live.pool(),
            &NewIssue {
                key: "owner/repo#3".to_string(),
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

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wpath("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![raw_issue(
                3,
                "a bug with steps to reproduce",
                &["bug"],
            )]))
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("GITHUB_API_URL", server.uri());
        }

        // Drift: hand-edit evidence_url directly via raw SQL, bypassing `Db`'s API entirely.
        // (Not `tier` or `priority`: rebuild never reconstructs either, so `issue_fields`
        // deliberately excludes both from the diff — see that function's doc comment.)
        sqlx::query!(
            "UPDATE issues SET evidence_url = 'https://tampered' WHERE key = 'owner/repo#3'"
        )
        .execute(live.pool())
        .await?;

        let report = verify(&live, &cfg).await?;
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }

        assert!(!report.is_clean());
        let d = report
            .diffs
            .iter()
            .find(|d| d.table == "issues" && d.key == "owner/repo#3" && d.field == "evidence_url")
            .expect("evidence_url drift recorded");
        assert_eq!(d.live.as_deref(), Some("https://tampered"));
        assert_eq!(
            d.rebuilt.as_deref(),
            Some("https://github.com/owner/repo/issues/3")
        );

        // The event log records the finding too.
        assert!(
            live.events()
                .read_all()
                .await?
                .iter()
                .any(|e| e.reason.as_deref() == Some("drift"))
        );

        let pool = live.pool().clone();
        pool.close().await;
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn verify_appends_a_clean_marker_when_nothing_drifted(pool: PgPool) -> Result<()> {
        let root = tempfile::tempdir()?;
        let cfg = crate::testing::controller_cfg(root.path(), vec![]);
        let live = db_with(pool);

        let report = verify(&live, &cfg).await?;
        assert!(report.is_clean());

        assert!(
            live.events()
                .read_all()
                .await?
                .iter()
                .any(|e| e.reason.as_deref() == Some("drift: clean"))
        );
        Ok(())
    }

    // --- 3. Unreachable-evidence gap -----------------------------------------------------------

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn rebuild_gaps_an_s3_run_and_a_missing_local_file(pool: PgPool) -> Result<()> {
        let root = tempfile::tempdir()?;
        let cfg = crate::testing::controller_cfg(root.path(), vec![]);

        let live_pool = crate::client::connect(cfg.db_url()).await?;
        let live = db_with(live_pool);
        crate::runs::store::insert_run(
            live.pool(),
            &NewRun {
                run_id: "run-s3".to_string(),
                scope: None,
                issue: None,
                identity_digest: None,
                status: "running".to_string(),
                pod: None,
                session_uri: Some("s3://bucket/run-s3/session.jsonl".to_string()),
                best_score: None,
                cost_usd: None,
            },
        )
        .await?;
        crate::runs::store::insert_run(
            live.pool(),
            &NewRun {
                run_id: "run-missing".to_string(),
                scope: None,
                issue: None,
                identity_digest: None,
                status: "running".to_string(),
                pod: None,
                session_uri: Some("/nonexistent/session.jsonl".to_string()),
                best_score: None,
                cost_usd: None,
            },
        )
        .await?;

        let into = db_with(pool);
        let report = rebuild(&into, &cfg).await?;

        assert_eq!(report.runs_ingested, 0, "neither run was reachable");
        assert!(report.gaps.iter().any(|g| g.table == "runs"
            && g.key == "run-s3"
            && g.reason == GapReason::S3EvidenceUnavailable));
        assert!(report.gaps.iter().any(|g| g.table == "runs"
            && g.key == "run-missing"
            && matches!(g.reason, GapReason::SessionLogUnreadable(_))));

        let pool = live.pool().clone();
        pool.close().await;
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn rebuild_gaps_scoped_issues_it_cannot_reconstruct(pool: PgPool) -> Result<()> {
        let _guard = crate::ENV_LOCK.lock().await;
        let root = tempfile::tempdir()?;
        let cfg = crate::testing::controller_cfg(root.path(), vec!["owner/repo".to_string()]);

        let live_pool = crate::client::connect(cfg.db_url()).await?;
        let live = db_with(live_pool);
        crate::issues::store::upsert_issue(
            live.pool(),
            &NewIssue {
                key: "owner/repo#4".to_string(),
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
        assert!(
            crate::issues::store::claim_issue(
                live.pool(),
                "owner/repo#4",
                Status::New,
                Status::Scoped
            )
            .await?
        );

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wpath("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![raw_issue(
                4,
                "a bug with steps to reproduce",
                &["bug"],
            )]))
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("GITHUB_API_URL", server.uri());
        }

        let into = db_with(pool);
        let report = rebuild(&into, &cfg).await?;
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }

        assert!(report.gaps.iter().any(|g| g.table == "scopes"
            && g.key == "owner/repo#4"
            && g.reason == GapReason::ScopePackPathUnavailable));

        let pool = live.pool().clone();
        pool.close().await;
        Ok(())
    }

    /// The rebuild swap must carry the pack store: `copy_artifact_store` moves `pack_tarballs`
    /// and `pack_steering` verbatim into the rebuilt DB, alongside the artifact tables.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn copy_artifact_store_carries_pack_tarballs_and_steering(pool: PgPool) -> Result<()> {
        let live_url = crate::test_ledger_url();
        let live_pool = crate::client::connect(&live_url).await?;

        let tgz = {
            let tree = tempfile::tempdir()?;
            std::fs::write(tree.path().join("SCOPE.md"), "identity: v1:beef\n")?;
            crate::playbooks::packs::tar_pack_tree(tree.path())?
        };
        let digest =
            crate::runs::blob_store::put_pack_tarball(&live_pool, "owner_repo_7", &tgz).await?;
        crate::runs::blob_store::append_steering(
            &live_pool,
            "owner_repo_7",
            "pick fail-closed",
            None,
        )
        .await?;

        let into = db_with(pool);
        copy_artifact_store(&live_pool, &into).await?;
        live_pool.close().await;

        let copied = crate::runs::blob_store::get_pack_tarball(into.pool(), "owner_repo_7")
            .await?
            .expect("tarball copied");
        assert_eq!(copied, tgz);
        assert_eq!(crucible_contract::content_digest(&copied), digest);
        let steering = crate::runs::blob_store::list_steering(into.pool(), "owner_repo_7").await?;
        assert_eq!(steering.len(), 1);
        assert_eq!(steering[0].body_md, "pick fail-closed");
        Ok(())
    }
}
