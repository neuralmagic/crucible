//! OAuth 2.1 resource-server support for MCP servers built on rmcp's Streamable HTTP service.
//!
//! ```text
//!   request ─▶ bearer (ResourceServer)            ─▶ policy (ToolPolicy)                 ─▶ rmcp
//!              validate JWT: iss, aud, exp, sig       tools/call: scopes, strip + mutate,
//!              Principal into extensions              validate; tools/list: hide injected
//!              401 / 403 + RFC 9728 challenge         params from inputSchema
//! ```
//!
//! [`metadata::router`] serves the RFC 9728 protected-resource document the challenge points at.

pub mod guard;
pub mod jwks;
pub mod metadata;
pub mod policy;
pub mod principal;
pub mod server;
#[cfg(test)]
mod testing;
