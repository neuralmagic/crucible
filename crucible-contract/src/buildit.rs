//! Names the controller, the loop, and the per-run buildit server share.

/// The URL sandboxes reach buildit at unless `[buildit] url` names another, and the audience of the
/// tokens it accepts.
pub const URL: &str = "http://host.openshell.internal:8848/mcp";

/// The port in [`URL`], which buildit listens on in the loop pod.
pub const PORT: u16 = 8848;

/// Where a loop pod mounts the shared cache's write token, when its builds may write.
pub const CACHE_TOKEN_FILE: &str = "/etc/buildit-cache/token";
