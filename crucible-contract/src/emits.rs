//! The types a task may declare for the fields of its JSON output.
//!
//! A typed `emits` promises each field's JSON type as well as its presence. The engine checks a
//! passing attempt against it and the compiler checks the graph's consumers against it; the
//! admitted-plan event carries it so a reader sees the same contract.

use std::collections::BTreeSet;

use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::ser::{SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::decision::Label;

/// The JSON type one declared output field holds.
///
/// Written as its token (`"string"`, `"integer"`, `"number"`, `"boolean"`, `"list"`, `"object"`)
/// or as a list of labels, which declares a string that is one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldType {
    String,
    /// A number with no fractional part.
    Integer,
    /// Any JSON number, integers included.
    Number,
    Boolean,
    List,
    Object,
    /// A string equal to one of these labels.
    OneOf(Vec<Label>),
}

/// Why a declared field type is not one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldTypeError {
    NoLabels,
    DuplicateLabel { label: Label },
}

impl std::fmt::Display for FieldTypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FieldTypeError::NoLabels => f.write_str("a label list names no labels"),
            FieldTypeError::DuplicateLabel { label } => {
                write!(f, "label {label:?} is listed twice")
            }
        }
    }
}

impl std::error::Error for FieldTypeError {}

impl FieldType {
    /// Every scalar type, by its token.
    pub const SCALARS: [(&'static str, FieldType); 6] = [
        ("string", FieldType::String),
        ("integer", FieldType::Integer),
        ("number", FieldType::Number),
        ("boolean", FieldType::Boolean),
        ("list", FieldType::List),
        ("object", FieldType::Object),
    ];

    /// The scalar type a token names.
    pub fn scalar(token: &str) -> Option<FieldType> {
        Self::SCALARS
            .iter()
            .find(|(name, _)| *name == token)
            .map(|(_, ty)| ty.clone())
    }

    /// The token of a scalar type; `None` for a label list.
    pub fn token(&self) -> Option<&'static str> {
        Self::SCALARS
            .iter()
            .find(|(_, ty)| ty == self)
            .map(|(name, _)| *name)
    }

    /// True for a type every value of which is a JSON number.
    pub fn is_numeric(&self) -> bool {
        matches!(self, FieldType::Integer | FieldType::Number)
    }

    pub fn validate(&self) -> Result<(), FieldTypeError> {
        let FieldType::OneOf(labels) = self else {
            return Ok(());
        };
        if labels.is_empty() {
            return Err(FieldTypeError::NoLabels);
        }
        let mut seen = BTreeSet::new();
        for label in labels {
            if !seen.insert(label) {
                return Err(FieldTypeError::DuplicateLabel {
                    label: label.clone(),
                });
            }
        }
        Ok(())
    }

    /// Whether `value` is of this type.
    pub fn admits(&self, value: &Value) -> bool {
        match (self, value) {
            (FieldType::String, Value::String(_))
            | (FieldType::Number, Value::Number(_))
            | (FieldType::Boolean, Value::Bool(_))
            | (FieldType::List, Value::Array(_))
            | (FieldType::Object, Value::Object(_)) => true,
            (FieldType::Integer, Value::Number(n)) => {
                n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
            }
            (FieldType::OneOf(labels), Value::String(s)) => labels.iter().any(|l| l.as_str() == s),
            _ => false,
        }
    }
}

/// The JSON type of a value, in the tokens [`FieldType`] uses, for saying what arrived instead.
pub fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "object",
    }
}

impl std::fmt::Display for FieldType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FieldType::OneOf(labels) => {
                let labels: Vec<&str> = labels.iter().map(Label::as_str).collect();
                write!(f, "one of {}", labels.join("|"))
            }
            scalar => f.write_str(scalar.token().unwrap_or_default()),
        }
    }
}

impl Serialize for FieldType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            FieldType::OneOf(labels) => {
                let mut seq = serializer.serialize_seq(Some(labels.len()))?;
                for label in labels {
                    seq.serialize_element(label)?;
                }
                seq.end()
            }
            scalar => serializer.serialize_str(scalar.token().unwrap_or_default()),
        }
    }
}

impl<'de> Deserialize<'de> for FieldType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldTypeVisitor;

        impl<'de> Visitor<'de> for FieldTypeVisitor {
            type Value = FieldType;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(
                    "a field type: \"string\", \"integer\", \"number\", \"boolean\", \"list\", \
                     \"object\", or a list of labels",
                )
            }

            fn visit_str<E: de::Error>(self, token: &str) -> Result<FieldType, E> {
                FieldType::scalar(token)
                    .ok_or_else(|| E::invalid_value(de::Unexpected::Str(token), &self))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<FieldType, A::Error> {
                let mut labels = Vec::new();
                while let Some(label) = seq.next_element::<Label>()? {
                    labels.push(label);
                }
                let ty = FieldType::OneOf(labels);
                ty.validate().map_err(de::Error::custom)?;
                Ok(ty)
            }
        }

        deserializer.deserialize_any(FieldTypeVisitor)
    }
}

