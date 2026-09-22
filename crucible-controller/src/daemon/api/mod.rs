//! The daemon slice's HTTP handlers, mounted by [`crate::api`].

#[cfg(feature = "autoresearch")]
pub(crate) mod autopilot;
pub(crate) mod overrides;
