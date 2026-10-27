//! Completion.

use std::collections::BTreeSet;
use std::ops::Range;

use crucible::plan::starlark::params::{FIELDS, ParamType};
use crucible::plan::starlark::reference::Function;
use crucible_contract::emits::FieldType;
use starlark_syntax::syntax::AstModule;
use tower_lsp_server::ls_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, Documentation, InsertTextFormat,
    MarkupContent, MarkupKind, TextEdit,
};

use crate::docs::{self, Reference, enum_values};
use crate::index::{BindingKind, Index, TASK_CONSTRUCTORS, ValueKind};
use crate::pack::Want;
use crate::scan::Literal;
use crate::snapshot::{Snapshot, StringSlot};

const KEYWORDS: &[&str] = &[
    "and", "break", "continue", "elif", "else", "for", "if", "in", "not", "or", "pass", "return",
];

pub struct Completer<'a> {
    pub reference: &'a Reference,
    pub snapshot: &'a Snapshot,
    pub snippets: bool,
    /// Reads another module of the pack, for the names a `load()` can import.
    pub read: &'a dyn Fn(&std::path::Path) -> Option<String>,
}

impl Completer<'_> {
    pub fn complete(&self, offset: usize) -> Vec<CompletionItem> {
        let structure = &self.snapshot.structure;
        if structure.in_comment(offset) {
            return Vec::new();
        }
        if let Some(literal) = structure.string_at(offset) {
            return self.in_string(literal, offset);
        }
        let word = self.snapshot.word_before(offset);
        if let Some(object) = self.snapshot.receiver(word.start) {
            return self.fields(&object, &word);
        }
        let call = structure.call_at(offset);
        if let Some(call) = call
            && structure.frame_at(offset) == Some(call)
            && let Some(frame) = structure.frame(call)
            && let Some(callee) = frame.callee.as_deref()
            && let Some(function) = self.constructor(callee, offset)
        {
            let slot = structure.slot_at(call, offset);
            let editing_key = slot
                .and_then(|slot| slot.key.as_ref())
                .is_none_or(|key| offset <= key.span.end);
            if editing_key {
                let used: BTreeSet<&str> = frame
                    .slots
                    .iter()
                    .filter(|s| slot.is_none_or(|current| current.span != s.span))
                    .filter_map(|s| s.key.as_ref().map(|k| k.name.as_str()))
                    .collect();
                let mut items = self.kwargs(function, &used, &word);
                if function.kwargs.is_empty() {
                    items.extend(self.expression(offset, &word, None));
                }
                return items;
            }
            let kwarg = slot
                .and_then(|slot| slot.key.as_ref())
                .and_then(|key| function.kwargs.iter().find(|k| k.name == key.name));
            return self.expression(offset, &word, kwarg.map(|k| k.ty));
        }
        self.expression(offset, &word, None)
    }

    /// The DSL constructor a call at `offset` reaches, unless a local binding took the name.
    fn constructor(&self, callee: &str, offset: usize) -> Option<&Function> {
        let shadowed = self
            .snapshot
            .index
            .as_ref()
            .is_some_and(|index| index.resolve(callee, offset).is_some());
        if shadowed {
            None
        } else {
            self.reference.function(callee)
        }
    }

    fn edit(&self, range: Range<usize>, new_text: String) -> Option<CompletionTextEdit> {
        Some(CompletionTextEdit::Edit(TextEdit {
            range: self.snapshot.text.range(range),
            new_text,
        }))
    }

    fn item(&self, label: &str, kind: CompletionItemKind, range: &Range<usize>) -> CompletionItem {
        CompletionItem {
            label: label.to_owned(),
            kind: Some(kind),
            text_edit: self.edit(range.clone(), label.to_owned()),
            ..CompletionItem::default()
        }
    }

    fn snippet(&self, mut item: CompletionItem, snippet: String) -> CompletionItem {
        if self.snippets {
            item.text_edit = match item.text_edit {
                Some(CompletionTextEdit::Edit(edit)) => Some(CompletionTextEdit::Edit(TextEdit {
                    range: edit.range,
                    new_text: snippet,
                })),
                other => other,
            };
            item.insert_text_format = Some(InsertTextFormat::SNIPPET);
        }
        item
    }

    fn kwargs(
        &self,
        function: &Function,
        used: &BTreeSet<&str>,
        word: &Range<usize>,
    ) -> Vec<CompletionItem> {
        function
            .kwargs
            .iter()
            .enumerate()
            .filter(|(_, kwarg)| !used.contains(kwarg.name))
            .map(|(rank, kwarg)| {
                let mut item = self.item(kwarg.name, CompletionItemKind::FIELD, word);
                item.detail = Some(kwarg.ty.to_owned());
                item.documentation = Some(docs::kwarg_documentation(kwarg));
                item.sort_text = Some(format!("0{rank:03}"));
                item.text_edit = self.edit(word.clone(), format!("{} = ", kwarg.name));
                self.snippet(item, format!("{} = $0", kwarg.name))
            })
            .collect()
    }

    /// Names that can start an expression at `offset`.
    fn expression(
        &self,
        offset: usize,
        word: &Range<usize>,
        ty: Option<&str>,
    ) -> Vec<CompletionItem> {
        let mut seen = BTreeSet::new();
        let mut items = Vec::new();
        if ty == Some("bool") {
            for value in ["True", "False"] {
                seen.insert(value.to_owned());
                let mut item = self.item(value, CompletionItemKind::CONSTANT, word);
                item.sort_text = Some(format!("00{value}"));
                items.push(item);
            }
        }
        if let Some(index) = &self.snapshot.index {
            let mut visible: Vec<_> = index
                .bindings
                .iter()
                .filter(|b| b.scope.as_ref().is_none_or(|scope| scope.contains(&offset)))
                .collect();
            visible.sort_by_key(|b| b.scope.as_ref().map_or(usize::MAX, Range::len));
            for binding in visible {
                if !seen.insert(binding.name.clone()) {
                    continue;
                }
                let (kind, detail) = describe(index, &binding.kind);
                let mut item = self.item(&binding.name, kind, word);
                item.detail = Some(detail);
                item.sort_text = Some(format!("1{}", binding.name));
                items.push(item);
            }
        }
        let lane = self.snapshot.context.lane;
        for function in &self.reference.functions {
            if lane.is_some_and(|lane| !function.lane.includes(lane))
                || !seen.insert(function.name.to_owned())
            {
                continue;
            }
            let mut item = self.item(function.name, CompletionItemKind::FUNCTION, word);
            item.detail = Some(docs::signature_label(function).0);
            item.documentation = Some(docs::function_documentation(function));
            item.sort_text = Some(format!("2{}", function.name));
            items.push(self.snippet(item, format!("{}($0)", function.name)));
        }
        for constant in ["True", "False", "None"] {
            if seen.insert(constant.to_owned()) {
                let mut item = self.item(constant, CompletionItemKind::CONSTANT, word);
                item.sort_text = Some(format!("3{constant}"));
                items.push(item);
            }
        }
        for name in self.reference.builtin_names() {
            if !seen.insert(name.to_owned()) {
                continue;
            }
            let mut item = self.item(name, CompletionItemKind::FUNCTION, word);
            item.documentation = self.reference.builtin(name).map(markdown);
            item.sort_text = Some(format!("3{name}"));
            items.push(item);
        }
        let dialect = crucible::plan::starlark::dialect();
        let gated = [
            ("def", dialect.enable_def),
            ("lambda", dialect.enable_lambda),
            ("load", dialect.enable_load),
        ];
        let keywords = KEYWORDS
            .iter()
            .copied()
            .chain(gated.iter().filter(|(_, on)| *on).map(|(k, _)| *k));
        for keyword in keywords {
            if seen.insert(keyword.to_owned()) {
                let mut item = self.item(keyword, CompletionItemKind::KEYWORD, word);
                item.sort_text = Some(format!("4{keyword}"));
                items.push(item);
            }
        }
        items
    }

    /// `review.` offers the fields `review` emits; `gate.` the questions a route asks.
    fn fields(&self, object: &str, word: &Range<usize>) -> Vec<CompletionItem> {
        let Some(index) = &self.snapshot.index else {
            return Vec::new();
        };
        let Some(call) = index.assigned_call(object) else {
            return Vec::new();
        };
        let (argument, detail) = match call.callee.as_str() {
            "route" => ("questions", format!("question of route {object}")),
            callee if TASK_CONSTRUCTORS.contains(&callee) => {
                ("emits", format!("emitted by {object}"))
            }
            _ => return Vec::new(),
        };
        let names: Vec<(String, Option<String>)> = match call.kwarg(argument).map(|v| &v.kind) {
            Some(ValueKind::List(items)) => items
                .iter()
                .filter_map(|v| Some((v.as_str()?.to_owned(), None)))
                .collect(),
            Some(ValueKind::Dict(entries)) => entries
                .iter()
                .filter_map(|(k, v)| Some((k.as_str()?.to_owned(), type_of(index, &v.kind))))
                .collect(),
            _ => Vec::new(),
        };
        names
            .into_iter()
            .map(|(name, ty)| {
                let mut item = self.item(&name, CompletionItemKind::FIELD, word);
                item.detail = Some(ty.map_or(detail.clone(), |ty| format!("{ty} · {detail}")));
                item
            })
            .collect()
    }

    fn in_string(&self, literal: &Literal, offset: usize) -> Vec<CompletionItem> {
        let typed = self.snapshot.text.slice(literal.content.start..offset);
        let whole = literal.content.start..offset;
        let values = |values: Vec<String>, kind: CompletionItemKind| -> Vec<CompletionItem> {
            values
                .iter()
                .map(|value| self.item(value, kind, &whole))
                .collect()
        };
        match self.snapshot.string_slot(literal) {
            StringSlot::Positional { callee, index: 0 } => match callee.as_str() {
                "prompt_file" => self.paths(typed, offset, Want::File { extension: None }),
                "schema_file" => self.paths(
                    typed,
                    offset,
                    Want::File {
                        extension: Some(".json"),
                    },
                ),
                "load" => self.paths(
                    typed,
                    offset,
                    Want::File {
                        extension: Some(".star"),
                    },
                ),
                "param" => self
                    .snapshot
                    .context
                    .params
                    .iter()
                    .map(|spec| {
                        let mut item = self.item(&spec.name, CompletionItemKind::VARIABLE, &whole);
                        item.detail = Some(spec.ty.as_str().to_owned());
                        item.documentation = Some(markdown(&docs::param_markdown(spec)));
                        item
                    })
                    .collect(),
                _ => Vec::new(),
            },
            StringSlot::Positional { callee, .. } if callee == "load" => {
                values(self.loadable(literal, offset), CompletionItemKind::VARIABLE)
            }
            StringSlot::Positional { .. } => Vec::new(),
            StringSlot::Kwarg { callee, name } => match (callee.as_str(), name.as_str()) {
                ("report", "template") => self.paths(typed, offset, Want::File { extension: None }),
                ("skill", "skill") => self.paths(typed, offset, Want::Skill),
                ("agent" | "skill", "sandbox") => values(
                    self.snapshot
                        .context
                        .sandboxes
                        .iter()
                        .flatten()
                        .cloned()
                        .collect(),
                    CompletionItemKind::ENUM_MEMBER,
                ),
                _ => self
                    .reference
                    .function(&callee)
                    .and_then(|f| f.kwargs.iter().find(|k| k.name == name))
                    .and_then(|kwarg| enum_values(kwarg.ty))
                    .map(|v| {
                        values(
                            v.into_iter().map(str::to_owned).collect(),
                            CompletionItemKind::ENUM_MEMBER,
                        )
                    })
                    .unwrap_or_default(),
            },
            StringSlot::ParamField { key: true, .. } => values(
                FIELDS.iter().map(|f| (*f).to_owned()).collect(),
                CompletionItemKind::PROPERTY,
            ),
            StringSlot::ParamField {
                field: Some(field), ..
            } if field == "type" => values(
                ParamType::ALL
                    .iter()
                    .map(|t| t.as_str().to_owned())
                    .collect(),
                CompletionItemKind::TYPE_PARAMETER,
            ),
            StringSlot::EmitsType => values(
                FieldType::SCALARS
                    .iter()
                    .map(|(name, _)| (*name).to_owned())
                    .collect(),
                CompletionItemKind::TYPE_PARAMETER,
            ),
            StringSlot::ParamField { .. } | StringSlot::Other => Vec::new(),
        }
    }

    fn paths(&self, typed: &str, offset: usize, want: Want) -> Vec<CompletionItem> {
        let Some(pack) = &self.snapshot.context.pack else {
            return Vec::new();
        };
        let segment = typed.rfind('/').map_or(0, |slash| slash + 1);
        let range = offset.saturating_sub(typed.len().saturating_sub(segment))..offset;
        pack.complete(typed, want)
            .into_iter()
            .map(|candidate| {
                let label = if candidate.dir {
                    format!("{}/", candidate.name)
                } else {
                    candidate.name
                };
                let kind = if candidate.dir {
                    CompletionItemKind::FOLDER
                } else {
                    CompletionItemKind::FILE
                };
                self.item(&label, kind, &range)
            })
            .collect()
    }

    /// The module-level names of the module a `load()` names in its first argument.
    fn loadable(&self, literal: &Literal, offset: usize) -> Vec<String> {
        let structure = &self.snapshot.structure;
        let module = literal
            .frame
            .and_then(|frame| self.snapshot.argument_string(frame, 0))
            .filter(|_| structure.call_at(offset).is_some());
        let Some(path) = module
            .zip(self.snapshot.context.pack.as_ref())
            .and_then(|(module, pack)| pack.resolve(module))
        else {
            return Vec::new();
        };
        let Some(source) = (self.read)(&path) else {
            return Vec::new();
        };
        let dialect = crucible::plan::starlark::dialect();
        let Ok(module) = AstModule::parse(&path.display().to_string(), source, &dialect) else {
            return Vec::new();
        };
        Index::build(&module)
            .bindings
            .into_iter()
            .filter(|b| b.scope.is_none() && !b.name.starts_with('_'))
            .map(|b| b.name)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

fn markdown(value: &str) -> Documentation {
    Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value: value.to_owned(),
    })
}

