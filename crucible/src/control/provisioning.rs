//! Pending mediated-provisioning, the agent's choice of how to wait for a human approval of a
//! judge-changing re-scope.
//!
//! When the broker's `request_trace` returns `pending_approval`, the *agent* can't sit and wait: its
//! turn runs in an ephemeral sandbox that is torn down when the turn ends. So it records the ask in
//! `PROVISIONING_PENDING.json` and ends its turn; the long-lived loop does the waiting. The marker's
//! `mode` is the agent's solution-space call (how to spend time is the agent's; whether the grant is
//! allowed stays the broker's):
//!
//!   - `block`: this regime is the only path; the loop parks until the approval lands.
//!   - `continue`: there is other frozen-regime work; keep iterating, the re-scope drains async.
//!
//! Either way the broker watches the approval and fires a `rescope` over the control bridge on yes
//! (the re-scope drain); the mode only decides whether the loop idles meanwhile. Same marker shape +
//! consume-once semantics as [`crate::control::escalation`].

use serde::{Deserialize, Serialize};
use std::path::Path;

/// How the agent chose to spend the wait for a pending approval, the one field the engine
/// branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitMode {
    /// Park the run until the approval resolves (the agent has no frozen-regime fallback).
    Block,
    /// Keep iterating in the frozen regime; the re-scope lands whenever approval arrives.
    Continue,
}

impl WaitMode {
    /// The serde token, for wire fields that carry the mode as a string.
    pub fn as_str(self) -> &'static str {
        match self {
            WaitMode::Block => "block",
            WaitMode::Continue => "continue",
        }
    }
}

/// A parsed `PROVISIONING_PENDING.json`. Domain-neutral: the engine only acts on `mode` and
/// surfaces `trace_id`/`handle`. Any `regime` params the agent records are for the broker / the
/// run record and are ignored here (serde drops unknown fields), so the engine never parses a
/// domain-shaped regime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingProvisioning {
    /// The agent's wait choice.
    pub mode: WaitMode,
    /// The pending request's content key (display / record).
    #[serde(default)]
    pub trace_id: String,
    /// The approval handle (a draft PR url) to show while parked.
    #[serde(default)]
    pub handle: String,
}

/// Read and CONSUME the pending-provisioning marker at `path`, if the agent wrote one this turn.
///
/// - `None`: no marker (the common path).
/// - `Some(Ok(p))`: a well-formed ask the loop should act on (park or note-and-continue).
/// - `Some(Err(msg))`: a marker we could not parse. The caller surfaces a note and keeps going,
///   so a garbled file can't wedge a paid run.
///
/// The marker is always removed when present (parsed or not), so a stale file never re-triggers on
/// the next turn or on a `--resume`.
pub fn take(path: &Path) -> Option<Result<PendingProvisioning, String>> {
    if !path.exists() {
        return None;
    }
    let raw = std::fs::read_to_string(path);
    // Consume regardless of how parsing goes: this marker has had its one shot.
    let _ = std::fs::remove_file(path);
    let raw = match raw {
        Ok(s) => s,
        Err(e) => return Some(Err(format!("reading {}: {e}", path.display()))),
    };
    if raw.trim().is_empty() {
        return Some(Err("marker is empty".into()));
    }
    match serde_json::from_str::<PendingProvisioning>(&raw) {
        Ok(p) => Some(Ok(p)),
        Err(e) => Some(Err(format!("invalid JSON: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_marker() -> std::path::PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "crucible-prov-test-{}-{n}.json",
            std::process::id()
        ))
    }

    #[test]
    fn no_marker_is_none() {
        assert!(take(&tmp_marker()).is_none());
    }

    #[test]
    fn parses_block_with_regime_ignored_and_consumes() {
        let p = tmp_marker();
        // The agent may record the full regime for the broker/record; the engine ignores it.
        std::fs::write(
            &p,
            r#"{"mode":"block","trace_id":"model=Qwen/Qwen3-0.6B;c=48","handle":"https://github.com/wseaton/llm-d-router/pull/7","regime":{"concurrency":48,"model":"Qwen/Qwen3-0.6B"}}"#,
        )
        .unwrap();
        let got = take(&p).expect("present").expect("well-formed");
        assert_eq!(got.mode, WaitMode::Block);
        assert!(got.trace_id.contains("c=48"));
        assert!(got.handle.ends_with("/pull/7"));
        // Consumed: a resume can't re-trigger it.
        assert!(take(&p).is_none());
    }

    #[test]
    fn parses_continue_with_minimal_fields() {
        let p = tmp_marker();
        std::fs::write(&p, r#"{"mode":"continue"}"#).unwrap();
        let got = take(&p).unwrap().unwrap();
        assert_eq!(got.mode, WaitMode::Continue);
        assert_eq!(got.trace_id, "");
        assert_eq!(got.handle, "");
    }

    #[test]
    fn unknown_mode_is_an_error_and_still_consumed() {
        let p = tmp_marker();
        std::fs::write(&p, r#"{"mode":"maybe"}"#).unwrap();
        assert!(matches!(take(&p), Some(Err(_))));
        assert!(take(&p).is_none(), "a poison marker must not survive");
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        let p = tmp_marker();
        std::fs::write(&p, r#"{"mode":"block""#).unwrap();
        assert!(matches!(take(&p), Some(Err(_))));
        assert!(take(&p).is_none());
    }
}
