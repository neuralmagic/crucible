//! A launcher outside the crate reads a pack's declarations and binds supplied text through the
//! engine's own binder. Being an integration test is the point: it links `crucible` the way the
//! controller does, so anything it touches has to be public.

use std::collections::BTreeMap;
use std::path::Path;

use crucible::plan::param::ParamValue;
use crucible::plan::starlark::params::{ParamType, Params};
use crucible::plan::starlark::{compile_source_with, declared_params, read_params};

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
fn a_multiline_string_is_marked_in_the_schema_and_binds_as_typed() {
    let source = r#"
params = {
    "log": {"type": "string", "required": True, "multiline": True},
    "title": {"type": "string", "default": "t", "multiline": False},
}
"#;
    let params = read_params(source, Path::new("workflow.star")).expect("read declarations");
    let multiline: Vec<(&str, bool)> = params
        .specs()
        .iter()
        .map(|s| (s.name.as_str(), s.multiline))
        .collect();
    assert_eq!(multiline, vec![("log", true), ("title", false)]);

    let schema = params.json_schema();
    assert_eq!(schema["properties"]["log"]["type"], "string");
    assert_eq!(schema["properties"]["log"]["x-multiline"], true);
    assert!(schema["properties"]["title"].get("x-multiline").is_none());

    let log = "error: boom\n  at step 3\n";
    let bound = params
        .bind(&supplied(&[("log", log)]))
        .expect("bind a value with newlines");
    assert_eq!(bound["log"], ParamValue::String(log.into()));

    let stored = serde_json::to_string(&params).expect("serialize");
    let reloaded: Params = serde_json::from_str(&stored).expect("deserialize");
    assert_eq!(reloaded, params);
}

/// A declaration stored before `multiline` existed reloads as a single-line string.
#[test]
fn a_declaration_stored_without_multiline_reloads_as_single_line() {
    let stored = r#"[{"name": "topic", "ty": "string", "required": false,
        "default": "x", "doc": "", "pattern": null, "min": null, "max": null, "choices": []}]"#;
    let reloaded: Params = serde_json::from_str(stored).expect("deserialize");
    assert!(!reloaded.specs()[0].multiline);
}

/// A launcher stores what it read and binds against it later, with no engine in between. This is
/// what lets a registration keep the declarations instead of a JSON Schema it would have to
/// re-interpret.
#[test]
fn declarations_survive_being_stored_and_reloaded() {
    let stored = serde_json::to_string(&params()).expect("serialize");
    let reloaded: Params = serde_json::from_str(&stored).expect("deserialize");
    assert_eq!(reloaded, params());

    let bound = reloaded
        .bind(&supplied(&[
            ("repo", "neuralmagic/crucible"),
            ("limit", "7"),
            ("dry_run", "true"),
        ]))
        .expect("bind through the reloaded declarations");
    assert_eq!(bound["limit"], ParamValue::Int(7));
    assert_eq!(bound["dry_run"], ParamValue::Bool(true));
    assert_eq!(bound["threshold"], ParamValue::Number(0.5));
    assert_eq!(
        reloaded
            .bind(&supplied(&[
                ("repo", "neuralmagic/crucible"),
                ("limit", "0")
            ]))
            .expect_err("constraints survive too")
            .to_string(),
        params()
            .bind(&supplied(&[
                ("repo", "neuralmagic/crucible"),
                ("limit", "0")
            ]))
            .expect_err("constraints")
            .to_string()
    );
}

/// The stored spelling of a kind is the one the declaration uses, so nothing has to translate
/// between a serde name and `list<string>`.
#[test]
fn a_stored_kind_spells_itself_the_way_the_declaration_does() {
    for ty in [
        ParamType::String,
        ParamType::Int,
        ParamType::Number,
        ParamType::Bool,
        ParamType::StringList,
        ParamType::Json,
        ParamType::File,
    ] {
        let stored = serde_json::to_value(ty).expect("serialize");
        assert_eq!(stored, serde_json::Value::String(ty.as_str().to_owned()));
        assert_eq!(ParamType::parse(ty.as_str()), Some(ty));
    }
}

