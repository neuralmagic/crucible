//! Names the controller, the loop, and the per-run buildit server share.

/// The URL sandboxes reach buildit at unless `[buildit] url` names another, and the audience of the
/// tokens it accepts.
pub const URL: &str = "http://host.openshell.internal:8848/mcp";

/// The port in [`URL`], which buildit listens on in the loop pod.
pub const PORT: u16 = 8848;
