//! The `calls` block: the targets a workflow may call, read without evaluating the source.
//!
//! ```text
//! params = {...}            # optional
//! calls = {
//!     "ci": {"kind": "github_workflow", "repo": "o/r", "workflow": ".github/workflows/x.yml",
//!            "ref": "main", "outputs": {"artifact": "result", "fields": {"ok": "boolean"}}},
//! }
//! ```

use std::collections::BTreeMap;

use crucible_contract::calls::{CallAlias, CallTarget};
use serde_json::{Map, Number, Value};
use starlark_syntax::syntax::AstModule;
use starlark_syntax::syntax::ast::{AssignTarget, AstLiteral, Expr, Stmt};

use crate::plan::starlark::error::CompileError;

type Result<T> = std::result::Result<T, CompileError>;

/// The names a source's header may bind, in any order, before its first other statement.
const HEADER: [&str; 2] = ["params", "calls"];

/// The leading statements of `module`.
pub(crate) fn statements(module: &AstModule) -> &[starlark_syntax::syntax::ast::AstStmt] {
    let statement = module.statement();
    match &statement.node {
        Stmt::Statements(statements) => statements,
        _ => std::slice::from_ref(statement),
    }
}

/// The top-level name a statement assigns, when it is a plain `name = ...`.
pub(crate) fn assigned(statement: &Stmt) -> Option<&str> {
    match statement {
        Stmt::Assign(assign) => match &assign.lhs.node {
            AssignTarget::Identifier(name) => Some(name.node.ident.as_str()),
            _ => None,
        },
        _ => None,
    }
}

/// Where `name` is assigned at the top level, and whether every statement before it is
/// another header block.
pub(crate) fn header_position(module: &AstModule, name: &str) -> Option<(usize, bool)> {
    let statements = statements(module);
    let position = statements
        .iter()
        .position(|s| assigned(&s.node) == Some(name))?;
    let leading = statements[..position]
        .iter()
        .all(|s| assigned(&s.node).is_some_and(|n| HEADER.contains(&n) && n != name));
    Some((position, leading))
}

/// The `calls` block of `module`: every target by its alias, or none when it declares none.
pub(crate) fn read(module: &AstModule) -> Result<BTreeMap<CallAlias, CallTarget>> {
    let Some((position, leading)) = header_position(module, "calls") else {
        return Ok(BTreeMap::new());
    };
    if !leading {
        return Err(CompileError::CallsNotLeading);
    }
    let Stmt::Assign(assign) = &statements(module)[position].node else {
        return Ok(BTreeMap::new());
    };
    let not_literal = |detail: String| CompileError::CallsNotLiteral { detail };
    let Expr::Dict(entries) = &assign.rhs.node else {
        return Err(not_literal("calls must be a dictionary literal".into()));
    };
    let mut calls = BTreeMap::new();
    for (key, value) in entries {
        let Expr::Literal(AstLiteral::String(name)) = &key.node else {
            return Err(not_literal(
                "every target name must be a string literal".into(),
            ));
        };
        let alias = CallAlias::parse(&name.node)
            .map_err(|e| not_literal(format!("target {:?}: {e}", name.node)))?;
        let json = literal(&value.node).ok_or_else(|| {
            not_literal(format!(
                "target {alias} must be built from dictionaries, lists, strings, numbers and \
                 booleans"
            ))
        })?;
        let target: CallTarget = serde_json::from_value(json)
            .map_err(|e| not_literal(format!("target {alias}: {e}")))?;
        if calls.insert(alias.clone(), target).is_some() {
            return Err(not_literal(format!("target {alias} is declared twice")));
        }
    }
    Ok(calls)
}

