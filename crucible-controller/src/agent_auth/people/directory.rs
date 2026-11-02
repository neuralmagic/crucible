//! What the people grant asks of its host: whether a subject may still sign in, and who they are
//! on a cluster. The controller answers both from its own tables; the grant never reads them.

use crate::agent_auth::people::Approved;

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

#[async_trait::async_trait]
pub trait SubjectDirectory: Send + Sync {
    /// `None` when the subject is gone or their identity provider refused them.
    async fn allowed(&self, sub: &str) -> anyhow::Result<Option<Allowed>>;
}

#[async_trait::async_trait]
pub trait LinkDirectory: Send + Sync {
    /// The username `approved` acts as on its cluster, read fresh on every call.
    async fn kube_user(&self, approved: &Approved) -> anyhow::Result<ClusterUser>;
}
