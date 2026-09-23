//! Steering a run from outside the process: the scored loop's control bridge, admission ledger
//! and signals, and the broker a sandboxed turn reaches.

#[cfg(feature = "autoresearch")]
pub(crate) mod admission;
#[cfg(feature = "autoresearch")]
pub(crate) mod bridge;
pub(crate) mod broker;
#[cfg(feature = "autoresearch")]
pub(crate) mod distress;
#[cfg(feature = "autoresearch")]
pub(crate) mod escalation;
#[cfg(feature = "autoresearch")]
pub(crate) mod heartbeat;
#[cfg(feature = "autoresearch")]
pub(crate) mod pr_watch;
#[cfg(feature = "autoresearch")]
pub(crate) mod provisioning;
#[cfg(feature = "autoresearch")]
pub(crate) mod recovery;
