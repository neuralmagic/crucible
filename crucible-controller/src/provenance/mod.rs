//! Signed provenance for the commits a run pushes.
//!
//! When a finished run's task reports a commit it pushed (a commit link in its declared output),
//! the controller signs an in-toto statement for it from its own records and stores the envelope,
//! addressable by envelope digest and by commit.

pub mod api;

use crate::runs::model::{Run, TaskResult};
use anyhow::{Context, Result};
use crucible_contract::provenance::{
    AgentRecord, CheckResult, CommitPredicate, DispatchProvenance, EnvelopeDigest, PlaybookRef,
    PushedCommit, Signer, Statement,
};
use crucible_contract::session::PlanTaskWire;
use crucible_contract::{LinkKind, content_digest};
use sqlx::{PgExecutor, PgPool, Row};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// The path of the PKCS#8 PEM ed25519 key the controller signs with.
pub const KEY_PATH_ENV: &str = "CONTROLLER_PROVENANCE_KEY_PATH";

/// The signing key, when the deployment mounted one. Unset signs nothing; a key that is set and
/// does not load fails startup.
pub fn signer_from_env() -> Result<Option<Arc<Signer>>> {
    match std::env::var(KEY_PATH_ENV) {
        Ok(path) if !path.trim().is_empty() => load_signer(std::path::Path::new(&path)).map(Some),
        _ => Ok(None),
    }
}

pub fn load_signer(path: &std::path::Path) -> Result<Arc<Signer>> {
    let pem = std::fs::read_to_string(path)
        .with_context(|| format!("reading the provenance key {}", path.display()))?;
    let signer = Signer::from_pkcs8_pem(&pem)
        .with_context(|| format!("loading the provenance key {}", path.display()))?;
    tracing::info!(keyid = %signer.keyid(), "commit provenance signing key loaded");
    Ok(Arc::new(signer))
}

/// Record what the controller handed a run at dispatch.
#[tracing::instrument(name = "db.set_dispatch_provenance", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", run_id = %run_id), err)]
pub async fn set_dispatch_provenance(
    ex: impl PgExecutor<'_>,
    run_id: &str,
    provenance: &DispatchProvenance,
) -> Result<()> {
    sqlx::query("UPDATE runs SET dispatch_provenance = $2 WHERE run_id = $1")
        .bind(run_id)
        .bind(serde_json::to_value(provenance).context("encoding the dispatch provenance")?)
        .execute(ex)
        .await
        .context("set_dispatch_provenance")?;
    Ok(())
}

async fn dispatch_provenance(pool: &PgPool, run_id: &str) -> Result<Option<DispatchProvenance>> {
    let raw: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT dispatch_provenance FROM runs WHERE run_id = $1")
            .bind(run_id)
            .fetch_optional(pool)
            .await
            .context("dispatch_provenance")?
            .flatten();
    raw.map(serde_json::from_value)
        .transpose()
        .context("decoding the stored dispatch provenance")
}

/// One stored statement, without its envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredStatement {
    pub digest: EnvelopeDigest,
    pub commit: String,
    pub repository: String,
    pub run_id: String,
    pub iter: i64,
    pub task: String,
    pub keyid: String,
    pub created_at: String,
}

const STATEMENT_COLS: &str =
    "envelope_digest, commit_sha, repository, run_id, iter, task, keyid, created_at";

fn stored(row: &sqlx::postgres::PgRow) -> Result<StoredStatement> {
    let digest: String = row.try_get("envelope_digest")?;
    Ok(StoredStatement {
        digest: EnvelopeDigest::parse(&digest)?,
        commit: row.try_get("commit_sha")?,
        repository: row.try_get("repository")?,
        run_id: row.try_get("run_id")?,
        iter: row.try_get("iter")?,
        task: row.try_get("task")?,
        keyid: row.try_get("keyid")?,
        created_at: row.try_get("created_at")?,
    })
}

/// Every statement whose subject is `commit`, newest first.
pub async fn statements_for_commit(pool: &PgPool, commit: &str) -> Result<Vec<StoredStatement>> {
    const SQL: &str = const_format::formatcp!(
        "SELECT {STATEMENT_COLS} FROM commit_provenance WHERE commit_sha = $1 \
         ORDER BY created_at DESC, envelope_digest"
    );
    sqlx::query(SQL)
        .bind(commit)
        .fetch_all(pool)
        .await
        .context("statements_for_commit")?
        .iter()
        .map(stored)
        .collect()
}

