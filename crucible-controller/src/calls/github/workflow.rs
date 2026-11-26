//! A workflow file as a call target reads it: its dispatch inputs as params, its jobs as a graph.

use std::collections::BTreeMap;

use crucible_contract::calls::{
    FileRef, GithubOutputs, MAX_GITHUB_INPUTS, NodeStatus, ProgressEdge, ProgressGraph,
    ProgressNode, TargetInterface,
};
use crucible_contract::emits::{DeclaredFile, EmitWire, value_type};
use forge::github::workflow::{InputType, Workflow};
use serde_json::{Map, Value, json};

use crate::calls::github::TargetError;

fn schema_type(ty: InputType) -> &'static str {
    match ty {
        InputType::String | InputType::Choice | InputType::Environment => "string",
        InputType::Boolean => "boolean",
        InputType::Number => "number",
    }
}

/// The parameters as the JSON Schema a params block emits.
pub fn params_schema(workflow: &Workflow) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for input in &workflow.inputs {
        let mut property = Map::new();
        property.insert("type".into(), json!(schema_type(input.ty())));
        if let Some(doc) = &input.description {
            property.insert("description".into(), json!(doc));
        }
        if let Some(default) = &input.default {
            property.insert("default".into(), default.clone());
        }
        if input.ty() == InputType::Choice {
            property.insert("enum".into(), json!(input.options));
        }
        if input.required && input.default.is_none() {
            required.push(input.name.clone());
        }
        properties.insert(input.name.clone(), Value::Object(property));
    }
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": required,
        "x-order": workflow.inputs.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
    })
}

/// The jobs as a graph, every node pending, with edges from `needs`.
pub fn graph(workflow: &Workflow) -> ProgressGraph {
    let nodes = workflow
        .jobs
        .iter()
        .map(|job| ProgressNode {
            id: job.key.clone(),
            label: job.label.clone(),
            status: NodeStatus::Pending,
            group: None,
            started: None,
            ended: None,
            detail: None,
            link: None,
        })
        .collect();
    let edges = workflow
        .jobs
        .iter()
        .flat_map(|job| {
            job.needs.iter().map(|need| ProgressEdge {
                from: need.clone(),
                to: job.key.clone(),
            })
        })
        .collect();
    ProgressGraph::bounded(nodes, edges)
}

/// The target's interface: the workflow's parameters and graph, and the result `outputs` declares.
pub fn interface(workflow: &Workflow, outputs: &GithubOutputs) -> TargetInterface {
    TargetInterface {
        params: params_schema(workflow),
        result: outputs
            .fields
            .iter()
            .map(|(field, ty)| EmitWire {
                field: field.clone(),
                ty: Some(ty.clone()),
            })
            .collect(),
        result_files: outputs
            .files
            .iter()
            .map(|path| DeclaredFile::from(path.as_str()))
            .collect(),
        graph: graph(workflow),
    }
}

