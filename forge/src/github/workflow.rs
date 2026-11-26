//! A workflow file as a dispatch reads it: its `workflow_dispatch` inputs and its jobs.

use std::collections::BTreeSet;

use serde_json::Value;

pub const MAX_INPUTS: usize = 25;

/// Why a workflow file cannot be dispatched as written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkflowError {
    #[error("not YAML: {0}")]
    Yaml(String),
    #[error("it has no workflow_dispatch trigger")]
    NotDispatchable,
    #[error("it declares {0} workflow_dispatch inputs; GitHub accepts at most {MAX_INPUTS}")]
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

/// The `type:` of one `workflow_dispatch` input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputType {
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

    pub fn as_str(self) -> &'static str {
        match self {
            InputType::String => "string",
            InputType::Choice => "choice",
            InputType::Environment => "environment",
            InputType::Boolean => "boolean",
            InputType::Number => "number",
        }
    }
}

/// One `workflow_dispatch` input.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowInput {
    pub name: String,
    pub declared: Option<InputType>,
    pub description: Option<String>,
    pub required: bool,
    pub default: Option<Value>,
    pub options: Vec<String>,
}

impl WorkflowInput {
    pub fn ty(&self) -> InputType {
        self.declared.unwrap_or(InputType::String)
    }
}

/// One job of the workflow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowJob {
    pub key: String,
    pub label: String,
    pub needs: Vec<String>,
}

