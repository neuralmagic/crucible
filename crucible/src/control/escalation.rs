//! Escalation, the agent's only sanctioned move against a frozen [`crate::Judge`].
//!
//! The judge is frozen: the agent may change its solution, never its evaluation. The honest valve
//! for a *blocked* agent is to declare the harness inadequate and stop for human review, instead of
//! shipping a doomed change or gaming the gate. The agent's toolbox writes a marker file
//! (`ESCALATION.json`) in its workspace (see `tools/escalate.nu`); the engine detects it after the
//! turn, records it, rolls the world back, and halts.
//!
//! The engine is deliberately incurious about *why*: `category`/`reason`/`evidence` are
//! free-form and never branched on here. Routing the report to a human is the whole point,
//! an escalation is a stop-for-review, not a success (it gets its own exit code, see
//! [`crate::report::Outcome`]).

use serde::{Deserialize, Serialize};
use std::path::Path;

/// A parsed `ESCALATION.json`, mirroring exactly what `tools/escalate.nu` writes. Domain-
/// neutral: the engine carries these as opaque strings and surfaces them, never enumerating
/// `category` (the writer validates that, the engine only needs a non-empty `reason`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Escalation {
    /// The agent's classification (`harness-limitation`/`infeasible`/`needs-info`). Free-form
    /// to the engine; the writer constrains it.
    #[serde(default)]
    pub category: String,
    /// Why the harness (not the task) is the blocker. Must be non-empty to count.
    pub reason: String,
    /// Supporting evidence (e.g. `inspect-rig` output across configs). The writer defaults this
    /// to `""` when omitted, so it is a plain string, not an option.
    #[serde(default)]
    pub evidence: String,
    /// Unix seconds the writer stamped, if present.
    #[serde(default)]
    pub ts: Option<i64>,
}

/// Read and CONSUME the escalation marker at `path`, if the agent wrote one this turn.
///
/// - `None`: no marker (the overwhelmingly common path).
/// - `Some(Ok(esc))`: a well-formed escalation the loop should act on (halt + restore).
/// - `Some(Err(msg))`: a marker we could not parse, or one with no `reason`. The agent tried
///   to signal but garbled it; the caller surfaces a note and keeps going rather than letting
///   a malformed file wedge a paid run.
///
/// The marker is always removed when present (parsed or not), so a stale file never
/// re-triggers on the next turn or on a `--resume`.
pub fn take(path: &Path) -> Option<Result<Escalation, String>> {
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
    match serde_json::from_str::<Escalation>(&raw) {
        Ok(esc) if !esc.reason.trim().is_empty() => Some(Ok(esc)),
        Ok(_) => Some(Err("escalation has no `reason`".into())),
        Err(e) => Some(Err(format!("invalid JSON: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A unique temp path per call so parallel tests don't collide.
    fn tmp_marker() -> std::path::PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("crucible-esc-test-{}-{n}.json", std::process::id()))
    }

    #[test]
    fn no_marker_is_none() {
        let p = tmp_marker();
        assert!(take(&p).is_none());
    }

    #[test]
    fn parses_the_escalate_nu_shape_and_consumes_it() {
        let p = tmp_marker();
        // Exactly what tools/escalate.nu emits, including the default empty evidence.
        std::fs::write(
            &p,
            r#"{"category":"harness-limitation","reason":"tokenload reads 0 for all endpoints across 3 configs; the gate cannot differentiate length-aware routing","evidence":"inspect-rig: all pods tokenLoad=0","ts":1750000000}"#,
        )
        .unwrap();
        let esc = take(&p).expect("marker present").expect("well-formed");
        assert_eq!(esc.category, "harness-limitation");
        assert!(esc.reason.contains("tokenload"));
        assert_eq!(esc.evidence, "inspect-rig: all pods tokenLoad=0");
        assert_eq!(esc.ts, Some(1_750_000_000));
        // Consumed: a second take sees nothing, so a resume can't re-trigger.
        assert!(take(&p).is_none());
    }

    #[test]
    fn empty_evidence_is_accepted() {
        let p = tmp_marker();
        std::fs::write(
            &p,
            r#"{"category":"infeasible","reason":"the fix needs a kernel capability the rig lacks","evidence":""}"#,
        )
        .unwrap();
        let esc = take(&p).unwrap().unwrap();
        assert_eq!(esc.evidence, "");
        assert_eq!(esc.ts, None);
    }

    #[test]
    fn missing_reason_is_an_error_not_a_halt() {
        let p = tmp_marker();
        std::fs::write(&p, r#"{"category":"needs-info","evidence":"x"}"#).unwrap();
        assert!(matches!(take(&p), Some(Err(_))));
    }

    #[test]
    fn malformed_json_is_an_error_and_still_consumed() {
        let p = tmp_marker();
        std::fs::write(&p, r#"{"category":"infeasible","reason""#).unwrap();
        assert!(matches!(take(&p), Some(Err(_))));
        assert!(
            take(&p).is_none(),
            "a poison marker must not survive to wedge the loop"
        );
    }
}
