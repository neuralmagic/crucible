//! Reading a pack's `calls.lock` and checking it against the call targets its workflow sources
//! declare.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crucible_contract::calls::{CallAlias, CallTarget, CallsLock, LOCK_FILE, Pin, TargetInterface};

fn workflow_files(manifest: &str) -> Vec<String> {
    let Ok(table) = manifest.parse::<toml::Table>() else {
        return Vec::new();
    };
    let file = |workflow: Option<&toml::Value>| {
        workflow
            .and_then(|w| w.get("file"))
            .and_then(toml::Value::as_str)
            .filter(|f| !f.is_empty())
            .map(str::to_string)
    };
    let mut files: Vec<String> = file(table.get("workflow")).into_iter().collect();
    if let Some(variants) = table.get("variant").and_then(toml::Value::as_table) {
        files.extend(variants.values().filter_map(|v| file(v.get("workflow"))));
    }
    files.sort();
    files.dedup();
    files
}

pub(crate) fn declared(
    file: impl Fn(&str) -> Option<String>,
) -> Result<BTreeMap<CallAlias, CallTarget>, String> {
    let Some(manifest) = file(crate::playbooks::packs::MANIFEST) else {
        return Ok(BTreeMap::new());
    };
    let mut calls: BTreeMap<CallAlias, CallTarget> = BTreeMap::new();
    for path in workflow_files(&manifest) {
        let Some(source) = file(&path) else {
            continue;
        };
        let found = crucible::plan::starlark::declared_calls(&source, Path::new(&path))
            .map_err(|e| format!("{path}: {e}"))?;
        for (alias, target) in found {
            match calls.get(&alias) {
                Some(seen) if seen != &target => {
                    return Err(format!(
                        "{path}: target {alias} differs from another workflow source's"
                    ));
                }
                _ => {
                    calls.insert(alias, target);
                }
            }
        }
    }
    Ok(calls)
}

pub(crate) fn of_tree(
    tree: &crucible_contract::pack_tree::PackTree,
) -> Result<(BTreeMap<CallAlias, CallTarget>, Option<CallsLock>), String> {
    let file = |name: &str| {
        tree.files()
            .iter()
            .find(|(path, _)| path.as_str() == name)
            .map(|(_, bytes)| String::from_utf8_lossy(bytes).into_owned())
    };
    let calls = declared(file)?;
    let lock = match file(LOCK_FILE) {
        Some(text) => {
            let lock = CallsLock::parse(&text).map_err(|e| e.to_string())?;
            lock.check(&calls).map_err(|e| e.to_string())?;
            Some(lock)
        }
        None if calls.is_empty() => None,
        None => {
            return Err(crucible_contract::calls::LockError::Missing.to_string());
        }
    };
    Ok((calls, lock))
}

pub(crate) fn registrable(tree: &crucible_contract::pack_tree::PackTree) -> Result<(), String> {
    let (_, lock) = of_tree(tree)?;
    let draft_head = lock
        .iter()
        .flat_map(|l| l.calls.iter())
        .find(|(_, entry)| matches!(entry.pin, Pin::DraftHead { .. }));
    match draft_head {
        Some((alias, _)) => Err(format!(
            "{LOCK_FILE} pins call target {alias} to a draft's head, which only a draft pack may \
             do; register the target and lock it by its registered name"
        )),
        None => Ok(()),
    }
}

fn names(schema: &serde_json::Value, key: &str) -> BTreeSet<String> {
    match schema.get(key) {
        Some(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|i| i.as_str().map(str::to_string))
            .collect(),
        _ => BTreeSet::new(),
    }
}

fn delta(what: &str, old: &BTreeSet<String>, new: &BTreeSet<String>) -> Option<String> {
    let added: Vec<&String> = new.difference(old).collect();
    let removed: Vec<&String> = old.difference(new).collect();
    let mut parts = Vec::new();
    if !added.is_empty() {
        parts.push(format!("added {added:?}"));
    }
    if !removed.is_empty() {
        parts.push(format!("removed {removed:?}"));
    }
    (!parts.is_empty()).then(|| format!("{what} {}", parts.join(", ")))
}

