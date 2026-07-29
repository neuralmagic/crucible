//! The broker's JIRA policy: one constructor, so both broker binaries get the same ceiling.
//!
//! The client itself (REST calls, the curated custom-field map, the compact renderers) lives in the
//! shared `jira-mcp` crate, which the standalone MCP server also uses. What belongs HERE is the part
//! that is crucible's business: how much authority the sandboxed agent's JIRA reach carries.

use jira_mcp::{Access, Config, JiraClient};
use std::sync::Arc;

/// The shared client, or `None` when JIRA isn't configured (the `jira_*` tools then report
/// `disabled` rather than failing the run).
///
/// Access is pinned to read+comment here, not left to config or env. The broker exists to hold
/// authority the agent doesn't have; "read the requirements, leave a comment" is the entire JIRA
/// contract, and the three tools the wire exposes match it. Pinning the client too means a create
/// can't happen even if some future tool asks for one: mediation by construction, the same as the
/// build and deploy tools.
pub fn from_env() -> Option<Arc<JiraClient>> {
    let mut cfg = Config::from_env()?;
    cfg.access = Access::ReadComment;
    Some(Arc::new(JiraClient::new(cfg)))
}