/// What a workflow file declares that a dispatch reads.
#[derive(Clone, Debug, PartialEq)]
pub struct Workflow {
    pub inputs: Vec<WorkflowInput>,
    pub jobs: Vec<WorkflowJob>,
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
        if inputs.len() > MAX_INPUTS {
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
}

fn input(
    key: &serde_norway::Value,
    spec: &serde_norway::Value,
) -> Result<WorkflowInput, WorkflowError> {
    use serde_norway::Value as Yaml;
    let name = key.as_str().ok_or(WorkflowError::InputName)?.to_owned();
    let declared = match spec.get("type").and_then(Yaml::as_str) {
        None => None,
        Some(raw) => Some(
            InputType::parse(raw).ok_or_else(|| WorkflowError::InputType {
                name: name.clone(),
                ty: raw.to_owned(),
            })?,
        ),
    };
    let ty = declared.unwrap_or(InputType::String);
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
        required: spec
            .get("required")
            .and_then(parse_yaml_bool)
            .unwrap_or(false),
        name,
        declared,
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

fn parse_yaml_bool(value: &serde_norway::Value) -> Option<bool> {
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
        InputType::Boolean => parse_yaml_bool(raw).map(Value::Bool),
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::github::workflow::*;

    const CONTRACT_V1: &str = include_str!("../../../.github/workflows/crucible-build.yml");

    const JOBS: &str = "jobs:\n  a:\n    runs-on: x\n";

    fn inputs(yaml: &str) -> Vec<WorkflowInput> {
        Workflow::parse(yaml).unwrap().inputs
    }

    #[test]
    fn introspects_the_contract_v1_workflow() {
        let mut names: Vec<String> = inputs(CONTRACT_V1).into_iter().map(|i| i.name).collect();
        names.sort();
        assert_eq!(
            names,
            ["containerfile", "context-ref", "correlation_id", "image"]
        );
        let all = inputs(CONTRACT_V1);
        assert!(all.iter().all(|i| i.required), "all v1 inputs required");
        assert!(all.iter().all(|i| i.declared == Some(InputType::String)));
    }

    #[test]
    fn introspects_a_workflow_dispatch_with_no_inputs() {
        let yaml = "on:\n  workflow_dispatch:\njobs:\n  b:\n    runs-on: ubuntu-latest\n";
        assert_eq!(inputs(yaml), Vec::new());
    }

    #[test]
    fn introspects_scalar_and_sequence_triggers_without_inputs() {
        for on in ["on: workflow_dispatch", "on: [push, workflow_dispatch]"] {
            assert_eq!(inputs(&format!("{on}\n{JOBS}")), Vec::new(), "{on}");
        }
    }

    #[test]
    fn introspection_errors_when_not_dispatchable() {
        for on in [
            "on:\n  push:\n    branches: [main]\n",
            "on: push\n",
            "on: [push, pull_request]\n",
            "",
        ] {
            let err = Workflow::parse(&format!("{on}{JOBS}")).unwrap_err();
            assert_eq!(err, WorkflowError::NotDispatchable, "{on}");
            assert!(err.to_string().contains("workflow_dispatch"));
        }
    }

    #[test]
    fn introspection_defaults_required_false_and_reads_type() {
        let yaml = format!(
            "\
on:
  workflow_dispatch:
    inputs:
      verbose:
        type: boolean
      tag:
        description: a tag
        required: true
{JOBS}"
        );
        let inputs = inputs(&yaml);
        let verbose = inputs.iter().find(|i| i.name == "verbose").unwrap();
        assert!(!verbose.required, "required defaults to false");
        assert_eq!(verbose.declared, Some(InputType::Boolean));
        let tag = inputs.iter().find(|i| i.name == "tag").unwrap();
        assert!(tag.required);
        assert_eq!(tag.declared, None);
        assert_eq!(tag.ty(), InputType::String);
        assert_eq!(tag.description.as_deref(), Some("a tag"));
    }

    #[test]
    fn introspection_reads_quoted_required_flag() {
        let yaml = format!(
            "\
on:
  workflow_dispatch:
    inputs:
      tag:
        required: \"true\"
      maybe:
        required: \"false\"
      plain:
        required: true
{JOBS}"
        );
        let inputs = inputs(&yaml);
        let required = |name: &str| inputs.iter().find(|i| i.name == name).unwrap().required;
        assert!(required("tag"), "quoted \"true\" is required");
        assert!(!required("maybe"), "quoted \"false\" is optional");
        assert!(required("plain"), "bare true is required");
    }

    #[test]
    fn parse_yaml_bool_reads_real_and_stringly_booleans() {
        use serde_norway::Value;
        assert_eq!(parse_yaml_bool(&Value::Bool(true)), Some(true));
        assert_eq!(parse_yaml_bool(&Value::String("true".into())), Some(true));
        assert_eq!(parse_yaml_bool(&Value::String("TRUE".into())), Some(true));
        assert_eq!(parse_yaml_bool(&Value::String("false".into())), Some(false));
        assert_eq!(parse_yaml_bool(&Value::String("yes".into())), None);
        assert_eq!(parse_yaml_bool(&Value::Null), None);
    }

    #[test]
    fn defaults_and_options_are_typed() {
        let yaml = format!(
            "\
on:
  workflow_dispatch:
    inputs:
      flavor:
        type: choice
        options: [fast, thorough]
        default: fast
      loud:
        type: boolean
        default: \"false\"
      count:
        type: number
        default: 3
      ratio:
        type: number
        default: \"2.5\"
{JOBS}"
        );
        let inputs = inputs(&yaml);
        let get = |name: &str| inputs.iter().find(|i| i.name == name).unwrap();
        assert_eq!(get("flavor").options, ["fast", "thorough"]);
        assert_eq!(get("flavor").default, Some(json!("fast")));
        assert_eq!(get("loud").default, Some(json!(false)));
        assert_eq!(get("count").default, Some(json!(3)));
        assert_eq!(get("ratio").default, Some(json!(2.5)));
    }

    #[test]
    fn jobs_keep_their_needs_and_labels() {
        let workflow = Workflow::parse(
            "on: workflow_dispatch\njobs:\n  a:\n    runs-on: x\n  b:\n    name: Build it\n    needs: a\n  c:\n    name: c ${{ inputs.x }}\n    needs: [a, b]\n",
        )
        .unwrap();
        let jobs: Vec<(&str, &str, Vec<&str>)> = workflow
            .jobs
            .iter()
            .map(|j| {
                (
                    j.key.as_str(),
                    j.label.as_str(),
                    j.needs.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            jobs,
            [
                ("a", "a", vec![]),
                ("b", "Build it", vec!["a"]),
                ("c", "c", vec!["a", "b"]),
            ]
        );
    }

    #[test]
    fn workflows_a_dispatch_cannot_take_are_refused() {
        let refused = |text: &str| Workflow::parse(text).unwrap_err();
        let many: String = (0..26)
            .map(|i| format!("      in{i}:\n        type: string\n"))
            .collect();
        assert_eq!(
            refused(&format!(
                "on:\n  workflow_dispatch:\n    inputs:\n{many}{JOBS}"
            )),
            WorkflowError::TooManyInputs(26)
        );
        let exactly: String = (0..25)
            .map(|i| format!("      in{i}:\n        type: string\n"))
            .collect();
        assert!(
            Workflow::parse(&format!(
                "on:\n  workflow_dispatch:\n    inputs:\n{exactly}{JOBS}"
            ))
            .is_ok()
        );
        let input =
            |spec: &str| format!("on:\n  workflow_dispatch:\n    inputs:\n      x:\n{spec}{JOBS}");
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
            refused(&format!(
                "on:\n  workflow_dispatch:\n    inputs: [a]\n{JOBS}"
            )),
            WorkflowError::InputsNotMapping
        );
        assert_eq!(
            refused("on: workflow_dispatch\njobs: {}\n"),
            WorkflowError::NoJobs
        );
        assert_eq!(refused("on: workflow_dispatch\n"), WorkflowError::NoJobs);
        assert_eq!(
            refused("on: workflow_dispatch\njobs:\n  a:\n    needs: ghost\n"),
            WorkflowError::UnknownNeed {
                job: "a".to_owned(),
                need: "ghost".to_owned()
            }
        );
        assert!(matches!(refused("on: [unclosed\n"), WorkflowError::Yaml(_)));
    }
}
