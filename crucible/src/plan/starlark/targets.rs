//! Call targets: the `@crucible//calls` constructors, and the targets a workflow names, read
//! without evaluating the source.
//!
//! ```text
//! load("@crucible//calls", "gitlab_pipeline", "job_outputs")
//!
//! shared = gitlab_pipeline(host = "gitlab.example.com", project = "g/p", ref = "main")
//!
//! a = call(name = "a", target = shared, ...)              # alias "shared"
//! b = call(name = "b", target = gitlab_pipeline(...), ...) # alias "b"
//! ```
//!
//! A target bound to a top-level name is declared under that name; one written inline in a
//! `call()` or `watch()` is declared under the task's name. Every argument is a literal or another
//! `@crucible//calls` value, so the controller can pin the targets before anything runs.

use std::collections::BTreeMap;

use crucible_contract::calls::{
    CallAlias, CallTarget, GithubOutputs, GitlabOutputs, TektonExpected,
};
use serde_json::{Map, Number, Value as Json};
use starlark_syntax::syntax::AstModule;
use starlark_syntax::syntax::ast::{
    ArgumentP, AssignTarget, AstExpr, AstLiteral, AstNoPayload, Expr, Stmt,
};
use starlark_syntax::syntax::uniplate::Visit;

use crate::plan::starlark::error::CompileError;

type Result<T> = std::result::Result<T, CompileError>;

pub(crate) const MODULE: &str = "@crucible//calls";

pub(crate) const TARGETS: [&str; 4] = ["playbook", "github_workflow", "gitlab_pipeline", "tekton"];

pub(crate) const PARTS: [&str; 3] = ["artifact_outputs", "job_outputs", "pipeline_run"];

pub(crate) const ENUMS: [(&str, &[&str]); 1] = [("event", &["push", "pull_request"])];

pub(crate) fn exported() -> impl Iterator<Item = &'static str> {
    TARGETS
        .into_iter()
        .chain(PARTS)
        .chain(ENUMS.into_iter().map(|(name, _)| name))
}

/// What one argument of a constructor takes.
#[derive(Clone, Copy)]
enum Slot {
    Plain,
    Part(&'static str),
    Parts(&'static str),
}

fn slots(ctor: &str) -> &'static [(&'static str, Slot)] {
    match ctor {
        "playbook" => &[("ref", Slot::Plain)],
        "github_workflow" => &[
            ("repo", Slot::Plain),
            ("workflow", Slot::Plain),
            ("ref", Slot::Plain),
            ("outputs", Slot::Part("artifact_outputs")),
        ],
        "gitlab_pipeline" => &[
            ("host", Slot::Plain),
            ("project", Slot::Plain),
            ("ref", Slot::Plain),
            ("outputs", Slot::Part("job_outputs")),
        ],
        "tekton" => &[
            ("cluster", Slot::Plain),
            ("namespace", Slot::Plain),
            ("repository", Slot::Plain),
            ("event", Slot::Part("event")),
            ("expected", Slot::Parts("pipeline_run")),
        ],
        "artifact_outputs" => &[
            ("artifact", Slot::Plain),
            ("fields", Slot::Plain),
            ("files", Slot::Plain),
        ],
        "job_outputs" => &[
            ("job", Slot::Plain),
            ("fields", Slot::Plain),
            ("files", Slot::Plain),
        ],
        "pipeline_run" => &[("name", Slot::Plain), ("results", Slot::Plain)],
        _ => &[],
    }
}

pub(crate) fn arguments(ctor: &str) -> Vec<&'static str> {
    slots(ctor).iter().map(|(name, _)| *name).collect()
}

pub(crate) fn leading(ctor: &str) -> Option<&'static str> {
    match ctor {
        "playbook" => Some("ref"),
        "pipeline_run" => Some("name"),
        _ => None,
    }
}

/// One argument as a constructor receives it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Arg {
    Json(Json),
    Part(&'static str, Json),
    List(Vec<Arg>),
}

/// A value an `@crucible//calls` name built, as the evaluator carries it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Built {
    pub(crate) ctor: &'static str,
    pub(crate) json: Json,
}

impl Built {
    pub(crate) fn target(&self) -> Option<std::result::Result<CallTarget, String>> {
        TARGETS
            .contains(&self.ctor)
            .then(|| serde_json::from_value(self.json.clone()).map_err(|e| e.to_string()))
    }
}

