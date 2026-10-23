//! The Kubernetes username a person acts as on the controller's own cluster. The agent token
//! endpoint mints it as `kube_user`, and crucible MCP servers run SubjectAccessReview as that user,
//! so a run can do only what its launcher could.
//!
//! Per-cluster links live in `kube_identity_links`; this is a platform administrator's override for
//! the hub, which a caller never names for itself.

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

/// The administrator's override for issuer subject `sub`, which applies on the hub alone. A stored
/// value that no longer parses is an error, never silently dropped.
pub async fn override_for_sub(pool: &PgPool, sub: &str) -> anyhow::Result<Option<KubeUser>> {
    let raw: Option<Option<String>> =
        sqlx::query_scalar("SELECT kube_user_override FROM users WHERE sub = $1")
            .bind(sub)
            .fetch_optional(pool)
            .await
            .context("reading a kubernetes username override")?;
    raw.flatten()
        .map(|raw| KubeUser::parse(&raw))
        .transpose()
        .context("the stored hub username override")
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
        for ok in ["alice-k8s", "kube:admin", "alice@example.com", "b64:AbC="] {
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
    async fn an_override_lands_on_the_logins_row_and_clears_with_none(pool: PgPool) {
        assert!(
            !set_override(&pool, "alice", Some(&user("alice-admin")))
                .await
                .unwrap(),
            "no row yet"
        );
        signed_in(&pool, "sub-alice", "alice").await;
        assert_eq!(override_for_sub(&pool, "sub-alice").await.unwrap(), None);
        assert!(
            set_override(&pool, " Alice ", Some(&user("alice-admin")))
                .await
                .unwrap()
        );
        assert_eq!(
            override_for_sub(&pool, "sub-alice").await.unwrap(),
            Some(user("alice-admin"))
        );
        assert_eq!(override_for_sub(&pool, "sub-ghost").await.unwrap(), None);
        assert!(set_override(&pool, "alice", None).await.unwrap());
        assert_eq!(override_for_sub(&pool, "sub-alice").await.unwrap(), None);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_stored_system_identity_is_an_error_not_a_claim(pool: PgPool) {
        signed_in(&pool, "sub-alice", "alice").await;
        sqlx::query("UPDATE users SET kube_user_override = 'system:admin' WHERE login = 'alice'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(override_for_sub(&pool, "sub-alice").await.is_err());
    }
}
