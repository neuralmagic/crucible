use anyhow::{Context, Result};
use sqlx::{PgExecutor, Row};

use crate::images::model::{CatalogImage, RepositoryStatus};

fn image_from_row(row: &sqlx::postgres::PgRow) -> Result<CatalogImage> {
    let tags: serde_json::Value = row.try_get("tags")?;
    let arches: serde_json::Value = row.try_get("arches")?;
    let capabilities: Option<serde_json::Value> = row.try_get("capabilities")?;
    Ok(CatalogImage {
        repository: row.try_get("repository")?,
        digest: row.try_get("digest")?,
        tags: serde_json::from_value(tags).context("decoding tags")?,
        arches: serde_json::from_value(arches).context("decoding arches")?,
        created_at: row.try_get("created_at")?,
        capabilities: capabilities
            .map(serde_json::from_value)
            .transpose()
            .context("decoding capabilities")?,
        capability_digest: row.try_get("capability_digest")?,
        intro_digest: row.try_get("intro_digest")?,
        first_seen: row.try_get("first_seen")?,
        last_seen: row.try_get("last_seen")?,
    })
}

const IMAGE_COLS: &str = "repository, digest, tags, arches, created_at, capabilities, capability_digest, intro_digest, first_seen, last_seen";

/// Insert or refresh one image. A known digest keeps its `first_seen` and the capability document
/// it was catalogued with; tags, arches and `last_seen` follow the sweep.
#[tracing::instrument(name = "db.upsert_catalog_image", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", repository = %image.repository), err)]
pub(crate) async fn upsert_image(ex: impl PgExecutor<'_>, image: &CatalogImage) -> Result<()> {
    let capabilities = image
        .capabilities
        .as_ref()
        .map(serde_json::to_value)
        .transpose()?;
    sqlx::query(
        r#"
        INSERT INTO catalog_images
            (repository, digest, tags, arches, created_at, capabilities, capability_digest, intro_digest, first_seen, last_seen)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT (repository, digest) DO UPDATE SET
            tags = EXCLUDED.tags,
            arches = EXCLUDED.arches,
            last_seen = EXCLUDED.last_seen
        "#,
    )
    .bind(&image.repository)
    .bind(&image.digest)
    .bind(serde_json::to_value(&image.tags)?)
    .bind(serde_json::to_value(&image.arches)?)
    .bind(&image.created_at)
    .bind(capabilities)
    .bind(&image.capability_digest)
    .bind(&image.intro_digest)
    .bind(&image.first_seen)
    .bind(&image.last_seen)
    .execute(ex)
    .await
    .context("upsert_catalog_image")?;
    Ok(())
}

/// Refresh a known digest's tags and `last_seen`; its description stays cached.
#[tracing::instrument(name = "db.touch_catalog_image", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", repository), err)]
pub(crate) async fn touch_image(
    ex: impl PgExecutor<'_>,
    repository: &str,
    digest: &str,
    tags: &[String],
    now: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE catalog_images SET tags = $3, last_seen = $4 WHERE repository = $1 AND digest = $2",
    )
    .bind(repository)
    .bind(digest)
    .bind(serde_json::to_value(tags)?)
    .bind(now)
    .execute(ex)
    .await
    .context("touch_catalog_image")?;
    Ok(())
}

/// Drop a repository's rows whose digest no tag points at any more.
#[tracing::instrument(name = "db.prune_catalog_images", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", repository), err)]
pub(crate) async fn prune_images(
    ex: impl PgExecutor<'_>,
    repository: &str,
    keep: &[String],
) -> Result<u64> {
    let res =
        sqlx::query("DELETE FROM catalog_images WHERE repository = $1 AND NOT (digest = ANY($2))")
            .bind(repository)
            .bind(keep)
            .execute(ex)
            .await
            .context("prune_catalog_images")?;
    Ok(res.rows_affected())
}

/// The digests already catalogued for a repository, so a sweep only describes new ones.
#[tracing::instrument(name = "db.catalog_digests", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", repository), err)]
pub(crate) async fn known_digests(
    ex: impl PgExecutor<'_>,
    repository: &str,
) -> Result<Vec<String>> {
    let rows = sqlx::query("SELECT digest FROM catalog_images WHERE repository = $1")
        .bind(repository)
        .fetch_all(ex)
        .await
        .context("catalog_digests")?;
    rows.iter().map(|r| Ok(r.try_get("digest")?)).collect()
}

/// Every catalogued image, newest first within a repository.
#[tracing::instrument(name = "db.list_catalog_images", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn list_images(ex: impl PgExecutor<'_>) -> Result<Vec<CatalogImage>> {
    let rows = sqlx::query(&format!(
        "SELECT {IMAGE_COLS} FROM catalog_images ORDER BY repository, created_at DESC NULLS LAST, digest"
    ))
    .fetch_all(ex)
    .await
    .context("list_catalog_images")?;
    rows.iter().map(image_from_row).collect()
}

#[tracing::instrument(name = "db.upsert_catalog_repository", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", repository = %status.repository), err)]
pub(crate) async fn upsert_repository(
    ex: impl PgExecutor<'_>,
    status: &RepositoryStatus,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO catalog_repositories (repository, last_polled, last_ok, last_error)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (repository) DO UPDATE SET
            last_polled = EXCLUDED.last_polled,
            last_ok = COALESCE(EXCLUDED.last_ok, catalog_repositories.last_ok),
            last_error = EXCLUDED.last_error
        "#,
    )
    .bind(&status.repository)
    .bind(&status.last_polled)
    .bind(&status.last_ok)
    .bind(&status.last_error)
    .execute(ex)
    .await
    .context("upsert_catalog_repository")?;
    Ok(())
}

#[tracing::instrument(name = "db.list_catalog_repositories", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn list_repositories(ex: impl PgExecutor<'_>) -> Result<Vec<RepositoryStatus>> {
    let rows = sqlx::query(
        "SELECT repository, last_polled, last_ok, last_error FROM catalog_repositories ORDER BY repository",
    )
    .fetch_all(ex)
    .await
    .context("list_catalog_repositories")?;
    rows.iter()
        .map(|r| {
            Ok(RepositoryStatus {
                repository: r.try_get("repository")?,
                last_polled: r.try_get("last_polled")?,
                last_ok: r.try_get("last_ok")?,
                last_error: r.try_get("last_error")?,
            })
        })
        .collect()
}
