//! Lints. Silence one on a line with `# starlark-lint-disable <code>`.

use std::collections::BTreeSet;
use std::ops::Range;

use crucible::plan::diag::suggest;
use crucible::plan::workflow::WorkflowType;
use starlark::analysis::{AstModuleLint, EvalSeverity};
use starlark_syntax::codemap::{Pos, Span};
use starlark_syntax::syntax::AstModule;
use tower_lsp_server::ls_types::DiagnosticSeverity;

use crate::check::{At, Finding, Origin};
use crate::docs::Reference;
use crate::index::{Call, Index, ValueKind, bytes};
use crate::pack::{Pack, Want};

pub struct LintContext<'a> {
    pub reference: &'a Reference,
    /// The lane the pack's entry declares; `None` when there is no entry to ask.
    pub lane: Option<WorkflowType>,
    pub pack: &'a Pack,
    pub sandboxes: Option<&'a BTreeSet<String>>,
    /// Every parameter the pack provably reads, or `None` when a read cannot be ruled out.
    pub param_reads: Option<&'a BTreeSet<String>>,
}

/// Lints that repeat a compile error; dropped where the compiler reported the same place.
pub const SHADOWS_COMPILE: &[&str] = &[
    "using-undefined",
    "missing-pack-file",
    "wrong-lane",
    "unknown-argument",
];

const UNNECESSARY: &[&str] = &[
    "unused-load",
    "unused-assign",
    "unused-argument",
    "redundant-return",
    "redundant-continue",
    "unreachable",
];

#[derive(Clone, Copy)]
enum Default {
    Bool(bool),
    Str(&'static str),
    Int(i64),
}

const TASKS: &[&str] = &["agent", "skill", "command", "evaluate"];

/// Arguments written out at the value the compiler would give them anyway.
const DEFAULTS: &[(&[&str], &str, Default)] = &[
    (TASKS, "required", Default::Bool(true)),
    (TASKS, "isolated", Default::Bool(false)),
    (TASKS, "join", Default::Str("all")),
    (TASKS, "stage", Default::Str("iteration")),
    (TASKS, "needs", Default::Str("any")),
    (&["agent", "skill"], "repair", Default::Int(0)),
    (&["route"], "required", Default::Bool(true)),
    (&["route"], "join", Default::Str("all")),
    (&["route"], "stage", Default::Str("iteration")),
    (&["grade"], "join", Default::Str("passed")),
    (&["top_k", "report"], "required", Default::Bool(true)),
];

pub fn run(module: &AstModule, index: &Index, cx: &LintContext<'_>) -> Vec<Finding> {
    let mut out = starlark_lints(module, cx);
    let mut lints = Lints {
        index,
        cx,
        out: Vec::new(),
    };
    lints.shadowed_builtins();
    lints.calls();
    lints.loads();
    lints.unused_params();
    out.extend(lints.out.into_iter().filter(|finding| match &finding.at {
        At::Bytes(range) => !module.is_suppressed(&finding.code, span(range)),
        At::Chars { .. } => true,
    }));
    out
}

fn span(range: &Range<usize>) -> Span {
    let pos = |n: usize| Pos::new(u32::try_from(n).unwrap_or(u32::MAX));
    Span::new(pos(range.start), pos(range.end))
}

fn starlark_lints(module: &AstModule, cx: &LintContext<'_>) -> Vec<Finding> {
    let globals = cx
        .reference
        .globals(cx.lane.unwrap_or(WorkflowType::Custom))
        .clone()
        .into_iter()
        .collect();
    module
        .lint(Some(&globals))
        .into_iter()
        .filter_map(|lint| {
            let severity = match lint.severity {
                EvalSeverity::Error => DiagnosticSeverity::ERROR,
                EvalSeverity::Warning => DiagnosticSeverity::WARNING,
                EvalSeverity::Advice => DiagnosticSeverity::HINT,
                EvalSeverity::Disabled => match lint.short_name.as_str() {
                    "using-undefined" => DiagnosticSeverity::WARNING,
                    code if UNNECESSARY.contains(&code) => DiagnosticSeverity::HINT,
                    _ => return None,
                },
            };
            Some(Finding {
                at: At::Bytes(bytes(lint.location.span)),
                severity,
                unnecessary: UNNECESSARY.contains(&lint.short_name.as_str()),
                code: lint.short_name,
                origin: Origin::Starlark,
                message: lint.problem,
            })
        })
        .collect()
}

struct Lints<'a> {
    index: &'a Index,
    cx: &'a LintContext<'a>,
    out: Vec<Finding>,
}

