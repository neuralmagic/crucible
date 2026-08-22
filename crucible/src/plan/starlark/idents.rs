//! An AST pre-pass, and the starlark-error mapping that depends on it.
//!
//! starlark resolves names before it evaluates, so an undefined identifier arrives as an
//! `ErrorKind::Scope` with the name quoted in its message and no clue whether the author meant a
//! DSL constructor or a variable. Reading the callee names out of the AST answers that, and the
//! module's bindings feed the did-you-mean.

use std::collections::BTreeSet;

use starlark_syntax::syntax::AstModule;
use starlark_syntax::syntax::ast::{AssignTarget, AstNoPayload, Expr, Stmt};
use starlark_syntax::syntax::uniplate::Visit;

use crate::plan::diag;
use crate::plan::starlark::{CompileError, DSL_FUNCTIONS};

#[derive(Debug, Default)]
pub(crate) struct Idents {
    callees: BTreeSet<String>,
    bindings: BTreeSet<String>,
}

pub(crate) fn scan(module: &AstModule) -> Idents {
    let mut idents = Idents::default();
    walk(Visit::Stmt(module.statement()), &mut idents);
    idents
}

fn walk<'a>(node: Visit<'a, AstNoPayload>, idents: &mut Idents) {
    match &node {
        Visit::Stmt(statement) => match &statement.node {
            Stmt::Assign(assign) => {
                if let AssignTarget::Identifier(name) = &assign.lhs.node {
                    idents.bindings.insert(name.node.ident.clone());
                }
            }
            Stmt::Def(def) => {
                idents.bindings.insert(def.name.node.ident.clone());
            }
            Stmt::Load(load) => {
                for argument in &load.args {
                    idents.bindings.insert(argument.local.node.ident.clone());
                }
            }
            _ => {}
        },
        Visit::Expr(expression) => {
            if let Expr::Call(function, _) = &expression.node
                && let Expr::Identifier(name) = &function.node
            {
                idents.callees.insert(name.node.ident.clone());
            }
        }
    }
    node.visit_children(|child| walk(child, idents));
}

impl Idents {
    fn undefined(&self, name: &str) -> CompileError {
        if self.callees.contains(name) {
            return CompileError::UnknownFunction {
                function: name.to_owned(),
                suggestion: diag::suggest(name, DSL_FUNCTIONS.iter().copied()).map(str::to_owned),
            };
        }
        CompileError::UnknownVariable {
            name: name.to_owned(),
            suggestion: diag::suggest(name, self.bindings.iter().map(String::as_str))
                .map(str::to_owned),
        }
    }
}

/// A starlark failure the DSL constructors did not raise themselves.
pub(crate) fn map_error(error: &starlark::Error, idents: &Idents) -> CompileError {
    let text = error.without_diagnostic().to_string();
    let inner = match error.kind() {
        starlark::ErrorKind::Scope(_) => match undefined_name(&text) {
            Some(name) => idents.undefined(name),
            None => CompileError::Eval(text),
        },
        starlark::ErrorKind::Fail(_) => CompileError::Failed(text),
        _ => CompileError::Eval(text),
    };
    match error.span() {
        Some(at) => CompileError::At {
            at: at.clone(),
            inner: Box::new(inner),
        },
        None => inner,
    }
}

/// starlark keeps `ScopeError` private, so the name comes out of the message: it is the only
/// backtick-quoted run before any "did you mean" tail.
fn undefined_name(text: &str) -> Option<&str> {
    text.split('`').nth(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    use starlark::environment::{Globals, GlobalsBuilder, Module};
    use starlark::eval::Evaluator;
    use starlark_syntax::syntax::Dialect;

    /// The mapping reads a name out of a message starlark owns. If upstream rewords it this
    /// fails here rather than silently degrading every unknown-name error to `Eval`.
    #[test]
    fn a_live_scope_error_still_yields_the_undefined_name() {
        let source = "candidate = 1\nmissing_name\n";
        let ast = AstModule::parse("workflow.star", source.to_owned(), &Dialect::Standard).unwrap();
        let idents = scan(&ast);
        let globals: Globals = GlobalsBuilder::standard().build();
        let error = Module::with_temp_heap(|module| {
            let mut eval = Evaluator::new(&module);
            eval.eval_module(ast, &globals).map(|_| ()).unwrap_err()
        });
        assert!(
            matches!(error.kind(), starlark::ErrorKind::Scope(_)),
            "{error}"
        );
        let mapped = crate::errors::report(&map_error(&error, &idents));
        assert!(
            mapped.contains("unknown workflow variable \"missing_name\""),
            "{mapped}"
        );
        assert!(mapped.contains("workflow.star:2:"), "{mapped}");
    }

    #[test]
    fn callees_are_told_apart_from_variables() {
        let source = "a = agnt(name = \"a\")\nworkflow([candidat])\n";
        let ast = AstModule::parse("workflow.star", source.to_owned(), &Dialect::Standard).unwrap();
        let idents = scan(&ast);
        assert!(matches!(
            idents.undefined("agnt"),
            CompileError::UnknownFunction { .. }
        ));
        assert!(matches!(
            idents.undefined("candidat"),
            CompileError::UnknownVariable { .. }
        ));
    }
}
