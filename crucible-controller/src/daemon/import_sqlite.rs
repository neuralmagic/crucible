//! One-shot cutover import: stream every row of a pre-Postgres `outer.sqlite` ledger into the
//! Postgres ledger. `crucible-controller db import-sqlite --from <file>` is the only caller; it
//! runs in-cluster, where both the state PVC (the sqlite file) and the Postgres server are
//! reachable. The whole import is one Postgres transaction against a REQUIRED-empty target, so a
//! half-import can't exist: it either all lands or the target stays empty.
//!
//! Fidelity rules:
//! - Explicit ids (`scopes`, `issue_comments`, `scope_reports`, `scope_transcripts`, `builds`)
//!   are preserved — other rows reference them — and each identity sequence is bumped past the
//!   imported maximum afterwards so the next insert can't collide.
//! - `runs` and `candidates` are copied in sqlite `rowid` order: their Postgres `seq` identity
//!   columns are assigned in insert order, and "newest by insertion order" queries depend on it.
//! - Sqlite's 0/1 integer flags decode into the baseline's real BOOLEAN columns.

#![allow(clippy::disallowed_macros)]

use anyhow::{Context, Result};
use sqlx::{PgPool, Row};
use std::fmt::Write as _;
use std::path::Path;

/// One column of a copied table: its name plus how to decode-and-rebind it. Sqlite's dynamic
/// typing means every read goes through an explicit kind, never inference.
enum Col {
    Text(&'static str),
    Int(&'static str),
    Real(&'static str),
    /// Sqlite 0/1 integer → Postgres BOOLEAN.
    Bool(&'static str),
    Bytes(&'static str),
}

impl Col {
    fn name(&self) -> &'static str {
        match self {
            Col::Text(n) | Col::Int(n) | Col::Real(n) | Col::Bool(n) | Col::Bytes(n) => n,
        }
    }
}

struct TableSpec {
    name: &'static str,
    cols: &'static [Col],
    /// `true` copies in sqlite `rowid` order (tables whose Postgres `seq` must mirror insertion
    /// order). Tables with an explicit PRIMARY KEY need no order.
    rowid_order: bool,
}

/// Every ledger table, in foreign-key dependency order (parents first).
const TABLES: &[TableSpec] = &[
    TableSpec {
        name: "repos",
        cols: &[
            Col::Text("repo"),
            Col::Text("last_seen_updated_at"),
            Col::Bool("watched"),
            Col::Bool("paused"),
            Col::Text("added_by"),
            Col::Text("added_at"),
            Col::Text("closed_repaired_at"),
        ],
        rowid_order: false,
    },
    TableSpec {
        name: "issues",
        cols: &[
            Col::Text("key"),
            Col::Text("repo"),
            Col::Text("tier"),
            Col::Text("status"),
            Col::Int("priority"),
            Col::Text("evidence_url"),
            Col::Text("parked_reason"),
            Col::Text("parked_by"),
            Col::Text("updated_at"),
            Col::Text("ranked_content_hash"),
            Col::Text("title"),
            Col::Text("author"),
            Col::Text("labels"),
            Col::Text("grounded_content_hash"),
            Col::Text("upstream_updated_at"),
            Col::Text("scope_now_justification"),
            Col::Real("scope_now_max_cost"),
            Col::Text("body"),
            Col::Text("pre_park_status"),
            Col::Text("redispatch_justification"),
            Col::Text("input_kind"),
            Col::Text("git_ref"),
            Col::Text("codegen_contract"),
        ],
        rowid_order: false,
    },
    TableSpec {
        name: "scenarios",
        cols: &[
            Col::Text("key"),
            Col::Text("title"),
            Col::Text("body"),
            Col::Text("created_by"),
            Col::Text("created_at"),
            Col::Bool("authoritative"),
        ],
        rowid_order: false,
    },
    TableSpec {
        name: "scenario_repos",
        cols: &[Col::Text("key"), Col::Text("repo"), Col::Int("position")],
        rowid_order: false,
    },
    TableSpec {
        name: "issue_comments",
        cols: &[
            Col::Int("id"),
            Col::Text("issue_key"),
            Col::Text("author"),
            Col::Text("created_at"),
            Col::Text("updated_at"),
            Col::Text("body"),
        ],
        rowid_order: false,
    },
    TableSpec {
        name: "scopes",
        cols: &[
            Col::Int("id"),
            Col::Text("issue"),
            Col::Text("pack_digest"),
            Col::Text("check_outcome"),
            Col::Bool("stale"),
            Col::Text("approval_pr"),
            Col::Text("approved_by"),
            Col::Text("approved_at"),
            Col::Text("frozen_issue_hash"),
            Col::Text("stale_comment_id"),
        ],
        rowid_order: false,
    },
    TableSpec {
        name: "runs",
        cols: &[
            Col::Text("run_id"),
            Col::Int("scope"),
            Col::Text("identity_digest"),
            Col::Text("status"),
            Col::Text("pod"),
            Col::Text("session_uri"),
            Col::Real("best_score"),
            Col::Real("cost_usd"),
        ],
        rowid_order: true,
    },
    TableSpec {
        name: "candidates",
        cols: &[
            Col::Text("run_id"),
            Col::Text("kind"),
            Col::Int("lane"),
            Col::Int("iter"),
            Col::Real("score"),
            Col::Text("decision"),
            Col::Text("worktree"),
            Col::Text("sandbox"),
            Col::Text("pr_url"),
            Col::Text("branch"),
        ],
        rowid_order: true,
    },
    TableSpec {
        name: "ledger",
        cols: &[
            Col::Text("ts"),
            Col::Text("run_id"),
            Col::Text("kind"),
            Col::Real("cost_usd"),
        ],
        rowid_order: true,
    },
    TableSpec {
        name: "work_pods",
        cols: &[
            Col::Text("pod_name"),
            Col::Text("kind"),
            Col::Text("issue_key"),
            Col::Text("state"),
            Col::Text("cost_tag"),
            Col::Text("result"),
            Col::Text("error"),
            Col::Text("created_at"),
            Col::Text("updated_at"),
            Col::Text("terminal_at"),
            Col::Text("cluster"),
        ],
        rowid_order: false,
    },
    TableSpec {
        name: "scope_reports",
        cols: &[
            Col::Int("id"),
            Col::Text("issue_key"),
            Col::Text("pod_name"),
            Col::Bool("survived"),
            Col::Text("report_json"),
            Col::Text("created_at"),
        ],
        rowid_order: false,
    },
    TableSpec {
        name: "scope_transcripts",
        cols: &[
            Col::Int("id"),
            Col::Int("scope_report_id"),
            Col::Text("issue_key"),
            Col::Bytes("transcript_gz"),
            Col::Text("created_at"),
        ],
        rowid_order: false,
    },
    TableSpec {
        name: "pod_artifacts",
        cols: &[
            Col::Text("pod"),
            Col::Text("kind"),
            Col::Text("digest"),
            Col::Int("bytes"),
            Col::Text("created_at"),
        ],
        rowid_order: false,
    },
    TableSpec {
        name: "mlflow_exports",
        cols: &[
            Col::Text("run_id"),
            Col::Text("state"),
            Col::Text("mlflow_run_id"),
            Col::Text("experiment_id"),
            Col::Int("attempts"),
            Col::Text("error"),
            Col::Text("created_at"),
            Col::Text("updated_at"),
        ],
        rowid_order: false,
    },
    TableSpec {
        name: "builds",
        cols: &[
            Col::Int("id"),
            Col::Int("scope"),
            Col::Text("name"),
            Col::Text("image"),
            Col::Text("tag"),
            Col::Text("context_digest"),
            Col::Text("backend"),
            Col::Text("state"),
            Col::Text("dispatch_id"),
            Col::Text("digest_ref"),
            Col::Text("evidence_url"),
            Col::Int("dispatch_attempts"),
            Col::Int("timeout_secs"),
            Col::Text("created_at"),
            Col::Text("dispatched_at"),
            Col::Text("finished_at"),
        ],
        rowid_order: false,
    },
    TableSpec {
        name: "emissions",
        cols: &[
            Col::Text("issue_key"),
            Col::Text("artifact"),
            Col::Text("tracker_issue_id"),
            Col::Text("created_at"),
        ],
        rowid_order: false,
    },
    TableSpec {
        name: "run_plans",
        cols: &[
            Col::Text("run_id"),
            Col::Int("plan_version"),
            Col::Text("graph_json"),
        ],
        rowid_order: false,
    },
    TableSpec {
        name: "run_task_results",
        cols: &[
            Col::Text("run_id"),
            Col::Int("iter"),
            Col::Text("task"),
            Col::Text("status"),
            Col::Text("note"),
            Col::Real("cost_usd"),
            Col::Real("secs"),
        ],
        rowid_order: false,
    },
];

/// Tables whose explicit ids were preserved: their identity sequence must jump past the imported
/// maximum or the next organic insert collides.
const IDENTITY_TABLES: &[&str] = &[
    "scopes",
    "issue_comments",
    "scope_reports",
    "scope_transcripts",
    "builds",
];

/// Per-table row counts, in copy order — the operator-facing receipt.
#[derive(Debug)]
pub struct ImportReport {
    pub tables: Vec<(&'static str, usize)>,
}

impl ImportReport {
    pub fn total(&self) -> usize {
        self.tables.iter().map(|(_, n)| n).sum()
    }
}

/// Copy every ledger table from the sqlite file at `from` into `pg`. The target must be empty
/// (a fresh, migrated database) — anything else refuses before touching a row.
pub async fn import_sqlite(from: &Path, pg: &PgPool) -> Result<ImportReport> {
    let opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(from)
        .read_only(true)
        // Never create: a typo'd path must be an error, not a silent empty import.
        .create_if_missing(false);
    let sqlite = sqlx::SqlitePool::connect_with(opts)
        .await
        .with_context(|| format!("opening sqlite ledger {}", from.display()))?;

    // Refuse a non-empty target: the import is a cutover, not a merge.
    for probe in ["issues", "runs", "work_pods", "ledger"] {
        let n: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {probe}")))
                .fetch_one(pg)
                .await
                .with_context(|| format!("counting target rows in {probe}"))?;
        anyhow::ensure!(
            n == 0,
            "target ledger is not empty ({probe} has {n} row(s)); import only cuts over into a fresh database"
        );
    }

    let mut tx = pg.begin().await.context("opening the import transaction")?;
    let mut report = ImportReport { tables: Vec::new() };
    for spec in TABLES {
        let copied = copy_table(&sqlite, &mut tx, spec)
            .await
            .with_context(|| format!("copying {}", spec.name))?;
        report.tables.push((spec.name, copied));
    }
    for table in IDENTITY_TABLES {
        // Bump the identity sequence past the imported ids; the third setval arg keeps a fresh
        // (empty-table) sequence at its start value instead of skipping 1.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT setval(pg_get_serial_sequence('{table}', 'id'), \
                    COALESCE(MAX(id), 1), MAX(id) IS NOT NULL) FROM {table}"
        )))
        .execute(&mut *tx)
        .await
        .with_context(|| format!("bumping {table}'s id sequence"))?;
    }
    tx.commit().await.context("committing the import")?;
    sqlite.close().await;
    Ok(report)
}

async fn copy_table(
    sqlite: &sqlx::SqlitePool,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    spec: &TableSpec,
) -> Result<usize> {
    let col_list = spec
        .cols
        .iter()
        .map(Col::name)
        .collect::<Vec<_>>()
        .join(", ");
    let mut select = format!("SELECT {col_list} FROM {}", spec.name);
    if spec.rowid_order {
        select.push_str(" ORDER BY rowid");
    }
    let mut insert = format!("INSERT INTO {} ({col_list}) VALUES (", spec.name);
    for i in 1..=spec.cols.len() {
        if i > 1 {
            insert.push_str(", ");
        }
        let _ = write!(insert, "${i}");
    }
    insert.push(')');

    let rows = sqlx::query(sqlx::AssertSqlSafe(select.as_str()))
        .fetch_all(sqlite)
        .await?;
    let copied = rows.len();
    for row in rows {
        let mut q = sqlx::query(sqlx::AssertSqlSafe(insert.as_str()));
        for col in spec.cols {
            q = match col {
                Col::Text(n) => q.bind(row.try_get::<Option<String>, _>(*n)?),
                Col::Int(n) => q.bind(row.try_get::<Option<i64>, _>(*n)?),
                Col::Real(n) => q.bind(row.try_get::<Option<f64>, _>(*n)?),
                Col::Bool(n) => q.bind(row.try_get::<Option<i64>, _>(*n)?.map(|v| v != 0)),
                Col::Bytes(n) => q.bind(row.try_get::<Option<Vec<u8>>, _>(*n)?),
            };
        }
        q.execute(&mut **tx).await?;
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use sqlx::PgPool;
    use std::path::PathBuf;

    /// A sqlite ledger file in the pre-Postgres shape (only the columns the import reads), with
    /// one FK-linked chain through every table and deliberate 0/1 flags + a blob.
    async fn seed_sqlite_fixture(dir: &Path) -> Result<PathBuf> {
        let path = dir.join("outer.sqlite");
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true),
        )
        .await?;
        let ddl = r#"
        CREATE TABLE repos (repo TEXT PRIMARY KEY, last_seen_updated_at TEXT, watched INTEGER, paused INTEGER, added_by TEXT, added_at TEXT, closed_repaired_at TEXT);
        CREATE TABLE issues (key TEXT PRIMARY KEY, repo TEXT, tier TEXT, status TEXT, priority INTEGER, evidence_url TEXT, parked_reason TEXT, parked_by TEXT, updated_at TEXT, ranked_content_hash TEXT, title TEXT, author TEXT, labels TEXT, grounded_content_hash TEXT, upstream_updated_at TEXT, scope_now_justification TEXT, scope_now_max_cost REAL, body TEXT, pre_park_status TEXT, redispatch_justification TEXT, input_kind TEXT, git_ref TEXT, codegen_contract TEXT);
        CREATE TABLE scenarios (key TEXT PRIMARY KEY, title TEXT, body TEXT, created_by TEXT, created_at TEXT, authoritative INTEGER);
        CREATE TABLE scenario_repos (key TEXT, repo TEXT, position INTEGER);
        CREATE TABLE issue_comments (id INTEGER PRIMARY KEY, issue_key TEXT, author TEXT, created_at TEXT, updated_at TEXT, body TEXT);
        CREATE TABLE scopes (id INTEGER PRIMARY KEY, issue TEXT, pack_digest TEXT, check_outcome TEXT, stale INTEGER, approval_pr TEXT, approved_by TEXT, approved_at TEXT, frozen_issue_hash TEXT, stale_comment_id TEXT);
        CREATE TABLE runs (run_id TEXT PRIMARY KEY, scope INTEGER, identity_digest TEXT, status TEXT, pod TEXT, session_uri TEXT, best_score REAL, cost_usd REAL);
        CREATE TABLE candidates (run_id TEXT, kind TEXT, lane INTEGER, iter INTEGER, score REAL, decision TEXT, worktree TEXT, sandbox TEXT, pr_url TEXT, branch TEXT);
        CREATE TABLE ledger (ts TEXT, run_id TEXT, kind TEXT, cost_usd REAL);
        CREATE TABLE work_pods (pod_name TEXT PRIMARY KEY, kind TEXT, issue_key TEXT, state TEXT, cost_tag TEXT, result TEXT, error TEXT, created_at TEXT, updated_at TEXT, terminal_at TEXT, cluster TEXT);
        CREATE TABLE scope_reports (id INTEGER PRIMARY KEY, issue_key TEXT, pod_name TEXT, survived INTEGER, report_json TEXT, created_at TEXT);
        CREATE TABLE scope_transcripts (id INTEGER PRIMARY KEY, scope_report_id INTEGER, issue_key TEXT, transcript_gz BLOB, created_at TEXT);
        CREATE TABLE pod_artifacts (pod TEXT, kind TEXT, path TEXT, digest TEXT, bytes INTEGER, created_at TEXT);
        CREATE TABLE mlflow_exports (run_id TEXT PRIMARY KEY, state TEXT, mlflow_run_id TEXT, experiment_id TEXT, attempts INTEGER, error TEXT, created_at TEXT, updated_at TEXT);
        CREATE TABLE builds (id INTEGER PRIMARY KEY, scope INTEGER, name TEXT, image TEXT, tag TEXT, context_digest TEXT, backend TEXT, state TEXT, dispatch_id TEXT, digest_ref TEXT, evidence_url TEXT, dispatch_attempts INTEGER, timeout_secs INTEGER, created_at TEXT, dispatched_at TEXT, finished_at TEXT);
        CREATE TABLE emissions (issue_key TEXT, artifact TEXT, tracker_issue_id TEXT, created_at TEXT);
        CREATE TABLE run_plans (run_id TEXT, plan_version INTEGER, graph_json TEXT);
        CREATE TABLE run_task_results (run_id TEXT, iter INTEGER, task TEXT, status TEXT, note TEXT, cost_usd REAL, secs REAL);
        "#;
        for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            sqlx::query(stmt).execute(&pool).await?;
        }
        let seed = r#"
        INSERT INTO repos VALUES ('owner/repo', '2026-08-01T00:00:00Z', 1, 0, 'alice', '2026-07-01T00:00:00Z', NULL);
        INSERT INTO issues VALUES ('owner/repo#1', 'owner/repo', 'T1', 'done', 5, NULL, NULL, NULL, '2026-08-01T00:00:00Z', 'hash1', 'a title', 'octocat', '["bug"]', NULL, '2026-07-31T00:00:00Z', NULL, NULL, 'a body', NULL, NULL, 'github', NULL, NULL);
        INSERT INTO issues VALUES ('owner/repo#2', 'owner/repo', NULL, 'parked', 0, NULL, 'stale scope', 'machine', '2026-08-02T00:00:00Z', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 'scenario', NULL, NULL);
        INSERT INTO scenarios VALUES ('owner/repo#2', 'scenario title', 'goal framing', 'alice', '2026-08-02T00:00:00Z', 1);
        INSERT INTO scenario_repos VALUES ('owner/repo#2', 'owner/repo', 0);
        INSERT INTO issue_comments VALUES (991, 'owner/repo#1', 'octocat', '2026-08-01T00:00:00Z', '2026-08-01T00:00:00Z', 'a comment');
        INSERT INTO scopes VALUES (7, 'owner/repo#1', 'v1:abc', 'passed', 0, NULL, 'alice', '2026-08-01T00:00:00Z', NULL, NULL);
        INSERT INTO runs VALUES ('run-b', 7, 'v1:abc', 'finished', 'loop-b', NULL, 231.0, 1.5);
        INSERT INTO runs VALUES ('run-a', 7, 'v1:abc', 'finished', 'loop-a', NULL, 230.0, 1.0);
        INSERT INTO candidates VALUES ('run-b', 'deep', NULL, 1, 231.0, 'keep', NULL, NULL, 'https://pr/1', NULL);
        INSERT INTO candidates VALUES ('run-a', 'deep', NULL, 1, 230.0, 'discard', NULL, NULL, NULL, NULL);
        INSERT INTO candidates VALUES ('run-a', 'deep', NULL, 2, 229.0, 'discard', NULL, NULL, NULL, NULL);
        INSERT INTO ledger VALUES ('2026-08-01T00:00:00Z', 'run-a', 'run', 1.0);
        INSERT INTO work_pods VALUES ('pod-1', 'scope', 'owner/repo#1', 'succeeded', 'scope', 'ok', NULL, '2026-08-01T00:00:00Z', '2026-08-01T00:00:00Z', '2026-08-01T01:00:00Z', 'hub');
        INSERT INTO scope_reports VALUES (3, 'owner/repo#1', 'pod-1', 1, '{"ok":true}', '2026-08-01T00:00:00Z');
        INSERT INTO scope_transcripts VALUES (5, 3, 'owner/repo#1', X'1f8b0800', '2026-08-01T00:00:00Z');
        INSERT INTO pod_artifacts VALUES ('pod-1', 'report', '/a/b', 'sha256:d', 42, '2026-08-01T00:00:00Z');
        INSERT INTO mlflow_exports VALUES ('run-a', 'exported', 'mlf-1', 'exp-1', 1, NULL, '2026-08-01T00:00:00Z', '2026-08-01T00:00:00Z');
        INSERT INTO builds VALUES (11, 7, 'sandbox', 'ghcr.io/org/img', 'v1', 'ctx1', 'cluster', 'succeeded', 'job-1', 'ghcr.io/org/img@sha256:e', NULL, 0, 600, '2026-08-01T00:00:00Z', '2026-08-01T00:10:00Z', '2026-08-01T00:20:00Z');
        INSERT INTO emissions VALUES ('owner/repo#1', 'epic', 'PROJ-1', '2026-08-01T00:00:00Z');
        INSERT INTO run_plans VALUES ('run-a', 1, '{"nodes":[]}');
        INSERT INTO run_task_results VALUES ('run-a', 1, 'build', 'passed', '', 0.1, 12.5);
        "#;
        for stmt in seed.split(";\n").map(str::trim).filter(|s| !s.is_empty()) {
            sqlx::query(stmt).execute(&pool).await?;
        }
        pool.close().await;
        Ok(path)
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn import_round_trips_every_table(pool: PgPool) -> Result<()> {
        let dir = tempfile::tempdir()?;
        let from = seed_sqlite_fixture(dir.path()).await?;

        let report = import_sqlite(&from, &pool).await?;
        let by_name: std::collections::BTreeMap<_, _> = report.tables.iter().copied().collect();
        assert_eq!(by_name["issues"], 2);
        assert_eq!(by_name["runs"], 2);
        assert_eq!(by_name["candidates"], 3);
        assert_eq!(report.total(), 22);

        // 0/1 flags landed as real booleans.
        let repo = sqlx::query!(r#"SELECT watched, paused FROM repos WHERE repo = 'owner/repo'"#)
            .fetch_one(&pool)
            .await?;
        assert!(repo.watched && !repo.paused);
        let survived: bool = sqlx::query_scalar("SELECT survived FROM scope_reports WHERE id = 3")
            .fetch_one(&pool)
            .await?;
        assert!(survived);

        // The blob round-trips byte-for-byte.
        let gz: Vec<u8> =
            sqlx::query_scalar("SELECT transcript_gz FROM scope_transcripts WHERE id = 5")
                .fetch_one(&pool)
                .await?;
        assert_eq!(gz, vec![0x1f, 0x8b, 0x08, 0x00]);

        // rowid order became seq order ("newest by insertion order" queries depend on it).
        let order: Vec<String> = sqlx::query_scalar("SELECT run_id FROM candidates ORDER BY seq")
            .fetch_all(&pool)
            .await?;
        assert_eq!(order, ["run-b", "run-a", "run-a"]);
        let newest: String =
            sqlx::query_scalar("SELECT run_id FROM runs ORDER BY seq DESC LIMIT 1")
                .fetch_one(&pool)
                .await?;
        assert_eq!(newest, "run-a", "run-a was inserted after run-b");

        // The FK chain survived intact: issue → scope → run → mlflow export, scope → build.
        let chained: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM mlflow_exports m
             JOIN runs r ON m.run_id = r.run_id
             JOIN scopes s ON r.scope = s.id
             JOIN issues i ON s.issue = i.key
             WHERE i.key = 'owner/repo#1'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(chained, 1);

        // Identity sequences jumped past the imported ids: the next organic insert can't collide.
        let next_scope = crate::issues::store::insert_scope(
            &pool,
            &crate::issues::model::NewScope {
                issue: "owner/repo#1".to_string(),
                pack_digest: Some("v1:new".to_string()),
                check_outcome: None,
            },
        )
        .await?;
        assert!(
            next_scope > 7,
            "sequence must clear the imported max, got {next_scope}"
        );

        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn import_refuses_a_non_empty_target(pool: PgPool) -> Result<()> {
        let dir = tempfile::tempdir()?;
        let from = seed_sqlite_fixture(dir.path()).await?;
        import_sqlite(&from, &pool).await?;

        let err = import_sqlite(&from, &pool)
            .await
            .expect_err("a second import into the same target must refuse");
        assert!(
            err.to_string().contains("not empty"),
            "want the not-empty refusal, got: {err:#}"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn import_errors_on_a_missing_file(pool: PgPool) -> Result<()> {
        let err = import_sqlite(Path::new("/nonexistent/outer.sqlite"), &pool)
            .await
            .expect_err("a bad path must error, not import nothing");
        assert!(err.to_string().contains("opening sqlite ledger"));
        Ok(())
    }
}
