//! A GitLab project's CI configuration as a call target reads it: its `spec:inputs` as params,
//! its jobs as a pending graph, and the declared outputs as the result.

use std::collections::BTreeMap;

use crucible_ci::gitlab::graph::{Shape, pipeline_graph};
use crucible_ci::gitlab::inputs::{Input, InputKind};
use crucible_ci::gitlab::model::{Job, Lint};
use crucible_contract::calls::{FileRef, GitlabOutputs, ProgressGraph, TargetInterface};
use crucible_contract::emits::{DeclaredFile, EmitWire};
use serde_json::{Map, Value, json};

pub fn params_schema(inputs: &[Input]) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for input in inputs {
        let mut property = Map::new();
        match &input.kind {
            InputKind::String { options, regex } => {
                property.insert("type".into(), json!("string"));
                if !options.is_empty() {
                    property.insert("enum".into(), json!(options));
                }
                if let Some(regex) = regex {
                    property.insert("pattern".into(), json!(regex));
                }
            }
            InputKind::Number => {
                property.insert("type".into(), json!("number"));
            }
            InputKind::Boolean => {
                property.insert("type".into(), json!("boolean"));
            }
            InputKind::StringList => {
                property.insert("type".into(), json!("array"));
                property.insert("items".into(), json!({"type": "string"}));
            }
            InputKind::Json => {
                property.insert("type".into(), json!("array"));
            }
        }
        if let Some(doc) = &input.description {
            property.insert("description".into(), json!(doc));
        }
        match &input.default {
            Some(default) => {
                property.insert("default".into(), default.clone());
            }
            None => required.push(input.name.clone()),
        }
        properties.insert(input.name.clone(), Value::Object(property));
    }
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": required,
        "x-order": inputs.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
    })
}

pub fn pending_graph(lint: &Lint) -> ProgressGraph {
    let shape = Shape::from_lint(lint);
    let jobs: Vec<Job> = lint
        .jobs
        .iter()
        .zip(1u64..)
        .map(|(job, id)| Job {
            id,
            name: job.name.clone(),
            stage: job.stage.clone(),
            status: "created".to_owned(),
            allow_failure: false,
            started_at: None,
            finished_at: None,
            web_url: None,
        })
        .collect();
    pipeline_graph(&jobs, &[], &shape).graph
}

pub fn shape_of(graph: &ProgressGraph) -> Shape {
    let mut stages: Vec<String> = Vec::new();
    for node in &graph.nodes {
        if let Some(stage) = &node.group
            && !stages.contains(stage)
        {
            stages.push(stage.clone());
        }
    }
    let mut needs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for edge in &graph.edges {
        needs
            .entry(edge.to.clone())
            .or_default()
            .push(edge.from.clone());
    }
    Shape { stages, needs }
}

pub fn interface(inputs: &[Input], lint: &Lint, outputs: &GitlabOutputs) -> TargetInterface {
    TargetInterface {
        params: params_schema(inputs),
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
        graph: pending_graph(lint),
    }
}

pub fn pipeline_inputs(
    params: &Value,
    values: &BTreeMap<String, Value>,
    files: &BTreeMap<String, FileRef>,
) -> Result<BTreeMap<String, Value>, String> {
    if !files.is_empty() {
        let names: Vec<&str> = files.keys().map(String::as_str).collect();
        return Err(format!(
            "a GitLab pipeline takes no file input, but the call binds {}",
            names.join(", ")
        ));
    }
    let declared = params.get("properties").and_then(Value::as_object);
    let mut inputs = BTreeMap::new();
    for (name, value) in values {
        if !declared.is_some_and(|d| d.contains_key(name)) {
            return Err(format!("the pipeline declares no input {name:?}"));
        }
        inputs.insert(name.clone(), value.clone());
    }
    Ok(inputs)
}

#[cfg(test)]
mod tests {
    use crate::calls::gitlab::interface::*;
    use crucible_ci::gitlab::model::LintJob;
    use crucible_contract::calls::NodeStatus;
    use crucible_contract::emits::FieldType;

