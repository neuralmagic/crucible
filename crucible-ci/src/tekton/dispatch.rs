//! Building the PipelineRun a call creates from a repository's `.tekton` file (RFC-0008
//! C-TEKTON dispatch).
//!
//! ```text
//!   .tekton/x.yaml at the pinned commit
//!     │ {{ revision }}, {{ source_url }}, … ─▶ set from the pin
//!     │ pipelinesascode.tekton.dev/* annotations ─▶ removed, so no PaC controller acts on it
//!     │ spec.params ─▶ the call's bound values replace the file's defaults
//!     ▼ labels ─▶ calling run, call, instance, attempt;  name ─▶ generateName
//!   POST /apis/tekton.dev/v1/namespaces/{ns}/pipelineruns
//! ```

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::commit::CommitId;

const PAC_PREFIX: &str = "pipelinesascode.tekton.dev/";
const MAX_FILE_BYTES: usize = 256 * 1024;
const GENERATE_NAME_MAX: usize = 57;

/// Where the file was read from, which fills the Pipelines-as-Code variables it uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// The repository's HTTPS URL, such as `https://github.com/org/repo`.
    pub repo_url: String,
    pub owner: String,
    pub name: String,
    /// The branch the pin's ref names.
    pub branch: String,
    pub commit: CommitId,
    pub namespace: String,
    /// The Secret holding git credentials the file's `{{ git_auth_secret }}` names, when the
    /// target declares one.
    pub git_auth_secret: Option<String>,
}

/// A parameter the file declares, with the file's value as its default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileParam {
    String { name: String, default: String },
    List { name: String, default: Vec<String> },
}

impl FileParam {
    pub fn name(&self) -> &str {
        match self {
            FileParam::String { name, .. } | FileParam::List { name, .. } => name,
        }
    }
}

/// A bound value for one parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamValue {
    String(String),
    List(Vec<String>),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DispatchError {
    #[error("the PipelineRun file is over {MAX_FILE_BYTES} bytes")]
    TooLarge,
    #[error("the PipelineRun file is not YAML: {0}")]
    Yaml(String),
    #[error("the file is a {0}, not a tekton.dev/v1 PipelineRun")]
    NotAPipelineRun(String),
    #[error("parameter {name} has a value that is neither a string nor a list of strings")]
    ParamType { name: String },
    #[error("the file uses the Pipelines-as-Code variable {{{{ {0} }}}}, which a call cannot set")]
    Variable(String),
    #[error("{0} is not a parameter of the PipelineRun")]
    UnknownParam(String),
    #[error("parameter {name} takes a {expected}")]
    WrongType {
        name: String,
        expected: &'static str,
    },
}

/// A `.tekton` PipelineRun file, parsed.
#[derive(Clone, Debug, PartialEq)]
pub struct PipelineRunFile {
    doc: Value,
}

impl PipelineRunFile {
    pub fn parse(text: &str) -> Result<Self, DispatchError> {
        if text.len() > MAX_FILE_BYTES {
            return Err(DispatchError::TooLarge);
        }
        let doc: Value =
            serde_norway::from_str(text).map_err(|e| DispatchError::Yaml(e.to_string()))?;
        let kind = doc.get("kind").and_then(Value::as_str).unwrap_or_default();
        let api = doc
            .get("apiVersion")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind != "PipelineRun" || api != "tekton.dev/v1" {
            return Err(DispatchError::NotAPipelineRun(format!("{api} {kind}")));
        }
        Ok(PipelineRunFile { doc })
    }

    /// The name the file gives its PipelineRun.
    pub fn name(&self) -> &str {
        self.doc
            .pointer("/metadata/name")
            .and_then(Value::as_str)
            .unwrap_or("pipelinerun")
    }

