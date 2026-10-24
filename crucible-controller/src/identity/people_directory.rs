//! The controller's answers to the people grant: who may still sign in, from `users` and the stored
//! credential, and who a person is on a cluster, from the deploy profile's rules and the links.

use crate::agent_auth::people::Approved;
use crate::agent_auth::people::directory::{Allowed, ClusterUser, LinkDirectory, SubjectDirectory};
use crate::identity::cluster_identity::{self, Person, Resolution};
use anyhow::Context;
use forge::fleet::ClusterIdentity;
use sqlx::PgPool;
use std::collections::BTreeMap;

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

#[async_trait::async_trait]
impl SubjectDirectory for ControllerDirectory {
    /// A subject whose row exists and whose stored credential the identity provider has not
    /// refused: the condition that downgrades a browser session.
    async fn allowed(&self, sub: &str) -> anyhow::Result<Option<Allowed>> {
        let row: Option<(String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT u.login, u.email, c.last_error FROM users u
               LEFT JOIN user_credentials c ON c.sub = u.sub
              WHERE u.sub = $1",
        )
        .bind(sub)
        .fetch_optional(&self.pool)
        .await
        .context("reading the person behind an approval")?;
        Ok(row
            .filter(|(_, _, refused)| refused.is_none())
            .map(|(login, email, _)| Allowed { login, email }))
    }
}

#[async_trait::async_trait]
impl LinkDirectory for ControllerDirectory {
    async fn kube_user(&self, approved: &Approved) -> anyhow::Result<ClusterUser> {
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

/// The people grant as the controller runs it: grants in its Postgres, subjects and links read
/// from its tables, metadata documents fetched over the public internet.
pub fn people(
    config: crate::agent_auth::people::PeopleConfig,
    pool: PgPool,
    profile: &crucible::deploy::DeployProfile,
) -> anyhow::Result<crate::agent_auth::people::People> {
    use crate::agent_auth::people::{cimd::Cimd, clients::Clients};
    let directory = std::sync::Arc::new(ControllerDirectory::new(pool.clone(), profile));
    let cimd = config
        .cimd
        .clone()
        .map(|policy| Cimd::new(policy, reqwest::Client::builder()))
        .transpose()?;
    Ok(crate::agent_auth::people::People {
        clients: Clients::new(config.clients.clone(), cimd),
        config,
        grants: std::sync::Arc::new(crate::agent_auth::people::grants::PgGrantStore::new(pool)),
        subjects: directory.clone(),
        links: directory,
    })
}