/// A field's declared type in a typed `emits`, as written.
fn type_of(index: &Index, value: &ValueKind) -> Option<String> {
    match value {
        ValueKind::Str(ty) => Some(ty.clone()),
        ValueKind::List(labels) => Some(
            labels
                .iter()
                .filter_map(|l| l.as_str().map(|s| format!("{s:?}")))
                .collect::<Vec<_>>()
                .join(" | "),
        ),
        ValueKind::Call(call) => index
            .call(*call)
            .filter(|c| c.callee == "schema_file")
            .map(|_| "schema".to_owned()),
        _ => None,
    }
}

/// The completion kind and one-line detail for a binding.
pub fn describe(index: &Index, kind: &BindingKind) -> (CompletionItemKind, String) {
    match kind {
        BindingKind::Assign { call: Some(call) } => match index.call(*call) {
            Some(call) => match call.declared_name() {
                Some(name) => (
                    CompletionItemKind::VARIABLE,
                    format!("{} {name:?}", call.callee),
                ),
                None => (
                    CompletionItemKind::VARIABLE,
                    format!("{}(...)", call.callee),
                ),
            },
            None => (CompletionItemKind::VARIABLE, String::new()),
        },
        BindingKind::Assign { call: None } => (CompletionItemKind::VARIABLE, "variable".to_owned()),
        BindingKind::Def => (CompletionItemKind::FUNCTION, "def".to_owned()),
        BindingKind::Load { module, original } => (
            CompletionItemKind::VARIABLE,
            format!("{original} from {module}"),
        ),
        BindingKind::Loop => (CompletionItemKind::VARIABLE, "loop variable".to_owned()),
        BindingKind::Parameter => (CompletionItemKind::VARIABLE, "parameter".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use crucible::plan::starlark::read_params;
    use crucible::plan::workflow::WorkflowType;

    use crate::check::parse;
    use crate::complete::*;
    use crate::pack::Pack;
    use crate::snapshot::PackContext;
    use crate::text::Text;

    struct Case {
        reference: Reference,
        _root: tempfile::TempDir,
        pack: Pack,
    }

    fn case() -> Case {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().to_path_buf();
        std::fs::create_dir_all(dir.join("prompts")).expect("prompts");
        std::fs::create_dir_all(dir.join("skills/triage")).expect("skills");
        std::fs::write(dir.join("prompts/review.md"), "r").expect("prompt");
        std::fs::write(dir.join("skills/triage/SKILL.md"), "s").expect("skill");
        std::fs::write(
            dir.join("lib.star"),
            "helper = 1\n_private = 2\ndef make():\n    return 3\n",
        )
        .expect("lib");
        Case {
            reference: Reference::new(),
            _root: root,
            pack: Pack { dir },
        }
    }

    impl Case {
        fn context(&self) -> PackContext {
            PackContext {
                pack: Some(self.pack.clone()),
                ..PackContext::default()
            }
        }

        /// Complete at `|` in `typed`, with `parsed` standing in for the last version that parsed.
        fn complete(
            &self,
            typed: &str,
            parsed: &str,
            context: PackContext,
            snippets: bool,
        ) -> Vec<CompletionItem> {
            let offset = typed.find('|').expect("cursor");
            let text = Arc::new(Text::new(typed.replacen('|', "", 1)));
            let index = parse(parsed, Path::new("workflow.star"))
                .ok()
                .map(|m| Arc::new(Index::build(&m)));
            let snapshot = Snapshot::new(None, text, index, Arc::new(context));
            let read = |path: &Path| std::fs::read_to_string(path).ok();
            Completer {
                reference: &self.reference,
                snapshot: &snapshot,
                snippets,
                read: &read,
            }
            .complete(offset)
        }

        fn labels(&self, typed: &str, parsed: &str) -> Vec<String> {
            labels(&self.complete(typed, parsed, self.context(), false))
        }
    }

    fn labels(items: &[CompletionItem]) -> Vec<String> {
        items.iter().map(|item| item.label.clone()).collect()
    }

    fn new_text(item: &CompletionItem) -> String {
        match &item.text_edit {
            Some(CompletionTextEdit::Edit(edit)) => edit.new_text.clone(),
            _ => String::new(),
        }
    }

    #[test]
    fn a_call_offers_the_arguments_not_yet_written() {
        let case = case();
        let items = case.complete("a = agent(name = \"a\", |)", "", case.context(), false);
        let names = labels(&items);
        assert!(names.contains(&"prompt".to_owned()));
        assert!(!names.contains(&"name".to_owned()), "already written");
        assert_eq!(
            names.first().map(String::as_str),
            Some("prompt"),
            "arguments come in reference order"
        );
        let prompt = items.iter().find(|i| i.label == "prompt").expect("prompt");
        assert_eq!(new_text(prompt), "prompt = ");
        assert!(
            !names.contains(&"len".to_owned()),
            "a named-only call offers only its arguments"
        );
    }

    #[test]
    fn arguments_written_after_the_cursor_count_as_used() {
        let case = case();
        let names = case.labels("agent(na|, prompt = \"p\")", "");
        assert!(names.contains(&"name".to_owned()));
        assert!(!names.contains(&"prompt".to_owned()));
    }

    #[test]
    fn snippets_place_the_cursor_after_the_equals() {
        let case = case();
        let items = case.complete("agent(|)", "", case.context(), true);
        let name = items.iter().find(|i| i.label == "name").expect("name");
        assert_eq!(new_text(name), "name = $0");
        assert_eq!(name.insert_text_format, Some(InsertTextFormat::SNIPPET));
        let items = case.complete("x = ag|", "", case.context(), true);
        let agent = items.iter().find(|i| i.label == "agent").expect("agent");
        assert_eq!(new_text(agent), "agent($0)");
    }

    #[test]
    fn the_lane_decides_which_constructors_are_offered() {
        let case = case();
        let mut playbook = case.context();
        playbook.lane = Some(WorkflowType::Playbook);
        let names = labels(&case.complete("x = |", "", playbook, false));
        assert!(names.contains(&"route".to_owned()) && !names.contains(&"measure".to_owned()));
        let names = case.labels("x = |", "");
        assert!(
            names.contains(&"route".to_owned()) && names.contains(&"measure".to_owned()),
            "a library sees every lane"
        );
    }

    #[test]
    fn module_names_and_builtins_are_in_scope() {
        let case = case();
        let parsed =
            "review = agent(name = \"review\", prompt = \"p\")\ndef helper(n):\n    return n\n";
        let items = case.complete(&format!("{parsed}x = |"), parsed, case.context(), false);
        let review = items.iter().find(|i| i.label == "review").expect("binding");
        assert_eq!(review.detail.as_deref(), Some("agent \"review\""));
        assert!(labels(&items).contains(&"helper".to_owned()));
        assert!(labels(&items).contains(&"len".to_owned()));
        assert!(
            !labels(&items).contains(&"n".to_owned()),
            "a parameter is local to its def"
        );
    }

    #[test]
    fn a_bool_argument_offers_true_and_false_first() {
        let case = case();
        let items = case.complete("agent(required = |)", "", case.context(), false);
        assert_eq!(labels(&items[..2]), ["True", "False"]);
    }

    #[test]
    fn a_literal_union_argument_offers_its_values() {
        let case = case();
        assert_eq!(
            case.labels("agent(join = \"|\")", ""),
            ["all", "passed", "settled"]
        );
        assert_eq!(
            case.labels("workflow(type = \"p|\")", ""),
            ["autoresearch", "custom", "playbook"]
        );
        assert!(case.labels("agent(prompt = \"|\")", "").is_empty());
    }

    #[test]
    fn path_arguments_complete_from_the_pack() {
        let case = case();
        let items = case.complete("p = prompt_file(\"prompts/r|\")", "", case.context(), false);
        assert_eq!(labels(&items), ["review.md"]);
        let Some(CompletionTextEdit::Edit(edit)) = &items[0].text_edit else {
            panic!("edit")
        };
        assert_eq!(
            edit.range.start.character,
            "p = prompt_file(\"prompts/".len() as u32,
            "only the last segment is replaced"
        );
        assert_eq!(
            case.labels("load(\"|\")", ""),
            ["lib.star", "prompts/", "skills/"]
        );
        assert_eq!(case.labels("skill(skill = \"skills/|\")", ""), ["triage/"]);
    }

    #[test]
    fn a_load_offers_the_public_names_of_the_module() {
        let case = case();
        assert_eq!(
            case.labels("load(\"lib.star\", \"|\")", ""),
            ["helper", "make"]
        );
    }

    #[test]
    fn param_names_come_from_the_entry() {
        let case = case();
        let source = "params = {\"repo\": {\"type\": \"string\", \"required\": True, \"doc\": \"Where.\"}}\n";
        let mut context = case.context();
        context.params = read_params(source, Path::new("workflow.star"))
            .expect("params")
            .specs()
            .to_vec();
        let items = case.complete("x = param(\"|\")", "", context, false);
        assert_eq!(labels(&items), ["repo"]);
        assert_eq!(items[0].detail.as_deref(), Some("string"));
    }

    #[test]
    fn sandboxes_come_from_the_manifest() {
        let case = case();
        let mut context = case.context();
        context.sandboxes = Some(["go".to_owned(), "rust".to_owned()].into());
        assert_eq!(
            labels(&case.complete("agent(sandbox = \"|\")", "", context, false)),
            ["go", "rust"]
        );
    }

    #[test]
    fn a_params_block_offers_fields_and_types() {
        let case = case();
        assert_eq!(case.labels("params = {\"repo\": {\"|\"}}", ""), FIELDS);
        assert_eq!(
            case.labels("params = {\"repo\": {\"type\": \"|\"}}", ""),
            ["string", "int", "number", "bool", "list<string>"]
        );
    }

    #[test]
    fn typed_emits_offer_the_field_types() {
        let case = case();
        let names = case.labels("agent(emits = {\"verdict\": \"|\"})", "");
        assert!(
            names.contains(&"string".to_owned()) && names.contains(&"links".to_owned()),
            "{names:?}"
        );
    }

    #[test]
    fn a_task_variable_offers_the_fields_it_emits() {
        let case = case();
        let parsed = "review = agent(name = \"review\", prompt = \"p\", emits = {\"verdict\": [\"ok\", \"bad\"], \"links\": \"links\"})\n\
                      gate = route(name = \"gate\", questions = {\"ship\": noul(ask = \"?\")})\n";
        let items = case.complete(
            &format!("{parsed}x = command(name = \"x\", run = \"y\", over = review.|"),
            parsed,
            case.context(),
            false,
        );
        assert_eq!(labels(&items), ["verdict", "links"]);
        assert_eq!(
            items[0].detail.as_deref(),
            Some("\"ok\" | \"bad\" · emitted by review")
        );
        assert_eq!(
            case.labels(&format!("{parsed}x = agent(when = gate.|"), parsed),
            ["ship"]
        );
    }

    #[test]
    fn nothing_is_offered_inside_a_comment() {
        let case = case();
        assert!(case.labels("# agent(|", "").is_empty());
    }

    #[test]
    fn a_local_function_named_like_a_constructor_gets_no_constructor_arguments() {
        let case = case();
        let parsed = "def agent(**kw):\n    return kw\n";
        let names = case.labels(&format!("{parsed}agent(|)"), parsed);
        assert!(!names.contains(&"prompt".to_owned()));
    }
}
