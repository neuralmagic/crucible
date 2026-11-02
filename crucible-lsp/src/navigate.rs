//! What is at a position: hover text, where it is defined, and the document's outline.

use std::ops::Range;
use std::path::{Path, PathBuf};

use crucible::plan::starlark::dialect;
use starlark_syntax::syntax::AstModule;
use tower_lsp_server::ls_types::{DocumentSymbol, SymbolKind};

use crate::complete::describe;
use crate::docs::{self, Reference};
use crate::index::{Binding, BindingKind, Index, TASK_CONSTRUCTORS, ValueKind};
use crate::pack::Want;
use crate::scan::Literal;
use crate::snapshot::{Snapshot, StringSlot};
use crate::text::Text;

/// A place in some file of the pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub path: PathBuf,
    pub bytes: Range<usize>,
}

const PREVIEW_LINES: usize = 12;

pub struct Navigator<'a> {
    pub reference: &'a Reference,
    pub snapshot: &'a Snapshot,
    pub read: &'a dyn Fn(&Path) -> Option<String>,
}

/// The pack file a path-valued string names, and what it must be.
fn path_want(slot: &StringSlot) -> Option<Want> {
    match slot {
        StringSlot::Positional { callee, index: 0 } => match callee.as_str() {
            "prompt_file" | "schema_file" | "load" => Some(Want::File { extension: None }),
            _ => None,
        },
        StringSlot::Kwarg { callee, name } => match (callee.as_str(), name.as_str()) {
            ("report", "template") => Some(Want::File { extension: None }),
            ("skill", "skill") => Some(Want::Skill),
            _ => None,
        },
        _ => None,
    }
}