/// The `inputs` of a dispatch: each bound value as the string GitHub takes, a json value as its
/// serialized JSON. Refuses file parameters, undeclared inputs, missing required inputs, and
/// values of the wrong type.
pub fn dispatch_inputs(
    params: &Value,
    values: &BTreeMap<String, Value>,
    files: &BTreeMap<String, FileRef>,
) -> Result<BTreeMap<String, String>, TargetError> {
    if !files.is_empty() {
        let names: Vec<&str> = files.keys().map(String::as_str).collect();
        return Err(TargetError::Refused(format!(
            "a GitHub workflow takes no file parameter, but the call binds {}",
            names.join(", ")
        )));
    }
    let empty = Map::new();
    let properties = params
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let mut inputs = BTreeMap::new();
    for (name, value) in values {
        let Some(property) = properties.get(name) else {
            return Err(TargetError::Refused(format!(
                "the workflow declares no input {name:?}"
            )));
        };
        let ty = property
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("string");
        let text = match (ty, value) {
            (_, Value::Null) => continue,
            ("boolean", Value::Bool(b)) => b.to_string(),
            ("number", Value::Number(n)) => n.to_string(),
            ("string", Value::String(s)) => s.clone(),
            ("string", json @ (Value::Object(_) | Value::Array(_))) => json.to_string(),
            (ty, other) => {
                return Err(TargetError::Refused(format!(
                    "input {name:?} takes a {ty}, not {}",
                    value_type(other)
                )));
            }
        };
        if let Some(options) = property.get("enum").and_then(Value::as_array)
            && !options.iter().any(|o| o.as_str() == Some(text.as_str()))
        {
            return Err(TargetError::Refused(format!(
                "input {name:?} is not one of its choices"
            )));
        }
        inputs.insert(name.clone(), text);
    }
    let required = params
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    for name in required {
        if !inputs.contains_key(name) {
            return Err(TargetError::Refused(format!(
                "required input {name:?} is not bound"
            )));
        }
    }
    if inputs.len() > MAX_GITHUB_INPUTS {
        return Err(TargetError::Refused(format!(
            "the call binds {} inputs; GitHub accepts at most {MAX_GITHUB_INPUTS}",
            inputs.len()
        )));
    }
    Ok(inputs)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crucible_contract::calls::{FileRef, GithubOutputs, NodeStatus, ProgressEdge};
    use crucible_contract::emits::FieldType;
    use serde_json::{Value, json};

    use forge::github::workflow::Workflow;

    use crate::calls::github::TargetError;
    use crate::calls::github::workflow::{dispatch_inputs, graph, interface, params_schema};

    const DEMO: &str = r#"
name: demo
on:
  push:
  workflow_dispatch:
    inputs:
      message:
        description: Text the report quotes back
        type: string
        required: true
      flavor:
        type: choice
        options: [fast, thorough]
        default: fast
        required: true
      loud:
        type: boolean
        default: false
      count:
        type: number
        default: 3
      target:
        type: environment
      untyped:
        required: "true"
jobs:
  prepare:
    runs-on: ubuntu-latest
    steps: [{run: "true"}]
  build:
    name: Build it
    needs: prepare
    strategy:
      matrix:
        leg: [left, right]
    runs-on: ubuntu-latest
    steps: [{run: "true"}]
  report:
    name: report ${{ inputs.flavor }}
    needs: [prepare, build]
    runs-on: ubuntu-latest
    steps: [{run: "true"}]
"#;

    fn outputs() -> GithubOutputs {
        GithubOutputs {
            artifact: "crucible-result".to_owned(),
            fields: BTreeMap::from([
                ("summary".to_owned(), FieldType::String),
                ("legs".to_owned(), FieldType::Integer),
                ("loud".to_owned(), FieldType::Boolean),
            ]),
            files: vec!["notes.md".to_owned()],
        }
    }

    #[test]
    fn inputs_become_a_params_schema() {
        let workflow = Workflow::parse(DEMO).unwrap();
        assert_eq!(
            params_schema(&workflow),
            json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "message": {"type": "string", "description": "Text the report quotes back"},
                    "flavor": {"type": "string", "default": "fast", "enum": ["fast", "thorough"]},
                    "loud": {"type": "boolean", "default": false},
                    "count": {"type": "number", "default": 3},
                    "target": {"type": "string"},
                    "untyped": {"type": "string"},
                },
                "required": ["message", "untyped"],
                "x-order": ["message", "flavor", "loud", "count", "target", "untyped"],
            })
        );
    }

    #[test]
    fn the_engine_reads_the_schema_types_back() {
        use crucible::plan::starlark::params::ParamType;
        let schema = params_schema(&Workflow::parse(DEMO).unwrap());
        let types: Vec<ParamType> = ["message", "flavor", "loud", "count", "target"]
            .iter()
            .map(|name| ParamType::from_schema(&schema["properties"][name]))
            .collect();
        assert_eq!(
            types,
            [
                ParamType::String,
                ParamType::String,
                ParamType::Bool,
                ParamType::Number,
                ParamType::String
            ]
        );
    }

    #[test]
    fn jobs_become_a_pending_graph_with_needs_edges() {
        let graph = graph(&Workflow::parse(DEMO).unwrap());
        let nodes: Vec<(&str, &str, NodeStatus)> = graph
            .nodes
            .iter()
            .map(|n| (n.id.as_str(), n.label.as_str(), n.status))
            .collect();
        assert_eq!(
            nodes,
            [
                ("prepare", "prepare", NodeStatus::Pending),
                ("build", "Build it", NodeStatus::Pending),
                ("report", "report", NodeStatus::Pending),
            ]
        );
        assert_eq!(
            graph.edges,
            [
                edge("prepare", "build"),
                edge("prepare", "report"),
                edge("build", "report"),
            ]
        );
        assert_eq!(graph.omitted, 0);
    }

    #[test]
    fn the_interface_carries_the_declared_outputs_and_a_stable_digest() {
        let workflow = Workflow::parse(DEMO).unwrap();
        let interface = interface(&workflow, &outputs());
        let fields: Vec<(&str, Option<&FieldType>)> = interface
            .result
            .iter()
            .map(|e| (e.field.as_str(), e.ty.as_ref()))
            .collect();
        assert_eq!(
            fields,
            [
                ("legs", Some(&FieldType::Integer)),
                ("loud", Some(&FieldType::Boolean)),
                ("summary", Some(&FieldType::String)),
            ]
        );
        assert_eq!(interface.result_files.len(), 1);
        assert_eq!(interface.result_files[0].path, "notes.md");
        let again =
            crate::calls::github::workflow::interface(&Workflow::parse(DEMO).unwrap(), &outputs());
        assert_eq!(interface.digest().unwrap(), again.digest().unwrap());
        let moved = crate::calls::github::workflow::interface(
            &Workflow::parse(&DEMO.replace("default: fast", "default: thorough")).unwrap(),
            &outputs(),
        );
        assert_ne!(interface.digest().unwrap(), moved.digest().unwrap());
    }

    #[test]
    fn scalar_and_list_triggers_are_dispatchable_without_inputs() {
        for on in ["on: workflow_dispatch", "on: [push, workflow_dispatch]"] {
            let text = format!("{on}\njobs:\n  a:\n    runs-on: x\n");
            let workflow = Workflow::parse(&text).unwrap();
            assert_eq!(params_schema(&workflow)["properties"], json!({}));
            assert_eq!(graph(&workflow).nodes.len(), 1);
        }
        let null = "on:\n  workflow_dispatch:\njobs:\n  a:\n    runs-on: x\n";
        assert!(Workflow::parse(null).is_ok());
    }

    #[test]
    fn dispatch_inputs_are_strings_and_json_is_serialized() {
        let params = params_schema(&Workflow::parse(DEMO).unwrap());
        let values = BTreeMap::from([
            ("message".to_owned(), json!({"a": [1, 2]})),
            ("flavor".to_owned(), json!("thorough")),
            ("loud".to_owned(), json!(true)),
            ("count".to_owned(), json!(2.5)),
            ("target".to_owned(), Value::Null),
            ("untyped".to_owned(), json!(["x"])),
        ]);
        let inputs = dispatch_inputs(&params, &values, &BTreeMap::new()).unwrap();
        assert_eq!(
            inputs,
            BTreeMap::from([
                ("count".to_owned(), "2.5".to_owned()),
                ("flavor".to_owned(), "thorough".to_owned()),
                ("loud".to_owned(), "true".to_owned()),
                ("message".to_owned(), r#"{"a":[1,2]}"#.to_owned()),
                ("untyped".to_owned(), r#"["x"]"#.to_owned()),
            ])
        );
    }

    #[test]
    fn dispatch_inputs_refuse_what_github_cannot_take() {
        let params = params_schema(&Workflow::parse(DEMO).unwrap());
        let base = BTreeMap::from([
            ("message".to_owned(), json!("hi")),
            ("untyped".to_owned(), json!("u")),
        ]);
        let refused = |extra: &[(&str, Value)], files: &[&str]| {
            let mut values = base.clone();
            for (k, v) in extra {
                values.insert((*k).to_owned(), v.clone());
            }
            let files: BTreeMap<String, FileRef> = files
                .iter()
                .map(|name| {
                    let file = FileRef {
                        digest: "sha256:00".to_owned(),
                        size: 1,
                        path: None,
                        media_type: None,
                    };
                    ((*name).to_owned(), file)
                })
                .collect();
            match dispatch_inputs(&params, &values, &files) {
                Err(TargetError::Refused(note)) => note,
                other => panic!("expected a refusal, got {other:?}"),
            }
        };
        assert!(dispatch_inputs(&params, &base, &BTreeMap::new()).is_ok());
        assert!(refused(&[], &["upload"]).contains("no file parameter"));
        assert!(refused(&[("ghost", json!("x"))], &[]).contains("no input \"ghost\""));
        assert!(refused(&[("loud", json!("yes"))], &[]).contains("takes a boolean, not string"));
        assert!(refused(&[("count", json!("3"))], &[]).contains("takes a number"));
        assert!(refused(&[("message", json!(5))], &[]).contains("takes a string, not integer"));
        assert!(refused(&[("flavor", json!("slow"))], &[]).contains("choices"));
        let mut missing = base.clone();
        missing.remove("message");
        assert!(matches!(
            dispatch_inputs(&params, &missing, &BTreeMap::new()),
            Err(TargetError::Refused(note)) if note.contains("\"message\" is not bound")
        ));
    }

    fn edge(from: &str, to: &str) -> ProgressEdge {
        ProgressEdge {
            from: from.to_owned(),
            to: to.to_owned(),
        }
    }
}
