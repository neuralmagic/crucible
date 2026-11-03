//! The people grant assembled from the controller's own Postgres and deploy profile.

use crate::agent_auth::people::directory::ControllerDirectory;
use sqlx::PgPool;

/// The people grant as the controller runs it: grants in its Postgres, subjects and links read
/// from its tables, metadata documents fetched over the public internet.
pub fn people(
    config: crate::agent_auth::people::PeopleConfig,
    pool: PgPool,
    profile: &crucible::deploy::DeployProfile,
) -> anyhow::Result<crate::agent_auth::people::People> {
    use crate::agent_auth::people::{cimd::Cimd, dynamic::Dcr, registry::Clients};
    let directory = std::sync::Arc::new(ControllerDirectory::new(pool.clone(), profile));
    let cimd = config
        .cimd
        .clone()
        .map(|policy| Cimd::new(policy, reqwest::Client::builder()))
        .transpose()?;
    Ok(crate::agent_auth::people::People {
        clients: Clients::new(
            config.clients.clone(),
            cimd,
            config
                .dcr
                .clone()
                .map(|policy| std::sync::Arc::new(Dcr::new(pool.clone(), policy))),
        ),
        config,
        grants: crate::agent_auth::people::grants::PgGrantStore::new(pool),
        directory,
    })
}
