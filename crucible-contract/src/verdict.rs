//! What one grounded ranking turn decided, as the `--json` document and the `VERDICT_MARKER` line
//! carry it.
//!
//! A turn either ruled or it did not, and both shapes carry what the turn spent. Spending is not
//! optional in either: a document missing `cost_usd` fails to decode rather than reading as free.

use serde::{Deserialize, Serialize};

use crate::tier::Disposition;

/// Why a grounded turn yielded no verdict. The two cases are not interchangeable: a turn that
/// never started tells the caller to look at the pod, a completed turn without a verdict tells it
/// to look at the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroundedErrorKind {
    /// The turn itself never completed (spawn, gateway, sandbox, transfer).
    TurnFailed,
    /// The turn ran and its last line was not a verdict.
    NoVerdict,
}

impl GroundedErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            GroundedErrorKind::TurnFailed => "turn_failed",
            GroundedErrorKind::NoVerdict => "no_verdict",
        }
    }
}

/// One grounded ranking turn's result. The two shapes are told apart by their own fields: a ruling
/// carries `tier`, a failure carries `error`.
///
/// Serializes untagged; decoded by dispatching on `error` rather than by `#[serde(untagged)]`,
/// which cannot read a float when `serde_json/arbitrary_precision` is on — and `starlark` turns it
/// on for any binary that links the engine (see [`crate::json`]). An untagged decode of this type
/// therefore passes in this crate's own tests and fails in the controller, which is the worst way
/// for a wire type to break.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum GroundedVerdict {
    Ruled {
        tier: Disposition,
        rationale: String,
        /// Absent on a line an older engine wrote; it says how sure the ruling is, not whether one
        /// was made.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confidence: Option<String>,
        /// Required on purpose: a turn that spent money must say so, and a document without this
        /// fails to decode rather than reading as free.
        cost_usd: f64,
        #[serde(default)]
        over_budget: bool,
    },
    Failed {
        error: String,
        error_kind: GroundedErrorKind,
        /// The tail of whatever the agent printed on its way down. A turn that dies during startup
        /// prints its diagnostic and nothing else.
        #[serde(default)]
        output_tail: Option<String>,
        /// Required for the same reason as a ruling's: a turn that broke after billing is not free.
        cost_usd: f64,
        #[serde(default)]
        over_budget: bool,
    },
}

/// The ruling shape, derived so the fields stay declarative; fed a `Value` so nothing goes through
/// serde's `Content` buffer.
#[derive(Deserialize)]
struct RuledWire {
    tier: Disposition,
    rationale: String,
    #[serde(default)]
    confidence: Option<String>,
    cost_usd: f64,
    #[serde(default)]
    over_budget: bool,
}

/// The failure shape, same treatment.
#[derive(Deserialize)]
struct FailedWire {
    error: String,
    error_kind: GroundedErrorKind,
    #[serde(default)]
    output_tail: Option<String>,
    cost_usd: f64,
    #[serde(default)]
    over_budget: bool,
}

impl<'de> Deserialize<'de> for GroundedVerdict {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        use serde::de::Error as _;
        let value = serde_json::Value::deserialize(deserializer)?;
        if value.get("error").is_some() {
            let w: FailedWire = serde_json::from_value(value).map_err(D::Error::custom)?;
            Ok(GroundedVerdict::Failed {
                error: w.error,
                error_kind: w.error_kind,
                output_tail: w.output_tail,
                cost_usd: w.cost_usd,
                over_budget: w.over_budget,
            })
        } else {
            let w: RuledWire = serde_json::from_value(value).map_err(D::Error::custom)?;
            Ok(GroundedVerdict::Ruled {
                tier: w.tier,
                rationale: w.rationale,
                confidence: w.confidence,
                cost_usd: w.cost_usd,
                over_budget: w.over_budget,
            })
        }
    }
}

impl GroundedVerdict {
    /// What the turn spent, whichever way it ended. A partial turn is never free.
    pub fn cost_usd(&self) -> f64 {
        match self {
            GroundedVerdict::Ruled { cost_usd, .. } | GroundedVerdict::Failed { cost_usd, .. } => {
                *cost_usd
            }
        }
    }

    pub fn over_budget(&self) -> bool {
        match self {
            GroundedVerdict::Ruled { over_budget, .. }
            | GroundedVerdict::Failed { over_budget, .. } => *over_budget,
        }
    }

