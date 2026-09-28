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
use serde_json::{Map, Value};

use crate::admission::MAX_KEY_LEN;

/// The most bytes an ask's parameter values may encode to. An ask describes work; material a
/// receiving run needs travels by published artifact location, and a value this large is file
/// content by another name.
pub const MAX_ASK_PARAMS_BYTES: usize = 4096;

/// Why a key was refused. Carries the offending value so a diagnostic can quote it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskKeyError {
    Empty,
    TooLong { len: usize },
    Control { value: String },
    Whitespace { value: String },
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
        }
    }
}

impl std::error::Error for AskKeyError {}

/// Why a workflow name was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowNameError {
    Empty,
    TooLong {
        len: usize,
    },
    /// Outside ASCII letters, digits, `.`, `_`, and `-`. A `:` in particular would make
    /// [`AskKey::input_key`] ambiguous: the orchestrator's key puts the workflow before the item
    /// key with `:` between, and parsing splits from the left.
    Character {
        value: String,
        found: char,
    },
}

impl std::fmt::Display for WorkflowNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkflowNameError::Empty => f.write_str("a workflow name is empty"),
            WorkflowNameError::TooLong { len } => {
                write!(
                    f,
                    "a workflow name is {len} bytes; maximum is {MAX_KEY_LEN}"
                )
            }
            WorkflowNameError::Character { value, found } => write!(
                f,
                "workflow name {value:?} contains {found:?}; use ASCII letters, digits, `.`, `_`, \
                 or `-`"
            ),
        }
    }
}

impl std::error::Error for WorkflowNameError {}

/// Why an ask's parameter values were refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskParamsError {
    /// A value no declared parameter type can hold: null, an object, or a list holding anything
    /// other than strings.
    Value {
        name: String,
        found: &'static str,
    },
    TooLarge {
        bytes: usize,
    },
}

impl std::fmt::Display for AskParamsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AskParamsError::Value { name, found } => write!(
                f,
                "ask parameter {name:?} is {found}; a parameter is a string, a number, a boolean, \
                 or a list of strings"
            ),
            AskParamsError::TooLarge { bytes } => write!(
                f,
                "ask parameters encode to {bytes} bytes; maximum is {MAX_ASK_PARAMS_BYTES}. Pass \
                 large material by published artifact location"
            ),
        }
    }
}

