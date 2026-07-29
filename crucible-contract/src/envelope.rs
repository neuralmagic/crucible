//! The kubelet-captured termination-message envelope the turn pod writes to
//! `/dev/termination-log`, read back from `pod.status.containerStatuses[].state.terminated.message`.
//!
//! The kubelet truncates *silently* past 4096 bytes, so the envelope self-caps at
//! [`TERMINATION_MESSAGE_CAP`] with honest field-level truncation: a self-capped envelope is still
//! valid JSON, a kubelet-truncated one is garbage.

use crate::artifact::ArtifactRef;
use crate::event::ModelUsage;
use serde::{Deserialize, Serialize};

/// The envelope schema version. Bumped only on a breaking change to the wire shape.
pub const SCHEMA_VERSION: u8 = 1;

/// The engine's self-cap on the serialized termination message, in bytes. Below the kubelet's
/// silent 4096-byte truncation, so a parse failure always indicates a bug rather than a
/// size-dependent truncation.
pub const TERMINATION_MESSAGE_CAP: usize = 3584;

/// The truncation marker appended to a shortened string field. Three UTF-8 bytes.
const ELLIPSIS: &str = "…";

/// Which turn produced this result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeKind {
    /// A `crucible rank-grounded` verdict.
    Verdict,
    /// A `crucible scope` report.
    ScopeReport,
}

/// The optional OTEL usage rollup that rides the envelope: authoritative cost on the ledger row.
/// Under self-cap pressure `models` drops first; the totals stay.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Usage {
    cost_usd: f64,
    total: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    models: Vec<ModelUsage>,
    api_requests: u32,
    api_ms: f64,
    active_seconds: f64,
}

/// The Tier 1 versioned envelope. `payload` is the turn's own result shape (verdict / scope report),
/// kept opaque here so the contract crate stays free of engine types; `artifacts` is the
/// authoritative manifest of every Tier 2 upload the pod attempted.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Envelope {
    v: u8,
    pub kind: EnvelopeKind,
    pub payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

impl Envelope {
    /// A new envelope at the current [`SCHEMA_VERSION`], with no artifacts and no usage.
    pub fn new(kind: EnvelopeKind, payload: serde_json::Value) -> Self {
        Envelope {
            v: SCHEMA_VERSION,
            kind,
            payload,
            artifacts: Vec::new(),
            usage: None,
        }
    }

    /// Serialize for the termination message, self-capped at [`TERMINATION_MESSAGE_CAP`]; always
    /// valid JSON. Truncation order: drop per-model usage rows, then shorten the longest payload
    /// string repeatedly (ellipsis-marked). The manifest and usage totals are never dropped
    /// because a missing artifact must stay detectable.
    pub fn to_capped_json(&self) -> Result<String, serde_json::Error> {
        let mut e = self.clone();
        let mut s = serde_json::to_string(&e)?;
        if s.len() <= TERMINATION_MESSAGE_CAP {
            return Ok(s);
        }

        // 1. Per-model rows drop first; the usage totals stay.
        if let Some(usage) = e.usage.as_mut()
            && !usage.models.is_empty()
        {
            usage.models.clear();
            s = serde_json::to_string(&e)?;
            if s.len() <= TERMINATION_MESSAGE_CAP {
                return Ok(s);
            }
        }

        // 2. Truncate the current longest payload string, repeatedly, until it fits.
        while s.len() > TERMINATION_MESSAGE_CAP {
            let over = s.len() - TERMINATION_MESSAGE_CAP;
            let Some(longest) = longest_string_mut(&mut e.payload) else {
                break; // no string left to shorten; return best-effort valid JSON
            };
            if longest.is_empty() {
                break;
            }
            let keep = longest.len().saturating_sub(over + ELLIPSIS.len());
            if keep == 0 {
                longest.clear();
            } else {
                let cut = floor_char_boundary(longest, keep);
                longest.truncate(cut);
                longest.push_str(ELLIPSIS);
            }
            s = serde_json::to_string(&e)?;
        }

        Ok(s)
    }
}

/// The longest `String` node anywhere in a JSON value (recursing objects and arrays), as a mutable
/// reference so the caller can truncate it in place.
fn longest_string_mut(v: &mut serde_json::Value) -> Option<&mut String> {
    match v {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Array(a) => a
            .iter_mut()
            .filter_map(longest_string_mut)
            .max_by_key(|s| s.len()),
        serde_json::Value::Object(o) => o
            .values_mut()
            .filter_map(longest_string_mut)
            .max_by_key(|s| s.len()),
        _ => None,
    }
}

