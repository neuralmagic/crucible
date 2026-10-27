//! Hover text and signatures from the engine's DSL reference, plus starlark's builtin docs.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use crucible::plan::starlark::lane_globals;
use crucible::plan::starlark::params::ParamSpec;
use crucible::plan::starlark::reference::{self, Function, Kwarg, Lane};
use crucible::plan::workflow::WorkflowType;
use starlark::docs::markdown::render_doc_item_no_link;
use starlark::docs::{DocItem, DocMember};
use starlark::environment::GlobalsBuilder;
use tower_lsp_server::ls_types::{
    Documentation, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, SignatureHelp,
    SignatureInformation,
};

pub struct Reference {
    pub functions: Vec<Function>,
    /// Starlark's standard globals and their rendered documentation.
    builtins: BTreeMap<String, String>,
    /// The callable ones: everything but `True`, `False`, and `None`.
    functions_builtin: BTreeSet<String>,
    autoresearch: HashSet<String>,
    custom: HashSet<String>,
    playbook: HashSet<String>,
}

impl Default for Reference {
    fn default() -> Self {
        Reference::new()
    }
}

impl Reference {
    pub fn new() -> Self {
        let members = GlobalsBuilder::standard().build().documentation().members;
        let builtins = members
            .iter()
            .map(|(name, item)| (name.clone(), render_doc_item_no_link(name, item)))
            .collect();
        let functions_builtin = members
            .iter()
            .filter(|(_, item)| !matches!(item, DocItem::Member(DocMember::Property(_))))
            .map(|(name, _)| name.clone())
            .collect();
        let names = |lane| {
            lane_globals(lane)
                .names()
                .map(|name| name.as_str().to_owned())
                .collect()
        };
        Reference {
            functions: reference::functions(),
            builtins,
            functions_builtin,
            autoresearch: names(WorkflowType::Autoresearch),
            custom: names(WorkflowType::Custom),
            playbook: names(WorkflowType::Playbook),
        }
    }

    pub fn function(&self, name: &str) -> Option<&Function> {
        self.functions.iter().find(|f| f.name == name)
    }

    /// Every name a source in `lane` can use without binding it.
    pub fn globals(&self, lane: WorkflowType) -> &HashSet<String> {
        match lane {
            WorkflowType::Autoresearch => &self.autoresearch,
            WorkflowType::Custom => &self.custom,
            WorkflowType::Playbook => &self.playbook,
        }
    }

    pub fn builtin(&self, name: &str) -> Option<&str> {
        self.builtins.get(name).map(String::as_str)
    }

    /// The callable builtins.
    pub fn builtin_names(&self) -> impl Iterator<Item = &str> {
        self.functions_builtin.iter().map(String::as_str)
    }
}

pub fn lanes(lane: Lane) -> &'static str {
    match lane {
        Lane::Common => "every lane",
        Lane::Scored => "autoresearch and custom lanes",
        Lane::Routed => "playbook and custom lanes",
    }
}

/// The literals of a union type such as `"all" | "passed"`; `None` if any part is not one.
pub fn enum_values(ty: &str) -> Option<Vec<&str>> {
    ty.split('|')
        .map(|part| part.trim().strip_prefix('"')?.strip_suffix('"'))
        .collect()
}

fn first_sentence(text: &str) -> &str {
    text.split_once(". ").map_or(text, |(first, _)| first)
}

fn markdown(value: String) -> Documentation {
    Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value,
    })
}

pub fn function_markdown(function: &Function) -> String {
    let mut out = format!(
        "```python\n{}\n```\n\n{}\n\n*{}*",
        signature_label(function).0,
        function.purpose,
        lanes(function.lane)
    );
    if !function.kwargs.is_empty() {
        out.push_str("\n\n");
        for kwarg in &function.kwargs {
            out.push_str(&format!(
                "- `{}` `{}`: {}\n",
                kwarg.name,
                kwarg.ty,
                first_sentence(&kwarg.purpose)
            ));
        }
    }
    out
}

pub fn kwarg_markdown(function: &Function, kwarg: &Kwarg) -> String {
    format!(
        "```python\n{}: {}\n```\n\n{}\n\n*`{}()` argument*",
        kwarg.name, kwarg.ty, kwarg.purpose, function.name
    )
}

pub fn kwarg_documentation(kwarg: &Kwarg) -> Documentation {
    markdown(format!("`{}`\n\n{}", kwarg.ty, kwarg.purpose))
}

pub fn function_documentation(function: &Function) -> Documentation {
    markdown(format!(
        "{}\n\n*{}*",
        function.purpose,
        lanes(function.lane)
    ))
}

