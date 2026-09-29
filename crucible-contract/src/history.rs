//! A launch series' history: the records an orchestrator supplies to a run, and the entry each
//! run leaves for the runs after it.

use serde::{Deserialize, Deserializer, Serialize};

/// Env var carrying the run's [`SuppliedHistory`] as JSON. Absent means the run belongs to no
/// series.
pub const ENV_HISTORY: &str = "CRUCIBLE_HISTORY";
/// Env var carrying the operator's bound on the encoded size of one task's history, in bytes.
pub const ENV_HISTORY_MAX_BYTES: &str = "CRUCIBLE_HISTORY_MAX_BYTES";
pub const HISTORY_WIRE_VERSION: u8 = 1;
/// The most records a run is supplied, and the deepest history a task may declare.
pub const MAX_HISTORY_DEPTH: u32 = 30;
/// The size bound where the operator sets none.
pub const DEFAULT_HISTORY_MAX_BYTES: u64 = 64 * 1024;

/// A task's terminal status, under the tokens the task-result event carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordedStatus {
    Pass,
    Fail,
    Transport,
    Skipped,
    NotTaken,
    Blocked,
    Truncated,
}

/// How a run ended, under the tokens the shutdown event carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Finished,
    Solved,
    Budget,
    Stopped,
    Escalated,
    Stalled,
    Error,
}

/// A playbook run's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunVerdict {
    Valid,
    Invalid,
}

/// What one run recorded for its series: the record task's name, empty where the playbook names
/// none; its terminal status, null where it never settled; and its declared output, null unless
/// it passed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryEntry {
    pub task: String,
    #[serde(deserialize_with = "nullable")]
    pub status: Option<RecordedStatus>,
    #[serde(deserialize_with = "nullable")]
    pub output: Option<serde_json::Value>,
}

/// One earlier terminal run of the series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryRecord {
    /// The run's identifier.
    pub run: String,
    /// RFC 3339.
    pub started_at: String,
    /// RFC 3339. Records are ordered by this.
    pub ended_at: String,
    /// Null where the run wrote no shutdown event.
    #[serde(deserialize_with = "nullable")]
    pub outcome: Option<RunOutcome>,
    #[serde(deserialize_with = "nullable")]
    pub verdict: Option<RunVerdict>,
    /// The pack revision the run ran.
    pub revision: String,
    /// A controller-owned link to the run.
    pub link: String,
    pub entry: HistoryEntry,
}

/// The document in [`ENV_HISTORY`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuppliedHistory {
    pub version: u8,
    pub records: Vec<HistoryRecord>,
}

/// A nullable field that must be present: a missing one is refused rather than read as null.
fn nullable<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryError {
    Decode { detail: String },
    Version { got: u8 },
    TooManyRecords { got: usize },
    DuplicateRun { run: String },
    EmptyRun,
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HistoryError::Decode { detail } => write!(f, "{ENV_HISTORY} does not decode: {detail}"),
            HistoryError::Version { got } => write!(
                f,
                "{ENV_HISTORY} is version {got}; this engine reads version {HISTORY_WIRE_VERSION}"
            ),
            HistoryError::TooManyRecords { got } => write!(
                f,
                "{ENV_HISTORY} carries {got} records; the most a run is supplied is \
                 {MAX_HISTORY_DEPTH}"
            ),
            HistoryError::DuplicateRun { run } => {
                write!(f, "{ENV_HISTORY} carries run {run:?} more than once")
            }
            HistoryError::EmptyRun => write!(f, "{ENV_HISTORY} carries a record with no run"),
        }
    }
}

impl std::error::Error for HistoryError {}

