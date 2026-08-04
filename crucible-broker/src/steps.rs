//! Where the broker's durable step ledger lives, and what it scopes records under.
//!
//! The loop pod dies mid-sequence often enough that the in-memory memos (the codegen map, the
//! `built` provenance set) are not enough: a resumed broker rebuilt an image whose digest was
//! already in the registry, then refused to measure the digest it had just been handed. The
//! ledger is the durable tier under those memos; see [`forge::steps`] for the semantics.

use forge::steps::{StepKey, StepLedger};
use std::path::PathBuf;

/// Every broker step shares one scope. Identities are full content keys (a tree hash plus a
/// config fingerprint, a digest plus kwargs), so a step recorded before the pod died is exactly
/// as valid as one recorded now. Deliberately NOT the turn token: the driver writes a fresh one
/// per turn sandbox, so scoping by it would throw away the replay this exists for.
pub(crate) const SCOPE: &str = "broker";

pub(crate) fn key(step: impl Into<String>) -> StepKey {
    StepKey::new(SCOPE, step)
}

/// The ledger on the shared build volume. Replay is only as durable as that volume: on an
/// emptyDir it degrades to broker-process lifetime (still better than per-request memos).
/// `BROKER_STEP_LEDGER_DIR` overrides it.
pub(crate) fn ledger() -> StepLedger {
    StepLedger::new(&dir())
}

fn dir() -> PathBuf {
    std::env::var("BROKER_STEP_LEDGER_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(forge::storage_root)
}