    /// The parameters the file sets, each with the file's value as its default.
    pub fn params(&self) -> Result<Vec<FileParam>, DispatchError> {
        let Some(items) = self.doc.pointer("/spec/params").and_then(Value::as_array) else {
            return Ok(Vec::new());
        };
        items
            .iter()
            .map(|p| {
                let name = p
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                match p.get("value") {
                    Some(Value::String(s)) => Ok(FileParam::String {
                        name,
                        default: s.clone(),
                    }),
                    Some(Value::Array(items)) => items
                        .iter()
                        .map(|v| v.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()
                        .map(|default| FileParam::List {
                            name: name.clone(),
                            default,
                        })
                        .ok_or(DispatchError::ParamType { name }),
                    _ => Err(DispatchError::ParamType { name }),
                }
            })
            .collect()
    }

    /// The Pipelines-as-Code variables the file uses, such as `revision`.
    pub fn variables(&self) -> Vec<String> {
        let mut found = Vec::new();
        collect_variables(&self.doc, &mut found);
        found.sort();
        found.dedup();
        found
    }

    /// The PipelineRun to create: the file with its variables set from `source`, its
    /// Pipelines-as-Code annotations removed, `values` replacing its params' defaults, and
    /// `labels` added.
    pub fn render(
        &self,
        source: &Source,
        values: &BTreeMap<String, ParamValue>,
        labels: &BTreeMap<String, String>,
    ) -> Result<Value, DispatchError> {
        let vars = variables_of(source);
        for used in self.variables() {
            if !vars.contains_key(used.as_str()) {
                return Err(DispatchError::Variable(used));
            }
        }
        let declared = self.params()?;
        for (name, value) in values {
            let Some(param) = declared.iter().find(|p| p.name() == name) else {
                return Err(DispatchError::UnknownParam(name.clone()));
            };
            match (param, value) {
                (FileParam::String { .. }, ParamValue::String(_))
                | (FileParam::List { .. }, ParamValue::List(_)) => {}
                (FileParam::String { .. }, ParamValue::List(_)) => {
                    return Err(DispatchError::WrongType {
                        name: name.clone(),
                        expected: "string",
                    });
                }
                (FileParam::List { .. }, ParamValue::String(_)) => {
                    return Err(DispatchError::WrongType {
                        name: name.clone(),
                        expected: "list of strings",
                    });
                }
            }
        }

        let mut doc = self.doc.clone();
        substitute(&mut doc, &vars);
        if let Some(params) = doc
            .pointer_mut("/spec/params")
            .and_then(Value::as_array_mut)
        {
            for p in params.iter_mut() {
                let Some(name) = p.get("name").and_then(Value::as_str) else {
                    continue;
                };
                if let Some(bound) = values.get(name) {
                    p["value"] = match bound {
                        ParamValue::String(s) => Value::String(s.clone()),
                        ParamValue::List(items) => {
                            Value::Array(items.iter().cloned().map(Value::String).collect())
                        }
                    };
                }
            }
        }
        let generate = format!("{}-", truncated(self.name(), GENERATE_NAME_MAX));
        let meta = doc.as_object_mut().map(|o| {
            o.entry("metadata")
                .or_insert_with(|| Value::Object(Map::new()))
        });
        if let Some(Value::Object(meta)) = meta {
            meta.remove("name");
            meta.remove("creationTimestamp");
            meta.insert("generateName".into(), Value::String(generate));
            meta.insert("namespace".into(), Value::String(source.namespace.clone()));
            if let Some(Value::Object(annotations)) = meta.get_mut("annotations") {
                annotations.retain(|k, _| !k.starts_with(PAC_PREFIX));
            }
            let entry = meta
                .entry("labels")
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(existing) = entry {
                existing.retain(|k, _| !k.starts_with(PAC_PREFIX));
                for (k, v) in labels {
                    existing.insert(k.clone(), Value::String(v.clone()));
                }
            }
        }
        if let Some(o) = doc.as_object_mut() {
            o.remove("status");
        }
        Ok(doc)
    }
}

fn variables_of(source: &Source) -> BTreeMap<&'static str, String> {
    let mut vars = BTreeMap::from([
        ("revision", source.commit.to_string()),
        ("repo_url", source.repo_url.clone()),
        ("source_url", source.repo_url.clone()),
        ("repo_owner", source.owner.clone()),
        ("repo_name", source.name.clone()),
        ("source_branch", source.branch.clone()),
        ("target_branch", source.branch.clone()),
        ("target_namespace", source.namespace.clone()),
        ("event_type", "push".to_owned()),
    ]);
    if let Some(secret) = &source.git_auth_secret {
        vars.insert("git_auth_secret", secret.clone());
    }
    vars
}

fn truncated(name: &str, max: usize) -> &str {
    let mut end = name.len().min(max);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name.get(..end).unwrap_or(name).trim_end_matches('-')
}