impl Navigator<'_> {
    fn literal_text(&self, literal: &Literal) -> &str {
        self.snapshot.text.slice(literal.content.clone())
    }

    pub fn hover(&self, offset: usize) -> Option<(String, Range<usize>)> {
        let snapshot = self.snapshot;
        if let Some(literal) = snapshot.structure.string_at(offset) {
            return self
                .hover_string(literal)
                .map(|text| (text, literal.span.clone()));
        }
        let word = snapshot.word_at(offset);
        if word.is_empty() {
            return None;
        }
        let name = snapshot.text.slice(word.clone());
        if let Some(object) = snapshot.receiver(word.start) {
            return self.hover_field(&object, name).map(|text| (text, word));
        }
        let index = snapshot.index.as_deref();
        let binding = index.and_then(|index| index.resolve(name, offset));
        let call = snapshot
            .structure
            .call_at(offset)
            .and_then(|call| snapshot.structure.frame(call));
        if let Some(frame) = call
            && frame
                .slots
                .iter()
                .any(|slot| slot.key.as_ref().is_some_and(|key| key.span == word))
            && let Some(callee) = frame.callee.as_deref()
            && index.is_none_or(|index| index.resolve(callee, offset).is_none())
            && let Some(function) = self.reference.function(callee)
            && let Some(kwarg) = function.kwargs.iter().find(|k| k.name == name)
        {
            return Some((docs::kwarg_markdown(function, kwarg), word));
        }
        if let (Some(binding), Some(index)) = (binding, index) {
            return Some((self.hover_binding(index, binding), word));
        }
        if let Some(function) = self.reference.function(name) {
            return Some((docs::function_markdown(function), word));
        }
        self.reference
            .builtin(name)
            .map(|doc| (doc.to_owned(), word))
    }

    fn hover_string(&self, literal: &Literal) -> Option<String> {
        let slot = self.snapshot.string_slot(literal);
        let raw = self.literal_text(literal);
        if let StringSlot::Positional { callee, index: 0 } = &slot
            && callee == "param"
        {
            let spec = self
                .snapshot
                .context
                .params
                .iter()
                .find(|s| s.name == raw)?;
            return Some(docs::param_markdown(spec));
        }
        if let Some(want) = path_want(&slot) {
            let pack = self.snapshot.context.pack.as_ref()?;
            let path = pack.target(raw, want)?;
            let preview: Vec<String> = (self.read)(&path)?
                .lines()
                .take(PREVIEW_LINES)
                .map(str::to_owned)
                .collect();
            let fence = if raw.ends_with(".star") { "python" } else { "" };
            return Some(format!(
                "`{}`\n\n```{fence}\n{}\n```",
                path.display(),
                preview.join("\n")
            ));
        }
        if let StringSlot::Kwarg { callee, name } = &slot {
            let function = self.reference.function(callee)?;
            let kwarg = function.kwargs.iter().find(|k| k.name == name)?;
            return docs::enum_values(kwarg.ty).map(|_| docs::kwarg_markdown(function, kwarg));
        }
        None
    }

    fn hover_field(&self, object: &str, field: &str) -> Option<String> {
        let index = self.snapshot.index.as_deref()?;
        let call = index.assigned_call(object)?;
        let what = if call.callee == "route" {
            "question"
        } else {
            "field"
        };
        Some(format!(
            "`{object}.{field}`: {what} of {} `{}`",
            call.callee,
            call.declared_name().unwrap_or(object)
        ))
    }

    fn hover_binding(&self, index: &Index, binding: &Binding) -> String {
        let (_, detail) = describe(index, &binding.kind);
        let line = line_of(&self.snapshot.text, binding.span.start);
        let mut out = format!("```python\n{}\n```\n\n*{detail}*", line.trim());
        if let BindingKind::Assign { call: Some(call) } = binding.kind
            && let Some(call) = index.call(call)
            && let Some(ValueKind::List(fields)) = call.kwarg("emits").map(|v| &v.kind)
        {
            let fields: Vec<_> = fields.iter().filter_map(|f| f.as_str()).collect();
            out.push_str(&format!("\n\nEmits: `{}`", fields.join("`, `")));
        }
        out
    }

    /// Every use in this document of the binding at `offset`, with the binding itself first when
    /// `declaration` is set.
    pub fn references(&self, offset: usize, declaration: bool) -> Vec<Range<usize>> {
        let snapshot = self.snapshot;
        let Some(index) = snapshot.index.as_deref() else {
            return Vec::new();
        };
        let name = snapshot.text.slice(snapshot.word_at(offset));
        let Some(binding) = index.resolve(name, offset) else {
            return Vec::new();
        };
        let same = |at: usize| {
            index
                .resolve(name, at)
                .is_some_and(|b| b.span == binding.span)
        };
        let uses = index
            .identifiers
            .iter()
            .filter(|(used, at)| used == name && same(at.start))
            .map(|(_, at)| at.clone());
        declaration
            .then(|| binding.span.clone())
            .into_iter()
            .chain(uses)
            .collect()
    }

    pub fn definition(&self, offset: usize) -> Option<Target> {
        let snapshot = self.snapshot;
        let here = snapshot.path.clone()?;
        let index = snapshot.index.as_deref();
        if let Some(literal) = snapshot.structure.string_at(offset) {
            let raw = self.literal_text(literal);
            let slot = snapshot.string_slot(literal);
            if let Some(want) = path_want(&slot) {
                let path = snapshot.context.pack.as_ref()?.target(raw, want)?;
                return Some(Target { path, bytes: 0..0 });
            }
            if let StringSlot::Positional { callee, index: 0 } = &slot
                && callee == "param"
            {
                let decl = snapshot
                    .context
                    .param_decls
                    .iter()
                    .find(|d| d.name == raw)?;
                let path = snapshot.context.entry.clone()?;
                return Some(Target {
                    path,
                    bytes: decl.span.clone(),
                });
            }
            if let StringSlot::Positional { callee, .. } = &slot
                && callee == "load"
            {
                let frame = literal.frame?;
                let module = snapshot.argument_string(frame, 0)?;
                return self.in_module(module, raw);
            }
            let task = index?.task_named(raw)?;
            let at = task.kwarg("name")?.span.clone();
            return Some(Target {
                path: here,
                bytes: at,
            });
        }
        let word = snapshot.word_at(offset);
        let name = snapshot.text.slice(word.clone());
        if name.is_empty() {
            return None;
        }
        let index = index?;
        if let Some(object) = snapshot.receiver(word.start) {
            let call = index.assigned_call(&object)?;
            let argument = if call.callee == "route" {
                "questions"
            } else {
                "emits"
            };
            let at = match &call.kwarg(argument)?.kind {
                ValueKind::List(items) => items
                    .iter()
                    .find(|v| v.as_str() == Some(name))?
                    .span
                    .clone(),
                ValueKind::Dict(entries) => entries
                    .iter()
                    .find(|(k, _)| k.as_str() == Some(name))?
                    .0
                    .span
                    .clone(),
                _ => return None,
            };
            return Some(Target {
                path: here,
                bytes: at,
            });
        }
        let binding = index.resolve(name, offset)?;
        if let BindingKind::Load { module, original } = &binding.kind {
            return self.in_module(module, original).or(Some(Target {
                path: here,
                bytes: binding.span.clone(),
            }));
        }
        Some(Target {
            path: here,
            bytes: binding.span.clone(),
        })
    }

    /// The module-level definition of `name` in the pack module `module`.
    fn in_module(&self, module: &str, name: &str) -> Option<Target> {
        let path = self.snapshot.context.pack.as_ref()?.resolve(module)?;
        let source = (self.read)(&path)?;
        let parsed = AstModule::parse(&path.display().to_string(), source, &dialect()).ok()?;
        let index = Index::build(&parsed);
        let binding = index
            .bindings
            .iter()
            .rev()
            .find(|b| b.name == name && b.scope.is_none())?;
        Some(Target {
            path,
            bytes: binding.span.clone(),
        })
    }
}