/// One declared output field on the wire: its name, and its type when the declaration gave one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmitWire {
    pub field: String,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub ty: Option<FieldType>,
}

#[cfg(test)]
mod tests {
    use crate::decision::Label;
    use crate::emits::{EmitWire, FieldType, FieldTypeError, value_type};
    use serde_json::json;

    fn labels(names: &[&str]) -> FieldType {
        FieldType::OneOf(names.iter().map(|n| Label::new(*n).unwrap()).collect())
    }

    #[test]
    fn each_type_admits_exactly_its_json_values() {
        let values = [
            json!(null),
            json!(true),
            json!(3),
            json!(-3),
            json!(3.0),
            json!(3.5),
            json!("high"),
            json!("other"),
            json!([1]),
            json!({"a": 1}),
        ];
        let cases: [(FieldType, [bool; 10]); 7] = [
            (
                FieldType::String,
                [
                    false, false, false, false, false, false, true, true, false, false,
                ],
            ),
            (
                FieldType::Integer,
                [
                    false, false, true, true, true, false, false, false, false, false,
                ],
            ),
            (
                FieldType::Number,
                [
                    false, false, true, true, true, true, false, false, false, false,
                ],
            ),
            (
                FieldType::Boolean,
                [
                    false, true, false, false, false, false, false, false, false, false,
                ],
            ),
            (
                FieldType::List,
                [
                    false, false, false, false, false, false, false, false, true, false,
                ],
            ),
            (
                FieldType::Object,
                [
                    false, false, false, false, false, false, false, false, false, true,
                ],
            ),
            (
                labels(&["high", "low"]),
                [
                    false, false, false, false, false, false, true, false, false, false,
                ],
            ),
        ];
        for (ty, expected) in cases {
            for (value, admitted) in values.iter().zip(expected) {
                assert_eq!(ty.admits(value), admitted, "{ty} vs {value}");
            }
        }
    }

    #[test]
    fn value_type_names_what_arrived() {
        assert_eq!(value_type(&json!(null)), "null");
        assert_eq!(value_type(&json!(false)), "boolean");
        assert_eq!(value_type(&json!(7)), "integer");
        assert_eq!(value_type(&json!(7.5)), "number");
        assert_eq!(value_type(&json!("x")), "string");
        assert_eq!(value_type(&json!([])), "list");
        assert_eq!(value_type(&json!({})), "object");
    }

    #[test]
    fn only_integer_and_number_are_numeric() {
        for (token, ty) in FieldType::SCALARS {
            assert_eq!(
                ty.is_numeric(),
                matches!(token, "integer" | "number"),
                "{token}"
            );
        }
        assert!(!labels(&["a"]).is_numeric());
    }

    #[test]
    fn a_label_list_must_name_distinct_labels() {
        assert_eq!(labels(&[]).validate(), Err(FieldTypeError::NoLabels));
        assert_eq!(
            labels(&["a", "b", "a"]).validate(),
            Err(FieldTypeError::DuplicateLabel {
                label: Label::new("a").unwrap()
            })
        );
        assert_eq!(labels(&["a"]).validate(), Ok(()));
        assert_eq!(FieldType::String.validate(), Ok(()));
    }

    #[test]
    fn types_round_trip_as_tokens_and_label_lists() {
        for (token, ty) in FieldType::SCALARS {
            let text = serde_json::to_string(&ty).unwrap();
            assert_eq!(text, format!("{token:?}"));
            assert_eq!(serde_json::from_str::<FieldType>(&text).unwrap(), ty);
            assert_eq!(ty.to_string(), token);
        }
        let ty = labels(&["high", "low"]);
        assert_eq!(serde_json::to_string(&ty).unwrap(), r#"["high","low"]"#);
        assert_eq!(
            serde_json::from_str::<FieldType>(r#"["high","low"]"#).unwrap(),
            ty
        );
        assert_eq!(ty.to_string(), "one of high|low");
    }

    #[test]
    fn a_malformed_type_does_not_decode() {
        for (text, needle) in [
            (r#""strng""#, "a field type"),
            (r#"[]"#, "names no labels"),
            (r#"["a", "a"]"#, "listed twice"),
            (r#"["not a label"]"#, "not an identifier"),
            (r#"3"#, "a field type"),
        ] {
            let err = serde_json::from_str::<FieldType>(text)
                .unwrap_err()
                .to_string();
            assert!(err.contains(needle), "{text}: {err}");
        }
    }

    #[test]
    fn an_untyped_wire_field_omits_its_type() {
        let untyped = EmitWire {
            field: "lines".into(),
            ty: None,
        };
        assert_eq!(
            serde_json::to_string(&untyped).unwrap(),
            r#"{"field":"lines"}"#
        );
        let typed = EmitWire {
            field: "severity".into(),
            ty: Some(labels(&["high", "low"])),
        };
        let text = serde_json::to_string(&typed).unwrap();
        assert_eq!(text, r#"{"field":"severity","type":["high","low"]}"#);
        assert_eq!(serde_json::from_str::<EmitWire>(&text).unwrap(), typed);
    }
}
