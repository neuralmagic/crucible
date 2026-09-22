//! Compile a pack tree with the pinned engine and return everything the import wizard's preview
//! gate shows: the params schema the launch form renders, the graph of the compiled plan, and the
//! engine's diagnostics verbatim.
//!
//! The tree is any local directory — a fetched checkout's pack subtree — so one function serves
//! both the import flow and anything else that needs to see a pack before it is registered.
//! Nothing here writes the registry: a refused compile is a preview that carries the engine's
//! `file:line:col` error, not a failed registration.

use crate::playbooks::plan_graph::{WorkflowGraphDto, graph_from_compiled};
use crate::playbooks::registry::RegisterError;
use anyhow::Context;
use crucible::plan::starlark::{compile_file_with, declared_params, parent_or_cwd};
use std::collections::BTreeMap;
use std::path::Path;

/// What the preview gate renders. Every field is optional because a pack that does not compile
/// still has a preview to show: its diagnostics.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PackPreview {
    /// The engine's params JSON Schema, the same document registration stores.
    pub params_schema: Option<serde_json::Value>,
    pub schema_digest: Option<String>,
    /// The compiled plan reduced to nodes and edges.
    pub graph: Option<WorkflowGraphDto>,
    /// The engine's stderr, verbatim, for whichever step refused.
    pub diagnostics: Vec<String>,
    /// The substrate the pack's `[agent]` asks for. `None` when the manifest did not parse.
    pub agent: Option<crate::playbooks::dispatch::PackAgent>,
    /// The credentials the pack's manifest declares. A binding is the only thing that maps one of
    /// these names to a value, so the gate shows them before anything is registered.
    pub declared_secrets: Vec<crate::secrets::manifest::DeclaredSecret>,
    /// The exposure the pinned engine computed: where a run of this pack may write and what reach
    /// it holds. `None` when the manifest could not be loaded; the diagnostics say why.
    pub exposure: Option<crate::playbooks::exposure::Exposure>,
    pub exposure_digest: Option<String>,
}

/// What a required parameter with no value means to a compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unvalued {
    /// The engine's refusal is the preview: the import wizard's gate, where the form is what
    /// supplies the value and the diagnostic is what asks for it.
    Refuse,
    /// A synthesized value stands in, so the graph and the schema derive anyway: compile-on-save,
    /// where the pack is being written and nobody has typed a value yet.
    Placeholder,
}

