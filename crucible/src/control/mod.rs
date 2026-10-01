//! Steering a run from outside the process: the scored loop's control bridge, admission ledger
//! and signals, and the MCP servers a sandboxed turn reaches.

#[cfg(feature = "autoresearch")]
pub(crate) mod admission;
#[cfg(feature = "autoresearch")]
pub(crate) mod bridge;
#[cfg(feature = "autoresearch")]
pub(crate) mod distress;
#[cfg(feature = "autoresearch")]
pub(crate) mod escalation;
#[cfg(feature = "autoresearch")]
pub(crate) mod heartbeat;
pub(crate) mod mcp;
#[cfg(feature = "autoresearch")]
pub(crate) mod pr_watch;
#[cfg(feature = "autoresearch")]
pub(crate) mod provisioning;
#[cfg(feature = "autoresearch")]
pub(crate) mod recovery;
