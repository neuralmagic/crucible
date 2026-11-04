//! Names the controller, the loop, and the per-run `crucible-git` server share.

/// The URL sandboxes reach `crucible-git` at unless `[git] url` names another, and the audience of
/// the tokens it accepts.
pub const URL: &str = "http://host.openshell.internal:8853/mcp";

/// The port in [`URL`], which `crucible-git` listens on in the loop pod.
pub const PORT: u16 = 8853;

/// Where the run's own command and evaluate tasks reach `crucible-git`: loopback, no token.
pub const LOCAL_BIND: &str = "127.0.0.1:8854";

/// Loop-pod env naming the file that holds the run's minted GitHub App token, set when the run
/// binds one as a file.
pub const ENV_TOKEN_FILE: &str = "CRUCIBLE_GITHUB_TOKEN_FILE";

/// Command and evaluate task env: the base URL of the run's `crucible-git`, set when it runs.
/// A repository is `$CRUCIBLE_GIT_URL/git/<owner>/<repo>.git`; the MCP endpoint is
/// `$CRUCIBLE_GIT_URL/mcp`.
pub const ENV_URL: &str = "CRUCIBLE_GIT_URL";
