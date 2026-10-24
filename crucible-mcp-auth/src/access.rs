//! What a server allows, declared in its code: every tool and method it serves, the scopes each
//! needs, and the Kubernetes access a call needs. Anything a server does not describe is refused,
//! so a new tool cannot run unchecked.

use crate::policy::Refusal;
use crate::principal::Principal;
use serde_json::{Map, Value};
use std::future::Future;
use std::pin::Pin;

/// A request the guard decides on: a `tools/call` (with its arguments after the tool policy ran)
/// or another JSON-RPC method.
#[derive(Debug, Clone, Copy)]
pub enum Call<'a> {
    Tool {
        name: &'a str,
        args: &'a Map<String, Value>,
    },
    Method {
        name: &'a str,
        params: &'a Value,
    },
}

impl Call<'_> {
    pub fn name(&self) -> &str {
        match self {
            Call::Tool { name, .. } | Call::Method { name, .. } => name,
        }
    }

    /// The refusal for a call the server does not serve.
    pub fn unserved(&self) -> Refusal {
        match self {
            Call::Tool { name, .. } => Refusal::Unlisted {
                tool: name.to_string(),
            },
            Call::Method { name, .. } => Refusal::Method {
                method: name.to_string(),
            },
        }
    }
}

/// SubjectAccessReview resource attributes to ask about the caller. Who the caller is in
/// Kubernetes terms is the reviewer's business.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AccessCheck {
    /// The tool or method the check is for.
    pub tool: String,
    /// A short name for the check, reported when it refuses.
    pub rule: String,
    pub message: String,
    pub group: String,
    pub resource: String,
    pub subresource: String,
    pub verb: String,
    pub namespace: String,
    pub name: String,
}

/// A server's rules for the calls it serves.
pub trait AccessRules: Send + Sync {
    /// The scopes `call` needs, or [`Call::unserved`] for a tool or method this server does not
    /// serve.
    fn scopes(&self, call: &Call<'_>) -> Result<&'static [&'static str], Refusal>;

    /// The access checks `call` needs for `principal`, given the arguments the tool policy left.
    /// Every one is reviewed and must be allowed.
    fn checks(&self, call: &Call<'_>, principal: &Principal) -> Result<Vec<AccessCheck>, Refusal>;
}

pub type ReviewFuture<'a> = Pin<Box<dyn Future<Output = Result<bool, ReviewError>> + Send + 'a>>;

/// Why an [`AccessCheck`] was not decided.
#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    /// The caller has no identity the reviewer can check, so the same call always fails.
    #[error("{0}")]
    Identity(Box<dyn std::error::Error + Send + Sync>),
    /// The review did not complete, so the same call may succeed later.
    #[error("{0}")]
    Unavailable(Box<dyn std::error::Error + Send + Sync>),
}

/// Decides an [`AccessCheck`] for a principal: `Ok(true)` allows, `Ok(false)` denies, an error
/// refuses the call too.
pub trait AccessReview: Send + Sync {
    fn review<'a>(&'a self, principal: &'a Principal, check: &'a AccessCheck) -> ReviewFuture<'a>;
}