/// An integer default must not come back as a float: the untagged decode order is load-bearing.
#[test]
fn a_stored_value_keeps_its_kind() {
    for value in [
        ParamValue::String("s".into()),
        ParamValue::Int(42),
        ParamValue::Number(0.5),
        ParamValue::Bool(true),
        ParamValue::StringList(vec!["a".into()]),
        ParamValue::Json(serde_json::json!({"os": ["linux"], "n": 2})),
        ParamValue::Json(serde_json::json!([1, {"a": null}])),
    ] {
        let stored = serde_json::to_string(&value).expect("serialize");
        let back: ParamValue = serde_json::from_str(&stored).expect("deserialize");
        assert_eq!(back, value, "{stored}");
        assert_eq!(stored, value.json().to_string());
    }
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

#[test]
fn a_string_list_with_choices_binds_a_subset_and_offers_checkboxes() {
    let source = r#"
params = {
    "workloads": {"type": "list<string>", "default": ["chat", "agentic"], "choices": ["chat", "agentic"]},
}
"#;
    let params = read_params(source, Path::new("workflow.star")).expect("read declarations");
    let schema = params.json_schema();
    assert_eq!(
        schema["properties"]["workloads"]["items"],
        serde_json::json!({"type": "string", "enum": ["chat", "agentic"]})
    );
    assert_eq!(schema["properties"]["workloads"]["uniqueItems"], true);
    assert!(schema["properties"]["workloads"].get("enum").is_none());

    let bound = params
        .bind(&supplied(&[("workloads", r#"["agentic"]"#)]))
        .expect("bind a subset");
    assert_eq!(
        bound["workloads"],
        ParamValue::StringList(vec!["agentic".into()])
    );
    let none = params
        .bind(&supplied(&[("workloads", "[]")]))
        .expect("bind none");
    assert_eq!(none["workloads"], ParamValue::StringList(Vec::new()));

    let refused = |raw: &str| {
        params
            .bind(&supplied(&[("workloads", raw)]))
            .expect_err("refused")
            .to_string()
    };
    assert!(refused("chat,batch").contains("chat, agentic"));
    assert!(refused("chat,chat").contains("once"));
}

#[test]
fn a_string_list_cannot_take_free_text_beside_its_choices() {
    let refused = read_params(
        r#"
params = {
    "workloads": {"type": "list<string>", "default": [], "choices": ["chat"], "allow_other": True},
}
"#,
        Path::new("workflow.star"),
    )
    .expect_err("allow_other on a list")
    .to_string();
    assert!(refused.contains("allow_other"), "{refused}");
}

#[test]
fn json_and_file_parameters_declare_and_describe_themselves() {
    let params = read_params(
        r#"
params = {
    "matrix": {"type": "json", "required": True, "doc": "build matrix"},
    "notes": {"type": "file", "required": True, "max_bytes": 1024},
}
"#,
        Path::new("workflow.star"),
    )
    .expect("read declarations");
    let schema = params.json_schema();
    let matrix = &schema["properties"]["matrix"];
    assert_eq!(matrix["type"], serde_json::json!(["object", "array"]));
    assert_eq!(ParamType::from_schema(matrix), ParamType::Json);
    let notes = &schema["properties"]["notes"];
    assert_eq!(ParamType::from_schema(notes), ParamType::File);

    let bound = params
        .bind(&BTreeMap::from([(
            "matrix".into(),
            r#"{"os": ["linux"]}"#.into(),
        )]))
        .expect_err("a file cannot be typed in");
    assert!(bound.to_string().contains("notes"), "{bound}");
    assert_eq!(
        ParamType::Json.read(r#"{"os": ["linux"]}"#),
        Ok(ParamValue::Json(serde_json::json!({"os": ["linux"]})))
    );
    assert!(ParamType::Json.read("\"text\"").is_err());
    assert!(
        ParamType::Json
            .read(&format!("[\"{}\"]", "x".repeat(64 * 1024)))
            .is_err()
    );
    assert!(ParamType::File.read("notes.md").is_err());

    for refused in [
        r#"params = {"m": {"type": "json", "default": {}}}"#,
        r#"params = {"m": {"type": "json", "required": True, "pattern": "x"}}"#,
        r#"params = {"f": {"type": "file", "required": True, "choices": ["a"]}}"#,
        r#"params = {"f": {"type": "file", "required": True, "multiline": True}}"#,
    ] {
        assert!(
            read_params(refused, Path::new("workflow.star")).is_err(),
            "{refused}"
        );
    }
}

#[test]
fn a_file_parameter_declares_its_limit_and_media_types() {
    for (refused, says) in [
        (
            r#"params = {"f": {"type": "file", "required": True}}"#,
            "max_bytes",
        ),
        (
            r#"params = {"f": {"type": "file", "required": True, "max_bytes": 99999999}}"#,
            "limit",
        ),
        (
            r#"params = {"f": {"type": "string", "default": "", "max_bytes": 10}}"#,
            "max_bytes",
        ),
        (
            r#"params = {"f": {"type": "string", "default": "", "media_type": "text/plain"}}"#,
            "media_type",
        ),
        (
            r#"params = {"f": {"type": "file", "required": True, "max_bytes": 10, "schema": "s.json"}}"#,
            "schema",
        ),
    ] {
        let error = read_params(refused, Path::new("workflow.star"))
            .expect_err(refused)
            .to_string();
        assert!(error.contains(says), "{refused}: {error}");
    }

    let params = read_params(
        r#"params = {"f": {"type": "file", "required": True, "max_bytes": 8, "media_type": ["text/plain"]}}"#,
        Path::new("workflow.star"),
    )
    .expect("read");
    let schema = params.json_schema();
    assert_eq!(
        schema["properties"]["f"]["x-max-bytes"],
        serde_json::json!(8)
    );
    assert_eq!(
        schema["properties"]["f"]["x-media-type"],
        serde_json::json!(["text/plain"])
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let small = dir.path().join("small.txt");
    let large = dir.path().join("large.txt");
    std::fs::write(&small, "hi").expect("write");
    std::fs::write(&large, "too many bytes").expect("write");
    let bind = |raw: String| params.bind(&BTreeMap::from([("f".to_owned(), raw)]));
    let bound = bind(format!(
        r#"{{"path": "{}", "media_type": "text/plain"}}"#,
        small.display()
    ))
    .expect("bind");
    let ParamValue::Json(file) = &bound["f"] else {
        panic!("{bound:?}")
    };
    assert_eq!(file["size"], serde_json::json!(2));
    assert_eq!(file["media_type"], serde_json::json!("text/plain"));
    assert_eq!(
        file["digest"],
        serde_json::json!(crucible_contract::artifact::content_digest(b"hi"))
    );
    for (raw, says) in [
        (
            format!(
                r#"{{"path": "{}", "media_type": "text/plain"}}"#,
                large.display()
            ),
            "at most 8",
        ),
        (small.display().to_string(), "names no media type"),
        (
            format!(
                r#"{{"path": "{}", "media_type": "image/png"}}"#,
                small.display()
            ),
            "must be one of",
        ),
        ("small.txt".to_owned(), "absolute path"),
        (
            format!(
                r#"{{"path": "{}", "media_type": "text/plain", "digest": "sha256:00"}}"#,
                small.display()
            ),
            "not the one supplied",
        ),
    ] {
        let error = bind(raw.clone()).expect_err(&raw).to_string();
        assert!(error.contains(says), "{raw}: {error}");
    }
}

#[test]
fn a_json_parameter_is_held_to_its_schema() {
    let pack = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        pack.path().join("matrix.json"),
        r#"{"type": "object", "required": ["os"], "properties": {"os": {"type": "array"}}}"#,
    )
    .expect("write schema");
    let source = r#"
params = {"matrix": {"type": "json", "required": True, "schema": "matrix.json"}}
t = command(name = "t", run = "true")
workflow(type = "playbook", tasks = [t])
"#;
    let compile = |raw: &str| {
        compile_source_with(
            source,
            Path::new("workflow.star"),
            pack.path(),
            &BTreeMap::from([("matrix".to_owned(), raw.to_owned())]),
        )
    };
    let compiled = compile(r#"{"os": ["linux"]}"#).expect("a matching value compiles");
    assert_eq!(
        compiled.workflow.params["matrix"],
        ParamValue::Json(serde_json::json!({"os": ["linux"]}))
    );
    let refused = compile(r#"{"arch": "arm64"}"#)
        .expect_err("a value outside the schema is refused")
        .message();
    assert!(refused.contains("schema"), "{refused}");

    let declared = read_params(source, Path::new("workflow.star")).expect("read");
    assert_eq!(
        declared.json_schema()["properties"]["matrix"]["x-schema-path"],
        serde_json::json!("matrix.json")
    );
    let missing = compile_source_with(
        source,
        Path::new("workflow.star"),
        tempfile::tempdir().expect("tempdir").path(),
        &BTreeMap::from([("matrix".to_owned(), "{}".to_owned())]),
    )
    .expect_err("a schema the pack does not hold is refused");
    assert!(missing.message().contains("matrix.json"), "{missing}");
}
