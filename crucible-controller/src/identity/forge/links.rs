//! `forge_links`: each person's linked account per forge host.

use sqlx::PgPool;

use crate::identity::forge::{Forge, ForgeHost};

/// A person's linked account on one forge host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForgeLink {
    pub forge: Forge,
    pub host: ForgeHost,
    pub account_id: u64,
    pub login: String,
}

/// Why a link was not recorded.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LinkError {
    #[error("{forge} account {login} on {host} is linked to another crucible login", forge = .forge.as_str(), host = .host.as_str())]
    Taken {
        forge: Forge,
        host: ForgeHost,
        login: String,
    },
    #[error(transparent)]
    Store(#[from] sqlx::Error),
}

pub(crate) async fn link(pool: &PgPool, owner: &str, link: &ForgeLink) -> Result<(), LinkError> {
    let id = i64::try_from(link.account_id)
        .map_err(|e| LinkError::Store(sqlx::Error::Encode(Box::new(e))))?;
    let written = sqlx::query(
        "INSERT INTO forge_links (owner, forge, host, account_id, login) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (owner, forge, host) DO UPDATE SET account_id = EXCLUDED.account_id, \
         login = EXCLUDED.login, linked_at = now()",
    )
    .bind(owner.to_lowercase())
    .bind(link.forge.as_str())
    .bind(link.host.as_str())
    .bind(id)
    .bind(&link.login)
    .execute(pool)
    .await;
    match written {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(LinkError::Taken {
            forge: link.forge,
            host: link.host.clone(),
            login: link.login.clone(),
        }),
        Err(e) => Err(e.into()),
    }
}

type LinkRow = (String, String, i64, String);

fn from_row((forge, host, id, login): LinkRow) -> Option<ForgeLink> {
    Some(ForgeLink {
        forge: Forge::parse(&forge)?,
        host: ForgeHost::parse(&host)?,
        account_id: u64::try_from(id).ok()?,
        login,
    })
}

pub(crate) async fn get(
    pool: &PgPool,
    owner: &str,
    forge: Forge,
    host: &ForgeHost,
) -> Result<Option<ForgeLink>, sqlx::Error> {
    let row: Option<LinkRow> = sqlx::query_as(
        "SELECT forge, host, account_id, login FROM forge_links \
         WHERE owner = $1 AND forge = $2 AND host = $3",
    )
    .bind(owner.to_lowercase())
    .bind(forge.as_str())
    .bind(host.as_str())
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(from_row))
}

pub(crate) async fn list(pool: &PgPool, owner: &str) -> Result<Vec<ForgeLink>, sqlx::Error> {
    let rows: Vec<LinkRow> = sqlx::query_as(
        "SELECT forge, host, account_id, login FROM forge_links WHERE owner = $1 \
         ORDER BY forge, host",
    )
    .bind(owner.to_lowercase())
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().filter_map(from_row).collect())
}

pub(crate) async fn unlink(
    pool: &PgPool,
    owner: &str,
    forge: Forge,
    host: &ForgeHost,
) -> Result<bool, sqlx::Error> {
    let done = sqlx::query("DELETE FROM forge_links WHERE owner = $1 AND forge = $2 AND host = $3")
        .bind(owner.to_lowercase())
        .bind(forge.as_str())
        .bind(host.as_str())
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use crate::identity::forge::links::*;

    fn gitlab(host: &str, id: u64, login: &str) -> ForgeLink {
        ForgeLink {
            forge: Forge::Gitlab,
            host: ForgeHost::parse(host).unwrap(),
            account_id: id,
            login: login.into(),
        }
    }

    fn github(id: u64, login: &str) -> ForgeLink {
        ForgeLink {
            forge: Forge::Github,
            host: ForgeHost::github(),
            account_id: id,
            login: login.into(),
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_person_keeps_one_link_per_forge_host(pool: PgPool) {
        let gh = ForgeHost::github();
        assert_eq!(get(&pool, "wren", Forge::Github, &gh).await.unwrap(), None);
        link(&pool, "Wren", &github(7, "wren-gh")).await.unwrap();
        link(&pool, "wren", &gitlab("gitlab.com", 7, "wren-gl"))
            .await
            .unwrap();
        link(&pool, "wren", &gitlab("gl.example.com", 7, "wren-ex"))
            .await
            .unwrap();
        assert_eq!(
            get(&pool, "WREN", Forge::Github, &gh).await.unwrap(),
            Some(github(7, "wren-gh"))
        );
        let listed = list(&pool, "wren").await.unwrap();
        assert_eq!(
            listed,
            vec![
                github(7, "wren-gh"),
                gitlab("gitlab.com", 7, "wren-gl"),
                gitlab("gl.example.com", 7, "wren-ex"),
            ]
        );
        link(&pool, "wren", &github(8, "wren-alt")).await.unwrap();
        assert_eq!(
            get(&pool, "wren", Forge::Github, &gh).await.unwrap(),
            Some(github(8, "wren-alt"))
        );
        assert!(unlink(&pool, "wren", Forge::Github, &gh).await.unwrap());
        assert!(!unlink(&pool, "wren", Forge::Github, &gh).await.unwrap());
        assert_eq!(list(&pool, "wren").await.unwrap().len(), 2);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn one_forge_account_links_to_one_person(pool: PgPool) {
        link(&pool, "wren", &gitlab("gitlab.com", 7, "wren-gl"))
            .await
            .unwrap();
        let taken = link(&pool, "ash", &gitlab("gitlab.com", 7, "wren-gl"))
            .await
            .unwrap_err();
        assert!(matches!(taken, LinkError::Taken { ref login, .. } if login == "wren-gl"));
        link(&pool, "ash", &gitlab("gl.example.com", 7, "ash-ex"))
            .await
            .unwrap();
    }
}