fn invalid(ctor: &str, detail: impl std::fmt::Display) -> CompileError {
    CompileError::CallTargetInvalid {
        detail: format!("{ctor}(): {detail}"),
    }
}

pub(crate) fn known(name: &str) -> Option<&'static str> {
    exported().find(|n| *n == name)
}

pub(crate) fn member(namespace: &str, name: &str) -> Result<Built> {
    let Some((ctor, members)) = ENUMS.iter().find(|(n, _)| *n == namespace) else {
        return Err(invalid(namespace, "not an enum"));
    };
    if !members.contains(&name) {
        return Err(CompileError::CallTargetInvalid {
            detail: format!("{namespace}.{name} is not a member; use one of {}", {
                let shown: Vec<String> =
                    members.iter().map(|m| format!("{namespace}.{m}")).collect();
                shown.join(", ")
            }),
        });
    }
    Ok(Built {
        ctor,
        json: Json::String(name.to_owned()),
    })
}

fn slot_value(ctor: &str, arg: &str, slot: Slot, value: Arg) -> Result<Json> {
    let expected = |part: &str| {
        invalid(
            ctor,
            format!(
                "{arg} takes {}",
                match part {
                    "event" => "an event member such as event.push".to_owned(),
                    part => format!("{part}(...)"),
                }
            ),
        )
    };
    match (slot, value) {
        (Slot::Plain, Arg::Json(json)) => Ok(json),
        (Slot::Plain, Arg::List(items)) => items
            .into_iter()
            .map(|item| match item {
                Arg::Json(json) => Ok(json),
                _ => Err(invalid(ctor, format!("{arg} takes plain values"))),
            })
            .collect::<Result<Vec<_>>>()
            .map(Json::Array),
        (Slot::Plain, Arg::Part(part, _)) => {
            Err(invalid(ctor, format!("{arg} does not take a {part} value")))
        }
        (Slot::Part(want), Arg::Part(got, json)) if want == got => Ok(json),
        (Slot::Part(want), _) => Err(expected(want)),
        (Slot::Parts(want), Arg::List(items)) => items
            .into_iter()
            .map(|item| match item {
                Arg::Part(got, json) if got == want => Ok(json),
                _ => Err(expected(want)),
            })
            .collect::<Result<Vec<_>>>()
            .map(Json::Array),
        (Slot::Parts(want), _) => Err(invalid(ctor, format!("{arg} takes a list of {want}(...)"))),
    }
}

pub(crate) fn build(ctor: &'static str, named: BTreeMap<String, Arg>) -> Result<Built> {
    let table = slots(ctor);
    let mut object = Map::new();
    for (arg, value) in named {
        let Some((_, slot)) = table.iter().find(|(name, _)| *name == arg) else {
            return Err(invalid(
                ctor,
                format!(
                    "no argument {arg:?}; it takes {}",
                    arguments(ctor).join(", ")
                ),
            ));
        };
        if matches!(value, Arg::Json(Json::Null)) {
            continue;
        }
        object.insert(arg.clone(), slot_value(ctor, &arg, *slot, value)?);
    }
    let checked =
        |result: std::result::Result<(), serde_json::Error>| result.map_err(|e| invalid(ctor, e));
    let json = match ctor {
        "playbook" => {
            let reference = object
                .remove("ref")
                .ok_or_else(|| invalid(ctor, "ref is required"))?;
            serde_json::json!({"kind": "playbook", "playbook": reference})
        }
        "artifact_outputs" => {
            let json = Json::Object(object);
            checked(serde_json::from_value::<GithubOutputs>(json.clone()).map(drop))?;
            json
        }
        "job_outputs" => {
            let json = Json::Object(object);
            checked(serde_json::from_value::<GitlabOutputs>(json.clone()).map(drop))?;
            json
        }
        "pipeline_run" => {
            let json = Json::Object(object);
            checked(serde_json::from_value::<TektonExpected>(json.clone()).map(drop))?;
            json
        }
        kind => {
            object.insert("kind".to_owned(), Json::String(kind.to_owned()));
            Json::Object(object)
        }
    };
    let built = Built { ctor, json };
    if let Some(target) = built.target() {
        target.map_err(|e| invalid(ctor, e))?;
    }
    Ok(built)
}

