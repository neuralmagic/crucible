//! The VS Code grammar highlights exactly the names the engine installs: every DSL constructor as
//! DSL, every starlark builtin as a builtin, and nothing either list has dropped.

use std::collections::BTreeSet;
use std::path::Path;

use crucible_lsp::docs::Reference;
use serde_json::Value;

fn grammar() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../editors/vscode/syntaxes/crucible-starlark.tmLanguage.json");
    let text = std::fs::read_to_string(&path).expect("grammar");
    serde_json::from_str(&text).expect("grammar is JSON")
}

/// The names in a `\b(a|b|c)(?=...)` rule.
fn names(rule: &str) -> BTreeSet<String> {
    let grammar = grammar();
    let pattern = grammar["repository"][rule]["match"]
        .as_str()
        .expect("match");
    let inner = pattern
        .strip_prefix("\\b(")
        .and_then(|rest| rest.split_once(')'))
        .map(|(names, _)| names)
        .expect("alternation");
    inner.split('|').map(str::to_owned).collect()
}

fn assert_same(rule: &str, expected: BTreeSet<String>) {
    let found = names(rule);
    assert_eq!(
        found,
        expected,
        "repository.{rule} should match \\\\b({})(?=\\\\s*\\\\()",
        expected.iter().cloned().collect::<Vec<_>>().join("|")
    );
}

#[test]
fn every_constructor_is_highlighted_as_dsl() {
    let expected = Reference::new()
        .functions
        .iter()
        .map(|f| f.name.to_owned())
        .collect();
    assert_same("dsl-call", expected);
}

#[test]
fn every_builtin_is_highlighted_as_a_builtin() {
    let expected = Reference::new()
        .builtin_names()
        .map(str::to_owned)
        .collect();
    assert_same("builtin-call", expected);
}

#[test]
fn every_included_rule_exists() {
    let grammar = grammar();
    let mut includes = Vec::new();
    fn walk(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                if let Some(Value::String(include)) = map.get("include") {
                    out.push(include.clone());
                }
                map.values().for_each(|v| walk(v, out));
            }
            Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    walk(&grammar, &mut includes);
    assert!(!includes.is_empty());
    for include in includes {
        if let Some(rule) = include.strip_prefix('#') {
            assert!(
                grammar["repository"].get(rule).is_some(),
                "missing rule {rule}"
            );
        } else {
            assert_eq!(include, "$self");
        }
    }
}