    /// The ruling, if the turn produced one.
    pub fn disposition(&self) -> Option<Disposition> {
        match self {
            GroundedVerdict::Ruled { tier, .. } => Some(*tier),
            GroundedVerdict::Failed { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier::Tier;

    fn ruled() -> GroundedVerdict {
        GroundedVerdict::Ruled {
            tier: Disposition::Tier(Tier::T1),
            rationale: "needs a live cluster".to_string(),
            confidence: Some("high".to_string()),
            cost_usd: 0.42,
            over_budget: false,
        }
    }

    fn failed() -> GroundedVerdict {
        GroundedVerdict::Failed {
            error: "the turn never completed: agent spawn failed: boom".to_string(),
            error_kind: GroundedErrorKind::TurnFailed,
            output_tail: Some("could not resolve host".to_string()),
            cost_usd: 0.01,
            over_budget: false,
        }
    }

    /// The bytes a deployed engine already emits, field for field.
    #[test]
    fn both_shapes_are_unchanged() {
        assert_eq!(
            serde_json::to_string(&ruled()).expect("serialize"),
            r#"{"tier":"T1","rationale":"needs a live cluster","confidence":"high","cost_usd":0.42,"over_budget":false}"#
        );
        assert_eq!(
            serde_json::to_string(&failed()).expect("serialize"),
            r#"{"error":"the turn never completed: agent spawn failed: boom","error_kind":"turn_failed","output_tail":"could not resolve host","cost_usd":0.01,"over_budget":false}"#
        );
        assert_eq!(
            serde_json::to_value(Disposition::Stale).expect("serialize"),
            "stale"
        );
    }

    #[test]
    fn each_shape_decodes_back_to_itself() {
        for verdict in [ruled(), failed()] {
            let encoded = serde_json::to_string(&verdict).expect("serialize");
            let decoded: GroundedVerdict = crate::json::from_str(&encoded).expect("deserialize");
            assert_eq!(decoded, verdict);
        }
    }

    /// A line an older engine wrote carries no `confidence` and no `over_budget`. Those say how
    /// sure a ruling is and whether it overran, not whether a ruling happened, so their absence
    /// decodes rather than refusing the whole document.
    #[test]
    fn the_fields_that_are_not_the_cost_may_be_absent() {
        let sparse = r#"{"tier":"stale","rationale":"already implemented","cost_usd":0.2}"#;
        let decoded: GroundedVerdict = crate::json::from_str(sparse).expect("deserialize");
        assert_eq!(decoded.disposition(), Some(Disposition::Stale));
        assert_eq!(decoded.cost_usd(), 0.2);
        assert!(!decoded.over_budget());

        let failed = r#"{"error":"boom","error_kind":"no_verdict","cost_usd":0.0}"#;
        let decoded: GroundedVerdict = crate::json::from_str(failed).expect("deserialize");
        assert!(decoded.disposition().is_none());
    }

    /// The bug this closes: a cost that goes missing must not read as zero.
    #[test]
    fn a_document_without_a_cost_is_refused() {
        let no_cost = r#"{"tier":"T1","rationale":"r","confidence":"high","over_budget":false}"#;
        assert!(crate::json::from_str::<GroundedVerdict>(no_cost).is_err());

        let with_cost = r#"{"tier":"T1","rationale":"r","confidence":"high","cost_usd":2.5,"over_budget":false}"#;
        let decoded: GroundedVerdict = crate::json::from_str(with_cost).expect("deserialize");
        assert_eq!(decoded.cost_usd(), 2.5);
    }

    #[test]
    fn a_failure_is_never_read_as_a_ruling() {
        let decoded: GroundedVerdict =
            crate::json::from_str(&serde_json::to_string(&failed()).expect("serialize"))
                .expect("deserialize");
        assert!(decoded.disposition().is_none());
        assert_eq!(decoded.cost_usd(), 0.01);
        assert_eq!(ruled().disposition(), Some(Disposition::Tier(Tier::T1)));
    }

    /// A stale ruling with a null tail still tells the two shapes apart.
    #[test]
    fn a_null_output_tail_still_decodes_as_a_failure() {
        let doc = r#"{"error":"no verdict","error_kind":"no_verdict","output_tail":null,"cost_usd":0.0,"over_budget":true}"#;
        let decoded: GroundedVerdict = crate::json::from_str(doc).expect("deserialize");
        assert_eq!(
            decoded,
            GroundedVerdict::Failed {
                error: "no verdict".to_string(),
                error_kind: GroundedErrorKind::NoVerdict,
                output_tail: None,
                cost_usd: 0.0,
                over_budget: true,
            }
        );
        assert!(decoded.over_budget());
    }
}
