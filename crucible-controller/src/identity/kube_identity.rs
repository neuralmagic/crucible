//! The Kubernetes username a person acts as on the controller's own cluster. The agent token
//! endpoint mints it as `kube_user`, and crucible MCP servers run SubjectAccessReview as that user,
//! so a run can do only what its launcher could.
//!
//! The value is only ever the API server's word (`users/~` answering for a cluster token the person
//! presented) or a platform administrator's override. A caller never names it for itself.

use anyhow::Context;
use sqlx::PgPool;

const MAX_LEN: usize = 253;

/// A Kubernetes username that may act for a person. `system:` identities (service accounts, nodes,
/// the API server's own) never do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KubeUser(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KubeUserError {
    #[error("kubernetes username {0:?} is empty, longer than {MAX_LEN}, or has whitespace")]
    Shape(String),
    #[error("kubernetes username {0:?} is a system identity, not a person")]
    System(String),
}

impl KubeUser {
    pub fn parse(raw: &str) -> Result<Self, KubeUserError> {
        if raw.is_empty()
            || raw.len() > MAX_LEN
            || raw.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(KubeUserError::Shape(raw.to_string()));
        }
        if raw.starts_with("system:") {
            return Err(KubeUserError::System(raw.to_string()));
        }
        Ok(Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Both values a person's row holds, and the one that applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KubeIdentity {
    pub learned: Option<String>,
    pub r#override: Option<String>,
}

impl KubeIdentity {
    /// The override when set, else the learned value.
    pub fn effective(&self) -> Option<&str> {
        self.r#override.as_deref().or(self.learned.as_deref())
    }
}

/// Record what the API server said `login` is. Writes only on change. Returns whether `login` has
/// a row.
pub async fn learn(pool: &PgPool, login: &str, user: &KubeUser) -> anyhow::Result<bool> {
    let login = login.trim().to_lowercase();
    let row: Option<bool> = sqlx::query_scalar(
        "WITH target AS (SELECT sub FROM users WHERE login = $1),
              changed AS (
                UPDATE users SET kube_user = $2, updated_at = $3
                 WHERE login = $1 AND kube_user IS DISTINCT FROM $2
                RETURNING 1)
         SELECT true FROM target",
    )
    .bind(&login)
    .bind(user.as_str())
    .bind(crate::clock::now_rfc3339())
    .fetch_optional(pool)
    .await
    .context("recording a learned kubernetes username")?;
    Ok(row.is_some())
}

/// [`learn`] for the row of issuer subject `sub`.
pub async fn learn_for_sub(pool: &PgPool, sub: &str, user: &KubeUser) -> anyhow::Result<bool> {
    let updated = sqlx::query("UPDATE users SET kube_user = $2, updated_at = $3 WHERE sub = $1")
        .bind(sub)
        .bind(user.as_str())
        .bind(crate::clock::now_rfc3339())
        .execute(pool)
        .await
        .context("recording a linked kubernetes username")?;
    Ok(updated.rows_affected() > 0)
}

/// Set or clear the administrator's override for `login`. Returns whether `login` has a row.
pub async fn set_override(
    pool: &PgPool,
    login: &str,
    user: Option<&KubeUser>,
) -> anyhow::Result<bool> {
    let updated =
        sqlx::query("UPDATE users SET kube_user_override = $2, updated_at = $3 WHERE login = $1")
            .bind(login.trim().to_lowercase())
            .bind(user.map(KubeUser::as_str))
            .bind(crate::clock::now_rfc3339())
            .execute(pool)
            .await
            .context("setting a kubernetes username override")?;
    Ok(updated.rows_affected() > 0)
}

/// What `login`'s row holds, `None` without a row.
pub async fn get(pool: &PgPool, login: &str) -> anyhow::Result<Option<KubeIdentity>> {
    let row: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT kube_user, kube_user_override FROM users WHERE login = $1")
            .bind(login.trim().to_lowercase())
            .fetch_optional(pool)
            .await
            .context("reading a kubernetes username")?;
    Ok(row.map(|(learned, r#override)| KubeIdentity {
        learned,
        r#override,
    }))
}

/// [`get`] for issuer subject `sub`.
pub async fn get_for_sub(pool: &PgPool, sub: &str) -> anyhow::Result<Option<KubeIdentity>> {
    let row: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT kube_user, kube_user_override FROM users WHERE sub = $1")
            .bind(sub)
            .fetch_optional(pool)
            .await
            .context("reading a kubernetes username")?;
    Ok(row.map(|(learned, r#override)| KubeIdentity {
        learned,
        r#override,
    }))
}

/// The Kubernetes username `login` acts as, `None` when unknown. A stored value that no longer
/// parses is an error, never silently dropped.
pub async fn effective(pool: &PgPool, login: &str) -> anyhow::Result<Option<KubeUser>> {
    match get(pool, login).await? {
        Some(identity) => identity
            .effective()
            .map(KubeUser::parse)
            .transpose()
            .with_context(|| format!("the stored kubernetes username for {login}")),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use crate::identity::kube_identity::*;

    async fn signed_in(pool: &PgPool, sub: &str, login: &str) {
        crate::identity::oidc::users::record_login(pool, sub, login, None, jiff::Timestamp::now())
            .await
            .unwrap();
    }

    fn user(raw: &str) -> KubeUser {
        KubeUser::parse(raw).unwrap()
    }

    #[test]
    fn a_person_may_act_as_any_username_but_a_system_identity() {
        for ok in ["wseaton", "kube:admin", "weaton@redhat.com", "b64:AbC="] {
            assert_eq!(KubeUser::parse(ok).unwrap().as_str(), ok);
        }
        for system in [
            "system:admin",
            "system:serviceaccount:crucible-system:crucible-spoke",
        ] {
            assert_eq!(
                KubeUser::parse(system),
                Err(KubeUserError::System(system.into()))
            );
        }
        for bad in ["", "two words", "tab\tbed", &"a".repeat(254)] {
            assert!(
                matches!(KubeUser::parse(bad), Err(KubeUserError::Shape(_))),
                "{bad:?}"
            );
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_learned_username_applies_until_an_override_replaces_it(pool: PgPool) {
        assert!(
            !learn(&pool, "weaton", &user("wseaton")).await.unwrap(),
            "no row yet"
        );
        assert_eq!(effective(&pool, "weaton").await.unwrap(), None);

        signed_in(&pool, "sub-weaton", "weaton").await;
        assert_eq!(
            effective(&pool, "weaton").await.unwrap(),
            None,
            "nothing learned yet"
        );
        assert!(learn(&pool, "Weaton ", &user("wseaton")).await.unwrap());
        assert_eq!(
            effective(&pool, "weaton").await.unwrap(),
            Some(user("wseaton"))
        );

        assert!(
            set_override(&pool, "weaton", Some(&user("weaton-admin")))
                .await
                .unwrap()
        );
        assert_eq!(
            get(&pool, "weaton").await.unwrap(),
            Some(KubeIdentity {
                learned: Some("wseaton".into()),
                r#override: Some("weaton-admin".into()),
            })
        );
        assert_eq!(
            effective(&pool, "weaton").await.unwrap(),
            Some(user("weaton-admin"))
        );
        learn(&pool, "weaton", &user("wseaton2")).await.unwrap();
        assert_eq!(
            effective(&pool, "weaton").await.unwrap(),
            Some(user("weaton-admin")),
            "a later learn never displaces the override"
        );

        assert!(set_override(&pool, "weaton", None).await.unwrap());
        assert_eq!(
            effective(&pool, "weaton").await.unwrap(),
            Some(user("wseaton2"))
        );
        assert!(
            !set_override(&pool, "nobody", Some(&user("x")))
                .await
                .unwrap()
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn linking_by_subject_lands_on_that_subjects_row(pool: PgPool) {
        signed_in(&pool, "sub-weaton", "weaton").await;
        assert!(
            learn_for_sub(&pool, "sub-weaton", &user("wseaton"))
                .await
                .unwrap()
        );
        assert!(
            !learn_for_sub(&pool, "sub-ghost", &user("wseaton"))
                .await
                .unwrap()
        );
        assert_eq!(
            get_for_sub(&pool, "sub-weaton")
                .await
                .unwrap()
                .unwrap()
                .effective(),
            Some("wseaton")
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_stored_system_identity_is_an_error_not_a_claim(pool: PgPool) {
        signed_in(&pool, "sub-weaton", "weaton").await;
        sqlx::query("UPDATE users SET kube_user = 'system:admin' WHERE login = 'weaton'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(effective(&pool, "weaton").await.is_err());
    }
}
