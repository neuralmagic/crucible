//! Names the controller, the loop, and the crucible-kueue server share.

/// OAuth scope to read queues, hardware, and workloads.
pub const SCOPE_READ: &str = "kueue.read";

/// OAuth scope to submit, stop, resume, and delete workloads.
pub const SCOPE_SUBMIT: &str = "kueue.submit";
