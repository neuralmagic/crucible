//! `crux`: the crucible controller as a CLI, and the tool surface the controller hosts at `/mcp`.
//!
//! It exists to replace `curl | jq` against the controller API: it encodes issue keys, carries
//! error bodies through verbatim, and hands a model a line where the API hands it kilobytes.
//!
//! The pieces, in the order a request moves through them:
//!
//! - [`config`] resolves where the controller is and the bearer every request carries.
//! - [`client`] speaks HTTP, encodes issue keys, and carries error bodies through verbatim.
//! - [`render`] turns the controller's SPA-shaped DTOs into compact plain text.
//! - [`ops`] is each operation once, shared by the CLI and the MCP tools.
//! - [`server`] is the MCP tool surface, which the controller serves in process at `/mcp`.

#[cfg(feature = "cli")]
pub mod cli;
pub mod client;
pub mod config;
pub mod dto;
#[cfg(feature = "cli")]
pub mod ops;
#[cfg(feature = "cli")]
pub mod render;
#[cfg(feature = "cli")]
pub mod server;
pub mod sse;
pub mod text;

pub use client::Client;
#[cfg(feature = "cli")]
pub use server::CrucibleMcp;