impl SuppliedHistory {
    pub fn parse(text: &str) -> Result<Self, HistoryError> {
        let doc: SuppliedHistory =
            crate::json::from_str(text).map_err(|e| HistoryError::Decode {
                detail: e.to_string(),
            })?;
        if doc.version != HISTORY_WIRE_VERSION {
            return Err(HistoryError::Version { got: doc.version });
        }
        if doc.records.len() > MAX_HISTORY_DEPTH as usize {
            return Err(HistoryError::TooManyRecords {
                got: doc.records.len(),
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        for record in &doc.records {
            if record.run.is_empty() {
                return Err(HistoryError::EmptyRun);
            }
            if !seen.insert(record.run.as_str()) {
                return Err(HistoryError::DuplicateRun {
                    run: record.run.clone(),
                });
            }
        }
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use crate::history::{
        HISTORY_WIRE_VERSION, HistoryEntry, HistoryError, HistoryRecord, MAX_HISTORY_DEPTH,
        RecordedStatus, RunOutcome, RunVerdict, SuppliedHistory,
    };

    fn record(run: &str) -> serde_json::Value {
        serde_json::json!({
            "run": run,
            "started_at": "2026-09-01T00:00:00Z",
            "ended_at": "2026-09-01T00:10:00Z",
            "outcome": "finished",
            "verdict": "valid",
            "revision": "abc123",
            "link": "https://controller.test/runs/r1",
            "entry": {"task": "triage", "status": "pass", "output": {"found": 2.5}},
        })
    }

    fn doc(records: Vec<serde_json::Value>) -> String {
        serde_json::json!({"version": HISTORY_WIRE_VERSION, "records": records}).to_string()
    }

    #[test]
    fn a_full_record_round_trips_byte_for_byte() {
        let text = doc(vec![record("r1")]);
        let parsed = SuppliedHistory::parse(&text).unwrap();
        assert_eq!(
            parsed.records[0],
            HistoryRecord {
                run: "r1".into(),
                started_at: "2026-09-01T00:00:00Z".into(),
                ended_at: "2026-09-01T00:10:00Z".into(),
                outcome: Some(RunOutcome::Finished),
                verdict: Some(RunVerdict::Valid),
                revision: "abc123".into(),
                link: "https://controller.test/runs/r1".into(),
                entry: HistoryEntry {
                    task: "triage".into(),
                    status: Some(RecordedStatus::Pass),
                    output: parsed.records[0].entry.output.clone(),
                },
            }
        );
        let original: serde_json::Value = serde_json::from_str(&text).unwrap();
        let again: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&parsed).unwrap()).unwrap();
        assert_eq!(original, again);
    }

    #[test]
    fn nulls_decode_but_a_missing_nullable_field_does_not() {
        let mut nulls = record("r1");
        nulls["outcome"] = serde_json::Value::Null;
        nulls["verdict"] = serde_json::Value::Null;
        nulls["entry"]["status"] = serde_json::Value::Null;
        nulls["entry"]["output"] = serde_json::Value::Null;
        let parsed = SuppliedHistory::parse(&doc(vec![nulls])).unwrap();
        assert_eq!(parsed.records[0].outcome, None);
        assert_eq!(parsed.records[0].entry.status, None);

        for path in [
            &["outcome"][..],
            &["verdict"],
            &["entry", "status"],
            &["entry", "output"],
        ] {
            let mut missing = record("r1");
            let mut at = &mut missing;
            for key in &path[..path.len() - 1] {
                at = &mut at[*key];
            }
            at.as_object_mut().unwrap().remove(path[path.len() - 1]);
            assert!(
                matches!(
                    SuppliedHistory::parse(&doc(vec![missing])),
                    Err(HistoryError::Decode { .. })
                ),
                "{path:?} missing must be refused"
            );
        }
    }

    #[test]
    fn a_field_nobody_defined_is_refused() {
        let mut extra = record("r1");
        extra["prompt"] = "earlier agent text".into();
        assert!(matches!(
            SuppliedHistory::parse(&doc(vec![extra])),
            Err(HistoryError::Decode { .. })
        ));
        let mut extra = record("r1");
        extra["entry"]["stdout"] = "noise".into();
        assert!(matches!(
            SuppliedHistory::parse(&doc(vec![extra])),
            Err(HistoryError::Decode { .. })
        ));
    }

    #[test]
    fn unknown_tokens_versions_and_oversized_or_repeated_series_are_refused() {
        let mut bad = record("r1");
        bad["outcome"] = "exploded".into();
        assert!(matches!(
            SuppliedHistory::parse(&doc(vec![bad])),
            Err(HistoryError::Decode { .. })
        ));
        let mut bad = record("r1");
        bad["entry"]["status"] = "passed".into();
        assert!(matches!(
            SuppliedHistory::parse(&doc(vec![bad])),
            Err(HistoryError::Decode { .. })
        ));
        assert_eq!(
            SuppliedHistory::parse(r#"{"version":2,"records":[]}"#),
            Err(HistoryError::Version { got: 2 })
        );
        let many: Vec<_> = (0..=MAX_HISTORY_DEPTH)
            .map(|i| record(&format!("r{i}")))
            .collect();
        assert_eq!(
            SuppliedHistory::parse(&doc(many)),
            Err(HistoryError::TooManyRecords {
                got: MAX_HISTORY_DEPTH as usize + 1
            })
        );
        assert_eq!(
            SuppliedHistory::parse(&doc(vec![record("r1"), record("r1")])),
            Err(HistoryError::DuplicateRun { run: "r1".into() })
        );
        assert_eq!(
            SuppliedHistory::parse(&doc(vec![record("")])),
            Err(HistoryError::EmptyRun)
        );
        assert!(matches!(
            SuppliedHistory::parse("not json"),
            Err(HistoryError::Decode { .. })
        ));
    }
}