pub(crate) fn drift(old: &TargetInterface, new: &TargetInterface) -> String {
    let params = |i: &TargetInterface| names(&i.params, "properties");
    let required = |i: &TargetInterface| names(&i.params, "required");
    let fields = |i: &TargetInterface| -> BTreeSet<String> {
        i.result.iter().map(|f| f.field.clone()).collect()
    };
    let files = |i: &TargetInterface| -> BTreeSet<String> {
        i.result_files.iter().map(|f| f.path.clone()).collect()
    };
    let tasks = |i: &TargetInterface| -> BTreeSet<String> {
        i.graph.nodes.iter().map(|n| n.id.clone()).collect()
    };
    let parts: Vec<String> = [
        delta("params", &params(old), &params(new)),
        delta("required params", &required(old), &required(new)),
        delta("result fields", &fields(old), &fields(new)),
        delta("result files", &files(old), &files(new)),
        delta("graph nodes", &tasks(old), &tasks(new)),
    ]
    .into_iter()
    .flatten()
    .collect();
    match parts.is_empty() {
        true => "a parameter's, field's, or task's type or settings changed".to_string(),
        false => parts.join("; "),
    }
}

pub(crate) fn check_bound<'a>(
    interface: &TargetInterface,
    bound: impl Iterator<Item = &'a String>,
) -> Result<(), String> {
    let declared = names(&interface.params, "properties");
    let mut given = BTreeSet::new();
    for name in bound {
        if !declared.contains(name) {
            return Err(format!("the target takes no parameter {name:?}"));
        }
        given.insert(name.clone());
    }
    if let Some(missing) = names(&interface.params, "required")
        .into_iter()
        .find(|r| !given.contains(r))
    {
        return Err(format!("required parameter {missing:?} is unbound"));
    }
    Ok(())
}

