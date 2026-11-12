//! Observing and steering a run from outside the process, plus the broker and MCP servers a
//! sandboxed turn reaches.

#[cfg(feature = "autoresearch")]
pub(crate) mod admission;
pub(crate) mod bridge;
pub(crate) mod broker;
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