    fn inputs() -> Vec<Input> {
        crucible_ci::gitlab::inputs::inputs(
            r#"spec:
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
---
build:
  script: echo
"#,
        )
        .unwrap()
    }

    fn lint() -> Lint {
        Lint {
            valid: true,
            errors: vec![],
            jobs: vec![
                LintJob {
                    name: "build".into(),
                    stage: "build".into(),
                    needs: vec![],
                },
                LintJob {
                    name: "test".into(),
                    stage: "test".into(),
                    needs: vec![],
                },
                LintJob {
                    name: "report".into(),
                    stage: "deploy".into(),
                    needs: vec![json!({"name": "build"})],
                },
            ],
        }
    }

    #[test]
    fn inputs_become_a_params_schema_with_defaults_optional() {
        let schema = params_schema(&inputs());
        assert_eq!(schema["required"], json!(["image"]));
        assert_eq!(
            schema["properties"]["target"],
            json!({
                "type": "string",
                "enum": ["staging", "production"],
                "description": "Where to deploy",
                "default": "staging",
            })
        );
        assert_eq!(
            schema["properties"]["image"]["pattern"],
            json!(r"^quay\.io/.+")
        );
        assert_eq!(schema["properties"]["replicas"]["type"], json!("number"));
        assert_eq!(schema["properties"]["dry_run"]["type"], json!("boolean"));
        assert_eq!(
            schema["properties"]["tags"]["items"],
            json!({"type": "string"})
        );
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    #[test]
    fn lint_jobs_become_a_pending_graph_by_stage_and_needs() {
        let graph = pending_graph(&lint());
        let ids: Vec<&str> = graph.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["build", "test", "report"]);
        assert!(graph.nodes.iter().all(|n| n.status == NodeStatus::Pending));
        let edges: Vec<(&str, &str)> = graph
            .edges
            .iter()
            .map(|e| (e.from.as_str(), e.to.as_str()))
            .collect();
        assert_eq!(edges, [("build", "test"), ("build", "report")]);
        let shape = shape_of(&graph);
        assert_eq!(shape.stages, ["build", "test", "deploy"]);
        assert_eq!(shape.needs.get("report"), Some(&vec!["build".to_owned()]));
    }

    #[test]
    fn the_interface_carries_the_declared_outputs() {
        let outputs = GitlabOutputs {
            job: Some("report".into()),
            fields: BTreeMap::from([("version".to_owned(), FieldType::String)]),
            files: vec!["out/sbom.json".into()],
        };
        let interface = interface(&inputs(), &lint(), &outputs);
        assert_eq!(interface.result.len(), 1);
        assert_eq!(interface.result[0].field, "version");
        assert_eq!(interface.result_files.len(), 1);
        assert_eq!(interface.graph.nodes.len(), 3);
    }

    #[test]
    fn pipeline_inputs_are_the_declared_values_and_no_files() {
        let params = params_schema(&inputs());
        let values = BTreeMap::from([
            ("target".to_owned(), json!("production")),
            ("tags".to_owned(), json!(["x"])),
        ]);
        let sent = pipeline_inputs(&params, &values, &BTreeMap::new()).unwrap();
        assert_eq!(sent.get("tags"), Some(&json!(["x"])));
        let unknown = BTreeMap::from([("nope".to_owned(), json!(1))]);
        assert!(
            pipeline_inputs(&params, &unknown, &BTreeMap::new())
                .unwrap_err()
                .contains("no input \"nope\"")
        );
        let files = BTreeMap::from([(
            "bundle".to_owned(),
            FileRef {
                digest: "sha256:x".into(),
                size: 1,
                path: None,
                media_type: None,
            },
        )]);
        assert!(
            pipeline_inputs(&params, &BTreeMap::new(), &files)
                .unwrap_err()
                .contains("no file input")
        );
    }
}
