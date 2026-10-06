//! `[mcp.<key>]`: the MCP servers a pack starts on the loop pod. A turn reaches only the servers
//! its scope names: `[agent].mcp` without a named sandbox, `[agent.sandbox.<name>].mcp` inside one.

use crate::manifest::AgentCfg;
use serde::Deserialize;
use std::collections::BTreeMap;

/// The first server's port; the rest follow in key order. The broker's default is 8849.
pub const FIRST_PORT: u16 = 8850;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpCfg {
    pub bin: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Env set on the server, values from the pack.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Env names copied from the loop pod when set. Nothing else of the loop pod's env reaches it.
    #[serde(default)]
    pub inherit: Vec<String>,
    /// Handed to the server as `MCP_TOOLS`.
    #[serde(default)]
    pub tools: Vec<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum McpError {
    #[error("[mcp.{key}] name must be 1..=64 characters of [a-z0-9_-]")]
    BadKey { key: String },
    #[error("[mcp.{key}].bin is empty")]
    NoBin { key: String },
    #[error("[mcp.{key}] shares its name with [agent.broker]")]
    BrokerClash { key: String },
    #[error("{scope} names {key:?}, which no [mcp.{key}] declares")]
    UnknownScope { scope: String, key: String },
    #[error("[mcp] declares more servers than there are ports after {FIRST_PORT}")]
    TooMany,
}

/// Each server's port, in key order.
pub fn ports(table: &BTreeMap<String, McpCfg>) -> impl Iterator<Item = ((&String, &McpCfg), u16)> {
    table.iter().zip(FIRST_PORT..=u16::MAX)
}

pub fn validate(table: &BTreeMap<String, McpCfg>, agent: &AgentCfg) -> Result<(), McpError> {
    if ports(table).count() < table.len() {
        return Err(McpError::TooMany);
    }
    for (key, cfg) in table {
        if !crate::manifest::plain_name(key) {
            return Err(McpError::BadKey { key: key.clone() });
        }
        if cfg.bin.trim().is_empty() {
            return Err(McpError::NoBin { key: key.clone() });
        }
        if agent.broker.enabled && agent.broker.name == *key {
            return Err(McpError::BrokerClash { key: key.clone() });
        }
    }
    let scopes = std::iter::once(("[agent].mcp".to_string(), &agent.mcp)).chain(
        agent
            .sandbox
            .iter()
            .map(|(name, p)| (format!("[agent.sandbox.{name}].mcp"), &p.mcp)),
    );
    for (scope, keys) in scopes {
        if let Some(key) = keys.iter().find(|k| !table.contains_key(*k)) {
            return Err(McpError::UnknownScope {
                scope,
                key: key.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::manifest::Manifest;
    use crate::manifest::mcp::McpError;

    fn refusal(extra: &str) -> McpError {
        let text = format!(
            "[repo]\npath = \".\"\n[agent]\nbackend = \"openshell\"\ngoal = \"g\"\n{extra}"
        );
        let m: Manifest = toml::from_str(&text).expect("parses");
        match m.validate().expect_err("refused").downcast::<McpError>() {
            Ok(e) => e,
            Err(e) => panic!("not an McpError: {e:#}"),
        }
    }

    #[test]
    fn a_bad_server_or_a_scope_naming_an_undeclared_one_is_refused() {
        assert_eq!(
            refusal("mcp = [\"jira\"]\n"),
            McpError::UnknownScope {
                scope: "[agent].mcp".into(),
                key: "jira".into()
            }
        );
        assert_eq!(
            refusal("[agent.sandbox.go]\nimage = \"i\"\nmcp = [\"x\"]\n[mcp.jira]\nbin = \"b\"\n"),
            McpError::UnknownScope {
                scope: "[agent.sandbox.go].mcp".into(),
                key: "x".into()
            }
        );
        assert_eq!(
            refusal("[mcp.Jira]\nbin = \"b\"\n"),
            McpError::BadKey { key: "Jira".into() }
        );
        assert_eq!(
            refusal("[mcp.jira]\nbin = \" \"\n"),
            McpError::NoBin { key: "jira".into() }
        );
        assert_eq!(
            refusal(
                "[agent.broker]\nenabled = true\nbin = \"b\"\nname = \"jira\"\n[mcp.jira]\nbin = \"b\"\n"
            ),
            McpError::BrokerClash { key: "jira".into() }
        );
    }
}
