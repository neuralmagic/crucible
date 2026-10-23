//! The Kubernetes username a person acts as on each cluster, minted into people's access tokens as
//! `kube_user` for MCP servers on that cluster.
//!
//! A row is only ever that cluster's own TokenReview answer for a token the person presented, typed
//! or redeemed through the cluster's OAuth server. The token is reviewed and dropped, never stored
//! or logged; a token another cluster minted does not authenticate here, so it cannot link this
//! cluster. The hub's link is also the username agent tokens for the person's runs carry.

use crate::daemon::queue::BoxFuture;
use crate::identity::kube_identity::{KubeUser, KubeUserError};
use crate::runs::clusters::ClusterClients;
use anyhow::Context;
use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec, TokenReviewStatus};
use kube::Api;
use kube::api::PostParams;
use sqlx::PgPool;
use std::sync::Arc;

/// How a link was proven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkSource {
    /// A token the person pasted.
    TokenReview,
    /// A token the cluster's OAuth server issued to the controller for the person.
    OpenshiftOauth,
}

impl LinkSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::TokenReview => "token-review",
            Self::OpenshiftOauth => "openshift-oauth",
        }
    }

    fn parse(raw: &str) -> Result<Self, UnknownLinkSource> {
        match raw {
            "token-review" => Ok(Self::TokenReview),
            "openshift-oauth" => Ok(Self::OpenshiftOauth),
            other => Err(UnknownLinkSource(other.to_string())),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("stored link source {0:?} is not token-review or openshift-oauth")]
pub struct UnknownLinkSource(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub cluster: String,
    pub kube_user: KubeUser,
    pub source: LinkSource,
    pub linked_at: String,
}

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("cluster {cluster:?} did not accept the presented token")]
    NotAuthenticated { cluster: String },
    #[error("cluster {cluster:?} accepted the token but named no user")]
    NoUser { cluster: String },
    #[error(transparent)]
    User(#[from] KubeUserError),
    #[error("cluster {cluster:?} could not review the token: {detail}")]
    Unreachable { cluster: String, detail: String },
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

/// TokenReview against one cluster's API server.
pub trait ClusterReviewer: Send + Sync {
    fn review(&self, cluster: &str, token: String) -> BoxFuture<Result<TokenReviewStatus, String>>;
}

/// The production reviewer, over the controller's per-cluster clients. A cluster without one fails
/// closed as unreachable.
pub struct KubeReviewer(pub Arc<ClusterClients>);

impl ClusterReviewer for KubeReviewer {
    fn review(&self, cluster: &str, token: String) -> BoxFuture<Result<TokenReviewStatus, String>> {
        let clusters = self.0.clone();
        let cluster = cluster.to_string();
        Box::pin(async move {
            let client = clusters
                .client(&cluster)
                .await
                .map_err(|e| format!("building the kube client: {e:#}"))?;
            let review = TokenReview {
                metadata: Default::default(),
                spec: TokenReviewSpec {
                    token: Some(token),
                    audiences: None,
                },
                status: None,
            };
            Api::<TokenReview>::all(client)
                .create(&PostParams::default(), &review)
                .await
                .map_err(|e| format!("TokenReview failed: {e}"))?
                .status
                .ok_or_else(|| "TokenReview returned no status".to_string())
        })
    }
}

/// Review `token` on `cluster` and record the user it names as `sub`'s username there.
pub async fn link(
    pool: &PgPool,
    reviewer: &dyn ClusterReviewer,
    sub: &str,
    cluster: &str,
    token: &str,
    source: LinkSource,
) -> Result<KubeUser, LinkError> {
    let status = reviewer
        .review(cluster, token.trim().to_string())
        .await
        .map_err(|detail| LinkError::Unreachable {
            cluster: cluster.to_string(),
            detail,
        })?;
    if status.authenticated != Some(true) {
        return Err(LinkError::NotAuthenticated {
            cluster: cluster.to_string(),
        });
    }
    let username = status
        .user
        .and_then(|u| u.username)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| LinkError::NoUser {
            cluster: cluster.to_string(),
        })?;
    let user = KubeUser::parse(&username)?;
    upsert(pool, sub, cluster, &user, source).await?;
    tracing::info!(
        sub,
        cluster,
        kube_user = user.as_str(),
        source = source.as_str(),
        "kubernetes username linked"
    );
    Ok(user)
}

async fn upsert(
    pool: &PgPool,
    sub: &str,
    cluster: &str,
    user: &KubeUser,
    source: LinkSource,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO kube_identity_links (sub, cluster, kube_user, source, linked_at)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (sub, cluster) DO UPDATE
            SET kube_user = EXCLUDED.kube_user, source = EXCLUDED.source,
                linked_at = EXCLUDED.linked_at",
    )
    .bind(sub)
    .bind(cluster)
    .bind(user.as_str())
    .bind(source.as_str())
    .bind(crate::clock::now_rfc3339())
    .execute(pool)
    .await
    .context("recording a kubernetes identity link")?;
    Ok(())
}

