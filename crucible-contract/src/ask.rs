//! What a run proposes for an orchestrator to admit.
//!
//! An ask is a description of work for another run to perform. The emitting run never dispatches
//! one: it writes asks to its session log and stops, and a receiving orchestrator decides what
//! becomes a run. That is what keeps a run from widening its own blast radius.
//!
//! The key is the whole idempotency story. It is supplied by the emitter, and it must name the
//! same thing on every run that finds that thing, so an orchestrator can recognize a repeat
//! without reading the ask's contents. Positional identity is what makes a mapped fan-out
//! reprocess the wrong item after its input list shifts; a key derived from the item does not
//! move when the list does.
//!
//! [`AskKey`] and [`crate::AdmissionKey`] share a bound and a discipline but not a scope. An
//! admission key is run-local: it settles once inside the run that admitted it. An ask key
//! crosses runs, and the orchestrator composes it with the named workflow to get the key it
//! queues under.

use serde::{Deserialize, Serialize};

use crate::admission::MAX_KEY_LEN;

/// Why a key or a workflow name was refused. Carries the offending value so a compile-time
/// diagnostic can quote it back to the pack author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskKeyError {
    Empty,
    TooLong {
        len: usize,
    },
    Control {
        value: String,
    },
    Whitespace {
        value: String,
    },
    /// Only a workflow name is refused for this: the orchestrator's key puts the workflow
    /// before the item key with `:` between, and parsing splits from the left.
    Separator {
        value: String,
    },
}

impl std::fmt::Display for AskKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AskKeyError::Empty => f.write_str("an ask key is empty"),
            AskKeyError::TooLong { len } => {
                write!(f, "an ask key is {len} bytes; maximum is {MAX_KEY_LEN}")
            }
            AskKeyError::Control { value } => {
                write!(f, "ask key {value:?} contains a control character")
            }
            AskKeyError::Whitespace { value } => {
                write!(f, "ask key {value:?} contains whitespace")
            }
            AskKeyError::Separator { value } => {
                write!(f, "workflow name {value:?} contains ':'")
            }
        }
    }
}

impl std::error::Error for AskKeyError {}

/// The emitter-supplied identity of the item an ask is about, stable across runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AskKey(String);

impl AskKey {
    /// Reject anything that cannot survive the round trip to an orchestrator: an NDJSON field,
    /// a queue key, and a database primary key all in turn.
    pub fn new(key: impl Into<String>) -> Result<Self, AskKeyError> {
        let key = key.into();
        if key.is_empty() {
            return Err(AskKeyError::Empty);
        }
        if key.len() > MAX_KEY_LEN {
            return Err(AskKeyError::TooLong { len: key.len() });
        }
        if key.chars().any(char::is_control) {
            return Err(AskKeyError::Control { value: key });
        }
        if key.chars().any(char::is_whitespace) {
            return Err(AskKeyError::Whitespace { value: key });
        }
        Ok(Self(key))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The key an orchestrator queues this ask under. Two workflows asking about one item are
    /// two pieces of work, so the workflow is part of the identity rather than a property of it.
    ///
    /// The `ask:` tag is what lets an orchestrator that predates asks treat the row as an input
    /// kind it does not recognize, and leave it inert, instead of failing the read.
    pub fn input_key(&self, workflow: &str) -> String {
        format!("ask:{workflow}:{}", self.0)
    }
}

impl std::fmt::Display for AskKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for AskKey {
    type Error = AskKeyError;

    fn try_from(key: String) -> Result<Self, Self::Error> {
        AskKey::new(key)
    }
}

impl From<AskKey> for String {
    fn from(key: AskKey) -> String {
        key.0
    }
}

/// Refuse a workflow name that would make [`AskKey::input_key`] ambiguous or unloggable.
pub fn validate_workflow_name(workflow: &str) -> Result<(), AskKeyError> {
    if workflow.is_empty() {
        return Err(AskKeyError::Empty);
    }
    if workflow.len() > MAX_KEY_LEN {
        return Err(AskKeyError::TooLong {
            len: workflow.len(),
        });
    }
    if workflow.chars().any(char::is_control) {
        return Err(AskKeyError::Control {
            value: workflow.to_owned(),
        });
    }
    if workflow.chars().any(char::is_whitespace) {
        return Err(AskKeyError::Whitespace {
            value: workflow.to_owned(),
        });
    }
    if workflow.contains(':') {
        return Err(AskKeyError::Separator {
            value: workflow.to_owned(),
        });
    }
    Ok(())
}

/// One proposed unit of work.
///
/// `params` travels with the emission and is stored by the orchestrator when it adopts the ask.
/// It is not carried on the queue afterwards: a queued key is re-read from durable state, so a
/// duplicate or stale enqueue is harmless. Material too large to inline belongs at a published
/// artifact location, named in `params`, never inlined here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ask {
    pub key: AskKey,
    /// The workflow this ask names. Its parameter schema is what `params` must satisfy.
    pub workflow: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

impl Ask {
    pub fn new(
        key: AskKey,
        workflow: impl Into<String>,
        params: serde_json::Value,
    ) -> Result<Self, AskKeyError> {
        let workflow = workflow.into();
        validate_workflow_name(&workflow)?;
        Ok(Ask {
            key,
            workflow,
            params,
        })
    }

