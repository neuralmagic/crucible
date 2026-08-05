//! Admission-ledger wire format for `state/admissions.jsonl`. One versioned NDJSON line
//! per event; per key, exactly one `Admitted` then at most one `Settled` (first terminal
//! wins). Blank/torn lines decode to `None`.

use serde::{Deserialize, Serialize};

/// Bump only on a breaking change to the on-disk shape.
pub const ADMISSION_WIRE_VERSION: u8 = 1;

/// Keys index an in-memory map for the life of the run, so they are bounded at the door.
pub const MAX_KEY_LEN: usize = 256;

/// Idempotency key: client-supplied when the source has a natural one
/// (`pr-comment:<owner>/<repo>#<n>:<comment_id>`), generated when absent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AdmissionKey(String);

impl AdmissionKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// A redelivered `approve` for the same ask converges instead of granting twice.
    pub fn approve(trace_id: &str) -> Self {
        Self(format!("approve:{trace_id}"))
    }

    /// The key of the re-scope an approval converts into. Derived, not generated, so a
    /// resume can tell "this grant was recorded" from "some other re-scope is pending".
    pub fn rescope_from(approve: &AdmissionKey) -> Self {
        Self(format!("rescope-from:{}", approve.0))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AdmissionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Steer provenance, queryable from the record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum SteerSource {
    /// A `steer` command over the control bridge.
    Operator,
    /// A PR review comment relayed by `watch-pr`.
    PrComment { url: String, author: String },
    /// Read out of `STEER.md` at the drain.
    SteerFile,
}

/// One variant per mutating bridge command; read-only commands are never admitted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "input", rename_all = "snake_case")]
pub enum AdmittedInput {
    Steer { text: String, from: SteerSource },
    Approve,
    Deny { reason: String },
    Rescope { regime: String },
    SetBudget { usd: f64 },
    Pause,
    Resume,
    Stop,
    Abort,
}

impl AdmittedInput {
    pub fn cmd(&self) -> &'static str {
        match self {
            AdmittedInput::Steer { .. } => "steer",
            AdmittedInput::Approve => "approve",
            AdmittedInput::Deny { .. } => "deny",
            AdmittedInput::Rescope { .. } => "rescope",
            AdmittedInput::SetBudget { .. } => "set-budget",
            AdmittedInput::Pause => "pause",
            AdmittedInput::Resume => "resume",
            AdmittedInput::Stop => "stop",
            AdmittedInput::Abort => "abort",
        }
    }
}

/// The one terminal state an admitted input reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionOutcome {
    /// Took effect (a steer reached a turn's prompt, a re-scope re-baselined).
    Applied,
    /// A newer input replaced it, or a resume overrode it.
    Superseded,
    /// Could not take effect (an `approve` with nothing pending).
    Rejected,
}

impl AdmissionOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            AdmissionOutcome::Applied => "applied",
            AdmissionOutcome::Superseded => "superseded",
            AdmissionOutcome::Rejected => "rejected",
        }
    }
}