pub(crate) fn check_values(
    kind: crucible_contract::calls::CallKind,
    interface: &TargetInterface,
    values: &BTreeMap<String, serde_json::Value>,
    files: &BTreeMap<String, crucible_contract::calls::FileRef>,
) -> Result<(), String> {
    use crucible::plan::starlark::params::{MAX_BYTES_KEY, MEDIA_TYPE_KEY, ParamType};
    use serde_json::Value;
    let property = |name: &str| {
        interface
            .params
            .get("properties")
            .and_then(|p| p.get(name))
            .cloned()
            .unwrap_or(Value::Null)
    };
    for (name, value) in values {
        let ty = ParamType::from_schema(&property(name));
        let fits = match ty {
            ParamType::String => {
                value.is_string()
                    || (kind == crucible_contract::calls::CallKind::GithubWorkflow
                        && (value.is_object() || value.is_array()))
            }
            ParamType::Int => value.as_i64().is_some_and(|n| i32::try_from(n).is_ok()),
            ParamType::Number => value.as_f64().is_some_and(f64::is_finite),
            ParamType::Bool => value.is_boolean(),
            ParamType::StringList => value
                .as_array()
                .is_some_and(|items| items.iter().all(Value::is_string)),
            ParamType::Json => {
                (value.is_object() || value.is_array())
                    && serde_json::to_vec(value)
                        .is_ok_and(|b| b.len() <= crucible_contract::calls::MAX_JSON_PARAM_BYTES)
            }
            ParamType::File => {
                return Err(format!(
                    "parameter {name:?} takes a file, but the call binds a value"
                ));
            }
        };
        if !fits {
            return Err(format!(
                "parameter {name:?} takes {}, but the call binds {value}",
                ty.as_str()
            ));
        }
    }
    for (name, file) in files {
        let declared = property(name);
        if ParamType::from_schema(&declared) != ParamType::File {
            return Err(format!(
                "parameter {name:?} is not a file parameter, but the call binds a file"
            ));
        }
        let max = declared
            .get(MAX_BYTES_KEY)
            .and_then(Value::as_u64)
            .unwrap_or(crucible_contract::calls::MAX_FILE_PARAM_BYTES)
            .min(crucible_contract::calls::MAX_FILE_PARAM_BYTES);
        if file.size > max {
            return Err(format!(
                "file parameter {name:?} takes at most {max} bytes, but the call binds {}",
                file.size
            ));
        }
        let media: Vec<&str> = declared
            .get(MEDIA_TYPE_KEY)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if !media.is_empty()
            && !file
                .media_type
                .as_deref()
                .is_some_and(|m| media.contains(&m))
        {
            return Err(format!(
                "file parameter {name:?} takes {}, but the call binds {}",
                media.join(", "),
                file.media_type
                    .as_deref()
                    .unwrap_or("a file of no media type")
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::calls::lock::*;
    use crucible_contract::calls::{
        CallKind, LOCK_VERSION, LockError, LockedCall, NodeStatus, PlaybookRef, ProgressGraph,
        ProgressNode,
    };
    use crucible_contract::emits::{DeclaredFile, EmitWire};
    use crucible_contract::pack_tree::PackTree;
    use serde_json::json;

    fn interface(params: serde_json::Value, fields: &[&str]) -> TargetInterface {
        TargetInterface {
            params,
            result: fields
                .iter()
                .map(|f| EmitWire {
                    field: f.to_string(),
                    ty: None,
                })
                .collect(),
            result_files: vec![DeclaredFile::from("report.md")],
            graph: ProgressGraph {
                nodes: vec![ProgressNode {
                    id: "scan".into(),
                    label: "scan".into(),
                    status: NodeStatus::Pending,
                    group: None,
                    started: None,
                    ended: None,
                    detail: None,
                    link: None,
                }],
                edges: vec![],
                omitted: 0,
            },
        }
    }

    const MANIFEST: &str = r#"
[workflow]
type = "playbook"
file = "workflow.star"

[variant.agent.workflow]
type = "playbook"
file = "workflow-agent.star"
"#;

    const WORKFLOW: &str = r#"load("@crucible//calls", "playbook")
triage = playbook("draft:triage-draft")
workflow(type = "playbook", tasks = [])
"#;

    fn pack<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |path| {
            pairs
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, text)| text.to_string())
        }
    }

    #[test]
    fn every_workflow_source_declares_its_calls_and_a_bad_entry_is_named() {
        let calls = declared(pack(&[
            ("crucible.toml", MANIFEST),
            ("workflow.star", WORKFLOW),
        ]))
        .expect("parses");
        let alias = CallAlias::parse("triage").expect("alias");
        assert_eq!(
            calls.get(&alias),
            Some(&CallTarget::Playbook {
                playbook: PlaybookRef::Draft("triage-draft".into())
            })
        );
        let ci = r#"load("@crucible//calls", "artifact_outputs", "github_workflow")
ci = github_workflow(repo = "acme/app", workflow = ".github/workflows/ci.yml", ref = "main", outputs = artifact_outputs(artifact = "result"))
"#;
        let both = declared(pack(&[
            ("crucible.toml", MANIFEST),
            ("workflow.star", WORKFLOW),
            ("workflow-agent.star", ci),
        ]))
        .expect("parses");
        assert_eq!(both.len(), 2, "a variant's source adds its targets");
        let clash = WORKFLOW.replace("draft:triage-draft", "triage");
        let refused = declared(pack(&[
            ("crucible.toml", MANIFEST),
            ("workflow.star", WORKFLOW),
            ("workflow-agent.star", &clash),
        ]))
        .expect_err("two targets under one name");
        assert!(refused.contains("differs"), "{refused}");
        assert!(
            declared(pack(&[(
                "crucible.toml",
                "[workflow]\ntype = \"playbook\"\n"
            )]))
            .expect("none")
            .is_empty()
        );
        assert!(
            declared(pack(&[("crucible.toml", "not = [toml")]))
                .expect("not toml")
                .is_empty()
        );
        assert!(
            declared(pack(&[
                ("crucible.toml", MANIFEST),
                ("workflow.star", "this is not starlark (\n"),
            ]))
            .expect("the compile step reports the parse error")
            .is_empty()
        );
        let refused = declared(pack(&[
            ("crucible.toml", MANIFEST),
            (
                "workflow.star",
                "load(\"@crucible//calls\", \"playbook\")\nx = playbook(\"has space\")\n",
            ),
        ]))
        .expect_err("unknown kind");
        assert!(refused.starts_with("workflow.star:"), "{refused}");
    }

    #[test]
    fn a_stored_pack_is_read_with_its_lock_and_refused_without_one() {
        let without = PackTree::from_pairs(&[
            ("crucible.toml", MANIFEST.as_bytes()),
            ("workflow.star", WORKFLOW.as_bytes()),
        ])
        .expect("tree");
        assert_eq!(
            of_tree(&without).expect_err("no lock"),
            LockError::Missing.to_string()
        );
        let alias = CallAlias::parse("triage").expect("alias");
        let lock = CallsLock {
            version: LOCK_VERSION,
            calls: BTreeMap::from([(
                alias.clone(),
                LockedCall::new(
                    Pin::DraftHead {
                        draft: "triage-draft".into(),
                    },
                    interface(json!({"type": "object"}), &[]),
                )
                .expect("locked"),
            )]),
        };
        let text = lock.render().expect("render");
        let with = PackTree::from_pairs(&[
            ("crucible.toml", MANIFEST.as_bytes()),
            ("workflow.star", WORKFLOW.as_bytes()),
            (LOCK_FILE, text.as_bytes()),
        ])
        .expect("tree");
        let (calls, read) = of_tree(&with).expect("reads");
        assert_eq!(calls.len(), 1);
        assert_eq!(
            read.as_ref().map(|l| l.calls[&alias].kind),
            Some(CallKind::Playbook)
        );
        let stale_source = WORKFLOW.replace("draft:triage-draft", "triage");
        let stale = PackTree::from_pairs(&[
            ("crucible.toml", MANIFEST.as_bytes()),
            ("workflow.star", stale_source.as_bytes()),
            (LOCK_FILE, text.as_bytes()),
        ])
        .expect("tree");
        assert!(
            of_tree(&stale)
                .expect_err("stale")
                .contains("different target")
        );
        let bare = PackTree::from_pairs(&[("crucible.toml", b"[workflow]\n")]).expect("tree");
        assert_eq!(of_tree(&bare).expect("no calls"), (BTreeMap::new(), None));
    }

    #[test]
    fn a_registered_pack_needs_a_current_lock_of_registered_pins() {
        let source = WORKFLOW.replace("draft:triage-draft", "triage");
        let tree = |lock: Option<&CallsLock>| {
            let text = lock.map(|l| l.render().expect("render"));
            let mut pairs: Vec<(&str, &[u8])> = vec![
                ("crucible.toml", MANIFEST.as_bytes()),
                ("workflow.star", source.as_bytes()),
            ];
            if let Some(text) = &text {
                pairs.push((LOCK_FILE, text.as_bytes()));
            }
            PackTree::from_pairs(&pairs).expect("tree")
        };
        let entry = |pin: Pin| {
            LockedCall::new(pin, interface(json!({"type": "object"}), &[])).expect("locked")
        };
        let triage = CallAlias::parse("triage").expect("alias");
        let revision = Pin::Revision {
            playbook: "triage".into(),
            digest: "sha256:aa".into(),
        };
        let good = CallsLock {
            version: LOCK_VERSION,
            calls: BTreeMap::from([(triage.clone(), entry(revision.clone()))]),
        };
        assert_eq!(registrable(&tree(Some(&good))), Ok(()));

        assert_eq!(
            registrable(&tree(None)).expect_err("no lock"),
            LockError::Missing.to_string()
        );

        let mut extra = good.clone();
        extra.calls.insert(
            CallAlias::parse("gone").expect("alias"),
            entry(revision.clone()),
        );
        assert!(
            registrable(&tree(Some(&extra)))
                .expect_err("undeclared entry")
                .contains("gone"),
        );

        let moved = CallsLock {
            version: LOCK_VERSION,
            calls: BTreeMap::from([(
                triage.clone(),
                entry(Pin::Revision {
                    playbook: "other".into(),
                    digest: "sha256:aa".into(),
                }),
            )]),
        };
        assert!(
            registrable(&tree(Some(&moved)))
                .expect_err("stale pin")
                .contains("different target")
        );

        let draft_source = WORKFLOW;
        let head = CallsLock {
            version: LOCK_VERSION,
            calls: BTreeMap::from([(
                triage,
                entry(Pin::DraftHead {
                    draft: "triage-draft".into(),
                }),
            )]),
        };
        let text = head.render().expect("render");
        let drafted = PackTree::from_pairs(&[
            ("crucible.toml", MANIFEST.as_bytes()),
            ("workflow.star", draft_source.as_bytes()),
            (LOCK_FILE, text.as_bytes()),
        ])
        .expect("tree");
        assert!(
            registrable(&drafted)
                .expect_err("draft head")
                .contains("draft's head")
        );
    }

    #[test]
    fn drift_names_what_moved() {
        let old = interface(
            json!({"type": "object", "properties": {"pr": {}, "repo": {}}, "required": ["pr"]}),
            &["verdict"],
        );
        let new = interface(
            json!({"type": "object", "properties": {"pr": {}, "branch": {}}, "required": ["pr", "branch"]}),
            &["verdict", "score"],
        );
        let said = drift(&old, &new);
        assert!(
            said.contains(r#"params added ["branch"], removed ["repo"]"#),
            "{said}"
        );
        assert!(
            said.contains(r#"required params added ["branch"]"#),
            "{said}"
        );
        assert!(said.contains(r#"result fields added ["score"]"#), "{said}");
        let mut typed = old.clone();
        typed.params = json!({"type": "object", "properties": {"pr": {"type": "integer"}, "repo": {}}, "required": ["pr"]});
        assert!(drift(&old, &typed).contains("type or settings"));
    }

    #[test]
    fn bound_values_must_fit_the_locked_params() {
        let locked = interface(
            json!({"type": "object", "properties": {"pr": {}, "note": {}}, "required": ["pr"]}),
            &[],
        );
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(check_bound(&locked, names(&["pr"]).iter()).is_ok());
        assert!(check_bound(&locked, names(&["pr", "note"]).iter()).is_ok());
        assert_eq!(
            check_bound(&locked, names(&["note"]).iter()).expect_err("unbound"),
            r#"required parameter "pr" is unbound"#
        );
        assert_eq!(
            check_bound(&locked, names(&["pr", "extra"]).iter()).expect_err("unknown"),
            r#"the target takes no parameter "extra""#
        );
    }

    #[test]
    fn bound_values_and_files_must_fit_the_locked_types() {
        let locked = interface(
            json!({"type": "object", "properties": {
                "name": {"type": "string"},
                "count": {"type": "integer"},
                "ratio": {"type": "number"},
                "loud": {"type": "boolean"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "config": {"type": "object", "x-param-type": "json"},
                "notes": {"type": "object", "x-param-type": "file", "x-max-bytes": 10,
                          "x-media-type": ["text/markdown"]},
            }}),
            &[],
        );
        let values = |v: serde_json::Value| -> BTreeMap<String, serde_json::Value> {
            serde_json::from_value(v).expect("values")
        };
        let file = |size: u64, media: Option<&str>| {
            BTreeMap::from([(
                "notes".to_string(),
                crucible_contract::calls::FileRef {
                    digest: "sha256:aa".into(),
                    size,
                    path: None,
                    media_type: media.map(str::to_string),
                },
            )])
        };
        let none = BTreeMap::new();
        const PLAYBOOK: crucible_contract::calls::CallKind =
            crucible_contract::calls::CallKind::Playbook;
        let object = values(json!({"name": {"k": 1}}));
        assert!(check_values(PLAYBOOK, &locked, &object, &none).is_err());
        assert!(
            check_values(
                crucible_contract::calls::CallKind::GithubWorkflow,
                &locked,
                &object,
                &none
            )
            .is_ok(),
            "a GitHub string input takes JSON, serialized"
        );
        assert!(
            check_values(
                PLAYBOOK,
                &locked,
                &values(json!({"name": "a", "count": 3, "ratio": 0.5, "loud": true,
                               "tags": ["x"], "config": {"k": [1]}})),
                &file(10, Some("text/markdown")),
            )
            .is_ok()
        );
        for (bad, says) in [
            (json!({"name": 1}), "takes string"),
            (json!({"count": 1.5}), "takes int"),
            (json!({"count": 4_000_000_000_i64}), "takes int"),
            (json!({"loud": "yes"}), "takes bool"),
            (json!({"tags": ["x", 1]}), "takes list<string>"),
            (json!({"config": "text"}), "takes json"),
            (json!({"notes": "text"}), "takes a file"),
        ] {
            let refused =
                check_values(PLAYBOOK, &locked, &values(bad), &none).expect_err("refused");
            assert!(refused.contains(says), "{says:?} in {refused}");
        }
        let big =
            json!({"config": {"k": "x".repeat(crucible_contract::calls::MAX_JSON_PARAM_BYTES)}});
        assert!(check_values(PLAYBOOK, &locked, &values(big), &none).is_err());
        assert!(
            check_values(
                PLAYBOOK,
                &locked,
                &BTreeMap::new(),
                &file(11, Some("text/markdown"))
            )
            .expect_err("too big")
            .contains("at most 10 bytes")
        );
        assert!(
            check_values(PLAYBOOK, &locked, &BTreeMap::new(), &file(1, None))
                .expect_err("no media type")
                .contains("no media type")
        );
        let mut misplaced = file(1, Some("text/markdown"));
        let notes = misplaced.remove("notes").expect("notes");
        misplaced.insert("name".into(), notes);
        assert!(
            check_values(PLAYBOOK, &locked, &BTreeMap::new(), &misplaced)
                .expect_err("not a file parameter")
                .contains("not a file parameter")
        );
    }
}
