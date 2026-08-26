//! A launcher outside the crate reads a pack's declarations and binds supplied text through the
//! engine's own binder. Being an integration test is the point: it links `crucible` the way the
//! controller does, so anything it touches has to be public.

use std::collections::BTreeMap;
use std::path::Path;

use crucible::plan::starlark::params::{ParamType, ParamValue, Params};
use crucible::plan::starlark::{declared_params, read_params};

const SOURCE: &str = r#"
params = {
    "repo": {
        "type": "string",
        "required": True,
        "doc": "owner/name",
        "pattern": "^[A-Za-z0-9-]+/[A-Za-z0-9._-]+$",
    },
    "limit": {"type": "int", "default": 10, "min": 1, "max": 100},
    "threshold": {"type": "number", "default": 0.5, "min": 0.0, "max": 1.0},
    "dry_run": {"type": "bool", "default": False},
    "labels": {"type": "list<string>", "default": ["bug"]},
    "tier": {"type": "string", "default": "cheap", "choices": ["cheap", "rich"]},
}
"#;

fn params() -> Params {
    read_params(SOURCE, Path::new("workflow.star")).expect("read declarations")
}

fn supplied(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[test]
fn declarations_carry_their_kind_and_constraints() {
    let params = params();
    let specs = params.specs();
    assert_eq!(specs.len(), 6);

    let by_name = |name: &str| {
        specs
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("{name} declared"))
    };
    let repo = by_name("repo");
    assert_eq!(repo.ty, ParamType::String);
    assert!(repo.required);
    assert!(repo.default.is_none());
    assert_eq!(repo.doc, "owner/name");
    assert_eq!(
        repo.pattern.as_deref(),
        Some("^[A-Za-z0-9-]+/[A-Za-z0-9._-]+$")
    );

    let limit = by_name("limit");
    assert_eq!(limit.ty, ParamType::Int);
    assert!(!limit.required);
    assert_eq!(limit.default, Some(ParamValue::Int(10)));
    assert_eq!((limit.min, limit.max), (Some(1.0), Some(100.0)));

    assert_eq!(by_name("threshold").ty, ParamType::Number);
    assert_eq!(by_name("dry_run").ty, ParamType::Bool);
    assert_eq!(by_name("labels").ty, ParamType::StringList);
    assert_eq!(by_name("tier").choices, vec!["cheap", "rich"]);
    assert_eq!(ParamType::StringList.as_str(), "list<string>");
    assert_eq!(ParamType::parse("int"), Some(ParamType::Int));
    assert_eq!(ParamType::parse("integer"), None);
}

/// The bug this unblocks: a launcher that validates every supplied value as a string cannot
/// launch a pack declaring an integer, number, boolean, or list. The binder parses the text.
#[test]
fn non_string_kinds_bind_from_text() {
    let bound = params()
        .bind(&supplied(&[
            ("repo", "neuralmagic/crucible"),
            ("limit", "42"),
            ("threshold", "0.75"),
            ("dry_run", "true"),
            ("labels", "bug,perf"),
        ]))
        .expect("bind");

    assert_eq!(
        bound["repo"],
        ParamValue::String("neuralmagic/crucible".into())
    );
    assert_eq!(bound["limit"], ParamValue::Int(42));
    assert_eq!(bound["threshold"], ParamValue::Number(0.75));
    assert_eq!(bound["dry_run"], ParamValue::Bool(true));
    assert_eq!(
        bound["labels"],
        ParamValue::StringList(vec!["bug".into(), "perf".into()])
    );
    assert_eq!(bound["tier"], ParamValue::String("cheap".into()));
    assert_eq!(bound["limit"].json(), serde_json::json!(42));
    assert_eq!(bound["labels"].json(), serde_json::json!(["bug", "perf"]));
}

#[test]
fn a_json_array_binds_when_items_carry_commas() {
    let bound = params()
        .bind(&supplied(&[
            ("repo", "neuralmagic/crucible"),
            ("labels", r#"["a,b","c"]"#),
        ]))
        .expect("bind");
    assert_eq!(
        bound["labels"],
        ParamValue::StringList(vec!["a,b".into(), "c".into()])
    );
}

#[test]
fn defaults_fill_what_was_not_supplied() {
    let bound = params()
        .bind(&supplied(&[("repo", "neuralmagic/crucible")]))
        .expect("bind");
    assert_eq!(bound["limit"], ParamValue::Int(10));
    assert_eq!(bound["threshold"], ParamValue::Number(0.5));
    assert_eq!(bound["dry_run"], ParamValue::Bool(false));
    assert_eq!(bound["labels"], ParamValue::StringList(vec!["bug".into()]));
}

#[test]
fn the_binder_refuses_what_the_declaration_forbids() {
    let refused = |pairs: &[(&str, &str)]| {
        let mut with_repo = vec![("repo", "neuralmagic/crucible")];
        with_repo.extend_from_slice(pairs);
        params()
            .bind(&supplied(&with_repo))
            .expect_err("refused")
            .to_string()
    };

    assert!(refused(&[("limit", "many")]).contains("limit"));
    assert!(refused(&[("limit", "0")]).contains("between 1 and 100"));
    assert!(refused(&[("limit", "1000")]).contains("between 1 and 100"));
    assert!(refused(&[("threshold", "2.0")]).contains("threshold"));
    assert!(refused(&[("dry_run", "yes")]).contains("true or false"));
    assert!(refused(&[("tier", "lavish")]).contains("cheap, rich"));
    assert!(refused(&[("limitt", "5")]).contains("limit"));

    let missing = params()
        .bind(&supplied(&[("limit", "5")]))
        .expect_err("required parameter missing")
        .to_string();
    assert!(missing.contains("repo"), "{missing}");

    let bad_repo = refused(&[("repo", "not a repo")]);
    assert!(bad_repo.contains("repo"), "{bad_repo}");
}

/// The schema and the binder read one declaration, so a form cannot drift from what will bind.
#[test]
fn the_schema_is_the_same_declaration() {
    let schema = declared_params(SOURCE, Path::new("workflow.star")).expect("schema");
    assert_eq!(schema, params().json_schema());
    assert_eq!(schema["properties"]["limit"]["type"], "integer");
    assert_eq!(schema["properties"]["labels"]["type"], "array");
    assert_eq!(schema["properties"]["dry_run"]["type"], "boolean");
    assert_eq!(schema["required"], serde_json::json!(["repo"]));
}

#[test]
fn a_source_declaring_nothing_binds_nothing() {
    let params = read_params("workflow(name = \"x\")\n", Path::new("workflow.star"))
        .expect("read declarations");
    assert!(params.is_empty());
    assert!(params.specs().is_empty());
    assert!(params.bind(&BTreeMap::new()).expect("bind").is_empty());
    assert!(
        params
            .bind(&supplied(&[("repo", "x/y")]))
            .expect_err("nothing declared, so nothing may be supplied")
            .to_string()
            .contains("repo")
    );
}
