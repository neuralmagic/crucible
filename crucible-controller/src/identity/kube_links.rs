//! The Kubernetes username a person acts as on each cluster, minted into people's access tokens as
//! `kube_user` for MCP servers on that cluster.
//!
//! A row is only ever that cluster's own TokenReview answer for a token the person presented. The
//! token is reviewed and dropped, never stored or logged; a token another cluster minted does not
//! authenticate here, so it cannot link this cluster.

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
    TokenReview,
}

impl LinkSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::TokenReview => "token-review",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub cluster: String,
    pub kube_user: String,
    pub source: String,
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
    upsert(pool, sub, cluster, &user, LinkSource::TokenReview).await?;
    tracing::info!(
        sub,
        cluster,
        kube_user = user.as_str(),
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

pub async fn list(pool: &PgPool, sub: &str) -> anyhow::Result<Vec<Link>> {
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT cluster, kube_user, source, linked_at FROM kube_identity_links
          WHERE sub = $1 ORDER BY cluster",
    )
    .bind(sub)
    .fetch_all(pool)
    .await
    .context("listing kubernetes identity links")?;
    Ok(rows
        .into_iter()
        .map(|(cluster, kube_user, source, linked_at)| Link {
            cluster,
            kube_user,
            source,
            linked_at,
        })
        .collect())
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
        let clusters = Clusters::with(&[("pirate", "sha256~pirate", "wseaton")]);
        let user = link(&pool, &clusters, "sub-1", "pirate", " sha256~pirate\n")
            .await
            .unwrap();
        assert_eq!(user.as_str(), "wseaton");
        assert_eq!(
            get(&pool, "sub-1", "pirate").await.unwrap(),
            Some(KubeUser::parse("wseaton").unwrap())
        );
        let links = list(&pool, "sub-1").await.unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].source, "token-review");
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
            ("pirate", "sha256~pirate", "wseaton"),
            ("mpp", "sha256~mpp", "weaton"),
        ]);
        let err = link(&pool, &clusters, "sub-1", "pirate", "sha256~mpp")
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
        let err = link(&pool, &clusters, "sub-1", "pirate", "sa-token")
            .await
            .unwrap_err();
        assert!(
            matches!(err, LinkError::User(KubeUserError::System(_))),
            "{err}"
        );
        clusters.down.push("pirate".into());
        let err = link(&pool, &clusters, "sub-1", "pirate", "sa-token")
            .await
            .unwrap_err();
        assert!(matches!(err, LinkError::Unreachable { .. }), "{err}");
        assert_eq!(get(&pool, "sub-1", "pirate").await.unwrap(), None);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn relinking_replaces_the_username_per_cluster(pool: PgPool) {
        let clusters = Clusters::with(&[
            ("pirate", "t1", "wseaton"),
            ("pirate", "t2", "wseaton2"),
            ("mpp", "t3", "weaton"),
        ]);
        link(&pool, &clusters, "sub-1", "pirate", "t1")
            .await
            .unwrap();
        link(&pool, &clusters, "sub-1", "mpp", "t3").await.unwrap();
        link(&pool, &clusters, "sub-1", "pirate", "t2")
            .await
            .unwrap();
        let links: Vec<(String, String)> = list(&pool, "sub-1")
            .await
            .unwrap()
            .into_iter()
            .map(|l| (l.cluster, l.kube_user))
            .collect();
        assert_eq!(
            links,
            [
                ("mpp".to_string(), "weaton".to_string()),
                ("pirate".to_string(), "wseaton2".to_string())
            ]
        );
    }
}
