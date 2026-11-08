//! What the Data pane shows for a task: the values reaching it from each dependency, its output,
//! and that output checked against the fields and files the pack declares.

use std::collections::HashSet;

use crucible_contract::emits::{EmitWire, JsonSchema, value_type};
use serde_json::Value;

use crate::session::TaskView;

/// JSON nested this deep or less shows open until clicked shut.
const OPEN_DEPTH: usize = 2;

/// What the Data pane is looking at beyond the selected task.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Inspect {
    /// The dependency whose edge was picked in the graph.
    pub from: Option<String>,
    /// The fan-out instance whose output shows, by key.
    pub instance: Option<String>,
    /// JSON paths toggled away from their default open state.
    flipped: HashSet<String>,
}

impl Inspect {
    pub fn is_open(&self, path: &str, depth: usize) -> bool {
        (depth < OPEN_DEPTH) != self.flipped.contains(path)
    }

    pub fn flip(&mut self, path: &str) {
        if !self.flipped.remove(path) {
            self.flipped.insert(path.to_string());
        }
    }
}

/// How a dependency's output reaches a task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Via {
    /// The whole output.
    Whole,
    /// One list field, an instance per element.
    Over(String),
    /// One field, each instance narrowed to its own entry.
    Keyed(String),
}

/// One dependency's contribution to a task's inputs.
#[derive(Clone, Debug, PartialEq)]
pub struct Flow<'a> {
    pub from: &'a str,
    pub via: Via,
    /// `None` until the dependency has settled with an output.
    pub value: Option<&'a Value>,
}

/// What each of `task`'s dependencies hands it, in `depends_on` order.
pub fn flows<'a>(tasks: &'a [TaskView], task: &'a TaskView) -> Vec<Flow<'a>> {
    task.depends_on
        .iter()
        .flat_map(|from| {
            let output = tasks
                .iter()
                .find(|t| t.name == *from)
                .and_then(|t| t.result.as_ref())
                .and_then(|r| r.output.as_ref());
            let field = |reference: &str| {
                reference
                    .split_once('.')
                    .filter(|(producer, _)| producer == from)
                    .map(|(_, field)| field.to_string())
            };
            let mut vias: Vec<Via> = field(&task.over).map(Via::Over).into_iter().collect();
            vias.extend(task.keyed.iter().filter_map(|k| field(k)).map(Via::Keyed));
            if vias.is_empty() {
                vias.push(Via::Whole);
            }
            vias.into_iter().map(move |via| {
                let value = match &via {
                    Via::Whole => output,
                    Via::Over(field) | Via::Keyed(field) => output.and_then(|o| o.get(field)),
                };
                Flow {
                    from: from.as_str(),
                    via,
                    value,
                }
            })
        })
        .collect()
}

/// The output to show: the picked instance's, else the task's own, else its first instance's.
pub fn output<'a>(task: &'a TaskView, instance: Option<&str>) -> Option<&'a Value> {
    let of_instance = |key: &str| {
        task.instances
            .iter()
            .find(|i| i.key == key)
            .and_then(|i| i.result.as_ref())
            .and_then(|r| r.output.as_ref())
    };
    if let Some(found) = instance.and_then(of_instance) {
        return Some(found);
    }
    task.result
        .as_ref()
        .and_then(|r| r.output.as_ref())
        .or_else(|| {
            task.instances
                .iter()
                .find_map(|i| i.result.as_ref().and_then(|r| r.output.as_ref()))
        })
}

/// A declared field against the output that should carry it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// No output yet.
    Waiting,
    Missing,
    /// Present; its declared type admits it, or none was declared.
    Fits,
    /// Present, and its declared type refuses it; carries the JSON type it holds.
    Refused(&'static str),
}

pub fn check(emit: &EmitWire, output: Option<&Value>) -> Check {
    let Some(output) = output else {
        return Check::Waiting;
    };
    match (output.get(&emit.field), &emit.ty) {
        (None, _) => Check::Missing,
        (Some(value), Some(ty)) if !ty.admits(value) => Check::Refused(value_type(value)),
        (Some(_), _) => Check::Fits,
    }
}

/// The schema as a JSON document, for showing as a tree.
pub fn document(schema: &JsonSchema) -> Option<Value> {
    match serde_json::to_value(schema).ok()? {
        Value::String(text) => serde_json::from_str(&text).ok(),
        other => Some(other),
    }
}