/// A literal expression as JSON, or `None` when it is anything else.
fn literal(expression: &Expr) -> Option<Value> {
    match expression {
        Expr::Literal(AstLiteral::String(s)) => Some(Value::String(s.node.clone())),
        Expr::Literal(AstLiteral::Int(n)) => match &n.node {
            starlark_syntax::lexer::TokenInt::I32(v) => Some(Value::from(*v)),
            starlark_syntax::lexer::TokenInt::BigInt(_) => None,
        },
        Expr::Literal(AstLiteral::Float(f)) => Number::from_f64(f.node).map(Value::Number),
        Expr::Minus(inner) => match literal(&inner.node)? {
            Value::Number(n) => n.as_i64().map(|i| Value::from(-i)).or_else(|| {
                n.as_f64()
                    .and_then(|f| Number::from_f64(-f))
                    .map(Value::Number)
            }),
            _ => None,
        },
        Expr::Identifier(name) => match name.node.ident.as_str() {
            "True" => Some(Value::Bool(true)),
            "False" => Some(Value::Bool(false)),
            _ => None,
        },
        Expr::List(items) | Expr::Tuple(items) => items
            .iter()
            .map(|i| literal(&i.node))
            .collect::<Option<Vec<_>>>()
            .map(Value::Array),
        Expr::Dict(entries) => {
            let mut map = Map::new();
            for (key, value) in entries {
                let Expr::Literal(AstLiteral::String(key)) = &key.node else {
                    return None;
                };
                map.insert(key.node.clone(), literal(&value.node)?);
            }
            Some(Value::Object(map))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::plan::starlark::error::CompileError;
    use crate::plan::starlark::targets::read;
    use crucible_contract::calls::{CallKind, CallTarget};
    use starlark_syntax::syntax::AstModule;

    fn parse(source: &str) -> AstModule {
        AstModule::parse(
            "workflow.star",
            source.to_owned(),
            &crate::plan::starlark::dialect(),
        )
        .unwrap()
    }

    const TARGETS: &str = r#"
calls = {
    "child": {"kind": "playbook", "playbook": "draft:calls-child"},
    "ci": {
        "kind": "github_workflow",
        "repo": "acme/app",
        "workflow": ".github/workflows/release.yml",
        "ref": "main",
        "outputs": {"artifact": "result", "fields": {"ok": "boolean", "legs": "integer"}, "files": ["notes.md"]},
    },
}
"#;

    #[test]
    fn a_calls_block_reads_every_target_without_evaluating() {
        let calls = read(&parse(TARGETS)).unwrap();
        let kinds: Vec<(&str, CallKind)> =
            calls.iter().map(|(a, t)| (a.as_str(), t.kind())).collect();
        assert_eq!(
            kinds,
            [
                ("child", CallKind::Playbook),
                ("ci", CallKind::GithubWorkflow)
            ]
        );
        let Some(CallTarget::GithubWorkflow { git_ref, .. }) = calls.values().nth(1) else {
            panic!("ci is a workflow target");
        };
        assert_eq!(git_ref.as_str(), "main");
    }

    #[test]
    fn the_header_takes_params_and_calls_in_either_order() {
        let params = "params = {\"v\": {\"type\": \"string\"}}\n";
        for source in [format!("{params}{TARGETS}"), format!("{TARGETS}{params}")] {
            assert_eq!(read(&parse(&source)).unwrap().len(), 2, "{source}");
        }
        assert!(read(&parse("x = 1\n")).unwrap().is_empty());
    }

    #[test]
    fn a_calls_block_that_is_late_or_computed_is_refused() {
        let late = format!("x = 1\n{TARGETS}");
        assert!(matches!(
            read(&parse(&late)),
            Err(CompileError::CallsNotLeading)
        ));
        for source in [
            "calls = dict(ci = {})\n",
            "calls = {\"ci\": {\"kind\": \"playbook\", \"playbook\": name}}\n",
            "calls = {\"ci\": {\"kind\": \"playbook\", \"playbook\": \"a\" + \"b\"}}\n",
            "calls = {\"ci\": {\"kind\": \"nope\"}}\n",
            "calls = {\"ci\": {\"kind\": \"playbook\", \"playbook\": \"t\", \"extra\": 1}}\n",
            "calls = {\"c i\": {\"kind\": \"playbook\", \"playbook\": \"t\"}}\n",
        ] {
            assert!(
                matches!(
                    read(&parse(source)),
                    Err(CompileError::CallsNotLiteral { .. })
                ),
                "{source}"
            );
        }
    }
}