fn line_of(text: &Text, offset: usize) -> &str {
    let source = text.as_str();
    let start = source
        .get(..offset)
        .and_then(|s| s.rfind('\n'))
        .map_or(0, |at| at + 1);
    let end = source
        .get(offset..)
        .and_then(|s| s.find('\n'))
        .map_or(source.len(), |at| offset + at);
    source.get(start..end).unwrap_or("")
}

/// Tasks by name, sessions, `def`s, declared parameters, and the `workflow()` call, in source
/// order.
pub fn symbols(index: &Index, text: &Text) -> Vec<DocumentSymbol> {
    let symbol =
        |range: &Range<usize>, selection: &Range<usize>, name: &str, detail: &str, kind| {
            #[allow(deprecated)]
            DocumentSymbol {
                name: name.to_owned(),
                detail: (!detail.is_empty()).then(|| detail.to_owned()),
                kind,
                tags: None,
                deprecated: None,
                range: text.range(range.clone()),
                selection_range: text.range(selection.clone()),
                children: None,
            }
        };
    let mut found = Vec::new();
    for call in &index.calls {
        let kind = match call.callee.as_str() {
            callee if TASK_CONSTRUCTORS.contains(&callee) => SymbolKind::EVENT,
            "session" => SymbolKind::OBJECT,
            "workflow" | "default_autoresearch" => {
                found.push(symbol(
                    &call.span,
                    &call.callee_span,
                    &call.callee,
                    "",
                    SymbolKind::MODULE,
                ));
                continue;
            }
            _ => continue,
        };
        if let (Some(name), Some(value)) = (call.declared_name(), call.kwarg("name")) {
            found.push(symbol(&call.span, &value.span, name, &call.callee, kind));
        }
    }
    let defs = index
        .bindings
        .iter()
        .filter(|b| b.scope.is_none() && b.kind == BindingKind::Def);
    for binding in defs {
        found.push(symbol(
            &binding.span,
            &binding.span,
            &binding.name,
            "def",
            SymbolKind::FUNCTION,
        ));
    }
    for param in &index.params {
        found.push(symbol(
            &param.span,
            &param.span,
            &param.name,
            "param",
            SymbolKind::PROPERTY,
        ));
    }
    found.sort_by_key(|s| (s.range.start.line, s.range.start.character));
    found
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crucible::plan::starlark::read_params;

    use crate::check::parse;
    use crate::navigate::*;
    use crate::pack::Pack;
    use crate::snapshot::PackContext;

    const SOURCE: &str = "params = {\"repo\": {\"type\": \"string\", \"required\": True, \"doc\": \"Which repo.\"}}\n\
load(\"lib.star\", \"make\")\n\
review = agent(name = \"review\", prompt = prompt_file(\"prompts/review.md\"), emits = [\"verdict\"])\n\
gate = command(name = \"gate\", run = \"x\" , depends_on = [\"review\"], over = review.verdict)\n\
def helper(n):\n    return n\n\
workflow(type = \"playbook\", tasks = [review, gate, make(param(\"repo\"))])\n";

    struct Case {
        _root: tempfile::TempDir,
        entry: PathBuf,
        snapshot: Snapshot,
        reference: Reference,
    }

    fn case() -> Case {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().to_path_buf();
        std::fs::create_dir_all(dir.join("prompts")).expect("prompts");
        std::fs::write(
            dir.join("prompts/review.md"),
            "Review the change.\nCarefully.\n",
        )
        .expect("prompt");
        std::fs::write(
            dir.join("lib.star"),
            "def make(r):\n    return command(name = \"m\", run = r)\n",
        )
        .expect("lib");
        let entry = dir.join("workflow.star");
        std::fs::write(&entry, SOURCE).expect("entry");
        let index = Index::build(&parse(SOURCE, &entry).expect("parses"));
        let context = PackContext {
            pack: Some(Pack { dir }),
            lane: index.lane,
            entry: Some(entry.clone()),
            params: read_params(SOURCE, &entry)
                .expect("params")
                .specs()
                .to_vec(),
            param_decls: index.params.clone(),
            sandboxes: None,
        };
        let snapshot = Snapshot::new(
            Some(entry.clone()),
            Arc::new(Text::new(SOURCE.to_owned())),
            Some(Arc::new(index)),
            Arc::new(context),
        );
        Case {
            _root: root,
            entry,
            snapshot,
            reference: Reference::new(),
        }
    }

    fn read(path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    impl Case {
        fn navigator(&self) -> Navigator<'_> {
            Navigator {
                reference: &self.reference,
                snapshot: &self.snapshot,
                read: &read,
            }
        }

        /// The offset `n` bytes into the first occurrence of `needle`.
        fn at(&self, needle: &str, n: usize) -> usize {
            SOURCE.find(needle).expect("needle") + n
        }

        fn hover(&self, needle: &str, n: usize) -> String {
            self.navigator()
                .hover(self.at(needle, n))
                .map(|(text, _)| text)
                .unwrap_or_default()
        }

        fn definition(&self, needle: &str, n: usize) -> Option<(PathBuf, String)> {
            let target = self.navigator().definition(self.at(needle, n))?;
            let source = read(&target.path)?;
            Some((target.path, source[target.bytes].to_owned()))
        }
    }

    #[test]
    fn hover_explains_constructors_and_their_arguments() {
        let case = case();
        let agent = case.hover("agent(", 2);
        assert!(
            agent.contains("An agent turn driven by a prompt."),
            "{agent}"
        );
        let emits = case.hover("emits =", 1);
        assert!(emits.starts_with("```python\nemits: "), "{emits}");
        assert!(emits.contains("`agent()` argument"), "{emits}");
        assert!(
            case.hover("def helper", 1).is_empty(),
            "a keyword has no hover"
        );
    }

    #[test]
    fn hover_on_a_string_explains_what_it_names() {
        let case = case();
        let param = case.hover("param(\"repo", 8);
        assert!(
            param.contains("Which repo.") && param.contains("Required"),
            "{param}"
        );
        let prompt = case.hover("prompts/review.md", 3);
        assert!(prompt.contains("Review the change."), "{prompt}");
    }

    #[test]
    fn hover_on_a_variable_shows_where_it_was_bound() {
        let case = case();
        let review = case.hover("[review, gate", 2);
        assert!(review.contains("review = agent("), "{review}");
        assert!(review.contains("Emits: `verdict`"), "{review}");
        let field = case.hover("review.verdict", 9);
        assert_eq!(field, "`review.verdict`: field of agent `review`");
    }

    #[test]
    fn definitions_resolve_variables_fields_and_task_names() {
        let case = case();
        let (path, text) = case.definition("[review, gate", 2).expect("variable");
        assert_eq!((path, text.as_str()), (case.entry.clone(), "review"));
        let (_, text) = case.definition("review.verdict", 9).expect("field");
        assert_eq!(text, "\"verdict\"");
        let (_, text) = case.definition("[\"review\"]", 3).expect("task name");
        assert_eq!(text, "\"review\"");
        let (_, text) = case.definition("param(\"repo", 8).expect("param");
        assert_eq!(text, "\"repo\"");
    }

    #[test]
    fn definitions_follow_loads_and_pack_paths() {
        let case = case();
        let (path, text) = case.definition("make(param", 1).expect("loaded");
        assert!(path.ends_with("lib.star"));
        assert_eq!(text, "make");
        let (path, _) = case.definition("\"make\")", 2).expect("load symbol");
        assert!(path.ends_with("lib.star"));
        let (path, _) = case.definition("prompts/review.md", 2).expect("prompt");
        assert!(path.ends_with("prompts/review.md"));
    }

    #[test]
    fn references_find_every_use_of_the_same_binding() {
        let case = case();
        let found = |needle: &str, n: usize, declaration: bool| -> Vec<(usize, String)> {
            case.navigator()
                .references(case.at(needle, n), declaration)
                .into_iter()
                .map(|at| (at.start, SOURCE[at].to_owned()))
                .collect()
        };
        let review = found("[review, gate", 2, true);
        assert_eq!(review.len(), 3, "{review:?}");
        assert!(review.iter().all(|(_, text)| text == "review"));
        assert_eq!(
            review[0].0,
            SOURCE.find("review =").expect("binding"),
            "the binding comes first"
        );
        assert_eq!(found("review =", 1, false).len(), 2);
        let n = found("return n", 7, true);
        assert_eq!(n.len(), 2, "the parameter and its one use: {n:?}");
        assert!(
            found("agent(", 1, true).is_empty(),
            "a constructor is not a binding"
        );
    }

    #[test]
    fn the_outline_lists_tasks_defs_params_and_the_workflow() {
        let case = case();
        let index = case.snapshot.index.as_deref().expect("index");
        let names: Vec<(String, Option<String>)> = symbols(index, &case.snapshot.text)
            .into_iter()
            .map(|s| (s.name, s.detail))
            .collect();
        let expected: Vec<(String, Option<String>)> = [
            ("repo", Some("param")),
            ("review", Some("agent")),
            ("gate", Some("command")),
            ("helper", Some("def")),
            ("workflow", None),
        ]
        .into_iter()
        .map(|(n, d)| (n.to_owned(), d.map(str::to_owned)))
        .collect();
        assert_eq!(names, expected);
    }
}