impl std::fmt::Display for AdmissionOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One line in `state/admissions.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AdmissionEvent {
    /// Durably recorded BEFORE the input took effect. `seq` is the admission order the
    /// steer drain replays in.
    Admitted {
        key: AdmissionKey,
        seq: u64,
        ts: u64,
        #[serde(flatten)]
        input: AdmittedInput,
    },
    /// The terminal outcome. A key with no `Settled` line is still outstanding, which is
    /// exactly what a resume re-arms.
    Settled {
        key: AdmissionKey,
        outcome: AdmissionOutcome,
        ts: u64,
        #[serde(default)]
        note: String,
    },
}

impl AdmissionEvent {
    pub fn key(&self) -> &AdmissionKey {
        match self {
            AdmissionEvent::Admitted { key, .. } | AdmissionEvent::Settled { key, .. } => key,
        }
    }
}

/// Borrowing envelope so encoding doesn't clone the event.
#[derive(Serialize)]
struct EnvelopeRef<'a> {
    v: u8,
    #[serde(flatten)]
    event: &'a AdmissionEvent,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(default = "default_version")]
    #[allow(dead_code)] // read for forward-compat; we accept any v for now
    v: u8,
    #[serde(flatten)]
    event: AdmissionEvent,
}

fn default_version() -> u8 {
    ADMISSION_WIRE_VERSION
}

/// One NDJSON line, no trailing newline. A serde failure yields a line that decodes to
/// `None` rather than a half-written record.
pub fn encode(ev: &AdmissionEvent) -> String {
    serde_json::to_string(&EnvelopeRef {
        v: ADMISSION_WIRE_VERSION,
        event: ev,
    })
    .unwrap_or_else(|e| format!("encode error: {e}"))
}

/// Decode one NDJSON line. `None` for a blank or torn line, so a fold over a killed
/// writer's file skips the tail instead of failing.
pub fn decode(line: &str) -> Option<AdmissionEvent> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    serde_json::from_str::<Envelope>(t).ok().map(|e| e.event)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> Vec<AdmittedInput> {
        vec![
            AdmittedInput::Steer {
                text: "try cache-first".into(),
                from: SteerSource::Operator,
            },
            AdmittedInput::Steer {
                text: "reviewer says hoist it".into(),
                from: SteerSource::PrComment {
                    url: "https://github.com/o/r/pull/7#issuecomment-1".into(),
                    author: "reviewer".into(),
                },
            },
            AdmittedInput::Steer {
                text: "from the file".into(),
                from: SteerSource::SteerFile,
            },
            AdmittedInput::Approve,
            AdmittedInput::Deny {
                reason: "over budget".into(),
            },
            AdmittedInput::Rescope {
                regime: "concurrency=48".into(),
            },
            AdmittedInput::SetBudget { usd: 2.5 },
            AdmittedInput::Pause,
            AdmittedInput::Resume,
            AdmittedInput::Stop,
            AdmittedInput::Abort,
        ]
    }

    #[test]
    fn every_event_round_trips_byte_identically() {
        let mut events: Vec<AdmissionEvent> = inputs()
            .into_iter()
            .enumerate()
            .map(|(i, input)| AdmissionEvent::Admitted {
                key: AdmissionKey::new(format!("k{i}")),
                seq: i as u64,
                ts: 1_700_000_000 + i as u64,
                input,
            })
            .collect();
        for outcome in [
            AdmissionOutcome::Applied,
            AdmissionOutcome::Superseded,
            AdmissionOutcome::Rejected,
        ] {
            events.push(AdmissionEvent::Settled {
                key: AdmissionKey::new("k0"),
                outcome,
                ts: 1_700_000_001,
                note: "because".into(),
            });
        }
        for ev in &events {
            let line = encode(ev);
            let back = decode(&line).expect("decodes");
            assert_eq!(&back, ev, "{line}");
            assert_eq!(encode(&back), line, "re-encode is byte-identical");
        }
    }

    #[test]
    fn admitted_carries_its_input_inline() {
        let line = encode(&AdmissionEvent::Admitted {
            key: AdmissionKey::new("k"),
            seq: 3,
            ts: 9,
            input: AdmittedInput::Rescope {
                regime: "c=48".into(),
            },
        });
        assert_eq!(
            line,
            r#"{"v":1,"kind":"admitted","key":"k","seq":3,"ts":9,"input":"rescope","regime":"c=48"}"#
        );
    }

    #[test]
    fn blank_and_torn_lines_decode_to_none() {
        assert!(decode("").is_none());
        assert!(decode("   ").is_none());
        assert!(decode(r#"{"v":1,"kind":"admitted","key":"k""#).is_none());
        // A well-formed line of some other schema is not an admission event.
        assert!(decode(r#"{"v":1,"kind":"note","msg":"a"}"#).is_none());
    }

    #[test]
    fn settled_note_is_optional_for_older_writers() {
        let ev = decode(r#"{"v":1,"kind":"settled","key":"k","outcome":"applied","ts":1}"#)
            .expect("decodes");
        assert!(matches!(
            ev,
            AdmissionEvent::Settled { ref note, .. } if note.is_empty()
        ));
    }

    #[test]
    fn derived_keys_are_computable_from_the_ask() {
        let approve = AdmissionKey::approve("model=Q;c=48");
        assert_eq!(approve.as_str(), "approve:model=Q;c=48");
        assert_eq!(
            AdmissionKey::rescope_from(&approve).as_str(),
            "rescope-from:approve:model=Q;c=48"
        );
    }
}
