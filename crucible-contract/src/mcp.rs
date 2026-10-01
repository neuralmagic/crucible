//! The contract between the engine and an `[mcp]` server it spawns: the env it hands the server,
//! and the token file the server authenticates sandboxes against.
//!
//! The engine mints one bearer token per (server, sandbox) and writes `<token> <sandbox> <workdir>`
//! lines to the file [`ENV_TOKENS_FILE`] names, replacing it atomically. The server re-reads the
//! file on every request and takes the caller's sandbox and workdir from the token alone. A
//! two-field `<token> <sandbox>` line parses with the workdir unknown.

use std::fmt;

/// The server name the agent sees (the `mcp__<name>__<tool>` prefix).
pub const ENV_NAME: &str = "MCP_NAME";
/// The `host:port` the server listens on.
pub const ENV_BIND: &str = "MCP_BIND";
/// The token file the server authenticates requests against.
pub const ENV_TOKENS_FILE: &str = "MCP_TOKENS_FILE";
/// The pack's tool selection, comma-separated.
pub const ENV_TOOLS: &str = "MCP_TOOLS";

/// The sandbox a token belongs to, and the workdir its agent runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenHolder {
    pub sandbox: String,
    pub workdir: Option<String>,
}

/// A holder field that would not survive a round trip through the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadHolder(pub String);

impl fmt::Display for BadHolder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} cannot be written to a token file", self.0)
    }
}

impl std::error::Error for BadHolder {}

impl TokenHolder {
    /// A holder whose workdir is known: both fields free of whitespace, the workdir absolute.
    pub fn new(sandbox: &str, workdir: &str) -> Result<Self, BadHolder> {
        let bad =
            |s: &str| s.is_empty() || s.contains(|c: char| c.is_whitespace() || c.is_control());
        if bad(sandbox) {
            return Err(BadHolder(sandbox.to_string()));
        }
        if bad(workdir) || !workdir.starts_with('/') {
            return Err(BadHolder(workdir.to_string()));
        }
        Ok(Self {
            sandbox: sandbox.to_string(),
            workdir: Some(workdir.to_string()),
        })
    }
}

/// A token file line that is not `<token> <sandbox> [<workdir>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedLine(pub usize);

impl fmt::Display for MalformedLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "token file line {} is not `<token> <sandbox> [<workdir>]`",
            self.0
        )
    }
}

impl std::error::Error for MalformedLine {}

/// Token to holder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenMap(Vec<(String, TokenHolder)>);

impl TokenMap {
    /// Parse the file body. Blank lines are skipped.
    pub fn parse(text: &str) -> Result<Self, MalformedLine> {
        let mut map = Self::default();
        for (index, line) in text.lines().enumerate() {
            let (token, sandbox, workdir) = match line.split_whitespace().collect::<Vec<_>>()[..] {
                [] => continue,
                [token, sandbox] => (token, sandbox, None),
                [token, sandbox, workdir] => (token, sandbox, Some(workdir.to_string())),
                _ => return Err(MalformedLine(index + 1)),
            };
            let holder = TokenHolder {
                sandbox: sandbox.to_string(),
                workdir,
            };
            map.0.push((token.to_string(), holder));
        }
        Ok(map)
    }

    pub fn render(&self) -> String {
        self.0
            .iter()
            .map(|(token, h)| match &h.workdir {
                Some(workdir) => format!("{token} {} {workdir}\n", h.sandbox),
                None => format!("{token} {}\n", h.sandbox),
            })
            .collect()
    }

    /// Give `holder`'s sandbox exactly one token, replacing any it held.
    pub fn grant(&mut self, holder: TokenHolder, token: &str) {
        self.revoke(&holder.sandbox);
        self.0.push((token.to_string(), holder));
    }

    pub fn revoke(&mut self, sandbox: &str) {
        self.0.retain(|(_, h)| h.sandbox != sandbox);
    }

    /// The holder of `token`, compared in constant time against every entry.
    pub fn holder_for(&self, token: &str) -> Option<&TokenHolder> {
        let mut found = None;
        for (candidate, holder) in &self.0 {
            let same = candidate.len() == token.len()
                && candidate
                    .bytes()
                    .zip(token.bytes())
                    .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                    == 0;
            if same {
                found = Some(holder);
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use crate::mcp::{BadHolder, MalformedLine, TokenHolder, TokenMap};

    #[test]
    fn a_token_names_only_its_own_sandbox_and_round_trips() {
        let mut map = TokenMap::default();
        map.grant(TokenHolder::new("ci-a", "/sandbox/old").unwrap(), "stale");
        map.grant(
            TokenHolder::new("ci-a", "/sandbox/workspace").unwrap(),
            "tok-a",
        );
        map.grant(
            TokenHolder::new("ci-b", "/sandbox/task-b").unwrap(),
            "tok-b",
        );
        assert_eq!(
            map.holder_for("stale"),
            None,
            "a grant replaces the old token"
        );
        assert_eq!(map.holder_for("tok-a").unwrap().sandbox, "ci-a");
        assert_eq!(map.holder_for("tok-"), None);
        let text = map.render();
        assert_eq!(
            text,
            "tok-a ci-a /sandbox/workspace\ntok-b ci-b /sandbox/task-b\n"
        );
        assert_eq!(TokenMap::parse(&text), Ok(map.clone()));
        map.revoke("ci-a");
        assert_eq!(map.holder_for("tok-a"), None);
    }

    #[test]
    fn a_two_field_line_parses_and_a_bad_line_or_holder_is_refused() {
        let map = TokenMap::parse("\ntok-a ci-a\n").unwrap();
        assert_eq!(map.holder_for("tok-a").unwrap().workdir, None);
        assert_eq!(TokenMap::parse("tok ci /w extra\n"), Err(MalformedLine(1)));
        assert_eq!(TokenMap::parse("ok ci\ntok\n"), Err(MalformedLine(2)));
        assert_eq!(
            TokenHolder::new("ci-a", "/sandbox/my work"),
            Err(BadHolder("/sandbox/my work".into()))
        );
        assert_eq!(
            TokenHolder::new("ci-a", "relative"),
            Err(BadHolder("relative".into()))
        );
        assert_eq!(TokenHolder::new("", "/w"), Err(BadHolder(String::new())));
    }
}
