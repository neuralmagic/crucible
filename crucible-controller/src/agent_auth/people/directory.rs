//! What the people grant reads about a subject: whether they may still sign in, and who they are
//! on a cluster. [`crate::identity::people_directory::ControllerDirectory`] answers both from the
//! controller's own tables; the grant never reads them.

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
