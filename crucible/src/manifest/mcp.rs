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
    /// Which of the server's stderr lines reach crucible's own stderr.
    #[serde(default)]
    pub log: McpLog,
}

/// The least severe `tracing` level of a server's stderr that crucible passes on. Lines that are
/// not `tracing` lines (a panic, a plain print) pass at every level but `off`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpLog {
    Off,
    Error,
    #[default]
    Warn,
    Info,
    Debug,
    Trace,
}

impl McpLog {
    /// Whether `line`, as the server wrote it, passes.
    pub fn passes(self, line: &str) -> bool {
        if self == McpLog::Off {
            return false;
        }
        tracing_level(&crucible_contract::console::strip_ansi(line))
            .is_none_or(|level| level <= self)
    }
}

/// The level of a `tracing_subscriber` line: its first word, or its second after a timestamp.
fn tracing_level(line: &str) -> Option<McpLog> {
    let level = |word: &str| match word {
        "ERROR" => Some(McpLog::Error),
        "WARN" => Some(McpLog::Warn),
        "INFO" => Some(McpLog::Info),
        "DEBUG" => Some(McpLog::Debug),
        "TRACE" => Some(McpLog::Trace),
        _ => None,
    };
    let mut words = line.split_whitespace();
    let first = words.next()?;
    level(first).or_else(|| {
        first
            .starts_with(|c: char| c.is_ascii_digit())
            .then(|| words.next().and_then(level))
            .flatten()
    })
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
    use crate::manifest::mcp::{McpError, McpLog};

    #[test]
    fn a_server_passes_its_warnings_and_untagged_lines_by_default() {
        let colored_info = "\u{1b}[2m2026-11-09T18:02:41.069974Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m \u{1b}[1mcve-triage\u{1b}[0m\u{1b}[2m:\u{1b}[0m [llm-d] Jira search returned 176 ticket(s)";
        let warn = "2026-11-09T18:02:41.07Z  WARN cve_triage::tickets: rate limited";
        let error = "ERROR cve_triage: token expired";
        let debug = "2026-11-09T18:02:41Z DEBUG hyper::client: connecting";
        let panic = "thread 'main' panicked at src/main.rs:4:5:";
        let quiet = McpLog::default();
        assert_eq!(quiet, McpLog::Warn);
        assert!(!quiet.passes(colored_info));
        assert!(!quiet.passes(debug));
        assert!(quiet.passes(warn));
        assert!(quiet.passes(error));
        assert!(quiet.passes(panic));
        assert!(McpLog::Info.passes(colored_info));
        assert!(!McpLog::Info.passes(debug));
        assert!(McpLog::Trace.passes(debug));
        assert!(!McpLog::Error.passes(warn));
        assert!(McpLog::Error.passes(error));
        assert!(!McpLog::Off.passes(panic));
        assert!(
            McpLog::Warn.passes("2026 was a year of INFO"),
            "a level must be the line's own"
        );
    }

    #[test]
    fn a_servers_log_level_is_read_from_its_table() {
        let text = "[repo]\npath = \".\"\n[agent]\nbackend = \"openshell\"\ngoal = \"g\"\n\
                    [mcp.triage]\nbin = \"cve-triage\"\nlog = \"info\"\n[mcp.quiet]\nbin = \"q\"\n";
        let m: Manifest = toml::from_str(text).expect("parses");
        assert_eq!(m.mcp["triage"].log, McpLog::Info);
        assert_eq!(m.mcp["quiet"].log, McpLog::Warn);
        let bad = text.replace("\"info\"", "\"loud\"");
        assert!(toml::from_str::<Manifest>(&bad).is_err());
    }

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