/// Forget `sub`'s username on `cluster`. Returns whether there was one. An administrator's
/// override is not a link and stays.
pub async fn unlink(pool: &PgPool, sub: &str, cluster: &str) -> anyhow::Result<bool> {
    let removed = sqlx::query("DELETE FROM kube_identity_links WHERE sub = $1 AND cluster = $2")
        .bind(sub)
        .bind(cluster)
        .execute(pool)
        .await
        .context("removing a kubernetes identity link")?
        .rows_affected();
    Ok(removed > 0)
}

/// `sub`'s username on `cluster`. A stored value that no longer parses is an error.
pub async fn get(pool: &PgPool, sub: &str, cluster: &str) -> anyhow::Result<Option<KubeUser>> {
    let row: Option<String> = sqlx::query_scalar(
        "SELECT kube_user FROM kube_identity_links WHERE sub = $1 AND cluster = $2",
    )
    .bind(sub)
    .bind(cluster)
    .fetch_optional(pool)
    .await
    .context("reading a kubernetes identity link")?;
    row.map(|u| KubeUser::parse(&u))
        .transpose()
        .context("a stored kubernetes identity link no longer parses")
}

/// Every link `sub` holds, by cluster. A stored row that no longer parses is an error.
pub async fn list(pool: &PgPool, sub: &str) -> anyhow::Result<Vec<Link>> {
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT cluster, kube_user, source, linked_at FROM kube_identity_links
          WHERE sub = $1 ORDER BY cluster",
    )
    .bind(sub)
    .fetch_all(pool)
    .await
    .context("listing kubernetes identity links")?;
    rows.into_iter()
        .map(|(cluster, kube_user, source, linked_at)| {
            Ok(Link {
                kube_user: KubeUser::parse(&kube_user)
                    .with_context(|| format!("the stored link for cluster {cluster}"))?,
                source: LinkSource::parse(&source)?,
                cluster,
                linked_at,
            })
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod testing {
    use crate::daemon::queue::BoxFuture;
    use crate::identity::kube_links::ClusterReviewer;
    use k8s_openapi::api::authentication::v1::{TokenReviewStatus, UserInfo};
    use std::collections::HashMap;

    /// Clusters that each accept their own tokens, as their API servers would: a token is valid
    /// only on the cluster that minted it.
    pub(crate) struct Clusters {
        pub(crate) tokens: HashMap<(String, String), String>,
        pub(crate) down: Vec<String>,
    }

    impl Clusters {
        pub(crate) fn with(entries: &[(&str, &str, &str)]) -> Self {
            Self {
                tokens: entries
                    .iter()
                    .map(|(c, t, u)| ((c.to_string(), t.to_string()), u.to_string()))
                    .collect(),
                down: Vec::new(),
            }
        }
    }

    impl ClusterReviewer for Clusters {
        fn review(
            &self,
            cluster: &str,
            token: String,
        ) -> BoxFuture<Result<TokenReviewStatus, String>> {
            let result = match self.down.iter().any(|d| d == cluster) {
                true => Err("connection refused".to_string()),
                false => Ok(
                    match self.tokens.get(&(cluster.to_string(), token.clone())) {
                        Some(user) => TokenReviewStatus {
                            authenticated: Some(true),
                            user: Some(UserInfo {
                                username: Some(user.clone()),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                        None => TokenReviewStatus {
                            authenticated: Some(false),
                            error: Some("token lookup failed".into()),
                            ..Default::default()
                        },
                    },
                ),
            };
            Box::pin(async move { result })
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::identity::kube_links::testing::Clusters;
    use crate::identity::kube_links::*;

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_reviewed_token_records_the_username_the_cluster_named(pool: PgPool) {
        let clusters = Clusters::with(&[("pirate", "sha256~pirate", "casey")]);
        let user = link(
            &pool,
            &clusters,
            "sub-1",
            "pirate",
            " sha256~pirate\n",
            LinkSource::TokenReview,
        )
        .await
        .unwrap();
        assert_eq!(user.as_str(), "casey");
        assert_eq!(
            get(&pool, "sub-1", "pirate").await.unwrap(),
            Some(KubeUser::parse("casey").unwrap())
        );
        let links = list(&pool, "sub-1").await.unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].source, LinkSource::TokenReview);
        let stored: Vec<String> = sqlx::query_scalar(
            "SELECT concat_ws(' ', sub, cluster, kube_user, source) FROM kube_identity_links",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(
            stored.iter().all(|row| !row.contains("sha256~")),
            "the token is never stored: {stored:?}"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_token_from_another_cluster_links_nothing(pool: PgPool) {
        let clusters = Clusters::with(&[
            ("pirate", "sha256~pirate", "casey"),
            ("east", "sha256~east", "jordan"),
        ]);
        let err = link(
            &pool,
            &clusters,
            "sub-1",
            "pirate",
            "sha256~east",
            LinkSource::TokenReview,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, LinkError::NotAuthenticated { .. }), "{err}");
        assert_eq!(get(&pool, "sub-1", "pirate").await.unwrap(), None);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn system_identities_and_unreachable_clusters_link_nothing(pool: PgPool) {
        let mut clusters = Clusters::with(&[(
            "pirate",
            "sa-token",
            "system:serviceaccount:crucible-system:crucible-kueue",
        )]);
        let err = link(
            &pool,
            &clusters,
            "sub-1",
            "pirate",
            "sa-token",
            LinkSource::TokenReview,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, LinkError::User(KubeUserError::System(_))),
            "{err}"
        );
        clusters.down.push("pirate".into());
        let err = link(
            &pool,
            &clusters,
            "sub-1",
            "pirate",
            "sa-token",
            LinkSource::TokenReview,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, LinkError::Unreachable { .. }), "{err}");
        assert_eq!(get(&pool, "sub-1", "pirate").await.unwrap(), None);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn relinking_replaces_the_username_per_cluster(pool: PgPool) {
        let clusters = Clusters::with(&[
            ("pirate", "t1", "casey"),
            ("pirate", "t2", "casey2"),
            ("east", "t3", "jordan"),
        ]);
        link(
            &pool,
            &clusters,
            "sub-1",
            "pirate",
            "t1",
            LinkSource::TokenReview,
        )
        .await
        .unwrap();
        link(
            &pool,
            &clusters,
            "sub-1",
            "east",
            "t3",
            LinkSource::TokenReview,
        )
        .await
        .unwrap();
        link(
            &pool,
            &clusters,
            "sub-1",
            "pirate",
            "t2",
            LinkSource::TokenReview,
        )
        .await
        .unwrap();
        let links: Vec<(String, String)> = list(&pool, "sub-1")
            .await
            .unwrap()
            .into_iter()
            .map(|l| (l.cluster, l.kube_user.as_str().to_string()))
            .collect();
        assert_eq!(
            links,
            [
                ("east".to_string(), "jordan".to_string()),
                ("pirate".to_string(), "casey2".to_string())
            ]
        );
    }

    async fn signed_in(pool: &PgPool, sub: &str, login: &str) {
        crate::identity::oidc::users::record_login(pool, sub, login, None, jiff::Timestamp::now())
            .await
            .unwrap();
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn unlinking_forgets_one_cluster_and_keeps_the_others(pool: PgPool) {
        let clusters = Clusters::with(&[("pirate", "t1", "casey"), ("east", "t2", "jordan")]);
        link(
            &pool,
            &clusters,
            "sub-1",
            "pirate",
            "t1",
            LinkSource::OpenshiftOauth,
        )
        .await
        .unwrap();
        link(
            &pool,
            &clusters,
            "sub-1",
            "east",
            "t2",
            LinkSource::TokenReview,
        )
        .await
        .unwrap();
        assert!(unlink(&pool, "sub-1", "pirate").await.unwrap());
        assert!(
            !unlink(&pool, "sub-1", "pirate").await.unwrap(),
            "nothing left to unlink"
        );
        assert_eq!(get(&pool, "sub-1", "pirate").await.unwrap(), None);
        let left = list(&pool, "sub-1").await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].cluster, "east");
        assert_eq!(left[0].source, LinkSource::TokenReview);
    }

    /// The hub link is a link like any other, and an administrator's override survives its unlink.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn the_hub_link_is_the_username_agent_tokens_carry(pool: PgPool) {
        signed_in(&pool, "sub-alice", "alice").await;
        let clusters = Clusters::with(&[("hub", "t1", "alice-k8s")]);
        link(
            &pool,
            &clusters,
            "sub-alice",
            "hub",
            "t1",
            LinkSource::OpenshiftOauth,
        )
        .await
        .unwrap();
        assert_eq!(
            get(&pool, "sub-alice", "hub")
                .await
                .unwrap()
                .unwrap()
                .as_str(),
            "alice-k8s"
        );
        crate::identity::kube_identity::set_override(
            &pool,
            "alice",
            Some(&KubeUser::parse("alice-admin").unwrap()),
        )
        .await
        .unwrap();
        assert!(unlink(&pool, "sub-alice", "hub").await.unwrap());
        assert_eq!(get(&pool, "sub-alice", "hub").await.unwrap(), None);
        assert_eq!(
            crate::identity::kube_identity::override_for_sub(&pool, "sub-alice")
                .await
                .unwrap()
                .unwrap()
                .as_str(),
            "alice-admin"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_spoke_link_leaves_the_hub_username_alone(pool: PgPool) {
        signed_in(&pool, "sub-alice", "alice").await;
        let clusters = Clusters::with(&[("pirate", "t1", "casey")]);
        link(
            &pool,
            &clusters,
            "sub-alice",
            "pirate",
            "t1",
            LinkSource::TokenReview,
        )
        .await
        .unwrap();
        assert_eq!(get(&pool, "sub-alice", "hub").await.unwrap(), None);
    }
}