impl Lints<'_> {
    fn push(
        &mut self,
        at: Range<usize>,
        severity: DiagnosticSeverity,
        code: &str,
        message: String,
    ) {
        self.out.push(Finding {
            at: At::Bytes(at),
            severity,
            code: code.to_owned(),
            origin: Origin::Crucible,
            message,
            unnecessary: code == "redundant-default" || code == "unused-param",
        });
    }

    /// A call reaches the DSL constructor only when no local binding has taken the name.
    fn is_constructor(&self, call: &Call) -> bool {
        self.cx.reference.function(&call.callee).is_some()
            && self.index.resolve(&call.callee, call.span.start).is_none()
    }

    /// Only names the lane installs count.
    fn shadowed_builtins(&mut self) {
        let reference = self.cx.reference;
        let globals = reference.globals(self.cx.lane.unwrap_or(WorkflowType::Custom));
        let shadows: Vec<_> = self
            .index
            .bindings
            .iter()
            .filter(|binding| globals.contains(&binding.name))
            .filter_map(|binding| {
                let what = if reference.function(&binding.name).is_some() {
                    "the DSL constructor"
                } else if reference.builtin(&binding.name).is_some() {
                    "the builtin"
                } else {
                    return None;
                };
                Some((
                    binding.span.clone(),
                    format!("`{}` shadows {what} `{}()`", binding.name, binding.name),
                ))
            })
            .collect();
        for (at, message) in shadows {
            self.push(at, DiagnosticSeverity::WARNING, "shadowed-builtin", message);
        }
    }

    fn calls(&mut self) {
        for call in &self.index.calls {
            if !self.is_constructor(call) {
                continue;
            }
            self.wrong_lane(call);
            self.unknown_arguments(call);
            self.redundant_defaults(call);
            self.pack_files(call);
            self.sandbox(call);
        }
    }

    fn wrong_lane(&mut self, call: &Call) {
        let (Some(lane), Some(function)) = (self.cx.lane, self.cx.reference.function(&call.callee))
        else {
            return;
        };
        if function.lane.includes(lane) {
            return;
        }
        let message = format!(
            "`{}()` is only available in the {}; this workflow is `type = \"{}\"`",
            call.callee,
            crate::docs::lanes(function.lane),
            lane_name(lane)
        );
        self.push(
            call.callee_span.clone(),
            DiagnosticSeverity::ERROR,
            "wrong-lane",
            message,
        );
    }

    fn unknown_arguments(&mut self, call: &Call) {
        let Some(function) = self.cx.reference.function(&call.callee) else {
            return;
        };
        let known: Vec<&str> = function.kwargs.iter().map(|k| k.name).collect();
        let unknown: Vec<_> = call
            .args
            .iter()
            .filter_map(|arg| arg.name.as_ref())
            .filter(|(name, _)| !known.contains(&name.as_str()))
            .map(|(name, at)| {
                let hint = suggest(name, known.iter().copied())
                    .map(|s| format!("; did you mean `{s}`?"))
                    .unwrap_or_default();
                (
                    at.clone(),
                    format!("`{}()` has no argument `{name}`{hint}", call.callee),
                )
            })
            .collect();
        for (at, message) in unknown {
            self.push(at, DiagnosticSeverity::ERROR, "unknown-argument", message);
        }
    }

    fn redundant_defaults(&mut self, call: &Call) {
        let mut found = Vec::new();
        for (functions, kwarg, default) in DEFAULTS {
            if !functions.contains(&call.callee.as_str()) {
                continue;
            }
            let Some(arg) = call
                .args
                .iter()
                .find(|a| a.name.as_ref().is_some_and(|(n, _)| n == kwarg))
            else {
                continue;
            };
            let same = match (default, &arg.value.kind) {
                (Default::Bool(d), ValueKind::Bool(v)) => d == v,
                (Default::Str(d), ValueKind::Str(v)) => d == v,
                (Default::Int(d), ValueKind::Int(v)) => d == v,
                _ => false,
            };
            if let (true, Some((_, name_at))) = (same, &arg.name) {
                let value = match default {
                    Default::Bool(true) => "True".to_owned(),
                    Default::Bool(false) => "False".to_owned(),
                    Default::Str(s) => format!("{s:?}"),
                    Default::Int(n) => n.to_string(),
                };
                found.push((
                    name_at.start..arg.value.span.end,
                    format!("`{kwarg}` is already `{value}` by default"),
                ));
            }
        }
        for (at, message) in found {
            self.push(at, DiagnosticSeverity::HINT, "redundant-default", message);
        }
    }

    fn pack_files(&mut self, call: &Call) {
        let path = match call.callee.as_str() {
            "prompt_file" => call
                .positional(0)
                .map(|v| (v, Want::File { extension: None })),
            "schema_file" => call
                .positional(0)
                .map(|v| (v, Want::File { extension: None })),
            "report" => call
                .kwarg("template")
                .map(|v| (v, Want::File { extension: None })),
            "skill" => call.kwarg("skill").map(|v| (v, Want::Skill)),
            _ => None,
        };
        if let Some((value, want)) = path
            && let Some(raw) = value.as_str()
        {
            let at = value.span.clone();
            self.pack_file(raw, at, want);
        }
    }

    fn loads(&mut self) {
        let loads: Vec<_> = self
            .index
            .loads
            .iter()
            .map(|load| (load.module.clone(), load.module_span.clone()))
            .collect();
        for (module, at) in loads {
            self.pack_file(&module, at, Want::File { extension: None });
        }
    }

    fn pack_file(&mut self, raw: &str, at: Range<usize>, want: Want) {
        let pack = self.cx.pack;
        let message = if pack.resolve(raw).is_none() {
            format!("`{raw}` must be a path inside the pack, without `..`")
        } else if pack.exists(raw, want) {
            return;
        } else {
            match want {
                Want::Skill => format!("`{raw}` has no {} in the pack", crate::pack::SKILL_FILE),
                Want::File { .. } => format!("`{raw}` does not exist in the pack"),
            }
        };
        self.push(at, DiagnosticSeverity::ERROR, "missing-pack-file", message);
    }

    fn sandbox(&mut self, call: &Call) {
        let Some(declared) = self.cx.sandboxes else {
            return;
        };
        if let Some(value) = call.kwarg("sandbox")
            && let Some(name) = value.as_str()
            && !declared.contains(name)
        {
            let hint = suggest(name, declared.iter().map(String::as_str))
                .map(|s| format!("; did you mean `{s}`?"))
                .unwrap_or_default();
            let message = format!("crucible.toml declares no `[agent.sandbox.{name}]`{hint}");
            self.push(
                value.span.clone(),
                DiagnosticSeverity::ERROR,
                "undeclared-sandbox",
                message,
            );
        }
    }

    fn unused_params(&mut self) {
        let Some(reads) = self.cx.param_reads else {
            return;
        };
        let unused: Vec<_> = self
            .index
            .params
            .iter()
            .filter(|param| !reads.contains(&param.name))
            .map(|param| {
                (
                    param.span.clone(),
                    format!(
                        "parameter `{}` is declared but no `param()` reads it",
                        param.name
                    ),
                )
            })
            .collect();
        for (at, message) in unused {
            self.push(at, DiagnosticSeverity::WARNING, "unused-param", message);
        }
    }
}