/// Each `{{ name }}` in `text`, in order.
fn placeholders(text: &str) -> Vec<(usize, usize, &str)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(open) = text
        .get(from..)
        .and_then(|t| t.find("{{"))
        .map(|i| i + from)
    {
        let Some(close) = text
            .get(open..)
            .and_then(|t| t.find("}}"))
            .map(|i| i + open)
        else {
            break;
        };
        if let Some(inner) = text.get(open + 2..close) {
            let name = inner.trim();
            if !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
            {
                out.push((open, close + 2, name));
            }
        }
        from = close + 2;
    }
    out
}

fn collect_variables(value: &Value, into: &mut Vec<String>) {
    match value {
        Value::String(s) => into.extend(placeholders(s).into_iter().map(|(_, _, n)| n.to_owned())),
        Value::Array(items) => items.iter().for_each(|v| collect_variables(v, into)),
        Value::Object(map) => map.values().for_each(|v| collect_variables(v, into)),
        _ => {}
    }
}

fn substitute(value: &mut Value, vars: &BTreeMap<&'static str, String>) {
    match value {
        Value::String(s) => {
            let spots = placeholders(s);
            if spots.is_empty() {
                return;
            }
            let mut out = String::with_capacity(s.len());
            let mut at = 0;
            for (start, end, name) in spots {
                out.push_str(s.get(at..start).unwrap_or_default());
                match vars.get(name) {
                    Some(v) => out.push_str(v),
                    None => out.push_str(s.get(start..end).unwrap_or_default()),
                }
                at = end;
            }
            out.push_str(s.get(at..).unwrap_or_default());
            *s = out;
        }
        Value::Array(items) => items.iter_mut().for_each(|v| substitute(v, vars)),
        Value::Object(map) => map.values_mut().for_each(|v| substitute(v, vars)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::Value;

    use crate::commit::CommitId;
    use crate::tekton::dispatch::{DispatchError, FileParam, ParamValue, PipelineRunFile, Source};

    const MODELEXPRESS_PUSH: &str =
        include_str!("../../testdata/tekton/odh-modelexpress-push.yaml");

    fn source(secret: Option<&str>) -> Source {
        Source {
            repo_url: "https://github.com/opendatahub-io/modelexpress".to_owned(),
            owner: "opendatahub-io".to_owned(),
            name: "modelexpress".to_owned(),
            branch: "main".to_owned(),
            commit: CommitId::parse("8793cb162115e87e0eab5ba1b7e8e65efb4b1096").expect("commit"),
            namespace: "open-data-hub-tenant".to_owned(),
            git_auth_secret: secret.map(str::to_owned),
        }
    }

    fn labels() -> BTreeMap<String, String> {
        BTreeMap::from([("crucible.dev/call".to_owned(), "call-1".to_owned())])
    }

    #[test]
    fn the_modelexpress_push_file_reads_its_params_and_variables() {
        let file = PipelineRunFile::parse(MODELEXPRESS_PUSH).expect("parses");
        assert_eq!(file.name(), "odh-modelexpress-on-push");
        let params = file.params().expect("params");
        let names: Vec<&str> = params.iter().map(FileParam::name).collect();
        assert_eq!(
            names,
            [
                "git-url",
                "revision",
                "output-image",
                "dockerfile",
                "path-context",
                "hermetic",
                "prefetch-input"
            ]
        );
        assert_eq!(
            params.iter().find(|p| p.name() == "output-image"),
            Some(&FileParam::String {
                name: "output-image".into(),
                default: "quay.io/opendatahub/odh-modelexpress:odh-stable".into()
            })
        );
        assert_eq!(
            file.variables(),
            ["git_auth_secret", "revision", "source_url", "target_branch"]
        );
    }

    #[test]
    fn a_file_needing_git_credentials_is_refused_without_a_secret() {
        let file = PipelineRunFile::parse(MODELEXPRESS_PUSH).expect("parses");
        assert_eq!(
            file.render(&source(None), &BTreeMap::new(), &labels()),
            Err(DispatchError::Variable("git_auth_secret".into()))
        );
    }

    #[test]
    fn rendering_sets_the_pin_strips_pac_and_binds_values() {
        let file = PipelineRunFile::parse(MODELEXPRESS_PUSH).expect("parses");
        let values = BTreeMap::from([(
            "output-image".to_owned(),
            ParamValue::String("quay.io/me/modelexpress:call".to_owned()),
        )]);
        let run = file
            .render(&source(Some("git-auth-crucible")), &values, &labels())
            .expect("renders");
        let text = serde_json::to_string(&run).expect("serializes");
        assert!(!text.contains("{{"), "every variable is set");
        assert!(
            !text.contains("pipelinesascode.tekton.dev/"),
            "no PaC annotation or label is left"
        );
        assert_eq!(
            run.pointer("/metadata/generateName"),
            Some(&Value::from("odh-modelexpress-on-push-"))
        );
        assert!(run.pointer("/metadata/name").is_none());
        assert!(run.get("status").is_none());
        assert_eq!(
            run.pointer("/metadata/namespace"),
            Some(&Value::from("open-data-hub-tenant"))
        );
        assert_eq!(
            run.pointer("/metadata/labels/crucible.dev~1call"),
            Some(&Value::from("call-1"))
        );
        assert_eq!(
            run.pointer("/metadata/labels/appstudio.openshift.io~1component"),
            Some(&Value::from("odh-modelexpress-ci")),
            "labels the file sets stay"
        );
        let param = |name: &str| {
            run.pointer("/spec/params")
                .and_then(Value::as_array)
                .and_then(|ps| ps.iter().find(|p| p["name"] == name))
                .map(|p| p["value"].clone())
        };
        assert_eq!(
            param("output-image"),
            Some(Value::from("quay.io/me/modelexpress:call"))
        );
        assert_eq!(
            param("revision"),
            Some(Value::from("8793cb162115e87e0eab5ba1b7e8e65efb4b1096"))
        );
        assert_eq!(
            param("git-url"),
            Some(Value::from(
                "https://github.com/opendatahub-io/modelexpress"
            ))
        );
        assert_eq!(
            run.pointer("/spec/workspaces/0/secret/secretName"),
            Some(&Value::from("git-auth-crucible"))
        );
        assert_eq!(
            run.pointer("/metadata/annotations/build.appstudio.redhat.com~1target_branch"),
            Some(&Value::from("main"))
        );
    }

    #[test]
    fn bound_values_must_name_a_param_of_its_type() {
        let file = PipelineRunFile::parse(MODELEXPRESS_PUSH).expect("parses");
        let src = source(Some("s"));
        let unknown = BTreeMap::from([("nope".to_owned(), ParamValue::String("x".into()))]);
        assert_eq!(
            file.render(&src, &unknown, &labels()),
            Err(DispatchError::UnknownParam("nope".into()))
        );
        let wrong = BTreeMap::from([("hermetic".to_owned(), ParamValue::List(vec![]))]);
        assert!(matches!(
            file.render(&src, &wrong, &labels()),
            Err(DispatchError::WrongType { .. })
        ));
    }

    #[test]
    fn only_v1_pipelineruns_parse() {
        let task = "apiVersion: tekton.dev/v1\nkind: Task\nmetadata: {name: t}\n";
        assert!(matches!(
            PipelineRunFile::parse(task),
            Err(DispatchError::NotAPipelineRun(_))
        ));
        let beta = "apiVersion: tekton.dev/v1beta1\nkind: PipelineRun\nmetadata: {name: t}\n";
        assert!(matches!(
            PipelineRunFile::parse(beta),
            Err(DispatchError::NotAPipelineRun(_))
        ));
        assert!(matches!(
            PipelineRunFile::parse(": : :"),
            Err(DispatchError::Yaml(_))
        ));
        assert_eq!(
            PipelineRunFile::parse(&"#".repeat(300 * 1024)),
            Err(DispatchError::TooLarge)
        );
    }

    #[test]
    fn placeholders_allow_spaces_and_leave_other_braces_alone() {
        let file = PipelineRunFile::parse(
            "apiVersion: tekton.dev/v1\nkind: PipelineRun\nmetadata: {name: x}\nspec:\n  params:\n  - {name: a, value: 'pre-{{revision}}-{{ repo_name }}-${HOME}-{{}}'}\n",
        )
        .expect("parses");
        let run = file
            .render(&source(None), &BTreeMap::new(), &BTreeMap::new())
            .expect("renders");
        assert_eq!(
            run.pointer("/spec/params/0/value"),
            Some(&Value::from(
                "pre-8793cb162115e87e0eab5ba1b7e8e65efb4b1096-modelexpress-${HOME}-{{}}"
            ))
        );
    }
}
