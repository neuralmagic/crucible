//! The `draft_promotions` rows, and the draft's graduation columns that registration reads to
//! retire the draft once its promoted pack is registered.

use anyhow::{Context, Result};
use sqlx::{PgPool, Row};

use crate::identity::forge::{Forge, ForgeHost};
use crate::playbooks::promotion::{Promotion, PromotionState};

const COLUMNS: &str = "draft_id, forge, host, project, path, base_branch, branch, number, url, \
                       version, digest, author, promoted_by, state, note, etag, created_at, \
                       updated_at";

fn decode(row: &sqlx::postgres::PgRow) -> Result<Promotion> {
    let forge: String = row.try_get("forge")?;
    let host: String = row.try_get("host")?;
    let state: String = row.try_get("state")?;
    let number: i64 = row.try_get("number")?;
    Ok(Promotion {
        draft: row.try_get("draft_id")?,
        forge: match forge.as_str() {
            "github" => Some(Forge::Github),
            "gitlab" => Some(Forge::Gitlab),
            _ => None,
        }
        .with_context(|| format!("a promotion names forge {forge:?}"))?,
        host: ForgeHost::parse(&host)
            .with_context(|| format!("a promotion names host {host:?}"))?,
        project: row.try_get("project")?,
        path: row.try_get("path")?,
        base: row.try_get("base_branch")?,
        branch: row.try_get("branch")?,
        number: u64::try_from(number).context("a promotion's number is negative")?,
        url: row.try_get("url")?,
        version: row.try_get("version")?,
        digest: row.try_get("digest")?,
        author: row.try_get("author")?,
        promoted_by: row.try_get("promoted_by")?,
        state: PromotionState::parse(&state)
            .with_context(|| format!("a promotion is in state {state:?}"))?,
        note: row.try_get("note")?,
        etag: row.try_get("etag")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

pub(crate) async fn record(pool: &PgPool, p: &Promotion, first_look: &str) -> Result<()> {
    let mut tx = pool.begin().await.context("opening a promotion write")?;
    sqlx::query(const_format::formatcp!(
        "INSERT INTO draft_promotions ({COLUMNS}, next_poll_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, NULL, $16, $16, $17) \
         ON CONFLICT (draft_id) DO UPDATE SET forge = $2, host = $3, project = $4, path = $5, \
           base_branch = $6, branch = $7, number = $8, url = $9, version = $10, digest = $11, \
           author = $12, promoted_by = $13, state = $14, note = $15, etag = NULL, \
           updated_at = $16, next_poll_at = $17"
    ))
    .bind(&p.draft)
    .bind(p.forge.as_str())
    .bind(p.host.as_str())
    .bind(&p.project)
    .bind(&p.path)
    .bind(&p.base)
    .bind(&p.branch)
    .bind(i64::try_from(p.number).context("a promotion number past i64")?)
    .bind(&p.url)
    .bind(p.version)
    .bind(&p.digest)
    .bind(&p.author)
    .bind(&p.promoted_by)
    .bind(p.state.as_str())
    .bind(&p.note)
    .bind(&p.updated_at)
    .bind(first_look)
    .execute(&mut *tx)
    .await
    .context("recording a promotion")?;
    sqlx::query(
        "UPDATE playbook_drafts SET graduation_repo = $2, graduation_path = $3, \
         graduation_pr_url = $4, updated_at = $5 WHERE id = $1",
    )
    .bind(&p.draft)
    .bind(p.registry_repo())
    .bind(&p.path)
    .bind(&p.url)
    .bind(&p.updated_at)
    .execute(&mut *tx)
    .await
    .context("marking the draft promoted")?;
    tx.commit().await.context("committing a promotion")?;
    Ok(())
}

pub(crate) async fn get(pool: &PgPool, draft: &str) -> Result<Option<Promotion>> {
    let row = sqlx::query(const_format::formatcp!(
        "SELECT {COLUMNS} FROM draft_promotions WHERE draft_id = $1"
    ))
    .bind(draft)
    .fetch_optional(pool)
    .await
    .context("reading a promotion")?;
    row.as_ref().map(decode).transpose()
}

const FOLLOWED: &str = "state IN ('open', 'merged')";

pub(crate) async fn earliest_due(pool: &PgPool) -> Result<Option<String>> {
    sqlx::query_scalar(const_format::formatcp!(
        "SELECT MIN(COALESCE(next_poll_at, updated_at)) FROM draft_promotions WHERE {FOLLOWED}"
    ))
    .fetch_one(pool)
    .await
    .context("reading when the next promotion is due")
}

pub(crate) async fn due(pool: &PgPool, now: &str, limit: i64) -> Result<Vec<Promotion>> {
    let rows = sqlx::query(const_format::formatcp!(
        "SELECT {COLUMNS} FROM draft_promotions \
         WHERE {FOLLOWED} AND COALESCE(next_poll_at, updated_at) <= $1 \
         ORDER BY COALESCE(next_poll_at, updated_at), draft_id LIMIT $2"
    ))
    .bind(now)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("listing the promotions due a look")?;
    rows.iter().map(decode).collect()
}

pub(crate) async fn looked(
    pool: &PgPool,
    draft: &str,
    etag: Option<&str>,
    next: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE draft_promotions SET etag = COALESCE($2, etag), next_poll_at = $3 \
         WHERE draft_id = $1",
    )
    .bind(draft)
    .bind(etag)
    .bind(next)
    .execute(pool)
    .await
    .context("scheduling a promotion's next look")?;
    Ok(())
}

pub(crate) async fn moved(
    pool: &PgPool,
    draft: &str,
    state: PromotionState,
    note: &str,
) -> Result<()> {
    let now = crate::clock::now_rfc3339();
    let mut tx = pool.begin().await.context("opening a promotion update")?;
    sqlx::query(
        "UPDATE draft_promotions SET state = $2, note = $3, updated_at = $4 WHERE draft_id = $1",
    )
    .bind(draft)
    .bind(state.as_str())
    .bind(note)
    .bind(&now)
    .execute(&mut *tx)
    .await
    .context("moving a promotion")?;
    if state == PromotionState::Closed {
        sqlx::query(
            "UPDATE playbook_drafts SET graduation_repo = NULL, graduation_path = NULL, \
             graduation_pr_url = NULL, updated_at = $2 WHERE id = $1 AND retired_at IS NULL",
        )
        .bind(draft)
        .bind(&now)
        .execute(&mut *tx)
        .await
        .context("reopening a draft whose promotion closed")?;
    }
    tx.commit().await.context("committing a promotion update")?;
    Ok(())
}