fn lane_name(lane: WorkflowType) -> &'static str {
    match lane {
        WorkflowType::Autoresearch => "autoresearch",
        WorkflowType::Custom => "custom",
        WorkflowType::Playbook => "playbook",
    }
}

/// Every parameter `index` reads; `None` when a computed `param()` name or a command or
/// evaluate task, which receives every parameter, makes that unknowable.
pub fn literal_param_reads(index: &Index) -> Option<BTreeSet<String>> {
    if index
        .calls
        .iter()
        .any(|call| matches!(call.callee.as_str(), "command" | "evaluate"))
    {
        return None;
    }
    index
        .param_reads
        .iter()
        .map(|(name, _)| name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use crucible::plan::starlark::compile_source_with;

    use crate::check::parse;
    use crate::lint::*;

    struct Fixture {
        _root: tempfile::TempDir,
        pack: Pack,
        reference: Reference,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().canonicalize().expect("canonical");
        std::fs::create_dir_all(dir.join("prompts")).expect("prompts");
        std::fs::create_dir_all(dir.join("skills/triage")).expect("skills");
        std::fs::write(
            dir.join("crucible.toml"),
            "[agent.sandbox.rust]\nimage = \"x\"\n",
        )
        .expect("manifest");
        std::fs::write(dir.join("prompts/a.md"), "a").expect("prompt");
        std::fs::write(dir.join("skills/triage/SKILL.md"), "s").expect("skill");
        std::fs::write(dir.join("lib.star"), "helper = 1\n").expect("lib");
        Fixture {
            _root: root,
            pack: Pack { dir },
            reference: Reference::new(),
        }
    }

    fn lint(fixture: &Fixture, source: &str, reads: Option<&BTreeSet<String>>) -> Vec<Finding> {
        let module = parse(source, &fixture.pack.dir.join("workflow.star")).expect("parses");
        let index = Index::build(&module);
        let sandboxes = fixture.pack.sandboxes();
        run(
            &module,
            &index,
            &LintContext {
                reference: &fixture.reference,
                lane: index.lane,
                pack: &fixture.pack,
                sandboxes: sandboxes.as_ref(),
                param_reads: reads,
            },
        )
    }

    fn coded<'a>(findings: &'a [Finding], code: &str) -> Vec<&'a Finding> {
        findings.iter().filter(|f| f.code == code).collect()
    }

    fn covered<'a>(source: &'a str, finding: &Finding) -> &'a str {
        match &finding.at {
            At::Bytes(range) => &source[range.clone()],
            At::Chars { .. } => "",
        }
    }

    #[test]
    fn a_binding_that_hides_a_constructor_or_builtin_is_flagged() {
        let fixture = fixture();
        let source = "agent = 1\ndef len(x):\n    return x\nfine = 2\n";
        let found = lint(&fixture, source, None);
        let shadows = coded(&found, "shadowed-builtin");
        let names: Vec<_> = shadows.iter().map(|f| covered(source, f)).collect();
        assert_eq!(names, ["agent", "len"]);
        assert!(
            shadows[0].message.contains("DSL constructor"),
            "{}",
            shadows[0].message
        );
    }

    #[test]
    fn a_name_the_lane_does_not_install_is_free_to_bind() {
        let fixture = fixture();
        let source = "score = evaluate(name = \"score\", run = \"x\")\nworkflow(type = \"autoresearch\", tasks = [score])\n";
        assert!(coded(&lint(&fixture, source, None), "shadowed-builtin").is_empty());
        let playbook = source.replace("autoresearch", "playbook");
        assert_eq!(
            coded(&lint(&fixture, &playbook, None), "shadowed-builtin").len(),
            1
        );
    }

    #[test]
    fn a_constructor_outside_its_lane_is_an_error_at_the_callee() {
        let fixture = fixture();
        let source = "m = measure(name = \"m\")\nworkflow(type = \"playbook\", tasks = [m])\n";
        let found = lint(&fixture, source, None);
        let wrong = coded(&found, "wrong-lane");
        assert_eq!(wrong.len(), 1);
        assert_eq!(covered(source, wrong[0]), "measure");
        assert!(
            wrong[0].message.contains("autoresearch and custom"),
            "{}",
            wrong[0].message
        );
        assert_eq!(wrong[0].severity, DiagnosticSeverity::ERROR);
    }

    #[test]
    fn every_unknown_argument_is_reported_with_a_suggestion() {
        let fixture = fixture();
        let source = "a = agent(name = \"a\", promt = \"p\", zzz = 1)\nworkflow(type = \"playbook\", tasks = [a])\n";
        let found = lint(&fixture, source, None);
        let unknown = coded(&found, "unknown-argument");
        let names: Vec<_> = unknown.iter().map(|f| covered(source, f)).collect();
        assert_eq!(names, ["promt", "zzz"]);
        assert!(
            unknown[0].message.contains("did you mean `prompt`"),
            "{}",
            unknown[0].message
        );
        assert!(
            !unknown[1].message.contains("did you mean"),
            "{}",
            unknown[1].message
        );
    }

    #[test]
    fn a_shadowed_constructor_is_not_checked_as_the_constructor() {
        let fixture = fixture();
        let source = "def agent(**kw):\n    return kw\nx = agent(anything = 1)\n";
        let found = lint(&fixture, source, None);
        assert!(coded(&found, "unknown-argument").is_empty(), "{found:?}");
    }

    #[test]
    fn arguments_spelled_at_their_default_are_unnecessary() {
        let fixture = fixture();
        let source = "a = command(name = \"a\", run = \"x\", required = True, join = \"all\", isolated = True)\n\
                      workflow(type = \"playbook\", tasks = [a])\n";
        let found = lint(&fixture, source, None);
        let redundant = coded(&found, "redundant-default");
        let spans: Vec<_> = redundant.iter().map(|f| covered(source, f)).collect();
        assert_eq!(spans, ["required = True", "join = \"all\""]);
        assert!(
            redundant
                .iter()
                .all(|f| f.unnecessary && f.severity == DiagnosticSeverity::HINT)
        );
    }

    #[test]
    fn pack_paths_that_name_nothing_are_errors_at_the_string() {
        let fixture = fixture();
        let source = "load(\"missing.star\", \"x\")\nload(\"lib.star\", \"helper\")\n\
                      p = prompt_file(\"prompts/nope.md\")\nq = prompt_file(\"prompts/a.md\")\n\
                      r = prompt_file(\"../escape.md\")\n\
                      s = skill(name = \"s\", skill = \"skills/none\")\nt = skill(name = \"t\", skill = \"skills/triage\")\n";
        let found = lint(&fixture, source, None);
        let missing = coded(&found, "missing-pack-file");
        let spans: Vec<_> = missing.iter().map(|f| covered(source, f)).collect();
        assert_eq!(
            spans,
            [
                "\"prompts/nope.md\"",
                "\"../escape.md\"",
                "\"skills/none\"",
                "\"missing.star\""
            ]
        );
        assert!(
            missing[1].message.contains("without `..`"),
            "{}",
            missing[1].message
        );
        assert!(
            missing[2].message.contains("SKILL.md"),
            "{}",
            missing[2].message
        );
    }

    #[test]
    fn a_sandbox_the_manifest_does_not_declare_is_an_error() {
        let fixture = fixture();
        let source = "a = agent(name = \"a\", prompt = \"p\", sandbox = \"rus\")\nb = agent(name = \"b\", prompt = \"p\", sandbox = \"rust\")\n";
        let found = lint(&fixture, source, None);
        let undeclared = coded(&found, "undeclared-sandbox");
        assert_eq!(undeclared.len(), 1);
        assert_eq!(covered(source, undeclared[0]), "\"rus\"");
        assert!(
            undeclared[0].message.contains("did you mean `rust`"),
            "{}",
            undeclared[0].message
        );
    }

    #[test]
    fn a_declared_parameter_nothing_reads_is_unused() {
        let fixture = fixture();
        let source = "params = {\"repo\": {\"type\": \"string\", \"required\": True}, \"depth\": {\"type\": \"int\", \"default\": 1}}\n\
                      a = agent(name = \"a\", prompt = \"p\" + param(\"repo\"))\n";
        let reads = BTreeSet::from(["repo".to_owned()]);
        let found = lint(&fixture, source, Some(&reads));
        let unused = coded(&found, "unused-param");
        assert_eq!(
            unused
                .iter()
                .map(|f| covered(source, f))
                .collect::<Vec<_>>(),
            ["\"depth\""]
        );
        assert!(
            coded(&lint(&fixture, source, None), "unused-param").is_empty(),
            "unknown reads lint nothing"
        );
    }

    #[test]
    fn computed_param_names_and_command_tasks_make_reads_unknowable() {
        let module = parse(
            "n = \"x\"\na = param(\"y\")\nb = param(n)\n",
            Path::new("w.star"),
        )
        .expect("parses");
        assert_eq!(literal_param_reads(&Index::build(&module)), None);
        let module = parse(
            "a = param(\"y\")\nc = command(name = \"c\", run = \"sh x.sh\")\n",
            Path::new("w.star"),
        )
        .expect("parses");
        assert_eq!(
            literal_param_reads(&Index::build(&module)),
            None,
            "a command reads params from its inputs"
        );
        let module = parse("a = param(\"y\")\n", Path::new("w.star")).expect("parses");
        assert_eq!(
            literal_param_reads(&Index::build(&module)),
            Some(BTreeSet::from(["y".to_owned()]))
        );
    }

    #[test]
    fn starlark_lints_run_with_the_lane_globals() {
        let fixture = fixture();
        let source = "load(\"lib.star\", \"helper\")\nx = undefined_thing\na = command(name = \"a\", run = \"x\")\nworkflow(type = \"playbook\", tasks = [a])\n";
        let found = lint(&fixture, source, None);
        let unused = coded(&found, "unused-load");
        assert_eq!(unused.len(), 1, "{found:?}");
        assert!(unused[0].unnecessary && unused[0].origin == Origin::Starlark);
        let undefined = coded(&found, "using-undefined");
        assert_eq!(
            undefined
                .iter()
                .map(|f| covered(source, f))
                .collect::<Vec<_>>(),
            ["undefined_thing"]
        );
    }

    #[test]
    fn a_disable_comment_silences_a_crucible_lint() {
        let fixture = fixture();
        let source = "a = command(name = \"a\", run = \"x\", required = True)  # starlark-lint-disable redundant-default\n";
        assert!(coded(&lint(&fixture, source, None), "redundant-default").is_empty());
    }

    /// Each [`DEFAULTS`] entry compiles to the same graph as leaving the argument out.
    #[test]
    fn every_listed_default_is_the_compilers_default() {
        let fixture = fixture();
        let template = |function: &str, extra: &str| -> String {
            match function {
                "agent" => format!(
                    "t = agent(name = \"t\", prompt = \"p\"{extra})\nworkflow(type = \"playbook\", tasks = [t])\n"
                ),
                "skill" => format!(
                    "t = skill(name = \"t\", skill = \"skills/triage\"{extra})\nworkflow(type = \"playbook\", tasks = [t])\n"
                ),
                "command" => format!(
                    "t = command(name = \"t\", run = \"true\"{extra})\nworkflow(type = \"playbook\", tasks = [t])\n"
                ),
                "evaluate" => format!(
                    "t = evaluate(name = \"t\", run = \"true\"{extra})\nworkflow(type = \"playbook\", tasks = [t])\n"
                ),
                "route" => format!(
                    "s = command(name = \"s\", run = \"true\", emits = {{\"ok\": \"boolean\"}})\n\
                     t = route(name = \"t\", questions = {{\"ok\": noul(ask = \"?\")}}, source = s, depends_on = [s]{extra})\n\
                     workflow(type = \"playbook\", tasks = [s, t])\n"
                ),
                "grade" => format!(
                    "e = evaluate(name = \"e\", run = \"true\")\nt = grade(name = \"t\", score = e, evidence = [e]{extra})\n\
                     workflow(type = \"custom\", tasks = [e, t])\n"
                ),
                "top_k" => format!(
                    "e = evaluate(name = \"e\", run = \"true\")\n\
                     t = top_k(name = \"t\", k = 1, direction = \"higher\", depends_on = [e]{extra})\n\
                     workflow(type = \"custom\", tasks = [e, t])\n"
                ),
                "report" => format!(
                    "t = report(name = \"t\", destination = {{\"kind\": \"slack\"}}, template = \"prompts/a.md\"{extra})\n\
                     workflow(type = \"playbook\", tasks = [t])\n"
                ),
                other => panic!("no template for {other}"),
            }
        };
        let entry = fixture.pack.dir.join("workflow.star");
        let compile = |source: &str| {
            compile_source_with(source, &entry, &fixture.pack.dir, &BTreeMap::new())
                .map(|compiled| compiled.canonical_json)
                .unwrap_or_else(|error| panic!("{source}\n{}", crucible::errors::report(&error)))
        };
        for (functions, kwarg, default) in DEFAULTS {
            let value = match default {
                Default::Bool(true) => "True".to_owned(),
                Default::Bool(false) => "False".to_owned(),
                Default::Str(s) => format!("{s:?}"),
                Default::Int(n) => n.to_string(),
            };
            for function in *functions {
                let bare = compile(&template(function, ""));
                let spelled = compile(&template(function, &format!(", {kwarg} = {value}")));
                assert_eq!(
                    bare, spelled,
                    "{function}({kwarg} = {value}) is not the default"
                );
            }
        }
    }
}
