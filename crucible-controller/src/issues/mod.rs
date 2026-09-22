//! Issues: a tracked repository's issues from triage through the reconcile sweep. Polling and
//! the GitHub client, the ranker that assigns a tier, the engine actions a sweep dispatches, the
//! approval gates it waits on, and the comparison, journey and refine-trail views over the ledger.

pub(crate) mod api;
pub mod approvals;
pub mod compare;
pub mod engine;
pub mod github;
pub mod http;
pub mod journey;
pub mod model;
pub mod ranker;
pub mod reconcile;
pub mod refine_trail;
pub mod repo_ref;
pub mod repo_watch;
pub mod store;
pub mod transitions;
pub mod triage;
