//! Names the controller, the loop, and the per-run `crucible-git` server share.

/// The URL sandboxes reach `crucible-git` at unless `[git] url` names another, and the audience of
/// the tokens it accepts.
pub const URL: &str = "http://host.openshell.internal:8853/mcp";

/// The port in [`URL`], which `crucible-git` listens on in the loop pod.
pub const PORT: u16 = 8853;

/// Where the run's own command and evaluate tasks reach `crucible-git`: loopback, no token.
pub const LOCAL_BIND: &str = "127.0.0.1:8854";

/// Where the controller mounts the run's minted GitHub App token in the `crucible-git` container,
/// and nowhere else in the pod.
pub const TOKEN_DIR: &str = "/var/run/crucible-git-token";

/// The token's file name in [`TOKEN_DIR`].
pub const TOKEN_FILE_NAME: &str = "token";

/// The directory the loop container and `crucible-git` share: the loop writes [`BOUNDS_FILE`]
/// there at setup, and `crucible-git` reads it.
pub const SHARED_DIR: &str = "/run/crucible-git";

/// The run's resolved output bounds and bound parameters, as [`Bounds`].
pub const BOUNDS_FILE: &str = "/run/crucible-git/bounds.json";

/// What the loop hands `crucible-git` about the run: the `[outputs]` table resolved for this run
/// and the parameter values an open target binds to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Bounds {
    pub outputs: crate::outputs::ResolvedOutputs,
    pub params: std::collections::BTreeMap<String, String>,
}

/// Command and evaluate task env: the base URL of the run's `crucible-git`, set when it runs.
/// A repository is `$CRUCIBLE_GIT_URL/git/<owner>/<repo>.git`; the MCP endpoint is
/// `$CRUCIBLE_GIT_URL/mcp`.
pub const ENV_URL: &str = "CRUCIBLE_GIT_URL";
