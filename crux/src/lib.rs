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

pub mod cli;
pub mod client;
pub mod config;
pub mod dto;
pub mod ops;
pub mod render;
pub mod server;

pub use client::Client;
pub use server::CrucibleMcp;
