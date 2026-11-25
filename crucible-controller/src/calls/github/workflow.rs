//! A workflow file as a call target reads it: its dispatch inputs as params, its jobs as a graph.

use std::collections::{BTreeMap, BTreeSet};

use crucible_contract::calls::{
    FileRef, GithubOutputs, MAX_GITHUB_INPUTS, NodeStatus, ProgressEdge, ProgressGraph,
    ProgressNode, TargetInterface,
};
use crucible_contract::emits::{DeclaredFile, EmitWire, value_type};
use serde_json::{Map, Value, json};

use crate::calls::github::GithubError;

/// Why a workflow file is not a dispatchable target.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkflowError {
    #[error("not YAML: {0}")]
    Yaml(String),
    #[error("it has no workflow_dispatch trigger")]
    NotDispatchable,
    #[error("it declares {0} workflow_dispatch inputs; GitHub accepts at most {MAX_GITHUB_INPUTS}")]
    TooManyInputs(usize),
    #[error("workflow_dispatch.inputs is not a mapping")]
    InputsNotMapping,
    #[error("an input name is not a string")]
    InputName,
    #[error(
        "input {name:?} has type {ty:?}; GitHub input types are string, choice, boolean, number and environment"
    )]
    InputType { name: String, ty: String },
    #[error("choice input {0:?} has no string options")]
    ChoiceOptions(String),
    #[error("input {0:?} has a default that is not of its type")]
    Default(String),
    #[error("it has no jobs")]
    NoJobs,
    #[error("job {job:?} needs {need:?}, which the workflow does not declare")]
    UnknownNeed { job: String, need: String },
}

/// The type of one `workflow_dispatch` input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputType {
    String,
    Choice,
    Environment,
    Boolean,
    Number,
}

