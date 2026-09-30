//! A run's display name, `benevolent-monkey`: two English words, reserved in `run_names` before
//! the run's pod is named. The run id stays the key every URL and record uses.

use anyhow::{Context, Result};
use sqlx::PgExecutor;

/// Two-word names first; three words once two-word draws keep colliding.
const DRAWS: [(u8, usize); 2] = [(2, 16), (3, 16)];

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    sqlx::Type,
)]
#[serde(transparent)]
#[sqlx(transparent)]
pub struct RunName(String);

impl RunName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RunName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RunNameError {
    #[error("the word lists produced no name")]
    NoWords,
    #[error("every drawn name was already taken")]
    Exhausted,
}

/// Draw names until one is not taken, and reserve it.
#[tracing::instrument(name = "db.reserve_run_name", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub(crate) async fn reserve(pool: &sqlx::PgPool) -> Result<RunName> {
    for (words, tries) in DRAWS {
        for _ in 0..tries {
            let name = petname::petname(words, "-").ok_or(RunNameError::NoWords)?;
            if claim(pool, &name).await? {
                return Ok(RunName(name));
            }
        }
    }
    Err(RunNameError::Exhausted.into())
}

async fn claim(ex: impl PgExecutor<'_>, name: &str) -> Result<bool> {
    let claimed = sqlx::query("INSERT INTO run_names (name) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(name)
        .execute(ex)
        .await
        .context("reserving a run name")?;
    Ok(claimed.rows_affected() == 1)
}

/// Give a run the name reserved for it. Once only: the ingest upsert rewrites the row on every
/// completion edge and never touches this column.
#[tracing::instrument(name = "db.set_run_name", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", %run_id), err)]
pub(crate) async fn set(ex: impl PgExecutor<'_>, run_id: &str, name: &RunName) -> Result<()> {
    sqlx::query("UPDATE runs SET name = $2 WHERE run_id = $1 AND name IS NULL")
        .bind(run_id)
        .bind(name.as_str())
        .execute(ex)
        .await
        .context("set_run_name")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::runs::names::{RunName, claim, reserve};

    #[sqlx::test(migrations = "./migrations")]
    async fn a_reserved_name_is_two_words_and_never_handed_out_twice(pool: sqlx::PgPool) {
        let name = reserve(&pool).await.unwrap();
        let words: Vec<&str> = name.as_str().split('-').collect();
        assert_eq!(words.len(), 2, "{name}");
        assert!(
            words
                .iter()
                .all(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_lowercase())),
            "{name}"
        );
        assert!(
            !claim(&pool, name.as_str()).await.unwrap(),
            "a reserved name cannot be claimed again"
        );
        let mut seen = std::collections::BTreeSet::from([name]);
        for _ in 0..200 {
            let next = reserve(&pool).await.unwrap();
            assert!(seen.insert(next.clone()), "{next} handed out twice");
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_run_keeps_the_first_name_it_is_given(pool: sqlx::PgPool) {
        let first = reserve(&pool).await.unwrap();
        let second = reserve(&pool).await.unwrap();
        sqlx::query("INSERT INTO runs (run_id, status) VALUES ('r1', 'running')")
            .execute(&pool)
            .await
            .unwrap();
        crate::runs::names::set(&pool, "r1", &first).await.unwrap();
        crate::runs::names::set(&pool, "r1", &second).await.unwrap();
        let name: Option<RunName> = sqlx::query_scalar("SELECT name FROM runs WHERE run_id = 'r1'")
            .fetch_one(&pool)
            .await
            .map(|n: Option<String>| n.map(RunName))
            .unwrap();
        assert_eq!(name, Some(first));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_name_nobody_reserved_cannot_be_given_to_a_run(pool: sqlx::PgPool) {
        sqlx::query("INSERT INTO runs (run_id, status) VALUES ('r1', 'running')")
            .execute(&pool)
            .await
            .unwrap();
        let unreserved = RunName("never-reserved".into());
        assert!(
            crate::runs::names::set(&pool, "r1", &unreserved)
                .await
                .is_err()
        );
    }
}