impl std::error::Error for AskParamsError {}

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
    pub fn input_key(&self, workflow: &WorkflowName) -> String {
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

/// The name an orchestrator registered a workflow under, as an ask names it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WorkflowName(String);

impl WorkflowName {
    pub fn new(name: impl Into<String>) -> Result<Self, WorkflowNameError> {
        let name = name.into();
        if name.is_empty() {
            return Err(WorkflowNameError::Empty);
        }
        if name.len() > MAX_KEY_LEN {
            return Err(WorkflowNameError::TooLong { len: name.len() });
        }
        if let Some(found) = name
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
        {
            return Err(WorkflowNameError::Character { value: name, found });
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WorkflowName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for WorkflowName {
    type Error = WorkflowNameError;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        WorkflowName::new(name)
    }
}

impl From<WorkflowName> for String {
    fn from(name: WorkflowName) -> String {
        name.0
    }
}

/// One proposed unit of work.
///
/// `params` travels with the emission and is stored by the orchestrator when it adopts the ask.
/// It is not carried on the queue afterwards: a queued key is re-read from durable state, so a
/// duplicate or stale enqueue is harmless. Every constructor, deserialization included, checks the
/// values' shape and encoded size; whether they satisfy the named workflow's schema is the
/// receiving orchestrator's check, since only it holds that schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "AskWire")]
pub struct Ask {
    key: AskKey,
    workflow: WorkflowName,
    params: Map<String, Value>,
}

/// The decoded shape before its values are checked. Unknown fields are refused, so an ask cannot
/// carry a body or a file beside its parameters.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AskWire {
    key: AskKey,
    workflow: WorkflowName,
    #[serde(default)]
    params: Map<String, Value>,
}

impl TryFrom<AskWire> for Ask {
    type Error = AskParamsError;

    fn try_from(wire: AskWire) -> Result<Self, Self::Error> {
        Ask::new(wire.key, wire.workflow, wire.params)
    }
}

impl Ask {
    pub fn new(
        key: AskKey,
        workflow: WorkflowName,
        params: Map<String, Value>,
    ) -> Result<Self, AskParamsError> {
        for (name, value) in &params {
            let found = match value {
                Value::String(_) | Value::Number(_) | Value::Bool(_) => continue,
                Value::Array(items) if items.iter().all(Value::is_string) => continue,
                Value::Array(_) => "a list holding something other than strings",
                Value::Null => "null",
                Value::Object(_) => "an object",
            };
            return Err(AskParamsError::Value {
                name: name.clone(),
                found,
            });
        }
        let bytes = serde_json::to_vec(&params).map_or(usize::MAX, |encoded| encoded.len());
        if bytes > MAX_ASK_PARAMS_BYTES {
            return Err(AskParamsError::TooLarge { bytes });
        }
        Ok(Ask {
            key,
            workflow,
            params,
        })
    }

    pub fn key(&self) -> &AskKey {
        &self.key
    }

    pub fn workflow(&self) -> &WorkflowName {
        &self.workflow
    }

    pub fn params(&self) -> &Map<String, Value> {
        &self.params
    }

    pub fn input_key(&self) -> String {
        self.key.input_key(&self.workflow)
    }
}

#[cfg(test)]
mod tests {
    use crate::admission::MAX_KEY_LEN;
    use crate::ask::{
        Ask, AskKey, AskKeyError, AskParamsError, MAX_ASK_PARAMS_BYTES, WorkflowName,
        WorkflowNameError,
    };
    use serde_json::{Map, Value, json};

    fn params(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    fn workflow(name: &str) -> WorkflowName {
        WorkflowName::new(name).expect("valid workflow name")
    }

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
            key.input_key(&workflow("implement-paper")),
            "ask:implement-paper:arxiv.org/abs/2401.12345"
        );
        assert_ne!(
            key.input_key(&workflow("implement-paper")),
            key.input_key(&workflow("summarize"))
        );
        assert!(key.input_key(&workflow("summarize")).starts_with("ask:"));
    }

    #[test]
    fn a_workflow_name_may_not_make_the_queued_key_ambiguous() {
        assert_eq!(
            WorkflowName::new("impl:paper"),
            Err(WorkflowNameError::Character {
                value: "impl:paper".into(),
                found: ':'
            })
        );
        assert_eq!(WorkflowName::new(""), Err(WorkflowNameError::Empty));
        assert!(matches!(
            WorkflowName::new("two words"),
            Err(WorkflowNameError::Character { found: ' ', .. })
        ));
        assert!(matches!(
            WorkflowName::new("x".repeat(MAX_KEY_LEN + 1)),
            Err(WorkflowNameError::TooLong { .. })
        ));
        assert!(WorkflowName::new("implement-paper").is_ok());
        assert!(WorkflowName::new("fix_issue.v2").is_ok());
        assert!(serde_json::from_str::<WorkflowName>(r#""a/b""#).is_err());
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
    fn an_ask_round_trips_and_names_its_queued_key() {
        let ask = Ask::new(
            AskKey::new("arxiv.org/abs/2401.12345").expect("valid"),
            workflow("implement-paper"),
            params(json!({
                "paper_url": "https://arxiv.org/abs/2401.12345",
                "rounds": 3,
                "dry_run": false,
                "labels": ["a", "b"],
            })),
        )
        .expect("valid");
        let line = serde_json::to_string(&ask).expect("encode");
        let back: Ask = serde_json::from_str(&line).expect("decode");
        assert_eq!(back, ask);
        assert_eq!(
            back.input_key(),
            "ask:implement-paper:arxiv.org/abs/2401.12345"
        );
        assert_eq!(back.params()["rounds"], json!(3));
    }

    /// A parameter value is one a declared parameter type can hold. Anything else is a payload,
    /// and a payload is what the no-file-content rule exists to keep off the wire.
    #[test]
    fn a_parameter_value_outside_the_parameter_types_is_refused() {
        for (value, found) in [
            (json!(null), "null"),
            (json!({"nested": "body"}), "an object"),
            (
                json!(["a", 1]),
                "a list holding something other than strings",
            ),
        ] {
            assert_eq!(
                Ask::new(
                    AskKey::new("item").expect("valid"),
                    workflow("w"),
                    params(json!({"p": value})),
                ),
                Err(AskParamsError::Value {
                    name: "p".into(),
                    found
                })
            );
        }
    }

    #[test]
    fn parameters_past_the_size_bound_are_refused_and_at_it_are_accepted() {
        let make = |len: usize| {
            Ask::new(
                AskKey::new("item").expect("valid"),
                workflow("w"),
                params(json!({"p": "x".repeat(len)})),
            )
        };
        // `{"p":""}` is 8 bytes of framing around the value.
        let at_bound = MAX_ASK_PARAMS_BYTES - 8;
        assert!(make(at_bound).is_ok());
        assert_eq!(
            make(at_bound + 1),
            Err(AskParamsError::TooLarge {
                bytes: MAX_ASK_PARAMS_BYTES + 1
            })
        );
    }

    /// Decoding goes through the same checks as construction, and a field beside the three an ask
    /// has is refused rather than dropped: an ask cannot smuggle a body past the parameter rules.
    #[test]
    fn decoding_an_ask_applies_every_constructor_check() {
        let decode = |line: &str| serde_json::from_str::<Ask>(line);
        assert!(decode(r#"{"key":"k","workflow":"w"}"#).is_ok());
        assert!(decode(r#"{"key":"k","workflow":"w","params":{"a":"b"}}"#).is_ok());
        for bad in [
            r#"{"key":"k","workflow":"w","params":{"a":"b"},"body":"file contents"}"#,
            r#"{"key":"k","workflow":"w","params":{"a":null}}"#,
            r#"{"key":"k","workflow":"w","params":null}"#,
            r#"{"key":"k","workflow":"w","params":["a"]}"#,
            r#"{"key":"k k","workflow":"w"}"#,
            r#"{"key":"k","workflow":"a:b"}"#,
            r#"{"workflow":"w"}"#,
        ] {
            assert!(decode(bad).is_err(), "{bad} decoded");
        }
    }
}