impl InputType {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "string" => Some(InputType::String),
            "choice" => Some(InputType::Choice),
            "environment" => Some(InputType::Environment),
            "boolean" => Some(InputType::Boolean),
            "number" => Some(InputType::Number),
            _ => None,
        }
    }

    fn schema_type(self) -> &'static str {
        match self {
            InputType::String | InputType::Choice | InputType::Environment => "string",
            InputType::Boolean => "boolean",
            InputType::Number => "number",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct WorkflowInput {
    name: String,
    ty: InputType,
    description: Option<String>,
    required: bool,
    default: Option<Value>,
    options: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkflowJob {
    key: String,
    label: String,
    needs: Vec<String>,
}

/// What a workflow file declares that a call reads: its dispatch inputs and its jobs.
#[derive(Clone, Debug, PartialEq)]
pub struct Workflow {
    inputs: Vec<WorkflowInput>,
    jobs: Vec<WorkflowJob>,
}

impl Workflow {
    pub fn parse(text: &str) -> Result<Self, WorkflowError> {
        use serde_norway::Value as Yaml;
        let doc: Yaml =
            serde_norway::from_str(text).map_err(|e| WorkflowError::Yaml(e.to_string()))?;
        let on = doc
            .get("on")
            .or_else(|| doc.get(Yaml::Bool(true)))
            .ok_or(WorkflowError::NotDispatchable)?;
        let dispatch = match on {
            Yaml::String(s) if s == "workflow_dispatch" => None,
            Yaml::Sequence(triggers)
                if triggers
                    .iter()
                    .any(|t| t.as_str() == Some("workflow_dispatch")) =>
            {
                None
            }
            Yaml::Mapping(_) => Some(
                on.get("workflow_dispatch")
                    .ok_or(WorkflowError::NotDispatchable)?,
            ),
            _ => return Err(WorkflowError::NotDispatchable),
        };
        let inputs = match dispatch.and_then(|d| d.get("inputs")) {
            Some(Yaml::Mapping(inputs)) => inputs
                .iter()
                .map(|(key, spec)| input(key, spec))
                .collect::<Result<Vec<_>, _>>()?,
            Some(Yaml::Null) | None => Vec::new(),
            Some(_) => return Err(WorkflowError::InputsNotMapping),
        };
        if inputs.len() > MAX_GITHUB_INPUTS {
            return Err(WorkflowError::TooManyInputs(inputs.len()));
        }
        let jobs = match doc.get("jobs") {
            Some(Yaml::Mapping(jobs)) if !jobs.is_empty() => jobs
                .iter()
                .filter_map(|(key, spec)| Some(job(key.as_str()?, spec)))
                .collect::<Vec<_>>(),
            _ => return Err(WorkflowError::NoJobs),
        };
        let keys: BTreeSet<&str> = jobs.iter().map(|j| j.key.as_str()).collect();
        for job in &jobs {
            if let Some(need) = job.needs.iter().find(|n| !keys.contains(n.as_str())) {
                return Err(WorkflowError::UnknownNeed {
                    job: job.key.clone(),
                    need: need.clone(),
                });
            }
        }
        Ok(Workflow { inputs, jobs })
    }

    /// The parameters as the JSON Schema a params block emits.
    pub fn params_schema(&self) -> Value {
        let mut properties = Map::new();
        let mut required = Vec::new();
        for input in &self.inputs {
            let mut property = Map::new();
            property.insert("type".into(), json!(input.ty.schema_type()));
            if let Some(doc) = &input.description {
                property.insert("description".into(), json!(doc));
            }
            if let Some(default) = &input.default {
                property.insert("default".into(), default.clone());
            }
            if input.ty == InputType::Choice {
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
            "x-order": self.inputs.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
        })
    }

    /// The jobs as a graph, every node pending, with edges from `needs`.
    pub fn graph(&self) -> ProgressGraph {
        let nodes = self
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
        let edges = self
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

    /// The target's interface: these parameters and graph, and the result `outputs` declares.
    pub fn interface(&self, outputs: &GithubOutputs) -> TargetInterface {
        TargetInterface {
            params: self.params_schema(),
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
            graph: self.graph(),
        }
    }
}

fn input(
    key: &serde_norway::Value,
    spec: &serde_norway::Value,
) -> Result<WorkflowInput, WorkflowError> {
    use serde_norway::Value as Yaml;
    let name = key.as_str().ok_or(WorkflowError::InputName)?.to_owned();
    let raw_ty = spec.get("type").and_then(Yaml::as_str).unwrap_or("string");
    let ty = InputType::parse(raw_ty).ok_or_else(|| WorkflowError::InputType {
        name: name.clone(),
        ty: raw_ty.to_owned(),
    })?;
    let options = match ty {
        InputType::Choice => {
            let options: Vec<String> = spec
                .get("options")
                .and_then(Yaml::as_sequence)
                .map(|items| items.iter().filter_map(yaml_scalar_string).collect())
                .unwrap_or_default();
            if options.is_empty() {
                return Err(WorkflowError::ChoiceOptions(name));
            }
            options
        }
        _ => Vec::new(),
    };
    let default = match spec.get("default") {
        None | Some(Yaml::Null) => None,
        Some(raw) => {
            Some(typed_default(ty, raw).ok_or_else(|| WorkflowError::Default(name.clone()))?)
        }
    };
    Ok(WorkflowInput {
        description: spec
            .get("description")
            .and_then(Yaml::as_str)
            .map(str::to_owned),
        required: spec.get("required").and_then(yaml_bool).unwrap_or(false),
        name,
        ty,
        default,
        options,
    })
}

fn job(key: &str, spec: &serde_norway::Value) -> WorkflowJob {
    use serde_norway::Value as Yaml;
    let label = spec
        .get("name")
        .and_then(Yaml::as_str)
        .filter(|name| !name.contains("${{"))
        .unwrap_or(key)
        .to_owned();
    let needs = match spec.get("needs") {
        Some(Yaml::String(one)) => vec![one.clone()],
        Some(Yaml::Sequence(many)) => many
            .iter()
            .filter_map(Yaml::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    };
    WorkflowJob {
        key: key.to_owned(),
        label,
        needs,
    }
}

fn yaml_bool(value: &serde_norway::Value) -> Option<bool> {
    value.as_bool().or_else(
        || match value.as_str()?.trim().to_ascii_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
    )
}

fn yaml_scalar_string(value: &serde_norway::Value) -> Option<String> {
    use serde_norway::Value as Yaml;
    match value {
        Yaml::String(s) => Some(s.clone()),
        Yaml::Bool(b) => Some(b.to_string()),
        Yaml::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn typed_default(ty: InputType, raw: &serde_norway::Value) -> Option<Value> {
    match ty {
        InputType::Boolean => yaml_bool(raw).map(Value::Bool),
        InputType::Number => raw
            .as_f64()
            .or_else(|| raw.as_str()?.trim().parse().ok())
            .and_then(serde_json::Number::from_f64)
            .map(|n| match raw.as_i64() {
                Some(i) => Value::from(i),
                None => Value::Number(n),
            }),
        InputType::String | InputType::Choice | InputType::Environment => {
            yaml_scalar_string(raw).map(Value::String)
        }
    }
}

/// The `inputs` of a dispatch: each bound value as the string GitHub takes, a json value as its
/// serialized JSON. Refuses file parameters, undeclared inputs, missing required inputs, and
/// values of the wrong type.
pub fn dispatch_inputs(
    params: &Value,
    values: &BTreeMap<String, Value>,
    files: &BTreeMap<String, FileRef>,
) -> Result<BTreeMap<String, String>, GithubError> {
    if !files.is_empty() {
        let names: Vec<&str> = files.keys().map(String::as_str).collect();
        return Err(GithubError::Refused(format!(
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
            return Err(GithubError::Refused(format!(
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
                return Err(GithubError::Refused(format!(
                    "input {name:?} takes a {ty}, not {}",
                    value_type(other)
                )));
            }
        };
        if let Some(options) = property.get("enum").and_then(Value::as_array)
            && !options.iter().any(|o| o.as_str() == Some(text.as_str()))
        {
            return Err(GithubError::Refused(format!(
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
            return Err(GithubError::Refused(format!(
                "required input {name:?} is not bound"
            )));
        }
    }
    if inputs.len() > MAX_GITHUB_INPUTS {
        return Err(GithubError::Refused(format!(
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

    use crate::calls::github::GithubError;
    use crate::calls::github::workflow::{Workflow, WorkflowError, dispatch_inputs};

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
            workflow.params_schema(),
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
        let schema = Workflow::parse(DEMO).unwrap().params_schema();
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
        let graph = Workflow::parse(DEMO).unwrap().graph();
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
        let interface = workflow.interface(&outputs());
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
        let again = Workflow::parse(DEMO).unwrap().interface(&outputs());
        assert_eq!(interface.digest().unwrap(), again.digest().unwrap());
        let moved = Workflow::parse(&DEMO.replace("default: fast", "default: thorough"))
            .unwrap()
            .interface(&outputs());
        assert_ne!(interface.digest().unwrap(), moved.digest().unwrap());
    }

    #[test]
    fn scalar_and_list_triggers_are_dispatchable_without_inputs() {
        for on in ["on: workflow_dispatch", "on: [push, workflow_dispatch]"] {
            let text = format!("{on}\njobs:\n  a:\n    runs-on: x\n");
            let workflow = Workflow::parse(&text).unwrap();
            assert_eq!(workflow.params_schema()["properties"], json!({}));
            assert_eq!(workflow.graph().nodes.len(), 1);
        }
        let null = "on:\n  workflow_dispatch:\njobs:\n  a:\n    runs-on: x\n";
        assert!(Workflow::parse(null).is_ok());
    }

    #[test]
    fn workflows_a_call_cannot_dispatch_are_refused() {
        let jobs = "jobs:\n  a:\n    runs-on: x\n";
        let refused = |text: &str| Workflow::parse(text).unwrap_err();
        assert_eq!(
            refused(&format!("on: push\n{jobs}")),
            WorkflowError::NotDispatchable
        );
        assert_eq!(
            refused(&format!("on: [push]\n{jobs}")),
            WorkflowError::NotDispatchable
        );
        assert_eq!(
            refused(&format!("on:\n  push:\n{jobs}")),
            WorkflowError::NotDispatchable
        );
        assert_eq!(refused(jobs), WorkflowError::NotDispatchable);
        let many: String = (0..26)
            .map(|i| format!("      in{i}:\n        type: string\n"))
            .collect();
        assert_eq!(
            refused(&format!(
                "on:\n  workflow_dispatch:\n    inputs:\n{many}{jobs}"
            )),
            WorkflowError::TooManyInputs(26)
        );
        let exactly: String = (0..25)
            .map(|i| format!("      in{i}:\n        type: string\n"))
            .collect();
        assert!(
            Workflow::parse(&format!(
                "on:\n  workflow_dispatch:\n    inputs:\n{exactly}{jobs}"
            ))
            .is_ok()
        );
        let input =
            |spec: &str| format!("on:\n  workflow_dispatch:\n    inputs:\n      x:\n{spec}{jobs}");
        assert_eq!(
            refused(&input("        type: secret\n")),
            WorkflowError::InputType {
                name: "x".to_owned(),
                ty: "secret".to_owned()
            }
        );
        assert_eq!(
            refused(&input("        type: choice\n")),
            WorkflowError::ChoiceOptions("x".to_owned())
        );
        assert_eq!(
            refused(&input("        type: boolean\n        default: maybe\n")),
            WorkflowError::Default("x".to_owned())
        );
        assert_eq!(
            refused("on:\n  workflow_dispatch:\n    inputs: [a]\njobs:\n  a:\n    runs-on: x\n"),
            WorkflowError::InputsNotMapping
        );
        assert_eq!(
            refused("on: workflow_dispatch\njobs: {}\n"),
            WorkflowError::NoJobs
        );
        assert_eq!(
            refused("on: workflow_dispatch\njobs:\n  a:\n    needs: ghost\n"),
            WorkflowError::UnknownNeed {
                job: "a".to_owned(),
                need: "ghost".to_owned()
            }
        );
        assert!(matches!(refused("on: [unclosed\n"), WorkflowError::Yaml(_)));
    }

    #[test]
    fn dispatch_inputs_are_strings_and_json_is_serialized() {
        let params = Workflow::parse(DEMO).unwrap().params_schema();
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
        let params = Workflow::parse(DEMO).unwrap().params_schema();
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
                Err(GithubError::Refused(note)) => note,
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
            Err(GithubError::Refused(note)) if note.contains("\"message\" is not bound")
        ));
    }

    fn edge(from: &str, to: &str) -> ProgressEdge {
        ProgressEdge {
            from: from.to_owned(),
            to: to.to_owned(),
        }
    }
}