    pub fn input_key(&self) -> String {
        self.key.input_key(&self.workflow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_refused_when_it_cannot_survive_the_round_trip() {
        assert_eq!(AskKey::new(""), Err(AskKeyError::Empty));
        assert_eq!(
            AskKey::new("x".repeat(MAX_KEY_LEN + 1)),
            Err(AskKeyError::TooLong {
                len: MAX_KEY_LEN + 1
            })
        );
        assert!(matches!(
            AskKey::new("a\nb"),
            Err(AskKeyError::Control { .. })
        ));
        assert!(matches!(
            AskKey::new("a b"),
            Err(AskKeyError::Whitespace { .. })
        ));
        assert!(AskKey::new("x".repeat(MAX_KEY_LEN)).is_ok());
        // A colon is fine in an item key: the orchestrator's key splits from the left.
        assert!(AskKey::new("arxiv.org/abs/2401.12345").is_ok());
        assert!(AskKey::new("jira:INFERENG-42").is_ok());
    }

    /// The workflow is part of the queued identity, not a property of it: two workflows asking
    /// about one item are two pieces of work and must not coalesce.
    #[test]
    fn the_queued_key_separates_two_workflows_asking_about_one_item() {
        let key = AskKey::new("arxiv.org/abs/2401.12345").expect("valid");
        assert_eq!(
            key.input_key("implement-paper"),
            "ask:implement-paper:arxiv.org/abs/2401.12345"
        );
        assert_ne!(key.input_key("implement-paper"), key.input_key("summarize"));
        assert!(key.input_key("summarize").starts_with("ask:"));
    }

    #[test]
    fn a_workflow_name_may_not_make_the_queued_key_ambiguous() {
        assert!(matches!(
            validate_workflow_name("impl:paper"),
            Err(AskKeyError::Separator { .. })
        ));
        assert_eq!(validate_workflow_name(""), Err(AskKeyError::Empty));
        assert!(matches!(
            validate_workflow_name("two words"),
            Err(AskKeyError::Whitespace { .. })
        ));
        assert!(validate_workflow_name("implement-paper").is_ok());
    }

    /// The wire is the boundary, so a key that would be refused by the constructor must also be
    /// refused on the way in. Without `try_from`, a hand-written line could smuggle one past.
    #[test]
    fn a_key_is_validated_on_deserialization_not_only_on_construction() {
        let good: AskKey = serde_json::from_str(r#""arxiv.org/abs/2401.12345""#).expect("valid");
        assert_eq!(good.as_str(), "arxiv.org/abs/2401.12345");
        assert!(serde_json::from_str::<AskKey>(r#""with space""#).is_err());
        assert!(serde_json::from_str::<AskKey>(r#""""#).is_err());
    }

    #[test]
    fn an_ask_round_trips_and_validates_its_workflow() {
        let ask = Ask::new(
            AskKey::new("arxiv.org/abs/2401.12345").expect("valid"),
            "implement-paper",
            serde_json::json!({"paper_url": "https://arxiv.org/abs/2401.12345"}),
        )
        .expect("valid");
        let line = serde_json::to_string(&ask).expect("encode");
        let back: Ask = serde_json::from_str(&line).expect("decode");
        assert_eq!(back, ask);
        assert_eq!(
            back.input_key(),
            "ask:implement-paper:arxiv.org/abs/2401.12345"
        );

        assert!(
            Ask::new(
                AskKey::new("item").expect("valid"),
                "bad:name",
                serde_json::Value::Null,
            )
            .is_err()
        );
    }
}