pub fn param_markdown(spec: &ParamSpec) -> String {
    let mut out = format!(
        "```python\nparam({:?}): {}\n```",
        spec.name,
        spec.ty.as_str()
    );
    if !spec.doc.is_empty() {
        out.push_str(&format!("\n\n{}", spec.doc));
    }
    match &spec.default {
        Some(default) => out.push_str(&format!("\n\nDefault: `{}`", default.json())),
        None if spec.required => out.push_str("\n\nRequired: the launcher must supply it."),
        None => {}
    }
    if !spec.choices.is_empty() {
        out.push_str(&format!("\n\nOne of: `{}`", spec.choices.join("`, `")));
    }
    out
}

/// `agent(name: str, prompt: str, ...)` and the UTF-16 offsets of each parameter in it.
pub fn signature_label(function: &Function) -> (String, Vec<[u32; 2]>) {
    let parts: Vec<String> = if function.kwargs.is_empty() {
        function
            .positional
            .iter()
            .map(|p| (*p).to_owned())
            .collect()
    } else {
        function
            .kwargs
            .iter()
            .map(|k| format!("{}: {}", k.name, k.ty))
            .collect()
    };
    let mut label = format!("{}(", function.name);
    let mut offsets = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            label.push_str(", ");
        }
        let start = utf16_len(&label);
        label.push_str(part);
        offsets.push([start, utf16_len(&label)]);
    }
    label.push(')');
    (label, offsets)
}

fn utf16_len(text: &str) -> u32 {
    u32::try_from(text.encode_utf16().count()).unwrap_or(u32::MAX)
}

/// Signature help for a call to `function`, with `active` the argument being written.
pub fn signature_help(function: &Function, active: Option<&str>) -> SignatureHelp {
    let (label, offsets) = signature_label(function);
    let parameters = if function.kwargs.is_empty() {
        offsets
            .iter()
            .map(|offset| ParameterInformation {
                label: ParameterLabel::LabelOffsets(*offset),
                documentation: None,
            })
            .collect()
    } else {
        function
            .kwargs
            .iter()
            .zip(&offsets)
            .map(|(kwarg, offset)| ParameterInformation {
                label: ParameterLabel::LabelOffsets(*offset),
                documentation: Some(markdown(kwarg.purpose.clone())),
            })
            .collect()
    };
    let active_parameter = match active {
        Some(name) => function
            .kwargs
            .iter()
            .position(|k| k.name == name)
            .and_then(|i| u32::try_from(i).ok()),
        None if function.kwargs.is_empty() => Some(0),
        None => None,
    };
    SignatureHelp {
        signatures: vec![SignatureInformation {
            label,
            documentation: Some(function_documentation(function)),
            parameters: Some(parameters),
            active_parameter,
        }],
        active_signature: Some(0),
        active_parameter,
    }
}

#[cfg(test)]
mod tests {
    use crate::docs::*;

    #[test]
    fn union_types_of_literals_are_enumerable() {
        assert_eq!(
            enum_values("\"all\" | \"passed\" | \"settled\""),
            Some(vec!["all", "passed", "settled"])
        );
        assert_eq!(enum_values("str"), None);
        assert_eq!(enum_values("\"lower\" | str"), None);
    }

    #[test]
    fn every_constructor_has_a_signature_with_one_offset_per_argument() {
        let reference = Reference::new();
        for function in &reference.functions {
            let (label, offsets) = signature_label(function);
            let expected = if function.kwargs.is_empty() {
                usize::from(function.positional.is_some())
            } else {
                function.kwargs.len()
            };
            assert_eq!(offsets.len(), expected, "{}", function.name);
            let units: Vec<u16> = label.encode_utf16().collect();
            if let (Some(first), Some(kwarg)) = (offsets.first(), function.kwargs.first()) {
                let named = String::from_utf16_lossy(&units[first[0] as usize..first[1] as usize]);
                assert!(named.starts_with(kwarg.name), "{named} in {label}");
            }
        }
    }

    #[test]
    fn the_active_parameter_follows_the_keyword_being_written() {
        let reference = Reference::new();
        let agent = reference.function("agent").expect("agent");
        let help = signature_help(agent, Some("prompt"));
        let prompt = agent
            .kwargs
            .iter()
            .position(|k| k.name == "prompt")
            .expect("prompt");
        assert_eq!(help.active_parameter, u32::try_from(prompt).ok());
        let prompt_file = reference.function("prompt_file").expect("prompt_file");
        assert_eq!(signature_help(prompt_file, None).active_parameter, Some(0));
        assert_eq!(signature_label(prompt_file).0, "prompt_file(path)");
    }

    #[test]
    fn lane_globals_include_the_lane_constructors_and_the_builtins() {
        let reference = Reference::new();
        let playbook = reference.globals(WorkflowType::Playbook);
        assert!(playbook.contains("route") && playbook.contains("len"));
        assert!(!playbook.contains("measure"));
        assert!(
            reference
                .globals(WorkflowType::Autoresearch)
                .contains("measure")
        );
        assert!(reference.builtin("len").is_some_and(|doc| !doc.is_empty()));
        assert!(reference.builtin("agent").is_none());
    }
}
