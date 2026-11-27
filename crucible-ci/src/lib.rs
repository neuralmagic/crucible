//! Clients for the CI systems a playbook call or watch follows.
//!
//! Each backend owns its requests, its response types, how its runs are matched to a commit, and
//! how a run maps onto a [`crucible_contract::calls::ProgressGraph`]. Callers pass the
//! [`reqwest::Client`] and a [`token::BearerToken`] on every call, and own retry and backoff, so
//! the crate builds for wasm32 as well as natively.

pub mod commit;
pub mod gitlab;
pub mod tekton;
pub mod token;
mod trace;
