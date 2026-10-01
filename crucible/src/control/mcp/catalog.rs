//! The loop image's MCP server catalog: one `<dir>/<name>.toml` per server the image can start.
//! A pack picks entries by name in `[mcp.<key>].catalog`; the entry, not the pack, says which
//! binary runs and which env it gets.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Where the loop image installs the catalog.
pub const DEFAULT_DIR: &str = "/etc/crucible/mcp.d";

/// Overrides [`DEFAULT_DIR`], for local runs and tests.
pub const ENV_DIR: &str = "CRUCIBLE_MCP_CATALOG";

/// The catalog directory this process reads.
pub fn dir() -> PathBuf {
    std::env::var_os(ENV_DIR)
        .filter(|d| !d.is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_DIR), PathBuf::from)
}

/// How the sandbox reaches the server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// Streamable HTTP on the port the engine assigns.
    #[default]
    Http,
}

/// One catalog entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpCatalogEntry {
    pub description: String,
    pub bin: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub transport: Transport,
    /// Env names the server cannot start without, passed through from the loop pod.
    #[serde(default)]
    pub env_required: Vec<String>,
    /// Env names passed through when set. A trailing `*` matches a prefix.
    #[serde(default)]
    pub env_optional: Vec<String>,
    /// The classes of secret the server holds. Disclosed, not enforced.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// The external systems the server reaches. Disclosed, not enforced.
    #[serde(default)]
    pub reach: Vec<String>,
}

impl McpCatalogEntry {
    /// Whether `name` is one of [`McpCatalogEntry::env_optional`].
    pub fn passes_optional(&self, name: &str) -> bool {
        self.env_optional
            .iter()
            .any(|pattern| match pattern.strip_suffix('*') {
                Some(prefix) => name.starts_with(prefix),
                None => name == pattern,
            })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("MCP catalog has no entry {name:?} (looked for {})", .path.display())]
    Missing { name: String, path: PathBuf },
    #[error("reading MCP catalog entry {}: {source}", .path.display())]
    Unreadable {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("MCP catalog entry {} is invalid: {reason}", .path.display())]
    Invalid { path: PathBuf, reason: String },
}

/// Load `<dir>/<name>.toml`.
pub fn load(dir: &Path, name: &str) -> Result<McpCatalogEntry, CatalogError> {
    let path = dir.join(format!("{name}.toml"));
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(CatalogError::Missing {
                name: name.to_string(),
                path,
            });
        }
        Err(source) => return Err(CatalogError::Unreadable { path, source }),
    };
    let entry: McpCatalogEntry = toml::from_str(&text).map_err(|e| CatalogError::Invalid {
        path: path.clone(),
        reason: e.to_string(),
    })?;
    if entry.bin.trim().is_empty() {
        return Err(CatalogError::Invalid {
            path,
            reason: "bin is empty".to_string(),
        });
    }
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use crate::control::mcp::catalog::{CatalogError, Transport, load};

    #[test]
    fn an_entry_parses_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ujira.toml"),
            r#"
            description = "JIRA comments"
            bin = "/usr/local/bin/ujira-mcp"
            args = ["serve"]
            env_required = ["JIRA_URL"]
            env_optional = ["JIRA_PROJECTS", "UJIRA_*"]
            secrets = ["jira"]
            reach = ["issues.redhat.com:443"]
            "#,
        )
        .unwrap();
        let entry = load(dir.path(), "ujira").expect("loads");
        assert_eq!(entry.bin, "/usr/local/bin/ujira-mcp");
        assert_eq!(entry.args, ["serve"]);
        assert_eq!(entry.transport, Transport::Http);
        assert_eq!(entry.env_required, ["JIRA_URL"]);
        assert!(entry.passes_optional("JIRA_PROJECTS"));
        assert!(entry.passes_optional("UJIRA_DEBUG"));
        assert!(!entry.passes_optional("JIRA_TOKEN"));
        assert!(!entry.passes_optional("JIRA_PROJECTS_EXTRA"));
    }

    #[test]
    fn a_missing_entry_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let err = load(dir.path(), "nope").expect_err("missing");
        assert!(matches!(&err, CatalogError::Missing { name, .. } if name == "nope"));
        assert!(err.to_string().contains("nope.toml"), "{err}");
    }

    #[test]
    fn an_invalid_entry_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in [
            (
                "unknown",
                "description = \"d\"\nbin = \"b\"\nshell = true\n",
            ),
            (
                "stdio",
                "description = \"d\"\nbin = \"b\"\ntransport = \"stdio\"\n",
            ),
            ("nobin", "description = \"d\"\nbin = \" \"\n"),
            ("nodesc", "bin = \"b\"\n"),
        ] {
            std::fs::write(dir.path().join(format!("{name}.toml")), body).unwrap();
            assert!(
                matches!(load(dir.path(), name), Err(CatalogError::Invalid { .. })),
                "{name}"
            );
        }
    }
}
