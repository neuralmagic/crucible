//! The GitHub account each person linked by presenting one of its tokens (`github_links`), and,
//! on a controller that dispatches with the person's own GitHub access, the token itself, held
//! in memory only.

use std::sync::{Arc, RwLock};

use sqlx::PgPool;

/// A person's linked GitHub account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GithubLink {
    pub github_id: u64,
    pub github_login: String,
}

/// Why a link was not recorded.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LinkError {
    #[error("GitHub account {login} is linked to another crucible login")]
    Taken { login: String },
    #[error(transparent)]
    Store(#[from] sqlx::Error),
}

fn id_column(id: u64) -> Result<i64, LinkError> {
    i64::try_from(id).map_err(|e| LinkError::Store(sqlx::Error::Encode(Box::new(e))))
}

pub(crate) async fn link(pool: &PgPool, owner: &str, link: &GithubLink) -> Result<(), LinkError> {
    let id = id_column(link.github_id)?;
    let written = sqlx::query(
        "INSERT INTO github_links (owner, github_id, github_login) VALUES ($1, $2, $3) \
         ON CONFLICT (owner) DO UPDATE SET github_id = EXCLUDED.github_id, \
         github_login = EXCLUDED.github_login, linked_at = now()",
    )
    .bind(owner.to_lowercase())
    .bind(id)
    .bind(&link.github_login)
    .execute(pool)
    .await;
    match written {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(LinkError::Taken {
            login: link.github_login.clone(),
        }),
        Err(e) => Err(e.into()),
    }
}

pub(crate) async fn get(pool: &PgPool, owner: &str) -> Result<Option<GithubLink>, sqlx::Error> {
    let row: Option<(i64, String)> =
        sqlx::query_as("SELECT github_id, github_login FROM github_links WHERE owner = $1")
            .bind(owner.to_lowercase())
            .fetch_optional(pool)
            .await?;
    Ok(row.and_then(|(id, github_login)| {
        u64::try_from(id).ok().map(|github_id| GithubLink {
            github_id,
            github_login,
        })
    }))
}

pub(crate) async fn unlink(pool: &PgPool, owner: &str) -> Result<bool, sqlx::Error> {
    let done = sqlx::query("DELETE FROM github_links WHERE owner = $1")
        .bind(owner.to_lowercase())
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// The GitHub token the person last linked, on a controller that dispatches as that person.
/// Clones share one slot.
#[derive(Clone, Default)]
pub struct SignedIn(Arc<RwLock<Option<String>>>);

impl SignedIn {
    pub(crate) fn set(&self, token: Option<String>) {
        if let Ok(mut slot) = self.0.write() {
            *slot = token;
        }
    }

    pub(crate) fn token(&self) -> Option<forge::github::Token> {
        self.0
            .read()
            .ok()
            .and_then(|slot| slot.clone())
            .map(forge::github::Token::new)
    }
}

impl std::fmt::Debug for SignedIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let held = self.0.read().is_ok_and(|slot| slot.is_some());
        write!(f, "SignedIn({})", if held { ".." } else { "none" })
    }
}

#[cfg(test)]
mod tests {
    use crate::identity::github_links::*;

    fn wren() -> GithubLink {
        GithubLink {
            github_id: 7,
            github_login: "wren-gh".into(),
        }
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_link_is_kept_per_person_and_replaced_by_the_next(pool: PgPool) {
        assert_eq!(get(&pool, "wren").await.unwrap(), None);
        link(&pool, "Wren", &wren()).await.unwrap();
        assert_eq!(get(&pool, "wren").await.unwrap(), Some(wren()));
        let other = GithubLink {
            github_id: 8,
            github_login: "wren-alt".into(),
        };
        link(&pool, "wren", &other).await.unwrap();
        assert_eq!(get(&pool, "WREN").await.unwrap(), Some(other));
        assert!(unlink(&pool, "wren").await.unwrap());
        assert!(!unlink(&pool, "wren").await.unwrap());
        assert_eq!(get(&pool, "wren").await.unwrap(), None);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn one_github_account_links_to_one_person(pool: PgPool) {
        link(&pool, "wren", &wren()).await.unwrap();
        let taken = link(&pool, "ash", &wren()).await.unwrap_err();
        assert!(matches!(taken, LinkError::Taken { ref login } if login == "wren-gh"));
        assert_eq!(get(&pool, "ash").await.unwrap(), None);
    }

    #[test]
    fn a_signed_in_token_is_shared_by_clones_and_never_formatted() {
        let slot = SignedIn::default();
        let other = slot.clone();
        assert!(other.token().is_none());
        slot.set(Some("ghu_secret".into()));
        assert!(other.token().is_some());
        assert_eq!(format!("{other:?}"), "SignedIn(..)");
        slot.set(None);
        assert!(other.token().is_none());
        assert_eq!(format!("{other:?}"), "SignedIn(none)");
    }
}
