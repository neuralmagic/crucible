//! The contract between the engine and an MCP server it spawns: the env it hands the server, and
//! the token file the server authenticates sandboxes against.
//!
//! The engine mints one bearer token per (server, sandbox) and writes `<token> <sandbox> <workdir>`
//! lines to the file named by [`ENV_TOKENS_FILE`], replacing it atomically. `<workdir>` is the
//! absolute path the sandbox's agent works in (`/sandbox/workspace` for a loop turn,
//! `/sandbox/task-<sha256>` for an isolated plan task). A two-field `<token> <sandbox>` line, the
//! format before the workdir was added, still parses, with the workdir unknown. The server re-reads
//! the file on every request and takes the caller's sandbox from the token, so a sandbox cannot
//! claim another sandbox's identity: it only ever holds its own token. A request carrying no known
//! token gets 401; the engine checks this with an unauthenticated `POST /mcp` before the run uses
//! the server.

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

/// Why a [`TokenHolder`] cannot be written to a token file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HolderError {
    /// A sandbox name that is not 1 to 63 ASCII letters, digits, `-`, `_` or `.`, starting and
    /// ending with a letter or digit.
    Sandbox(String),
    /// A workdir that is not an absolute path of plain segments: no empty, `.` or `..` segment,
    /// no whitespace or control character.
    Workdir(String),
}

impl fmt::Display for HolderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sandbox(name) => write!(f, "invalid sandbox name {name:?}"),
            Self::Workdir(path) => write!(f, "invalid sandbox workdir {path:?}"),
        }
    }
}

impl std::error::Error for HolderError {}

/// The sandbox a token belongs to, and the workdir its agent runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenHolder {
    sandbox: String,
    workdir: Option<String>,
}

impl TokenHolder {
    /// A holder whose workdir is known. See [`HolderError`] for what each field allows.
    pub fn new(sandbox: &str, workdir: &str) -> Result<Self, HolderError> {
        let mut holder = Self::without_workdir(sandbox)?;
        if !valid_workdir(workdir) {
            return Err(HolderError::Workdir(workdir.to_string()));
        }
        holder.workdir = Some(workdir.to_string());
        Ok(holder)
    }

    /// A holder from a two-field line, whose workdir is unknown.
    pub fn without_workdir(sandbox: &str) -> Result<Self, HolderError> {
        if !valid_sandbox(sandbox) {
            return Err(HolderError::Sandbox(sandbox.to_string()));
        }
        Ok(Self {
            sandbox: sandbox.to_string(),
            workdir: None,
        })
    }

    pub fn sandbox(&self) -> &str {
        &self.sandbox
    }

    pub fn workdir(&self) -> Option<&str> {
        self.workdir.as_deref()
    }
}

fn valid_sandbox(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() <= 63
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn valid_workdir(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    let rest = rest.trim_end_matches('/');
    !rest.is_empty()
        && rest.split('/').all(|part| !matches!(part, "" | "." | ".."))
        && !path.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Why a token file did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenMapError {
    /// A line that is not `<token> <sandbox> [<workdir>]`.
    Malformed { line: usize },
    /// The same token appears twice.
    DuplicateToken { line: usize },
}

impl fmt::Display for TokenMapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { line } => {
                write!(
                    f,
                    "token file line {line} is not `<token> <sandbox> [<workdir>]`"
                )
            }
            Self::DuplicateToken { line } => {
                write!(f, "token file line {line} repeats a token")
            }
        }
    }
}

impl std::error::Error for TokenMapError {}