/// Every statement a run produced, oldest first.
pub async fn statements_for_run(pool: &PgPool, run_id: &str) -> Result<Vec<StoredStatement>> {
    const SQL: &str = const_format::formatcp!(
        "SELECT {STATEMENT_COLS} FROM commit_provenance WHERE run_id = $1 \
         ORDER BY created_at, envelope_digest"
    );
    sqlx::query(SQL)
        .bind(run_id)
        .fetch_all(pool)
        .await
        .context("statements_for_run")?
        .iter()
        .map(stored)
        .collect()
}

/// An envelope's stored bytes.
pub async fn envelope(pool: &PgPool, digest: &EnvelopeDigest) -> Result<Option<Vec<u8>>> {
    sqlx::query_scalar("SELECT envelope FROM commit_provenance WHERE envelope_digest = $1")
        .bind(digest.as_str())
        .fetch_optional(pool)
        .await
        .context("envelope")
}

async fn attested(
    pool: &PgPool,
    run_id: &str,
    result: &TaskResult,
    commit: &PushedCommit,
) -> Result<bool> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM commit_provenance WHERE run_id = $1 AND iter = $2 AND task = $3 \
         AND repository = $4 AND commit_sha = $5",
    )
    .bind(run_id)
    .bind(result.iter)
    .bind(&result.task)
    .bind(&commit.repository)
    .bind(commit.sha.as_str())
    .fetch_optional(pool)
    .await
    .context("attested")?;
    Ok(found.is_some())
}

async fn insert(
    pool: &PgPool,
    run_id: &str,
    result: &TaskResult,
    commit: &PushedCommit,
    keyid: &str,
    signed: &crucible_contract::provenance::SignedEnvelope,
    created_at: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO commit_provenance \
         (envelope_digest, commit_sha, repository, run_id, iter, task, keyid, envelope, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) ON CONFLICT DO NOTHING",
    )
    .bind(signed.digest.as_str())
    .bind(commit.sha.as_str())
    .bind(&commit.repository)
    .bind(run_id)
    .bind(result.iter)
    .bind(&result.task)
    .bind(keyid)
    .bind(&signed.bytes)
    .bind(created_at)
    .execute(pool)
    .await
    .context("inserting a commit statement")?;
    Ok(())
}

/// What the controller knows about a run, read once for all of its statements.
struct RunRecord {
    run: Run,
    dispatch: Option<DispatchProvenance>,
    playbook: Option<PlaybookRef>,
    schema: Option<String>,
    plan: Option<String>,
    graph: Vec<PlanTaskWire>,
    results: Vec<TaskResult>,
}

impl RunRecord {
    async fn read(pool: &PgPool, run_id: &str) -> Result<Option<Self>> {
        let Some(run) = crate::runs::store::get_run(pool, run_id).await? else {
            return Ok(None);
        };
        let launch = match run.issue.as_deref() {
            Some(key) => crate::launches::store::get_playbook_launch(pool, key).await?,
            None => None,
        };
        let stored_plan = crate::runs::task_results::latest_run_plan(pool, run_id).await?;
        let graph = match stored_plan.as_ref() {
            Some(p) => serde_json::from_str(&p.graph_json)
                .with_context(|| format!("decoding the stored task graph for run {run_id}"))?,
            None => Vec::new(),
        };
        Ok(Some(RunRecord {
            dispatch: dispatch_provenance(pool, run_id).await?,
            playbook: launch.as_ref().map(|l| PlaybookRef {
                id: l.playbook.clone(),
                draft_version: l.draft_version,
            }),
            schema: launch.map(|l| l.schema_digest),
            plan: stored_plan.map(|p| content_digest(p.graph_json.as_bytes())),
            graph,
            results: crate::runs::task_results::list_task_results(pool, run_id).await?,
            run,
        }))
    }

    /// `task` and every task it depends on, directly or not.
    fn upstream<'a>(&'a self, task: &'a str) -> BTreeSet<&'a str> {
        let by_name: BTreeMap<&str, &PlanTaskWire> =
            self.graph.iter().map(|t| (t.name.as_str(), t)).collect();
        let mut seen = BTreeSet::new();
        let mut stack = vec![task];
        while let Some(name) = stack.pop() {
            if !seen.insert(name) {
                continue;
            }
            if let Some(node) = by_name.get(name) {
                stack.extend(node.depends_on.iter().map(String::as_str));
            }
        }
        seen
    }

