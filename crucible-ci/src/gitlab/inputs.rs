//! A CI configuration's `spec:inputs`, the parameters a pipeline created through the API takes
//!.

use serde::Deserialize;
use serde_json::Value;

/// One declared input.
#[derive(Clone, Debug, PartialEq)]
pub struct Input {
    pub name: String,
    pub kind: InputKind,
    pub default: Option<Value>,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum InputKind {
    String {
        options: Vec<String>,
        regex: Option<String>,
    },
    Number,
    Boolean,
    StringList,
    Json,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InputsError {
    #[error("the CI configuration is not YAML: {0}")]
    Yaml(String),
    #[error("input {name} has type {kind}, which a call cannot bind")]
    Type { name: String, kind: String },
}

#[derive(Deserialize)]
struct Header {
    #[serde(default)]
    spec: Option<Spec>,
}

#[derive(Deserialize)]
struct Spec {
    #[serde(default)]
    inputs: serde_json::Map<String, Value>,
}

pub fn inputs(config: &str) -> Result<Vec<Input>, InputsError> {
    let mut docs = serde_norway::Deserializer::from_str(config);
    let first = match docs.next() {
        Some(doc) => doc,
        None => return Ok(Vec::new()),
    };
    let more = docs.next().is_some();
    if !more {
        return Ok(Vec::new());
    }
    let header = Header::deserialize(first).map_err(|e| InputsError::Yaml(e.to_string()))?;
    let Some(spec) = header.spec else {
        return Ok(Vec::new());
    };
    spec.inputs
        .into_iter()
        .map(|(name, decl)| {
            let get = |k: &str| decl.get(k);
            let default = get("default").cloned();
            let kind = match get("type").and_then(Value::as_str).unwrap_or("string") {
                "string" => InputKind::String {
                    options: get("options")
                        .and_then(Value::as_array)
                        .map(|o| {
                            o.iter()
                                .map(|v| match v {
                                    Value::String(s) => s.clone(),
                                    other => other.to_string(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    regex: get("regex").and_then(Value::as_str).map(str::to_owned),
                },
                "number" => InputKind::Number,
                "boolean" => InputKind::Boolean,
                "array" => match &default {
                    Some(Value::Array(items)) if items.iter().all(Value::is_string) => {
                        InputKind::StringList
                    }
                    _ => InputKind::Json,
                },
                other => {
                    return Err(InputsError::Type {
                        name,
                        kind: other.to_owned(),
                    });
                }
            };
            Ok(Input {
                description: get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                name,
                kind,
                default,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::gitlab::inputs::{Input, InputKind, InputsError, inputs};

    const CONFIG: &str = r#"spec:
  inputs:
    target:
      description: Where to deploy
      options: [staging, production]
      default: staging
    image:
      regex: ^quay\.io/.+
    replicas:
      type: number
      default: 2
    dry_run:
      type: boolean
      default: true
    tags:
      type: array
      default: [a, b]
    matrix:
      type: array
      default: [{arch: amd64}]
---
build:
  stage: build
  script: echo $[[ inputs.target ]]
"#;

    #[test]
    fn the_header_declares_typed_inputs() {
        let found = inputs(CONFIG).expect("parses");
        let by = |n: &str| found.iter().find(|i| i.name == n).cloned();
        assert_eq!(
            by("target"),
            Some(Input {
                name: "target".into(),
                kind: InputKind::String {
                    options: vec!["staging".into(), "production".into()],
                    regex: None
                },
                default: Some(json!("staging")),
                description: Some("Where to deploy".into()),
            })
        );
        assert!(matches!(
            by("image").map(|i| i.kind),
            Some(InputKind::String { regex: Some(_), .. })
        ));
        assert_eq!(by("replicas").map(|i| i.kind), Some(InputKind::Number));
        assert_eq!(by("dry_run").map(|i| i.kind), Some(InputKind::Boolean));
        assert_eq!(by("tags").map(|i| i.kind), Some(InputKind::StringList));
        assert_eq!(by("matrix").map(|i| i.kind), Some(InputKind::Json));
        assert_eq!(found.len(), 6);
    }

    #[test]
    fn a_configuration_without_a_header_has_no_inputs() {
        assert_eq!(inputs("build:\n  script: make\n"), Ok(vec![]));
        assert_eq!(inputs(""), Ok(vec![]));
        assert_eq!(
            inputs("spec:\n  inputs: {}\n---\nbuild: {script: x}\n"),
            Ok(vec![])
        );
    }

    #[test]
    fn an_unknown_input_type_is_refused() {
        let config = "spec:\n  inputs:\n    x: {type: blob}\n---\nj: {script: x}\n";
        assert_eq!(
            inputs(config),
            Err(InputsError::Type {
                name: "x".into(),
                kind: "blob".into()
            })
        );
    }
}
