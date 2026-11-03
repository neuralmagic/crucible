//! The `distress` handoff between the broker and the engine: three files under the run's storage
//! root. The broker writes the marker and the notes; the engine reads them and writes the turn
//! stamp the broker's page quotes.
//!
//! ```text
//!   distress              the park marker; while it exists the loop is suspended
//!   distress-notes.jsonl  info/warn notes, one per line, consumed into the next decided row
//!   turn-meta.json        the engine's per-iteration stamp
//! ```

use serde::{Deserialize, Serialize};
use std::fmt;

pub const MARKER_FILE: &str = "distress";
pub const NOTES_FILE: &str = "distress-notes.jsonl";
pub const TURN_META_FILE: &str = "turn-meta.json";

/// How loud a distress signal is. Only `Error` suspends the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warn,
    Error,
}

impl Severity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Error => "error",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The park marker. Its presence is the suspend; the operator deleting it is the resume grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub reason: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    pub ts_ms: u64,
    /// The engine's `turn-token` when the call was made; empty before the first turn stamps one.
    #[serde(default)]
    pub turn_token: String,
}

/// One info or warn note, a line of [`NOTES_FILE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub severity: Severity,
    pub reason: String,
    pub ts_ms: u64,
}

/// The engine's stamp of the iteration in progress and what the run has spent so far.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnMeta {
    pub iter: u32,
    #[serde(default)]
    pub spent_usd: f64,
}

#[cfg(test)]
mod tests {
    use crate::distress::*;

    #[test]
    fn the_marker_encodes_in_the_order_the_broker_has_always_written_it() {
        let marker = Marker {
            reason: "torch skew".into(),
            evidence: vec!["log-3".into()],
            ts_ms: 1234,
            turn_token: "t-1".into(),
        };
        let line = serde_json::to_string(&marker).unwrap();
        assert_eq!(
            line,
            r#"{"reason":"torch skew","evidence":["log-3"],"ts_ms":1234,"turn_token":"t-1"}"#
        );
        assert_eq!(serde_json::from_str::<Marker>(&line).unwrap(), marker);
    }

    #[test]
    fn a_marker_without_a_turn_token_or_evidence_still_reads() {
        let marker: Marker = serde_json::from_str(r#"{"reason":"r","ts_ms":1}"#).unwrap();
        assert_eq!(marker.evidence, Vec::<String>::new());
        assert_eq!(marker.turn_token, "");
        assert!(serde_json::from_str::<Marker>(r#"{"reason":"r"}"#).is_err());
    }

    #[test]
    fn a_note_encodes_its_severity_as_the_lowercase_wire_token() {
        let note = Note {
            severity: Severity::Warn,
            reason: "flaky node".into(),
            ts_ms: 2,
        };
        let line = serde_json::to_string(&note).unwrap();
        assert_eq!(
            line,
            r#"{"severity":"warn","reason":"flaky node","ts_ms":2}"#
        );
        assert_eq!(serde_json::from_str::<Note>(&line).unwrap(), note);
        assert!(serde_json::from_str::<Severity>("\"fatal\"").is_err());
        assert_eq!(Severity::Error.to_string(), "error");
    }

    #[test]
    fn the_turn_stamp_round_trips_and_tolerates_a_missing_spend() {
        let meta = TurnMeta {
            iter: 7,
            spent_usd: 1.5,
        };
        let text = serde_json::to_string(&meta).unwrap();
        assert_eq!(text, r#"{"iter":7,"spent_usd":1.5}"#);
        assert_eq!(serde_json::from_str::<TurnMeta>(&text).unwrap(), meta);
        let older: TurnMeta = serde_json::from_str(r#"{"iter":3}"#).unwrap();
        assert_eq!((older.iter, older.spent_usd), (3, 0.0));
    }
}
