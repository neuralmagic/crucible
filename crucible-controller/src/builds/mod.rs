//! Builds: the sandbox and world images a scope needs before its run, from request through
//! dispatch to a pinned digest, with their ledger rows and HTTP surface.

pub(crate) mod api;
pub mod lifecycle;
pub mod model;
pub(crate) mod store;
