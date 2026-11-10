//! What one `crucible draft` turn decided, as the `--json` document its last stdout line carries.
//!
//! The engine exits nonzero when the pack never compiled or blew the budget but still prints the
//! document, so the exit status is not the contract: these fields are. The controller decodes this
//! at the subprocess boundary; the findings are the engine's own compile diagnostics and `declined`
//! is the drafting agent's refusal as the engine read it back.

use serde::{Deserialize, Serialize};

/// The drafting loop's outcome. `ok` says the pack in `--out` passed the compile check;
/// `over_budget` and `declined` are separate answers a caller must not collapse into it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DraftReport {
    pub ok: bool,
    /// Required on purpose: a turn that spent money must say so, and a document without this
    /// fails to decode rather than reading as free.
    pub cost_usd: f64,
    #[serde(default)]
    pub over_budget: bool,
    #[serde(default)]
    pub rounds: u32,
    #[serde(default)]
    pub findings: Vec<String>,
    /// The drafting agent's own refusal (its `REJECTED.md`): a clean answer, not a failure to run.
    #[serde(default)]
    pub declined: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiled() -> DraftReport {
        DraftReport {
            ok: true,
            cost_usd: 0.42,
            over_budget: false,
            rounds: 2,
            findings: Vec::new(),
            declined: None,
        }
    }

    /// The bytes the engine emits, field for field.
    #[test]
    fn the_document_is_unchanged() {
        assert_eq!(
            serde_json::to_string(&compiled()).expect("serialize"),
            r#"{"ok":true,"cost_usd":0.42,"over_budget":false,"rounds":2,"findings":[],"declined":null}"#
        );
    }

    #[test]
    fn a_document_decodes_back_to_itself() {
        let encoded = serde_json::to_string(&compiled()).expect("serialize");
        let decoded: DraftReport = crate::json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, compiled());
    }

    /// A line an older engine wrote carries no `over_budget`, no `rounds`, no `findings`, no
    /// `declined`. Those describe how the turn went, not whether it went, so their absence decodes
    /// rather than refusing the whole document.
    #[test]
    fn the_fields_that_are_not_the_cost_may_be_absent() {
        let sparse = r#"{"ok":false,"cost_usd":1.5}"#;
        let decoded: DraftReport = crate::json::from_str(sparse).expect("deserialize");
        assert!(!decoded.ok);
        assert_eq!(decoded.cost_usd, 1.5);
        assert!(decoded.findings.is_empty());
        assert!(decoded.declined.is_none());
    }

    /// The bug this closes: a cost that goes missing must not read as zero.
    #[test]
    fn a_document_without_a_cost_is_refused() {
        let no_cost = r#"{"ok":true,"rounds":1,"findings":[]}"#;
        assert!(crate::json::from_str::<DraftReport>(no_cost).is_err());
    }
}
