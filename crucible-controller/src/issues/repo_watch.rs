//! Raw SQL over the watched-repo set.

use anyhow::{Context, Result};
use sqlx::PgExecutor;

// --- runtime repo watch-set (Lane O3) -----------------------------------------------------------

/// Seed every repo in `repos` (the boot-time `cfg.repos` set). Idempotent; the first per-repo
/// failure propagates.
#[tracing::instrument(name = "db.seed_watched_repos", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub async fn seed_watched_repos(ex: impl PgExecutor<'_> + Copy, repos: &[String]) -> Result<()> {
    for repo in repos {
        let _ = insert_watched_repo(ex, repo, Some("env")).await?;
    }
    Ok(())
}

/// Insert a new watched row for `repo` (the `POST /api/repos` add path). Returns `false` (no rows
/// changed) if a row for this repo already exists — the caller's 409 signal — and `true` on a
/// fresh insert.
#[tracing::instrument(name = "db.insert_watched_repo", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", repo = %repo), err)]
pub(crate) async fn insert_watched_repo(
    ex: impl PgExecutor<'_>,
    repo: &str,
    added_by: Option<&str>,
) -> Result<bool> {
    let added_at = crate::clock::now_rfc3339();
    let result = sqlx::query!(
        r#"
        INSERT INTO repos (repo, watched, paused, added_by, added_at)
        VALUES ($1, TRUE, FALSE, $2, $3)
        ON CONFLICT(repo) DO NOTHING
        "#,
        repo,
        added_by,
        added_at,
    )
    .execute(ex)
    .await
    .context("insert_watched_repo")?;
    Ok(result.rows_affected() > 0)
}

/// Every repo discovery should poll this sweep: `watched AND NOT paused`, re-read fresh every
/// call so a newly-added or newly-resumed repo is picked up on the very next sweep, no restart.
#[tracing::instrument(name = "db.watched_repos", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn watched_repos(ex: impl PgExecutor<'_>) -> Result<Vec<String>> {
    let rows = sqlx::query!(
        r#"SELECT repo AS "repo!" FROM repos WHERE watched AND NOT paused ORDER BY repo"#
    )
    .fetch_all(ex)
    .await
    .context("watched_repos")?;
    Ok(rows.into_iter().map(|r| r.repo).collect())
}

/// The watched repos the closed-upstream repair pass still owes a full-listing sweep
/// (`closed_repaired_at IS NULL`) — its self-quiescing gate: a stamped repo costs zero GitHub
/// calls, mirroring `count_null_upstream_updated_at`'s role for the 0007 backfill.
#[tracing::instrument(name = "db.repos_needing_closed_repair", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn repos_needing_closed_repair(ex: impl PgExecutor<'_>) -> Result<Vec<String>> {
    let rows = sqlx::query!(
        r#"SELECT repo AS "repo!" FROM repos
           WHERE watched AND NOT paused AND closed_repaired_at IS NULL ORDER BY repo"#
    )
    .fetch_all(ex)
    .await
    .context("repos_needing_closed_repair")?;
    Ok(rows.into_iter().map(|r| r.repo).collect())
}

/// Stamp a repo's closed-upstream repair as done, quiescing every later pass for it.
#[tracing::instrument(name = "db.mark_closed_repaired", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", repo = %repo), err)]
pub(crate) async fn mark_closed_repaired(ex: impl PgExecutor<'_>, repo: &str) -> Result<()> {
    let at = crate::clock::now_rfc3339();
    sqlx::query!(
        "UPDATE repos SET closed_repaired_at = $1 WHERE repo = $2",
        at,
        repo,
    )
    .execute(ex)
    .await
    .context("mark_closed_repaired")?;
    Ok(())
}

/// One repo's watch state alone, or `None` if `repo` has never been seeded/added. Test-only
/// read-back: the pause/resume/unwatch handlers get their 404 from the UPDATE's `rows_affected`.
#[cfg(test)]
#[tracing::instrument(name = "db.get_repo_watch", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", repo = %repo), err)]
pub(crate) async fn get_repo_watch(
    ex: impl PgExecutor<'_>,
    repo: &str,
) -> Result<Option<crate::issues::model::RepoWatch>> {
    let row = sqlx::query!(
        r#"SELECT repo AS "repo!", watched, paused, added_by, added_at FROM repos WHERE repo = $1"#,
        repo,
    )
    .fetch_optional(ex)
    .await
    .context("get_repo_watch")?;
    Ok(row.map(|r| crate::issues::model::RepoWatch {
        repo: r.repo,
        watched: r.watched,
        paused: r.paused,
        added_by: r.added_by,
        added_at: r.added_at,
    }))
}

