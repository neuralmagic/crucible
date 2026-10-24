//! Names the controller, the loop, and the crucible-kueue server share.

/// Loop-pod env carrying the server's MCP URL.
pub const ENV_KUEUE_URL: &str = "CRUCIBLE_KUEUE_URL";

/// OAuth scope to read queues, hardware, and workloads.
pub const SCOPE_READ: &str = "kueue.read";

/// OAuth scope to submit, stop, resume, and delete workloads.
pub const SCOPE_SUBMIT: &str = "kueue.submit";
