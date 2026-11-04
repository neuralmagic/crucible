//! The operations, once.
//!
//! Each is a struct whose fields are its arguments on both surfaces: the CLI parses it with
//! clap and the MCP tool decodes it with serde, and both print the same bytes. That is the
//! point: an agent can loop over `crux issues` in a shell script when that is cheaper than a
//! tool call per issue, and get exactly what the tool would have returned.

#![allow(clippy::disallowed_macros)]

pub mod blueprints;
pub mod drafts;
pub mod issues;
pub mod launches;
pub mod playbooks;
pub mod providers;
pub mod runs;
pub mod secrets;

use crate::client::Client;
use crate::render;
use anyhow::{Context, Result};
use rmcp::schemars;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::future::Future;
use std::path::Path;

/// One operation against the controller.
pub trait Operation: Send + Sync {
    /// The reply, decoded for rendering.
    type Output: DeserializeOwned + Send;

    /// Perform the operation. The reply as the controller sent it, plus any fact this client
    /// adds for the reader (a link, a local digest).
    fn call(&self, c: &Client) -> impl Future<Output = Result<Value>> + Send;

    /// The compact text for one reply.
    fn render(&self, c: &Client, out: Self::Output) -> Result<String>;

    /// Whether the caller asked for the reply verbatim.
    fn json(&self) -> bool;

    /// What the caller prints: the reply verbatim under `--json`, else decoded and rendered.
    fn run(&self, c: &Client) -> impl Future<Output = Result<String>> + Send {
        async move {
            let raw = self.call(c).await?;
            if self.json() {
                return pretty(&raw);
            }
            self.render(c, serde_json::from_value(raw)?)
        }
    }
}

/// `--json` renders the controller's payload verbatim instead of the compact text. Pretty-printed,
/// because the caller asked for JSON in order to read or pipe it, not to save four bytes.
pub(crate) fn pretty(v: &Value) -> Result<String> {
    Ok(serde_json::to_string_pretty(v)?)
}

/// Set `field` to `value` trimmed, or leave it absent when there is nothing to send, so the
/// controller's own default applies instead of a 422 on a blank.
pub(crate) fn put_trimmed(body: &mut Value, field: &str, value: Option<&str>) {
    if let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) {
        body[field] = Value::String(v.to_string());
    }
}

/// The request body a subcommand takes from a JSON file.
pub(crate) fn read_body(file: &Path) -> Result<Value> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{} is not JSON", file.display()))
}

/// Who the controller thinks you are, and how you got there. The first thing to run when a
/// mutation 403s.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Whoami {}

impl Operation for Whoami {
    type Output = crate::dto::Whoami;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.get("/api/whoami").await
    }

    fn render(&self, c: &Client, w: Self::Output) -> Result<String> {
        Ok(render::whoami(&c.endpoint(), &c.credential(), &w))
    }

    fn json(&self) -> bool {
        false
    }
}

#[cfg(test)]
pub(crate) fn test_client(url: &str) -> Client {
    let cfg = crate::config::Config {
        url: url.to_string(),
        auth: crate::config::Auth::None,
    };
    Client::connect(&cfg).expect("connect")
}
