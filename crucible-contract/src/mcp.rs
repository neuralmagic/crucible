//! The contract between the engine and an MCP server it spawns: the env it hands the server, and
//! the token file the server authenticates sandboxes against.
//!
//! The engine mints one bearer token per (server, sandbox) and writes `<token> <sandbox>` lines to
//! the file named by [`ENV_TOKENS_FILE`], replacing it atomically. The server re-reads the file on
//! every request and takes the caller's sandbox from the token, so a sandbox cannot claim another
//! sandbox's identity: it only ever holds its own token.

use std::fmt;

/// The server name the agent sees (the `mcp__<name>__<tool>` prefix).
pub const ENV_NAME: &str = "MCP_NAME";
/// The `host:port` the server listens on.
pub const ENV_BIND: &str = "MCP_BIND";
/// The token file the server authenticates requests against.
pub const ENV_TOKENS_FILE: &str = "MCP_TOKENS_FILE";
/// The tool selection the pack made, comma-separated. Not enforced by the engine.
pub const ENV_TOOLS: &str = "MCP_TOOLS";
/// The loop's control bridge on loopback, for a server that sends re-scopes back.
pub const ENV_CONTROL_ADDR: &str = "MCP_CONTROL_ADDR";

/// Why a token file did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenMapError {
    /// A line that is not `<token> <sandbox>`.
    Malformed { line: usize },
    /// The same token appears twice.
    DuplicateToken { line: usize },
}

impl fmt::Display for TokenMapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { line } => {
                write!(f, "token file line {line} is not `<token> <sandbox>`")
            }
            Self::DuplicateToken { line } => {
                write!(f, "token file line {line} repeats a token")
            }
        }
    }
}

impl std::error::Error for TokenMapError {}

/// Token to sandbox, one entry per sandbox.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenMap {
    entries: Vec<(String, String)>,
}

impl TokenMap {
    /// Parse the file body. Blank lines are skipped.
    pub fn parse(text: &str) -> Result<Self, TokenMapError> {
        let mut map = Self::default();
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            let mut fields = raw.split_whitespace();
            let (Some(token), Some(sandbox), None) = (fields.next(), fields.next(), fields.next())
            else {
                return Err(TokenMapError::Malformed { line });
            };
            if map.entries.iter().any(|(t, _)| t == token) {
                return Err(TokenMapError::DuplicateToken { line });
            }
            map.entries.push((token.to_string(), sandbox.to_string()));
        }
        Ok(map)
    }

    /// The file body, one `<token> <sandbox>` line per entry.
    pub fn render(&self) -> String {
        self.entries
            .iter()
            .map(|(token, sandbox)| format!("{token} {sandbox}\n"))
            .collect()
    }

    /// Give `sandbox` exactly one token, replacing any it held.
    pub fn grant(&mut self, sandbox: &str, token: &str) {
        self.revoke(sandbox);
        self.entries.push((token.to_string(), sandbox.to_string()));
    }

    /// Drop every token `sandbox` holds.
    pub fn revoke(&mut self, sandbox: &str) {
        self.entries.retain(|(_, s)| s != sandbox);
    }

    /// The sandbox `token` belongs to. Every entry is compared in constant time, so the lookup
    /// leaks neither which entry matched nor how much of a token was right.
    pub fn sandbox_for(&self, token: &str) -> Option<&str> {
        let mut found = None;
        for (candidate, sandbox) in &self.entries {
            if constant_time_eq(candidate.as_bytes(), token.as_bytes()) {
                found = Some(sandbox.as_str());
            }
        }
        found
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use crate::mcp::{TokenMap, TokenMapError};

    #[test]
    fn a_token_names_only_its_own_sandbox() {
        let mut map = TokenMap::default();
        map.grant("ci-a", "tok-a");
        map.grant("ci-b", "tok-b");
        assert_eq!(map.sandbox_for("tok-a"), Some("ci-a"));
        assert_eq!(map.sandbox_for("tok-b"), Some("ci-b"));
        assert_eq!(map.sandbox_for("tok-c"), None);
        assert_eq!(map.sandbox_for("tok-"), None, "a prefix is not a match");
        assert_eq!(map.sandbox_for(""), None);
    }

    #[test]
    fn a_grant_replaces_the_sandboxs_old_token() {
        let mut map = TokenMap::default();
        map.grant("ci-a", "old");
        map.grant("ci-a", "new");
        assert_eq!(map.sandbox_for("old"), None);
        assert_eq!(map.sandbox_for("new"), Some("ci-a"));
        map.revoke("ci-a");
        assert!(map.is_empty());
    }

    #[test]
    fn the_file_round_trips() {
        let mut map = TokenMap::default();
        map.grant("ci-a", "tok-a");
        map.grant("ci-b", "tok-b");
        assert_eq!(TokenMap::parse(&map.render()), Ok(map));
        assert_eq!(TokenMap::parse("\n\n"), Ok(TokenMap::default()));
    }

    #[test]
    fn a_malformed_or_ambiguous_file_is_refused() {
        assert_eq!(
            TokenMap::parse("tok-a\n"),
            Err(TokenMapError::Malformed { line: 1 })
        );
        assert_eq!(
            TokenMap::parse("tok-a ci-a extra\n"),
            Err(TokenMapError::Malformed { line: 1 })
        );
        assert_eq!(
            TokenMap::parse("tok ci-a\ntok ci-b\n"),
            Err(TokenMapError::DuplicateToken { line: 2 })
        );
    }
}