    /// The newest attempt of each task.
    fn latest(&self) -> BTreeMap<&str, &TaskResult> {
        let mut latest: BTreeMap<&str, &TaskResult> = BTreeMap::new();
        for r in &self.results {
            match latest.get(r.task.as_str()) {
                Some(seen) if seen.iter >= r.iter => {}
                _ => {
                    latest.insert(r.task.as_str(), r);
                }
            }
        }
        latest
    }

    fn predicate(&self, result: &TaskResult, commit: &PushedCommit, now: &str) -> CommitPredicate {
        let upstream = self.upstream(&result.task);
        let latest = self.latest();
        let checks = self
            .graph
            .iter()
            .filter(|t| t.name != result.task && upstream.contains(t.name.as_str()))
            .filter(|t| matches!(t.kind.as_str(), "command" | "evaluate"))
            .filter_map(|t| {
                latest.get(t.name.as_str()).map(|r| CheckResult {
                    task: t.name.clone(),
                    status: r.status.clone(),
                })
            })
            .collect();
        let agents = self
            .graph
            .iter()
            .filter(|t| upstream.contains(t.name.as_str()))
            .filter_map(|t| {
                let r = if t.name == result.task {
                    result
                } else {
                    latest.get(t.name.as_str())?
                };
                let agent = r.agent.as_ref()?;
                Some(AgentRecord {
                    task: t.name.clone(),
                    harness: agent.harness.clone(),
                    model: agent.model.clone(),
                })
            })
            .collect();
        let pull_request = result
            .links
            .iter()
            .filter(|l| matches!(l.kind, LinkKind::PullRequest | LinkKind::MergeRequest))
            .map(|l| l.url.to_string())
            .find(|url| url.starts_with(&format!("{}/", commit.repository)));
        let dispatch = self.dispatch.clone().unwrap_or_default();
        CommitPredicate {
            run: self.run.run_id.clone(),
            task: result.task.clone(),
            iter: result.iter,
            playbook: self.playbook.clone(),
            plan: self.plan.clone(),
            schema: self.schema.clone(),
            agents,
            provider: dispatch.provider,
            sandbox_image: dispatch.sandbox_image.or_else(|| self.pinned_image()),
            cluster: dispatch
                .cluster
                .or_else(|| Some(self.run.location.cluster.clone())),
            pull_request,
            checks,
            signed_at: now.to_owned(),
        }
    }

    fn pinned_image(&self) -> Option<String> {
        pinned_image(
            self.run.image.reference.as_deref(),
            self.run.image.digest.as_deref(),
        )
    }
}

/// An image named by digest: the reference with the digest appended, unless it already carries
/// one. A reference without a resolved digest names no one image.
pub(crate) fn pinned_image(reference: Option<&str>, digest: Option<&str>) -> Option<String> {
    match (reference, digest) {
        (Some(r), _) if r.contains('@') => Some(r.to_owned()),
        (Some(r), Some(d)) => Some(format!("{r}@{d}")),
        (None, Some(d)) => Some(d.to_owned()),
        (_, None) => None,
    }
}

/// Sign and store a statement for every commit a passing task of `run_id` reported, skipping the
/// ones already attested. Returns the digests this call stored.
#[tracing::instrument(skip(pool, signer))]
pub async fn attest_run(
    pool: &PgPool,
    signer: &Signer,
    run_id: &str,
) -> Result<Vec<EnvelopeDigest>> {
    let Some(record) = RunRecord::read(pool, run_id).await? else {
        return Ok(Vec::new());
    };
    let keyid = signer.keyid().to_string();
    let mut stored = Vec::new();
    for result in record.results.iter().filter(|r| r.status == "pass") {
        for commit in PushedCommit::from_links(&result.links) {
            if attested(pool, run_id, result, &commit).await? {
                continue;
            }
            let now = crate::clock::now_rfc3339();
            let statement = Statement::new(&commit, record.predicate(result, &commit, &now));
            let signed = signer
                .sign(&statement)
                .context("signing a commit statement")?;
            insert(pool, run_id, result, &commit, &keyid, &signed, &now).await?;
            tracing::info!(
                run_id,
                task = %result.task,
                commit = %commit.sha,
                digest = %signed.digest,
                "commit provenance signed"
            );
            stored.push(signed.digest);
        }
    }
    Ok(stored)
}

#[cfg(test)]
mod tests;