/// The largest byte index `<= i` that lands on a UTF-8 char boundary of `s`.
fn floor_char_boundary(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    let mut b = i;
    while b > 0 && !s.is_char_boundary(b) {
        b -= 1;
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn small_envelope_round_trips_unchanged() {
        let e = Envelope::new(
            EnvelopeKind::Verdict,
            json!({"tier":"T1","rationale":"needs a new bench","confidence":"low"}),
        );
        let s = e.to_capped_json().unwrap();
        assert!(s.len() <= TERMINATION_MESSAGE_CAP);
        let back: Envelope = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
        assert_eq!(back.v, SCHEMA_VERSION);
    }

    #[test]
    fn oversize_payload_stays_under_cap_and_valid() {
        let huge = "x".repeat(20_000);
        let e = Envelope::new(
            EnvelopeKind::ScopeReport,
            json!({"summary": huge, "digest": "v1:beef"}),
        );
        let s = e.to_capped_json().unwrap();
        assert!(
            s.len() <= TERMINATION_MESSAGE_CAP,
            "capped size {} exceeds {}",
            s.len(),
            TERMINATION_MESSAGE_CAP
        );
        let back: Envelope = serde_json::from_str(&s).unwrap();
        assert_eq!(back.kind, EnvelopeKind::ScopeReport);
        assert_eq!(back.payload["digest"], json!("v1:beef"));
        let summary = back.payload["summary"].as_str().unwrap();
        assert!(summary.len() < huge.len());
        assert!(summary.ends_with(ELLIPSIS));
    }

    #[test]
    fn per_model_rows_drop_before_totals() {
        let models: Vec<ModelUsage> = (0..200)
            .map(|i| ModelUsage {
                model: format!("claude-model-variant-number-{i}"),
                input: 1000,
                output: 500,
                cache_read: 88_000,
                cache_write: 0,
                cost_usd: 0.42,
            })
            .collect();
        let mut e = Envelope::new(EnvelopeKind::Verdict, json!({"tier":"T1"}));
        e.usage = Some(Usage {
            cost_usd: 12.34,
            total: 89_200,
            models,
            api_requests: 12,
            api_ms: 80_110.0,
            active_seconds: 372.0,
        });
        let s = e.to_capped_json().unwrap();
        assert!(s.len() <= TERMINATION_MESSAGE_CAP);
        let back: Envelope = serde_json::from_str(&s).unwrap();
        let usage = back.usage.expect("usage totals survive");
        assert!(usage.models.is_empty());
        assert_eq!(usage.cost_usd, 12.34);
        assert_eq!(usage.total, 89_200);
        assert_eq!(usage.api_requests, 12);
    }

    #[test]
    fn totals_kept_when_only_rows_are_the_overshoot() {
        let models: Vec<ModelUsage> = (0..50)
            .map(|i| ModelUsage {
                model: format!("m{i}"),
                cost_usd: 0.1,
                ..Default::default()
            })
            .collect();
        let mut e = Envelope::new(EnvelopeKind::Verdict, json!({"tier":"N","rationale":"x"}));
        e.usage = Some(Usage {
            cost_usd: 1.0,
            total: 10,
            models,
            api_requests: 3,
            api_ms: 1.0,
            active_seconds: 2.0,
        });
        let s = e.to_capped_json().unwrap();
        assert!(s.len() <= TERMINATION_MESSAGE_CAP);
        let back: Envelope = serde_json::from_str(&s).unwrap();
        // Dropping rows alone was enough; the payload must be untouched.
        assert_eq!(back.payload["rationale"], json!("x"));
    }

    #[test]
    fn truncates_the_longest_of_several_strings() {
        let e = Envelope::new(
            EnvelopeKind::ScopeReport,
            json!({"short":"keep me", "long": "y".repeat(10_000)}),
        );
        let s = e.to_capped_json().unwrap();
        assert!(s.len() <= TERMINATION_MESSAGE_CAP);
        let back: Envelope = serde_json::from_str(&s).unwrap();
        assert_eq!(back.payload["short"], json!("keep me"));
        assert!(back.payload["long"].as_str().unwrap().ends_with(ELLIPSIS));
    }

    #[test]
    fn floor_char_boundary_never_splits_a_codepoint() {
        let s = "abc€def"; // '€' is 3 bytes at index 3..6
        for i in 0..=s.len() {
            let b = floor_char_boundary(s, i);
            assert!(s.is_char_boundary(b));
            let _ = &s[..b]; // must not panic
        }
    }

    #[test]
    fn truncation_respects_multibyte_boundaries() {
        let e = Envelope::new(
            EnvelopeKind::ScopeReport,
            json!({"rationale": "€".repeat(5_000)}),
        );
        let s = e.to_capped_json().unwrap();
        assert!(s.len() <= TERMINATION_MESSAGE_CAP);
        let back: Envelope = serde_json::from_str(&s).unwrap(); // valid UTF-8 + JSON
        assert!(
            back.payload["rationale"]
                .as_str()
                .unwrap()
                .ends_with(ELLIPSIS)
        );
    }
}
