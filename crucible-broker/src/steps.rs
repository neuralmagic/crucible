//! The broker's durable step ledger: the tier under the in-memory memos, so a resumed
//! broker replays finished builds and measures instead of repaying them. Semantics in
//! [`forge::steps`].

use forge::steps::{StepKey, StepLedger};
use std::path::PathBuf;

/// One shared scope, deliberately NOT the turn token: the driver writes a fresh token per
/// turn sandbox, so scoping by it would throw away the replay this exists for.
pub(crate) const SCOPE: &str = "broker";

pub(crate) fn key(step: impl Into<String>) -> StepKey {
    StepKey::new(SCOPE, step)
}

/// Replay is only as durable as the build volume: on an emptyDir it degrades to
/// broker-process lifetime. `BROKER_STEP_LEDGER_DIR` overrides the location.
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
