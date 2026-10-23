//! Names the controller, the loop, and the per-run buildit server share.

/// Loop-pod env naming the issuer buildit trusts for run tokens, as `<issuer>=<jwks uri>`. Its
/// presence starts buildit for the run.
pub const ENV_BUILDIT_RUN_ISSUER: &str = "CRUCIBLE_BUILDIT_RUN_ISSUER";

/// Loop-pod env naming the one repository buildit pushes builds to; unset, it pushes nothing.
pub const ENV_BUILDIT_PUSH_REPO: &str = "CRUCIBLE_BUILDIT_PUSH_REPO";

/// The port buildit serves on in the loop pod.
pub const PORT: u16 = 8851;

/// The URL sandboxes reach buildit at, and the audience of the tokens it accepts.
pub const URL: &str = "http://host.openshell.internal:8851/mcp";

/// OAuth scope to read builds' status and logs.
pub const SCOPE_READ: &str = "buildit.read";

/// OAuth scope to build, run, push, and clean.
pub const SCOPE_BUILD: &str = "buildit.build";