/// Flip `repo`'s `paused` flag. Returns whether a row matched (an unknown repo is a harmless miss
/// the caller turns into a 404).
#[tracing::instrument(name = "db.set_repo_paused", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", repo = %repo), err)]
pub(crate) async fn set_repo_paused(
    ex: impl PgExecutor<'_>,
    repo: &str,
    paused: bool,
) -> Result<bool> {
    let result = sqlx::query!("UPDATE repos SET paused = $1 WHERE repo = $2", paused, repo)
        .execute(ex)
        .await
        .context("set_repo_paused")?;
    Ok(result.rows_affected() > 0)
}

/// Flip `repo`'s `watched` flag (unwatch = `false`). The row (and every issue tied to it) is kept
/// either way — only the watch bit moves. Returns whether a row matched.
#[tracing::instrument(name = "db.set_repo_watched", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", repo = %repo), err)]
pub(crate) async fn set_repo_watched(
    ex: impl PgExecutor<'_>,
    repo: &str,
    watched: bool,
) -> Result<bool> {
    let result = sqlx::query!(
        "UPDATE repos SET watched = $1 WHERE repo = $2",
        watched,
        repo
    )
    .execute(ex)
    .await
    .context("set_repo_watched")?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    use anyhow::Result;
    use sqlx::PgPool;

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn insert_watched_repo_is_idempotent_and_never_overwrites(pool: PgPool) -> Result<()> {
        insert_watched_repo(&pool, "owner/repo", Some("env")).await?;
        let w = get_repo_watch(&pool, "owner/repo").await?.expect("seeded");
        assert!(w.watched);
        assert!(!w.paused);
        assert_eq!(w.added_by.as_deref(), Some("env"));
        let original_added_at = w.added_at.clone();
        assert!(original_added_at.is_some());

        // An admin pauses it — a re-seed (the next boot) must not clobber that.
        assert!(set_repo_paused(&pool, "owner/repo", true).await?);
        insert_watched_repo(&pool, "owner/repo", Some("env")).await?;
        let w = get_repo_watch(&pool, "owner/repo")
            .await?
            .expect("still there");
        assert!(w.paused, "re-seeding must not un-pause an admin's pause");
        assert_eq!(
            w.added_at, original_added_at,
            "re-seeding must not overwrite the original added_at"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn insert_watched_repo_reports_conflict_without_clobbering(pool: PgPool) -> Result<()> {
        assert!(insert_watched_repo(&pool, "owner/repo", Some("alice")).await?);
        assert!(
            !insert_watched_repo(&pool, "owner/repo", Some("bob")).await?,
            "a second insert reports the conflict"
        );
        let w = get_repo_watch(&pool, "owner/repo")
            .await?
            .expect("still there");
        assert_eq!(w.added_by.as_deref(), Some("alice"), "first writer wins");
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn watched_repos_excludes_paused_and_unwatched(pool: PgPool) -> Result<()> {
        for repo in ["a/one", "a/two", "a/three"] {
            insert_watched_repo(&pool, repo, Some("env")).await?;
        }
        assert!(set_repo_paused(&pool, "a/two", true).await?);
        assert!(set_repo_watched(&pool, "a/three", false).await?);

        let watched = watched_repos(&pool).await?;
        assert_eq!(watched, vec!["a/one".to_string()]);
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn resume_and_rewatch_restore_discovery_eligibility(pool: PgPool) -> Result<()> {
        insert_watched_repo(&pool, "a/one", Some("env")).await?;
        assert!(set_repo_paused(&pool, "a/one", true).await?);
        assert!(watched_repos(&pool).await?.is_empty());

        assert!(set_repo_paused(&pool, "a/one", false).await?);
        assert_eq!(watched_repos(&pool).await?, vec!["a/one".to_string()]);

        assert!(set_repo_watched(&pool, "a/one", false).await?);
        assert!(watched_repos(&pool).await?.is_empty());
        assert!(set_repo_watched(&pool, "a/one", true).await?);
        assert_eq!(watched_repos(&pool).await?, vec!["a/one".to_string()]);
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn set_repo_paused_and_watched_report_a_miss_for_an_unknown_repo(
        pool: PgPool,
    ) -> Result<()> {
        assert!(!set_repo_paused(&pool, "no/such-repo", true).await?);
        assert!(!set_repo_watched(&pool, "no/such-repo", false).await?);
        assert!(get_repo_watch(&pool, "no/such-repo").await?.is_none());
        Ok(())
    }
}
