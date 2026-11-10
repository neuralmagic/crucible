//! Two runs of a playbook compared task by task: what each task settled as, the fields its
//! output changed, and for a fan-out which instances came, went or changed.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::session::{Instance, TaskView};

/// The most field changes one output reports.
const FIELDS: usize = 200;

/// Whether a task or instance is in one run, the other, or both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    Added,
    Removed,
    Both,
}

/// One value that differs, by its path from the output's top (`.verdict.reachability`).
#[derive(Clone, Debug, PartialEq)]
pub struct FieldChange {
    pub path: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InstanceDiff {
    pub key: String,
    pub presence: Presence,
    /// What it settled as before and after, when that differs.
    pub state: Option<(String, String)>,
    pub fields: Vec<FieldChange>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskDiff {
    pub name: String,
    pub presence: Presence,
    /// What it settled as before and after, when that differs.
    pub state: Option<(String, String)>,
    pub fields: Vec<FieldChange>,
    /// Instances in either run that differ.
    pub instances: Vec<InstanceDiff>,
}

impl TaskDiff {
    pub fn is_same(&self) -> bool {
        self.presence == Presence::Both
            && self.state.is_none()
            && self.fields.is_empty()
            && self.instances.is_empty()
    }
}

/// `after` against `before`: its tasks in its own order, then those only `before` had.
pub fn diff(before: &[TaskView], after: &[TaskView]) -> Vec<TaskDiff> {
    let find = |tasks: &'_ [TaskView], name: &str| tasks.iter().find(|t| t.name == name).cloned();
    let mut out: Vec<TaskDiff> = after
        .iter()
        .map(|task| task_diff(find(before, &task.name).as_ref(), Some(task)))
        .collect();
    out.extend(
        before
            .iter()
            .filter(|task| find(after, &task.name).is_none())
            .map(|task| task_diff(Some(task), None)),
    );
    out
}

fn task_diff(before: Option<&TaskView>, after: Option<&TaskView>) -> TaskDiff {
    let output = |task: Option<&TaskView>| {
        task.and_then(|t| t.result.as_ref())
            .and_then(|r| r.output.clone())
    };
    let state = |task: &TaskView| task.state().label().to_string();
    let keys: BTreeSet<&str> = before
        .iter()
        .chain(after.iter())
        .flat_map(|t| t.instances.iter().map(|i| i.key.as_str()))
        .collect();
    let fanned = !keys.is_empty();
    let instance = |task: Option<&TaskView>, key: &str| -> Option<Instance> {
        task.and_then(|t| t.instances.iter().find(|i| i.key == key).cloned())
    };
    let instances = keys
        .into_iter()
        .map(|key| instance_diff(key, instance(before, key), instance(after, key)))
        .filter(|d| d.presence != Presence::Both || d.state.is_some() || !d.fields.is_empty())
        .collect();
    TaskDiff {
        name: after.or(before).map(|t| t.name.clone()).unwrap_or_default(),
        presence: presence(before.is_some(), after.is_some()),
        state: before
            .zip(after)
            .map(|(b, a)| (state(b), state(a)))
            .filter(|(b, a)| b != a),
        fields: fields(output(before).as_ref(), output(after).as_ref())
            .into_iter()
            .filter(|field| !(fanned && field.path.starts_with(".outputs.")))
            .collect(),
        instances,
    }
}

fn instance_diff(key: &str, before: Option<Instance>, after: Option<Instance>) -> InstanceDiff {
    let output = |i: &Option<Instance>| {
        i.as_ref()
            .and_then(|i| i.result.as_ref())
            .and_then(|r| r.output.clone())
    };
    let state = |i: &Instance| i.state().label().to_string();
    InstanceDiff {
        key: key.to_string(),
        presence: presence(before.is_some(), after.is_some()),
        state: before
            .as_ref()
            .zip(after.as_ref())
            .map(|(b, a)| (state(b), state(a)))
            .filter(|(b, a)| b != a),
        fields: fields(output(&before).as_ref(), output(&after).as_ref()),
    }
}

fn presence(before: bool, after: bool) -> Presence {
    match (before, after) {
        (true, false) => Presence::Removed,
        (false, true) => Presence::Added,
        _ => Presence::Both,
    }
}

/// The values that differ between two outputs: objects compared key by key, anything else whole.
pub fn fields(before: Option<&Value>, after: Option<&Value>) -> Vec<FieldChange> {
    let mut out = Vec::new();
    walk("", before, after, &mut out);
    out
}

fn walk(path: &str, before: Option<&Value>, after: Option<&Value>, out: &mut Vec<FieldChange>) {
    if out.len() >= FIELDS || before == after {
        return;
    }
    if let (Some(Value::Object(b)), Some(Value::Object(a))) = (before, after) {
        let keys: BTreeSet<&String> = b.keys().chain(a.keys()).collect();
        for key in keys {
            walk(&format!("{path}.{key}"), b.get(key), a.get(key), out);
        }
        return;
    }
    out.push(FieldChange {
        path: if path.is_empty() {
            ".".to_string()
        } else {
            path.to_string()
        },
        before: before.cloned(),
        after: after.cloned(),
    });
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::diff::{FieldChange, Presence, diff, fields};
    use crate::session::{RunFold, TaskView};

    fn fold(lines: &[&str]) -> Vec<TaskView> {
        let mut fold = RunFold::default();
        for line in lines {
            fold.apply_line(line);
        }
        fold.tasks
    }

    const PLAN: &str = r#"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":[{"name":"scan","kind":"command","required":true},{"name":"analyze","kind":"agent","depends_on":["scan"],"required":true,"over":"scan.cves"},{"name":"report","kind":"command","depends_on":["analyze"],"required":true}]}"#;

    fn result(task: &str, status: &str, output: &str) -> String {
        format!(
            r#"{{"v":1,"kind":"task_result","task":"{task}","status":"{status}","attempts":1,"cost_usd":0.0,"secs":1.0,"note":"","output":{output}}}"#
        )
    }

    #[test]
    fn outputs_differ_field_by_field() {
        let before = json!({"verdict": {"reach": "yes", "why": "x"}, "n": 1, "list": [1, 2]});
        let after = json!({"verdict": {"reach": "no", "why": "x"}, "list": [1, 2, 3], "new": true});
        let change =
            |path: &str, b: Option<serde_json::Value>, a: Option<serde_json::Value>| FieldChange {
                path: path.into(),
                before: b,
                after: a,
            };
        assert_eq!(
            fields(Some(&before), Some(&after)),
            [
                change(".list", Some(json!([1, 2])), Some(json!([1, 2, 3]))),
                change(".n", Some(json!(1)), None),
                change(".new", None, Some(json!(true))),
                change(".verdict.reach", Some(json!("yes")), Some(json!("no"))),
            ]
        );
        assert!(fields(Some(&before), Some(&before)).is_empty());
        assert_eq!(
            fields(None, Some(&json!(3))),
            [change(".", None, Some(json!(3)))]
        );
    }

    #[test]
    fn a_fanout_reports_instances_that_came_went_or_changed() {
        let before = fold(&[
            PLAN,
            &result("scan", "pass", r#"{"cves":["a","b"]}"#),
            &result("analyze[a]", "pass", r#"{"reach":"yes"}"#),
            &result("analyze[b]", "pass", r#"{"reach":"no"}"#),
            &result("report", "pass", r#"{"sent":2}"#),
        ]);
        let after = fold(&[
            PLAN,
            &result("scan", "pass", r#"{"cves":["a","c"]}"#),
            &result("analyze[a]", "fail", r#"{"reach":"no"}"#),
            &result("analyze[c]", "pass", r#"{"reach":"yes"}"#),
            &result("report", "pass", r#"{"sent":2}"#),
        ]);
        let diffs = diff(&before, &after);
        assert_eq!(diffs.len(), 3);
        assert_eq!(diffs[0].fields[0].path, ".cves");
        let analyze = &diffs[1];
        assert_eq!(
            analyze.state, None,
            "neither run recorded the fan-out's own result"
        );
        let seen: Vec<(&str, Presence)> = analyze
            .instances
            .iter()
            .map(|i| (i.key.as_str(), i.presence))
            .collect();
        assert_eq!(
            seen,
            [
                ("a", Presence::Both),
                ("b", Presence::Removed),
                ("c", Presence::Added)
            ]
        );
        assert_eq!(
            analyze.instances[0].state,
            Some(("pass".into(), "fail".into()))
        );
        assert_eq!(analyze.instances[0].fields[0].path, ".reach");
        assert!(diffs[2].is_same());

        let tallied = |outputs: &str| {
            fold(&[
                PLAN,
                &result("analyze[a]", "pass", r#"{"reach":"yes"}"#),
                &result(
                    "analyze",
                    "pass",
                    &format!(r#"{{"instances":1,"outputs":{outputs}}}"#),
                ),
            ])
        };
        let tally = diff(
            &tallied(r#"{"a":{"reach":"no"}}"#),
            &tallied(r#"{"a":{"reach":"yes"},"b":{}}"#),
        );
        assert!(
            tally[1].fields.is_empty(),
            "a fan-out's tally repeats its instances: {:?}",
            tally[1].fields
        );
    }

    #[test]
    fn tasks_only_one_run_had_show_as_added_or_removed() {
        let plan = |tasks: &str| {
            format!(
                r#"{{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":[{tasks}]}}"#
            )
        };
        let before = fold(&[&plan(
            r#"{"name":"old","kind":"command"},{"name":"kept","kind":"command"}"#,
        )]);
        let after = fold(&[&plan(
            r#"{"name":"kept","kind":"command"},{"name":"new","kind":"command"}"#,
        )]);
        let seen: Vec<(String, Presence, bool)> = diff(&before, &after)
            .into_iter()
            .map(|d| (d.name.clone(), d.presence, d.is_same()))
            .collect();
        assert_eq!(
            seen,
            [
                ("kept".into(), Presence::Both, true),
                ("new".into(), Presence::Added, false),
                ("old".into(), Presence::Removed, false),
            ]
        );
    }
}
