//! OAuth 2.1 resource-server support for MCP servers built on rmcp's Streamable HTTP service.
//!
//! ```text
//!   request ─▶ bearer (ResourceServer)        ─▶ guard (PolicyGuard)                       ─▶ rmcp
//!              validate JWT: iss, aud, exp, sig   the server's AccessRules: scopes;
//!              Principal into extensions          ToolPolicy: strip + mutate, validate;
//!              401 / 403 + RFC 9728 challenge     AccessRules checks, each reviewed;
//!                                                 tools/list: hide injected params
//! ```
//!
//! [`metadata::router`] serves the RFC 9728 protected-resource document the challenge points at.

pub mod access;
pub mod guard;
pub mod jwks;
/// Kubernetes RBAC (SubjectAccessReview) as the authority for access checks.
#[cfg(feature = "kube")]
pub mod kube;
pub mod metadata;
pub mod policy;
pub mod principal;
pub mod server;
/// Real keys, a loopback JWKS endpoint, and token minting for tests of servers built on this
/// crate.
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod verify;