/// The value each declared param compiles with: the caller's, else the pack's own declared
/// default, else — under [`Unvalued::Placeholder`] — a stand-in derived from its declaration. A
/// pack whose optional params carry defaults compiles with no values supplied at all.
fn effective(
    schema: Option<&serde_json::Value>,
    params: &BTreeMap<String, String>,
    unvalued: Unvalued,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let properties = schema
        .and_then(|s| s.get("properties"))
        .and_then(|p| p.as_object());
    if let Some(properties) = properties {
        for (name, property) in properties {
            if let Some(default) = property.get("default").and_then(|d| d.as_str()) {
                out.insert(name.clone(), default.to_string());
            }
        }
    }
    out.extend(
        params
            .iter()
            .filter(|(name, _)| properties.is_some_and(|p| p.contains_key(name.as_str())))
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    if unvalued == Unvalued::Refuse {
        return out;
    }
    let required = schema
        .and_then(|s| s.get("required"))
        .and_then(|r| r.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();
    for name in required.iter().filter_map(|n| n.as_str()) {
        if out.contains_key(name) {
            continue;
        }
        let Some(property) = properties.and_then(|p| p.get(name)) else {
            continue;
        };
        if let Some(value) = crate::playbooks::param_placeholder::placeholder(name, property) {
            out.insert(name.to_string(), value);
        }
    }
    out
}

/// Compile `pack_root` through the linked engine and collect the preview. `Err` is a tree that is
/// not a pack at all (no manifest, no playbook workflow, no source file); a source the engine
/// refuses is an `Ok` preview whose diagnostics carry the refusal. Blocking — callers
/// spawn_blocking.
pub fn preview_pack(
    pack_root: &Path,
    params: &BTreeMap<String, String>,
    unvalued: Unvalued,
) -> Result<PackPreview, RegisterError> {
    preview_pack_for(
        pack_root,
        crate::playbooks::registry::PackWorkflowKind::Playbook,
        params,
        unvalued,
    )
}

/// Preview a pack at a door that explicitly accepts a non-registry workflow family.
pub(crate) fn preview_pack_for(
    pack_root: &Path,
    kind: crate::playbooks::registry::PackWorkflowKind,
    params: &BTreeMap<String, String>,
    unvalued: Unvalued,
) -> Result<PackPreview, RegisterError> {
    let source = crate::playbooks::registry::workflow_source_for(pack_root, kind)?;
    let mut preview = PackPreview {
        agent: crate::playbooks::dispatch::pack_agent(pack_root).ok(),
        ..PackPreview::default()
    };
    match crate::secrets::manifest::declared_secrets(pack_root) {
        Ok(declared) => preview.declared_secrets = declared,
        Err(e) => preview.diagnostics.push(format!("{e:#}")),
    }
    match crate::playbooks::exposure::extract(pack_root, None) {
        Ok(exposure) => {
            preview.exposure_digest = Some(exposure.digest().map_err(RegisterError::Internal)?);
            preview.exposure = Some(exposure);
        }
        Err(e) => preview.diagnostics.push(format!("{e}")),
    }

    let text = std::fs::read_to_string(&source)
        .with_context(|| format!("reading {}", source.display()))
        .map_err(RegisterError::Internal)?;
    match declared_params(&text, &source) {
        Ok(schema) => {
            preview.schema_digest = Some(
                crate::playbooks::registry::schema_digest(&schema)
                    .map_err(RegisterError::Internal)?,
            );
            preview.params_schema = Some(schema);
        }
        Err(e) => {
            preview.diagnostics.push(crucible::errors::report(&e));
            return Ok(preview);
        }
    }

    let params = effective(preview.params_schema.as_ref(), params, unvalued);
    let compiled = match compile_file_with(&source, parent_or_cwd(&source), &params) {
        Ok(compiled) => compiled,
        Err(e) => {
            preview.diagnostics.push(crucible::errors::report(&e));
            return Ok(preview);
        }
    };

    match graph_from_compiled(compiled.canonical_json.as_bytes()) {
        Ok(graph) => preview.graph = Some(graph),
        Err(message) => preview.diagnostics.push(message),
    }
    Ok(preview)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::fixtures::{
        WORKFLOW_BROKEN, WORKFLOW_NO_PARAMS, WORKFLOW_TOPIC, schema_of, write_playbook_pack,
    };

    #[test]
    fn a_compiling_pack_previews_a_schema_a_digest_and_a_graph() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = write_playbook_pack(dir.path(), WORKFLOW_TOPIC);

        let values = BTreeMap::from([("topic".to_string(), "fences".to_string())]);
        let preview = preview_pack(&root, &values, Unvalued::Refuse).expect("previews");
        assert_eq!(preview.params_schema, Some(schema_of(WORKFLOW_TOPIC)));
        assert!(preview.schema_digest.is_some_and(|d| !d.is_empty()));
        let graph = preview.graph.expect("the graph rides the preview");
        assert_eq!(graph.result.as_deref(), Some("hello"));
        assert_eq!(
            graph
                .nodes
                .iter()
                .map(|n| n.name.as_str())
                .collect::<Vec<_>>(),
            vec!["hello"]
        );
        assert!(preview.diagnostics.is_empty(), "{:?}", preview.diagnostics);
    }

    #[test]
    fn a_refused_compile_keeps_the_schema_and_carries_the_engine_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = write_playbook_pack(dir.path(), WORKFLOW_BROKEN);

        let preview = preview_pack(&root, &BTreeMap::new(), Unvalued::Refuse).expect("previews");
        assert!(preview.params_schema.is_some(), "the form still renders");
        assert_eq!(preview.graph, None, "no graph without a compiled plan");
        assert_eq!(preview.diagnostics.len(), 1, "{:?}", preview.diagnostics);
        assert!(
            preview.diagnostics[0].contains("workflow.star:3:39"),
            "the engine's anchored diagnostic rides verbatim: {:?}",
            preview.diagnostics
        );
        assert!(
            preview.diagnostics[0].contains("dpeth"),
            "the span alone is not a diagnostic: the source chain names the identifier: {:?}",
            preview.diagnostics
        );
    }

    /// A required param with no value is the engine's refusal, and the form still renders.
    #[test]
    fn an_unvalued_required_param_is_a_diagnostic_not_a_graph() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = write_playbook_pack(dir.path(), WORKFLOW_TOPIC);

        let bare = preview_pack(&root, &BTreeMap::new(), Unvalued::Refuse).expect("previews");
        assert!(bare.params_schema.is_some());
        assert_eq!(bare.graph, None);
        assert!(
            bare.diagnostics.iter().any(|d| d.contains("topic")),
            "{:?}",
            bare.diagnostics
        );

        let filled =
            preview_pack(&root, &BTreeMap::new(), Unvalued::Placeholder).expect("previews");
        assert!(filled.diagnostics.is_empty(), "{:?}", filled.diagnostics);
        assert!(
            filled.graph.is_some(),
            "a placeholder stands in for the unvalued required param"
        );
    }

    /// A source the engine refuses before it can read a params block previews nothing but its
    /// error.
    #[test]
    fn a_failing_params_extraction_previews_nothing_but_its_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = write_playbook_pack(dir.path(), &format!("x = {}1\n", "lambda: ".repeat(4800)));

        let preview = preview_pack(&root, &BTreeMap::new(), Unvalued::Refuse).expect("previews");
        assert_eq!(preview.params_schema, None);
        assert_eq!(preview.schema_digest, None);
        assert_eq!(preview.graph, None);
        assert_eq!(preview.diagnostics.len(), 1, "{:?}", preview.diagnostics);
        assert!(
            preview.diagnostics[0].contains("levels deep"),
            "{:?}",
            preview.diagnostics
        );
    }

    /// A directory that is not a pack is an error, not an empty preview: the wizard's 422 is what
    /// tells the importer they picked the wrong path.
    #[test]
    fn a_tree_without_a_playbook_manifest_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err =
            preview_pack(dir.path(), &BTreeMap::new(), Unvalued::Refuse).expect_err("refused");
        assert!(matches!(err, RegisterError::Invalid(_)), "{err:#}");
    }

    /// The reduction is a claim about the engine's compiled-plan JSON: every node kind, dependency
    /// edge, and agent field the studio draws comes through.
    #[test]
    fn a_parameterized_pack_previews_its_whole_graph() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = write_playbook_pack(
            dir.path(),
            concat!(
                "params = {\n",
                "    \"repo\": {\"type\": \"string\", \"required\": True, \"pattern\": \"^[a-z]+/[a-z]+$\"},\n",
                "    \"limit\": {\"type\": \"string\", \"default\": \"6\"},\n",
                "}\n",
                "s = command(name = \"s\", run = \"true\")\n",
                "t = command(name = \"t\", run = \"true\", depends_on = [s])\n",
                "a = agent(name = \"a\", prompt = \"go\", model = \"opus\", effort = \"high\", depends_on = [t])\n",
                "workflow(type = \"playbook\", tasks = [s, t, a])\n",
            ),
        );

        let values = BTreeMap::from([("repo".to_string(), "owner/pack".to_string())]);
        let preview = preview_pack(&root, &values, Unvalued::Refuse).expect("previews");
        assert!(preview.diagnostics.is_empty(), "{:?}", preview.diagnostics);
        let schema = preview.params_schema.expect("schema");
        assert_eq!(schema["required"], serde_json::json!(["repo"]));
        assert_eq!(schema["properties"]["limit"]["default"], "6");
        let graph = preview.graph.expect("graph");
        assert_eq!(graph.workflow_type, "playbook");
        assert_eq!(
            graph
                .nodes
                .iter()
                .map(|n| n.name.as_str())
                .collect::<Vec<_>>(),
            vec!["s", "t", "a"]
        );
        assert_eq!(
            graph.nodes[1].kind,
            crate::playbooks::plan_graph::TaskKind::Command
        );
        assert_eq!(graph.nodes[1].command.as_deref(), Some("true"));
        assert_eq!(
            graph.nodes[2].kind,
            crate::playbooks::plan_graph::TaskKind::Agent
        );
        assert_eq!(graph.nodes[2].model.as_deref(), Some("opus"));
        assert_eq!(graph.nodes[2].effort.as_deref(), Some("high"));
        assert_eq!(graph.nodes[2].prompt.as_deref(), Some("go"));
        assert_eq!(
            graph.edges,
            vec![
                crate::playbooks::plan_graph::GraphEdgeDto {
                    from: "s".to_string(),
                    to: "t".to_string(),
                    join: crate::playbooks::plan_graph::Join::All,
                    required: true,
                },
                crate::playbooks::plan_graph::GraphEdgeDto {
                    from: "t".to_string(),
                    to: "a".to_string(),
                    join: crate::playbooks::plan_graph::Join::All,
                    required: true,
                }
            ],
            "the dependency edge is drawn"
        );
    }

    /// A pack with no parameters compiles with nothing supplied.
    #[test]
    fn a_pack_without_params_needs_no_values() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = write_playbook_pack(dir.path(), WORKFLOW_NO_PARAMS);
        let preview = preview_pack(&root, &BTreeMap::new(), Unvalued::Refuse).expect("previews");
        assert!(preview.diagnostics.is_empty(), "{:?}", preview.diagnostics);
        assert_eq!(preview.params_schema, Some(schema_of(WORKFLOW_NO_PARAMS)));
        assert!(preview.graph.is_some());
    }

    /// A supplied value wins over the placeholder, and a required param with no value only gets
    /// one under `Placeholder`.
    #[test]
    fn a_placeholder_fills_only_an_unvalued_required_param() {
        let schema: serde_json::Value = serde_json::from_str(
            r#"{"properties":{"topic":{"type":"string"},"depth":{"type":"string","default":"deep"}},
                "required":["topic"]}"#,
        )
        .expect("fixture");
        assert_eq!(
            effective(Some(&schema), &BTreeMap::new(), Unvalued::Refuse),
            BTreeMap::from([("depth".to_string(), "deep".to_string())])
        );
        assert_eq!(
            effective(Some(&schema), &BTreeMap::new(), Unvalued::Placeholder),
            BTreeMap::from([
                ("depth".to_string(), "deep".to_string()),
                ("topic".to_string(), "<topic>".to_string()),
            ])
        );
        let supplied = BTreeMap::from([("topic".to_string(), "fences".to_string())]);
        assert_eq!(
            effective(Some(&schema), &supplied, Unvalued::Placeholder),
            BTreeMap::from([
                ("depth".to_string(), "deep".to_string()),
                ("topic".to_string(), "fences".to_string()),
            ])
        );
    }

    /// Only params the pack declares reach the compiler.
    #[test]
    fn undeclared_values_are_dropped_before_the_engine_sees_them() {
        let schema: serde_json::Value =
            serde_json::from_str(r#"{"properties":{"topic":{},"depth":{}}}"#).expect("fixture");
        let supplied = BTreeMap::from([
            ("topic".to_string(), "fences".to_string()),
            ("stray".to_string(), "value".to_string()),
        ]);
        assert_eq!(
            effective(Some(&schema), &supplied, Unvalued::Refuse),
            BTreeMap::from([("topic".to_string(), "fences".to_string())])
        );
        assert_eq!(
            effective(Some(&serde_json::json!({})), &supplied, Unvalued::Refuse),
            BTreeMap::new()
        );
        assert_eq!(
            effective(None, &supplied, Unvalued::Refuse),
            BTreeMap::new()
        );
    }
}
