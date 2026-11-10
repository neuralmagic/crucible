//! What the people grant reads about a subject: whether they may still sign in, and who they are
//! on a cluster. [`ControllerDirectory`] answers both from the controller's own tables.

use crate::agent_auth::people::Approved;
use crate::identity::cluster_identity::{self, Person, Resolution};
use anyhow::Context;
use forge::fleet::ClusterIdentity;
use sqlx::PgPool;
use std::collections::BTreeMap;

/// A subject who may still sign in, as their login recorded them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allowed {
    pub login: String,
    pub email: Option<String>,
}

/// Who a person is on a cluster, or why that is not known yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClusterUser {
    /// The Kubernetes username, already checked to be one a person may act as.
    Known(String),
    /// The cluster's rule is `link` and the person has not linked.
    Unlinked,
    /// The cluster's rule names a claim this login did not carry, or one that is no username.
    Unavailable(String),
}

pub struct ControllerDirectory {
    pool: PgPool,
    rules: BTreeMap<String, ClusterIdentity>,
}

impl ControllerDirectory {
    /// The rules come from the deploy profile once; a cluster it does not list is `link`.
    pub fn new(pool: PgPool, profile: &crucible::deploy::DeployProfile) -> Self {
        Self {
            pool,
            rules: profile
                .cluster_identities()
                .into_iter()
                .map(|(cluster, rule)| (cluster.to_string(), rule.clone()))
                .collect(),
        }
    }
}

impl ControllerDirectory {
    /// A subject whose row exists. `None` when the subject is gone.
    pub async fn allowed(&self, sub: &str) -> anyhow::Result<Option<Allowed>> {
        let row: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT login, email FROM users WHERE sub = $1")
                .bind(sub)
                .fetch_optional(&self.pool)
                .await
                .context("reading the person behind an approval")?;
        Ok(row.map(|(login, email)| Allowed { login, email }))
    }

    /// The username `approved` acts as on its cluster, read fresh on every call.
    pub async fn kube_user(&self, approved: &Approved) -> anyhow::Result<ClusterUser> {
        let rule = self
            .rules
            .get(&approved.cluster)
            .cloned()
            .unwrap_or_default();
        let person = Person {
            sub: &approved.sub,
            login: &approved.login,
            email: approved.email.as_deref(),
        };
        Ok(
            match cluster_identity::resolve(&self.pool, &approved.cluster, &rule, person).await? {
                Resolution::Known { user, .. } => ClusterUser::Known(user.as_str().to_string()),
                Resolution::Unlinked => ClusterUser::Unlinked,
                Resolution::Unavailable(why) => ClusterUser::Unavailable(why),
            },
        )
    }
}