/// Token to holder, one entry per sandbox.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenMap {
    entries: Vec<(String, TokenHolder)>,
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
            let fields: Vec<&str> = raw.split_whitespace().collect();
            let (token, holder) = match fields.as_slice() {
                [token, sandbox] => (token, TokenHolder::without_workdir(sandbox)),
                [token, sandbox, workdir] => (token, TokenHolder::new(sandbox, workdir)),
                _ => return Err(TokenMapError::Malformed { line }),
            };
            let holder = holder.map_err(|_| TokenMapError::Malformed { line })?;
            if map.entries.iter().any(|(t, _)| t == token) {
                return Err(TokenMapError::DuplicateToken { line });
            }
            map.entries.push((token.to_string(), holder));
        }
        Ok(map)
    }

    /// The file body, one `<token> <sandbox> <workdir>` line per entry, or `<token> <sandbox>`
    /// when the workdir is unknown.
    pub fn render(&self) -> String {
        self.entries
            .iter()
            .map(|(token, holder)| match &holder.workdir {
                Some(workdir) => format!("{token} {} {workdir}\n", holder.sandbox),
                None => format!("{token} {}\n", holder.sandbox),
            })
            .collect()
    }

    /// Give `holder`'s sandbox exactly one token, replacing any it held.
    pub fn grant(&mut self, holder: TokenHolder, token: &str) {
        self.revoke(&holder.sandbox);
        self.entries.push((token.to_string(), holder));
    }

    /// Drop every token `sandbox` holds.
    pub fn revoke(&mut self, sandbox: &str) {
        self.entries.retain(|(_, h)| h.sandbox != sandbox);
    }

    /// The holder `token` belongs to. Every entry is compared in constant time, so the lookup
    /// leaks neither which entry matched nor how much of a token was right.
    pub fn holder_for(&self, token: &str) -> Option<&TokenHolder> {
        let mut found = None;
        for (candidate, holder) in &self.entries {
            if constant_time_eq(candidate.as_bytes(), token.as_bytes()) {
                found = Some(holder);
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
    use crate::mcp::{HolderError, TokenHolder, TokenMap, TokenMapError};

    fn holder(sandbox: &str, workdir: &str) -> TokenHolder {
        TokenHolder::new(sandbox, workdir).unwrap()
    }

    #[test]
    fn a_token_names_only_its_own_sandbox() {
        let mut map = TokenMap::default();
        map.grant(holder("ci-a", "/sandbox/workspace"), "tok-a");
        map.grant(holder("ci-b", "/sandbox/task-b"), "tok-b");
        assert_eq!(
            map.holder_for("tok-a"),
            Some(&holder("ci-a", "/sandbox/workspace"))
        );
        assert_eq!(
            map.holder_for("tok-b"),
            Some(&holder("ci-b", "/sandbox/task-b"))
        );
        assert_eq!(map.holder_for("tok-c"), None);
        assert_eq!(map.holder_for("tok-"), None, "a prefix is not a match");
        assert_eq!(map.holder_for(""), None);
    }

    #[test]
    fn a_grant_replaces_the_sandboxs_old_token() {
        let mut map = TokenMap::default();
        map.grant(holder("ci-a", "/sandbox/old"), "old");
        map.grant(holder("ci-a", "/sandbox/new"), "new");
        assert_eq!(map.holder_for("old"), None);
        assert_eq!(map.holder_for("new"), Some(&holder("ci-a", "/sandbox/new")));
        map.revoke("ci-a");
        assert!(map.is_empty());
    }

    #[test]
    fn the_file_round_trips() {
        let mut map = TokenMap::default();
        map.grant(holder("ci-a", "/sandbox/workspace"), "tok-a");
        map.grant(TokenHolder::without_workdir("ci-b").unwrap(), "tok-b");
        let text = map.render();
        assert_eq!(text, "tok-a ci-a /sandbox/workspace\ntok-b ci-b\n");
        assert_eq!(TokenMap::parse(&text), Ok(map));
        assert_eq!(TokenMap::parse("\n\n"), Ok(TokenMap::default()));
    }

    #[test]
    fn a_two_field_line_parses_with_the_workdir_unknown() {
        let map = TokenMap::parse("tok-a ci-a\ntok-b ci-b /sandbox/task-b\n").unwrap();
        let a = map.holder_for("tok-a").unwrap();
        assert_eq!(a.sandbox(), "ci-a");
        assert_eq!(a.workdir(), None);
        let b = map.holder_for("tok-b").unwrap();
        assert_eq!(b.sandbox(), "ci-b");
        assert_eq!(b.workdir(), Some("/sandbox/task-b"));
        assert_eq!(map.holder_for("tok-c"), None);
    }

    #[test]
    fn a_malformed_or_ambiguous_file_is_refused() {
        for (text, line) in [
            ("tok-a\n", 1),
            ("tok-a ci-a /sandbox/w extra\n", 1),
            ("tok-a ci-a sandbox/relative\n", 1),
            ("\ntok-a ci-a /ok\ntok-b ci-b ~/w\n", 3),
        ] {
            assert_eq!(
                TokenMap::parse(text),
                Err(TokenMapError::Malformed { line }),
                "{text:?}"
            );
        }
        assert_eq!(
            TokenMap::parse("tok ci-a\ntok ci-b /sandbox/w\n"),
            Err(TokenMapError::DuplicateToken { line: 2 })
        );
    }

    #[test]
    fn sandbox_names_follow_the_servers_charset() {
        for ok in ["ci-0123456789abcdef", "a", "sb_1.x", &"a".repeat(63)] {
            assert!(TokenHolder::without_workdir(ok).is_ok(), "{ok:?}");
        }
        for bad in [
            "",
            "-ci",
            "ci-",
            ".ci",
            "ci.",
            "ci/a",
            "ci:a",
            "ci\u{7f}a",
            "cï",
            &"a".repeat(64),
        ] {
            assert_eq!(
                TokenHolder::without_workdir(bad),
                Err(HolderError::Sandbox(bad.to_string())),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn workdirs_are_absolute_paths_of_plain_segments() {
        for ok in [
            "/sandbox/workspace",
            "/sandbox/task-0123abcd",
            "/sandbox/workspace/",
            "/w",
        ] {
            assert!(TokenHolder::new("ci-a", ok).is_ok(), "{ok:?}");
        }
        for bad in [
            "",
            "/",
            "//",
            "sandbox/w",
            "/sandbox//w",
            "/sandbox/./w",
            "/sandbox/../etc",
            "/sandbox/..",
            "/sandbox/w\u{1}",
            "/sandbox/w\u{85}",
        ] {
            assert_eq!(
                TokenHolder::new("ci-a", bad),
                Err(HolderError::Workdir(bad.to_string())),
                "{bad:?}"
            );
        }
        assert_eq!(
            TokenMap::parse("tok ci-a /sandbox/../etc\n"),
            Err(TokenMapError::Malformed { line: 1 })
        );
        assert_eq!(
            TokenMap::parse("tok -ci\n"),
            Err(TokenMapError::Malformed { line: 1 })
        );
    }

    #[test]
    fn a_holder_that_would_not_parse_back_is_refused() {
        assert_eq!(
            TokenHolder::new("ci a", "/sandbox/w"),
            Err(HolderError::Sandbox("ci a".into()))
        );
        assert_eq!(
            TokenHolder::without_workdir(""),
            Err(HolderError::Sandbox(String::new()))
        );
        assert_eq!(
            TokenHolder::new("ci-a", "/sandbox/my work"),
            Err(HolderError::Workdir("/sandbox/my work".into()))
        );
        assert_eq!(
            TokenHolder::new("ci-a", "workspace"),
            Err(HolderError::Workdir("workspace".into()))
        );
    }
}