#[cfg(test)]
mod tests {
    use crucible_contract::emits::{EmitWire, FieldType, JsonSchema};
    use serde_json::json;

    use crate::inspect::{Check, Flow, Inspect, Via, check, document, flows, output};
    use crate::session::RunFold;

    const RUN: &str = concat!(
        r#"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":["#,
        r#"{"name":"scan","kind":"command","emits":[{"field":"cves","type":"list"},{"field":"repo","type":"string"}]},"#,
        r#"{"name":"fix","kind":"agent","depends_on":["scan"],"over":"scan.cves","keyed":["scan.repo"]},"#,
        r#"{"name":"report","kind":"agent","depends_on":["scan","fix"]}"#,
        r#"]}"#,
        "\n",
        r#"{"v":1,"kind":"task_result","task":"scan","status":"pass","attempts":1,"cost_usd":0.0,"secs":1.0,"note":"","output":{"cves":["a","b"],"repo":7}}"#,
        "\n",
        r#"{"v":1,"kind":"task_result","task":"fix[a]","status":"pass","attempts":1,"cost_usd":0.0,"secs":1.0,"note":"","output":{"patched":"a"}}"#,
        "\n",
    );

    fn run() -> RunFold {
        let mut fold = RunFold::default();
        for line in RUN.lines() {
            fold.apply_line(line);
        }
        fold
    }

    #[test]
    fn a_fanout_reads_its_list_field_and_its_keyed_field() {
        let fold = run();
        assert_eq!(fold.tasks.len(), 3, "{fold:?}");
        let (cves, repo) = (json!(["a", "b"]), json!(7));
        assert_eq!(
            flows(&fold.tasks, &fold.tasks[1]),
            [
                Flow {
                    from: "scan",
                    via: Via::Over("cves".into()),
                    value: Some(&cves)
                },
                Flow {
                    from: "scan",
                    via: Via::Keyed("repo".into()),
                    value: Some(&repo)
                },
            ]
        );
    }

    #[test]
    fn a_plain_dependency_hands_over_its_whole_output_once_it_has_one() {
        let fold = run();
        let flows = flows(&fold.tasks, &fold.tasks[2]);
        assert_eq!(flows.len(), 2);
        assert_eq!(flows[0].via, Via::Whole);
        assert_eq!(
            flows[0].value,
            Some(&json!({"cves": ["a", "b"], "repo": 7}))
        );
        assert_eq!((flows[1].from, flows[1].value), ("fix", None));
    }

    #[test]
    fn a_fanout_shows_the_picked_instance_or_falls_back_to_one_that_finished() {
        let fold = run();
        let fix = &fold.tasks[1];
        let patched = json!({"patched": "a"});
        assert_eq!(output(fix, Some("a")), Some(&patched));
        assert_eq!(output(fix, Some("zz")), Some(&patched));
        assert_eq!(output(fix, None), Some(&patched));
        assert_eq!(output(&fold.tasks[2], None), None);
    }

    #[test]
    fn declared_fields_check_against_the_output() {
        let fold = run();
        let scan = &fold.tasks[0];
        let out = output(scan, None);
        assert_eq!(check(&scan.emits[0], out), Check::Fits);
        assert_eq!(check(&scan.emits[1], out), Check::Refused("integer"));
        let absent = EmitWire {
            field: "nope".into(),
            ty: Some(FieldType::String),
        };
        assert_eq!(check(&absent, out), Check::Missing);
        assert_eq!(check(&absent, None), Check::Waiting);
        let untyped = EmitWire {
            field: "repo".into(),
            ty: None,
        };
        assert_eq!(check(&untyped, out), Check::Fits);
    }

    #[test]
    fn a_schema_reads_back_as_its_document() {
        let doc = json!({"type": "object", "properties": {"n": {"type": "integer"}}});
        let schema = JsonSchema::new(doc.clone()).unwrap();
        assert_eq!(document(&schema), Some(doc));
    }

    #[test]
    fn json_opens_shallow_and_each_click_flips_one_path() {
        let mut inspect = Inspect::default();
        assert!(inspect.is_open("a", 0) && inspect.is_open("a/b", 1));
        assert!(!inspect.is_open("a/b/c", 2));
        inspect.flip("a/b/c");
        inspect.flip("a");
        assert!(inspect.is_open("a/b/c", 2) && !inspect.is_open("a", 0));
        inspect.flip("a");
        assert!(inspect.is_open("a", 0));
    }
}