fn imports(module: &AstModule) -> BTreeMap<String, &'static str> {
    let mut bound = BTreeMap::new();
    for load in module.loads() {
        if load.module_id != MODULE {
            continue;
        }
        for (local, export) in &load.symbols {
            if let Some(export) = known(export) {
                bound.insert((*local).to_owned(), export);
            }
        }
    }
    bound
}

struct Reader {
    imports: BTreeMap<String, &'static str>,
    shared: BTreeMap<String, CallTarget>,
    declared: BTreeMap<CallAlias, CallTarget>,
    error: Option<CompileError>,
}

fn not_literal(detail: impl Into<String>) -> CompileError {
    CompileError::CallTargetNotLiteral {
        detail: detail.into(),
    }
}

impl Reader {
    fn constructor(&self, expression: &Expr) -> Option<&'static str> {
        match expression {
            Expr::Identifier(name) => self.imports.get(name.node.ident.as_str()).copied(),
            _ => None,
        }
    }

    fn arg(&self, expression: &Expr) -> Result<Arg> {
        match expression {
            Expr::Call(function, arguments) => {
                let ctor = self
                    .constructor(&function.node)
                    .filter(|ctor| PARTS.contains(ctor))
                    .ok_or_else(|| {
                        not_literal(format!(
                            "a target argument is a literal or a value from {MODULE}"
                        ))
                    })?;
                let built = self.built(ctor, &arguments.args)?;
                Ok(Arg::Part(built.ctor, built.json))
            }
            Expr::Dot(namespace, attribute) => match self.constructor(&namespace.node) {
                Some(enumeration) if ENUMS.iter().any(|(n, _)| *n == enumeration) => {
                    let built = member(enumeration, &attribute.node)?;
                    Ok(Arg::Part(built.ctor, built.json))
                }
                _ => Err(not_literal(format!(
                    "a target argument is a literal or a value from {MODULE}"
                ))),
            },
            Expr::List(items) | Expr::Tuple(items) => items
                .iter()
                .map(|item| self.arg(&item.node))
                .collect::<Result<Vec<_>>>()
                .map(Arg::List),
            other => literal(other).map(Arg::Json).ok_or_else(|| {
                not_literal(
                    "target arguments are built from dictionaries, lists, strings, numbers, \
                     booleans and values from @crucible//calls",
                )
            }),
        }
    }

    fn built(
        &self,
        ctor: &'static str,
        arguments: &[starlark_syntax::syntax::ast::AstArgument],
    ) -> Result<Built> {
        let mut named = BTreeMap::new();
        for (index, argument) in arguments.iter().enumerate() {
            match &argument.node {
                ArgumentP::Named(name, value) => {
                    named.insert(name.node.clone(), self.arg(&value.node)?);
                }
                ArgumentP::Positional(value) if index == 0 && leading(ctor).is_some() => {
                    let Some(slot) = leading(ctor) else {
                        continue;
                    };
                    named.insert(slot.to_owned(), self.arg(&value.node)?);
                }
                _ => {
                    return Err(not_literal(format!(
                        "{ctor}() takes named arguments{}",
                        leading(ctor)
                            .map(|slot| format!(" after its {slot}"))
                            .unwrap_or_default()
                    )));
                }
            }
        }
        build(ctor, named)
    }

    fn target(&self, expression: &Expr) -> Option<Result<CallTarget>> {
        let Expr::Call(function, arguments) = expression else {
            return None;
        };
        let ctor = self
            .constructor(&function.node)
            .filter(|ctor| TARGETS.contains(ctor))?;
        Some(self.built(ctor, &arguments.args).and_then(|built| {
            built
                .target()
                .unwrap_or_else(|| Err(format!("{ctor} builds no target")))
                .map_err(|e| invalid(ctor, e))
        }))
    }

    fn declare(&mut self, alias: &str, target: CallTarget) -> Result<()> {
        let alias =
            CallAlias::parse(alias).map_err(|e| not_literal(format!("target {alias:?}: {e}")))?;
        match self.declared.get(&alias) {
            Some(existing) if *existing != target => Err(not_literal(format!(
                "two different targets are declared as {alias}"
            ))),
            _ => {
                self.declared.insert(alias, target);
                Ok(())
            }
        }
    }

    fn use_site(&mut self, arguments: &[starlark_syntax::syntax::ast::AstArgument]) -> Result<()> {
        let named = |key: &str| {
            arguments.iter().find_map(|argument| match &argument.node {
                ArgumentP::Named(name, value) if name.node == key => Some(value),
                _ => None,
            })
        };
        let Some(target) = named("target") else {
            return Ok(());
        };
        if let Expr::Identifier(name) = &target.node
            && let Some(shared) = self.shared.get(name.node.ident.as_str())
        {
            let shared = shared.clone();
            return self.declare(&name.node.ident, shared);
        }
        let Some(built) = self.target(&target.node) else {
            return Err(not_literal(format!(
                "a target is a {MODULE} constructor such as gitlab_pipeline(...), written in \
                 the call or bound to a top-level name in workflow.star"
            )));
        };
        let built = built?;
        let name = match named("name").map(|value| &value.node) {
            Some(Expr::Literal(AstLiteral::String(name))) => name.node.clone(),
            _ => {
                return Err(not_literal(
                    "a call with its target written inline needs a literal name",
                ));
            }
        };
        self.declare(&name, built)
    }

    fn visit(&mut self, node: Visit<'_, AstNoPayload>) {
        if self.error.is_some() {
            return;
        }
        if let Visit::Expr(expression) = &node
            && let Expr::Call(function, arguments) = &expression.node
            && let Expr::Identifier(name) = &function.node
            && matches!(name.node.ident.as_str(), "call" | "watch")
            && let Err(error) = self.use_site(&arguments.args)
        {
            self.error = Some(error);
            return;
        }
        node.visit_children(|child| self.visit(child));
    }
}

