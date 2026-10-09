//! The caller's Kubernetes username on every cluster the deployment knows, and the routes that
//! link, relink, and unlink it. A rule cluster's name is read off the person's SSO login and
//! cannot be changed here; a link cluster's name is only ever that cluster's own answer.

use serde::Serialize;
use utoipa::ToSchema;

/// The SSO claim a rule cluster's usernames come from.
#[derive(Debug, Clone, Copy, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClaimDto {
    Login,
    Email,
}

/// How a cluster learns who a person is.
#[derive(Debug, Clone, Serialize, ToSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IdentityRuleDto {
    /// The username is the person's SSO claim, behind `prefix` when set.
    Claim {
        claim: ClaimDto,
        prefix: Option<String>,
    },
    /// Each person proves their username to the cluster. `oauth` is whether the cluster's OAuth
    /// server can do it; without it the person pastes a token.
    Link { oauth: bool },
}

/// Where the username shown came from.
#[derive(Debug, Clone, Copy, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum IdentitySourceDto {
    /// Read off the SSO login by the cluster's rule.
    Rule,
    /// The cluster's OAuth server signed the person in.
    OpenshiftOauth,
    /// The cluster's TokenReview accepted a token the person pasted.
    TokenReview,
    /// A platform administrator set it; the event log records who.
    Override,
}

/// One cluster's identity for the caller.
#[derive(Debug, Clone, Serialize, ToSchema, PartialEq, Eq)]
pub struct ClusterIdentityDto {
    pub cluster: String,
    pub rule: IdentityRuleDto,
    /// `None` on a link cluster nobody has linked yet.
    pub kube_user: Option<String>,
    pub source: Option<IdentitySourceDto>,
    /// When a link cluster's username was last proven.
    pub linked_at: Option<String>,
}

/// Where to send the browser to link a cluster through its OAuth server.
#[derive(Debug, Serialize, ToSchema, PartialEq, Eq)]
pub struct LinkStartDto {
    pub authorize_url: String,
}
