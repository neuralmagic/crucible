//! Writing and reading a pack's `calls.lock` (RFC-0008 C-LOCK) against the `calls` blocks of its
//! workflow sources.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crucible_contract::calls::{
    CallAlias, CallTarget, CallsLock, LOCK_FILE, LOCK_VERSION, LockedCall, Pin, TargetInterface,
};

use crate::calls::{Backends, CallCtx, CallError};

/// The workflow sources a manifest names: `[workflow].file` and each `[variant.<name>.workflow].file`.
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

/// Every target the workflow sources of a pack declare in their `calls` blocks. `file` reads a
/// pack file by its path. A manifest that is not TOML, or a source that is absent, declares
/// nothing here: its compile reports that, not the lock. Two sources that give one name
/// different targets are refused.
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

/// Resolve every declared target through its backend and lock it. `None` when nothing is
/// declared. `draft_pack` is whether the pack is a draft, the only kind that may pin a draft head.
pub(crate) async fn write(
    backends: &Backends,
    ctx: &CallCtx,
    calls: &BTreeMap<CallAlias, CallTarget>,
    draft_pack: bool,
) -> Result<Option<CallsLock>, String> {
    if calls.is_empty() {
        return Ok(None);
    }
    let mut locked = BTreeMap::new();
    for (alias, target) in calls {
        let resolved = backends
            .get(target.kind())
            .map_err(|e| format!("call target {alias}: {}", e.note()))?
            .interface(ctx, target)
            .await
            .map_err(|e| match e {
                CallError::Internal(e) => format!("call target {alias}: {e:#}"),
                other => format!("call target {alias}: {}", other.note()),
            })?;
        if matches!(resolved.pin, Pin::DraftHead { .. }) && !draft_pack {
            return Err(format!(
                "call target {alias} pins a draft's head, which only a draft pack may do; register \
                 the target and name it by its registered name"
            ));
        }
        let entry = LockedCall::new(resolved.pin, resolved.interface)
            .map_err(|e| format!("call target {alias}: digesting the interface: {e}"))?;
        locked.insert(alias.clone(), entry);
    }
    Ok(Some(CallsLock {
        version: LOCK_VERSION,
        calls: locked,
    }))
}

/// Write the lock a draft's files need into them: the lock of its `calls` blocks, or no
/// lock file at all when it declares none.
pub(crate) async fn into_files(
    backends: &Backends,
    ctx: &CallCtx,
    files: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let calls = declared(|path| files.get(path).cloned())?;
    match write(backends, ctx, &calls, true).await? {
        Some(lock) => {
            let text = lock
                .render()
                .map_err(|e| format!("rendering {LOCK_FILE}: {e}"))?;
            files.insert(LOCK_FILE.to_string(), text);
        }
        None => {
            files.remove(LOCK_FILE);
        }
    }
    Ok(())
}

/// The declared targets and lock of a stored pack, checked against each other.
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

/// What changed between the locked interface and the one the target has now, in a few words.
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

/// Refuse bound values the locked interface does not take, and a required parameter left
/// unbound.
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

#[cfg(test)]
mod tests {
    use crate::calls::lock::*;
    use crucible_contract::calls::{
        CallKind, LockError, NodeStatus, PlaybookRef, ProgressGraph, ProgressNode,
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

    const WORKFLOW: &str = r#"calls = {"triage": {"kind": "playbook", "playbook": "draft:triage-draft"}}
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
        let ci = r#"calls = {"ci": {"kind": "github_workflow", "repo": "acme/app", "workflow": ".github/workflows/ci.yml", "ref": "main", "outputs": {"artifact": "result"}}}
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
        let refused = declared(pack(&[
            ("crucible.toml", MANIFEST),
            ("workflow.star", "calls = {\"x\": {\"kind\": \"gitlab\"}}\n"),
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
}