fn statements(module: &AstModule) -> &[starlark_syntax::syntax::ast::AstStmt] {
    let statement = module.statement();
    match &statement.node {
        Stmt::Statements(statements) => statements,
        _ => std::slice::from_ref(statement),
    }
}

fn assigned_expr(statement: &Stmt) -> Option<(&str, &AstExpr)> {
    match statement {
        Stmt::Assign(assign) => match &assign.lhs.node {
            AssignTarget::Identifier(name) => Some((name.node.ident.as_str(), &assign.rhs)),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn read(module: &AstModule) -> Result<BTreeMap<CallAlias, CallTarget>> {
    let mut reader = Reader {
        imports: imports(module),
        shared: BTreeMap::new(),
        declared: BTreeMap::new(),
        error: None,
    };
    if reader.imports.is_empty() {
        return Ok(BTreeMap::new());
    }
    for statement in statements(module) {
        let Some((name, value)) = assigned_expr(&statement.node) else {
            continue;
        };
        if let Some(target) = reader.target(&value.node) {
            let target = target?;
            reader.shared.insert(name.to_owned(), target.clone());
            reader.declare(name, target)?;
        }
    }
    reader.visit(Visit::Stmt(module.statement()));
    match reader.error {
        Some(error) => Err(error),
        None => Ok(reader.declared),
    }
}

pub(crate) fn alias_of(
    declared: &BTreeMap<CallAlias, CallTarget>,
    task: &str,
    target: &CallTarget,
) -> Option<CallAlias> {
    if let Ok(own) = CallAlias::parse(task)
        && declared.get(&own) == Some(target)
    {
        return Some(own);
    }
    declared
        .iter()
        .find(|(_, declared)| *declared == target)
        .map(|(alias, _)| alias.clone())
}

fn literal(expression: &Expr) -> Option<Json> {
    match expression {
        Expr::Literal(AstLiteral::String(s)) => Some(Json::String(s.node.clone())),
        Expr::Literal(AstLiteral::Int(n)) => match &n.node {
            starlark_syntax::lexer::TokenInt::I32(v) => Some(Json::from(*v)),
            starlark_syntax::lexer::TokenInt::BigInt(_) => None,
        },
        Expr::Literal(AstLiteral::Float(f)) => Number::from_f64(f.node).map(Json::Number),
        Expr::Minus(inner) => match literal(&inner.node)? {
            Json::Number(n) => n.as_i64().map(|i| Json::from(-i)).or_else(|| {
                n.as_f64()
                    .and_then(|f| Number::from_f64(-f))
                    .map(Json::Number)
            }),
            _ => None,
        },
        Expr::Identifier(name) => match name.node.ident.as_str() {
            "True" => Some(Json::Bool(true)),
            "False" => Some(Json::Bool(false)),
            "None" => Some(Json::Null),
            _ => None,
        },
        Expr::List(items) | Expr::Tuple(items) => items
            .iter()
            .map(|i| literal(&i.node))
            .collect::<Option<Vec<_>>>()
            .map(Json::Array),
        Expr::Dict(entries) => {
            let mut map = Map::new();
            for (key, value) in entries {
                let Expr::Literal(AstLiteral::String(key)) = &key.node else {
                    return None;
                };
                map.insert(key.node.clone(), literal(&value.node)?);
            }
            Some(Json::Object(map))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::plan::starlark::error::CompileError;
    use crate::plan::starlark::targets::{alias_of, read};
    use crucible_contract::calls::{CallKind, CallTarget, PacEvent};
    use starlark_syntax::syntax::AstModule;

    fn parse(source: &str) -> AstModule {
        AstModule::parse(
            "workflow.star",
            source.to_owned(),
            &crate::plan::starlark::dialect(),
        )
        .unwrap()
    }

    const LOAD: &str = "load(\"@crucible//calls\", \"artifact_outputs\", \"event\", \
                        \"github_workflow\", \"gitlab_pipeline\", \"job_outputs\", \"pipeline_run\", \
                        \"playbook\", \"tekton\")\n";

    fn targets(body: &str) -> Result<Vec<(String, CallKind)>, CompileError> {
        read(&parse(&format!("{LOAD}{body}"))).map(|calls| {
            calls
                .iter()
                .map(|(a, t)| (a.as_str().to_owned(), t.kind()))
                .collect()
        })
    }

    #[test]
    fn a_shared_target_is_declared_under_its_name_and_an_inline_one_under_its_task() {
        let declared = targets(
            r#"
ci = github_workflow(
    repo = "acme/app",
    workflow = ".github/workflows/release.yml",
    ref = "main",
    outputs = artifact_outputs(artifact = "result", fields = {"ok": "boolean"}),
)
a = call(name = "a", target = ci, params = {})
b = call(name = "b", target = ci, params = {})
child = call(name = "child", target = playbook("draft:triage"), params = {})
deploy = call(
    name = "deploy",
    target = gitlab_pipeline(host = "gitlab.example.com", project = "g/p", ref = "main",
                             outputs = job_outputs(job = "report")),
)
"#,
        )
        .unwrap();
        assert_eq!(
            declared,
            [
                ("child".to_owned(), CallKind::Playbook),
                ("ci".to_owned(), CallKind::GithubWorkflow),
                ("deploy".to_owned(), CallKind::GitlabPipeline),
            ]
        );
    }

    #[test]
    fn a_tekton_target_reads_its_event_and_runs() {
        let calls = read(&parse(&format!(
            "{LOAD}k = tekton(cluster = \"c\", namespace = \"n\", repository = \"r\", \
             event = event.pull_request, expected = [pipeline_run(\"server\", results = [\"IMAGE\"])])\n\
             w = watch(name = \"w\", target = k, commit = \"abc\")\n"
        )))
        .unwrap();
        let Some(CallTarget::Tekton {
            event, expected, ..
        }) = calls.values().next()
        else {
            panic!("k is a tekton target");
        };
        assert_eq!(*event, Some(PacEvent::PullRequest));
        assert_eq!(expected[0].name, "server");
        assert_eq!(expected[0].results, ["IMAGE"]);
    }

    #[test]
    fn a_renamed_import_still_declares_its_target() {
        let source = "load(\"@crucible//calls\", gl = \"gitlab_pipeline\")\n\
                      t = call(name = \"t\", target = gl(host = \"h.example.com\", project = \"g/p\", ref = \"main\"))\n";
        assert_eq!(read(&parse(source)).unwrap().len(), 1);
    }

    #[test]
    fn a_source_without_the_module_declares_nothing() {
        assert!(read(&parse("x = 1\n")).unwrap().is_empty());
    }

    #[test]
    fn a_target_that_cannot_be_read_without_evaluating_is_refused() {
        for body in [
            "t = call(name = \"t\", target = \"ci\")\n",
            "r = \"main\"\nt = call(name = \"t\", target = gitlab_pipeline(host = \"h\", project = \"g/p\", ref = r))\n",
            "n = \"t\"\nt = call(name = n, target = playbook(\"x\"))\n",
            "t = call(name = \"t\", target = make())\n",
        ] {
            assert!(
                matches!(
                    targets(body),
                    Err(CompileError::CallTargetNotLiteral { .. })
                ),
                "{body}: {:?}",
                targets(body)
            );
        }
    }

    #[test]
    fn a_misbuilt_target_is_refused_with_its_constructor() {
        for (body, says) in [
            (
                "t = call(name = \"t\", target = gitlab_pipeline(host = \"h\", project = \"g/p\", ref = \"main\", outputs = artifact_outputs(artifact = \"a\")))\n",
                "outputs takes job_outputs(...)",
            ),
            (
                "t = call(name = \"t\", target = gitlab_pipeline(host = \"h\", project = \"g/p\", ref = \"main\", outputs = {\"job\": \"r\"}))\n",
                "outputs takes job_outputs(...)",
            ),
            (
                "t = call(name = \"t\", target = playbook(\"x\", branch = \"main\"))\n",
                "no argument \"branch\"",
            ),
            (
                "k = tekton(cluster = \"c\", namespace = \"n\", repository = \"r\", event = event.merge)\n",
                "event.push, event.pull_request",
            ),
            (
                "t = call(name = \"t\", target = github_workflow(repo = \"acme/app\", workflow = \".github/workflows/w.yml\", ref = \"main\"))\n",
                "outputs",
            ),
        ] {
            let error = targets(body).unwrap_err().to_string();
            assert!(error.contains(says), "{body}: {error}");
        }
    }

    #[test]
    fn a_target_constructor_used_without_its_load_names_the_load() {
        let pack = tempfile::tempdir().unwrap();
        let error = crate::plan::starlark::compile_source(
            "t = call(name = \"t\", target = playbook(\"x\"))\nworkflow(type = \"playbook\", tasks = [t])\n",
            std::path::Path::new("workflow.star"),
            pack.path(),
        )
        .unwrap_err();
        let error = crate::errors::report(&error);
        assert!(
            error.contains("load(\"@crucible//calls\", \"playbook\")"),
            "{error}"
        );
    }

    #[test]
    fn an_unknown_builtin_module_names_the_ones_there_are() {
        let pack = tempfile::tempdir().unwrap();
        let error = crate::plan::starlark::compile_source(
            "load(\"@crucible//cals\", \"playbook\")\nworkflow(type = \"playbook\", tasks = [])\n",
            std::path::Path::new("workflow.star"),
            pack.path(),
        )
        .unwrap_err();
        let error = crate::errors::report(&error);
        assert!(error.contains("@crucible//calls"), "{error}");
    }

    #[test]
    fn two_different_targets_under_one_alias_are_refused() {
        let body = "t = call(name = \"t\", target = playbook(\"a\"))\n\
                    u = call(name = \"t\", target = playbook(\"b\"))\n";
        assert!(
            targets(body)
                .unwrap_err()
                .to_string()
                .contains("two different targets")
        );
    }

    #[test]
    fn a_target_value_finds_its_alias_by_task_then_by_value() {
        let calls = read(&parse(&format!(
            "{LOAD}shared = playbook(\"a\")\n\
             one = call(name = \"one\", target = playbook(\"b\"))\n\
             two = call(name = \"two\", target = shared)\n"
        )))
        .unwrap();
        let b = calls
            .values()
            .find(|t| matches!(t, CallTarget::Playbook { .. }))
            .cloned();
        let (Some(one), Some(shared)) = (
            calls
                .iter()
                .find(|(a, _)| a.as_str() == "one")
                .map(|(_, t)| t.clone()),
            calls
                .iter()
                .find(|(a, _)| a.as_str() == "shared")
                .map(|(_, t)| t.clone()),
        ) else {
            panic!("both are declared: {calls:?} {b:?}");
        };
        assert_eq!(alias_of(&calls, "one", &one).unwrap().as_str(), "one");
        assert_eq!(alias_of(&calls, "two", &shared).unwrap().as_str(), "shared");
        assert!(
            alias_of(
                &calls,
                "x",
                &CallTarget::Playbook {
                    playbook: crucible_contract::calls::PlaybookRef::parse("zzz").unwrap(),
                }
            )
            .is_none()
        );
    }
}
