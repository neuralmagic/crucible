//! What a call task binds each of its target's parameters to (RFC-0008 C-CALL).

use serde::{Deserialize, Serialize};

use crate::plan::ir::{OutputField, TaskName};

/// Where one target parameter's value comes from. Every value is bound when the call becomes
/// ready, never at compile time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "snake_case", deny_unknown_fields)]
pub enum Binding {
    /// A literal of the parameter's type.
    Literal { value: serde_json::Value },
    /// A parameter of the calling workflow.
    Param { name: String },
    /// A field a direct dependency declares in `emits`.
    Field {
        producer: TaskName,
        field: OutputField,
    },
    /// A path a direct dependency declares in `emits_files`.
    File { producer: TaskName, path: String },
    /// A mapped call's item, or one field of it.
    Item {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        field: Option<OutputField>,
    },
}

impl Binding {
    /// The dependency the value is read from, when it is read from one.
    pub fn producer(&self) -> Option<&TaskName> {
        match self {
            Binding::Field { producer, .. } | Binding::File { producer, .. } => Some(producer),
            Binding::Literal { .. } | Binding::Param { .. } | Binding::Item { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::plan::call::Binding;
    use crate::plan::ir::{OutputField, TaskName};
    use serde_json::json;

    #[test]
    fn bindings_round_trip_by_source() {
        for (binding, wire) in [
            (
                Binding::Literal {
                    value: json!({"a": [1]}),
                },
                json!({"from": "literal", "value": {"a": [1]}}),
            ),
            (
                Binding::Param {
                    name: "repo".into(),
                },
                json!({"from": "param", "name": "repo"}),
            ),
            (
                Binding::Field {
                    producer: TaskName("plan".into()),
                    field: OutputField("version".into()),
                },
                json!({"from": "field", "producer": "plan", "field": "version"}),
            ),
            (
                Binding::File {
                    producer: TaskName("plan".into()),
                    path: "notes.md".into(),
                },
                json!({"from": "file", "producer": "plan", "path": "notes.md"}),
            ),
            (Binding::Item { field: None }, json!({"from": "item"})),
        ] {
            assert_eq!(serde_json::to_value(&binding).unwrap(), wire);
            assert_eq!(
                serde_json::from_value::<Binding>(wire).unwrap(),
                binding.clone()
            );
        }
        assert_eq!(
            Binding::File {
                producer: TaskName("p".into()),
                path: "x".into()
            }
            .producer(),
            Some(&TaskName("p".into()))
        );
        assert!(serde_json::from_value::<Binding>(json!({"from": "env"})).is_err());
    }
}
