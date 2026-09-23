//! Issues: a tracked repository's issues from triage through the reconcile sweep. Polling and
//! the GitHub client, the ranker that assigns a tier, the engine actions a sweep dispatches, the
//! approval gates it waits on, and the comparison, journey and refine-trail views over the ledger.

pub(crate) mod api;
#[cfg(feature = "autoresearch")]
pub mod approvals;
#[cfg(feature = "autoresearch")]
pub mod compare;
#[cfg(feature = "autoresearch")]
pub mod engine;
#[cfg(feature = "autoresearch")]
pub mod github;
#[cfg(feature = "autoresearch")]
pub mod http;
#[cfg(feature = "autoresearch")]
pub mod journey;
pub mod model;
#[cfg(feature = "autoresearch")]
pub mod ranker;
pub mod reconcile;
#[cfg(feature = "autoresearch")]
pub mod refine_trail;
pub mod repo_ref;
#[cfg(feature = "autoresearch")]
pub mod repo_watch;
pub mod store;
pub mod transitions;
#[cfg(feature = "autoresearch")]
pub mod triage;
