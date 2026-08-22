//! An AST pre-pass, and the starlark-error mapping that depends on it.
//!
//! starlark resolves names before it evaluates, so an undefined identifier arrives as an
//! `ErrorKind::Scope` with the name quoted in its message and no clue whether the author meant a
//! DSL constructor or a variable. Reading the callee names out of the AST answers that, and the
//! module's bindings feed the did-you-mean.

use std::collections::{BTreeMap, BTreeSet};

use starlark_syntax::codemap::CodeMap;
use starlark_syntax::codemap::FileSpan;
use starlark_syntax::syntax::AstModule;
use starlark_syntax::syntax::ast::{ArgumentP, AssignTarget, AstLiteral, AstNoPayload, Expr, Stmt};
use starlark_syntax::syntax::module::AstModuleFields;
use starlark_syntax::syntax::uniplate::Visit;

use crate::manifest::WorkflowType;
use crate::plan::diag;
use crate::plan::starlark::{CompileError, dsl_functions};

/// One call's named arguments, and where each was written.
#[derive(Debug)]
struct CallArgs {
    call: FileSpan,
    function: String,
    named: BTreeMap<String, FileSpan>,
}

#[derive(Debug, Default)]
pub(crate) struct Idents {
    callees: BTreeSet<String>,
    bindings: BTreeSet<String>,
    lane: WorkflowType,
    /// The evaluator knows only the call site, so an unknown-kwarg error can point at the whole
    /// call and nothing narrower. A multi-line `agent(...)` then underlines fourteen lines to
    /// say one of them is misspelled. The AST knows where each argument was written.
    calls: Vec<CallArgs>,
}

pub(crate) fn scan(module: &AstModule, lane: WorkflowType) -> Idents {
    let mut idents = Idents {
        lane,
        ..Idents::default()
    };
    walk(
        Visit::Stmt(module.statement()),
        module.codemap(),
        &mut idents,
    );
    idents
}

/// The lane a source declares, read from `workflow(type = ...)` before the source is evaluated.
/// The lane decides which constructors exist, so it has to be known before the globals are built.
/// An unreadable or absent declaration yields the default and the evaluator reports the real
/// error, which keeps this pre-pass out of the business of diagnosing anything.
pub(crate) fn declared_lane(module: &AstModule) -> WorkflowType {
    let mut lane = WorkflowType::default();
    find_lane(Visit::Stmt(module.statement()), &mut lane);
    lane
}

fn find_lane<'a>(node: Visit<'a, AstNoPayload>, lane: &mut WorkflowType) {
    if let Visit::Expr(expression) = &node
        && let Expr::Call(function, arguments) = &expression.node
        && let Expr::Identifier(name) = &function.node
        && name.node.ident == "workflow"
    {
        for argument in arguments.args.iter() {
            if let ArgumentP::Named(key, value) = &argument.node
                && key.node == "type"
                && let Expr::Literal(AstLiteral::String(declared)) = &value.node
                && let Some(declared) = WorkflowType::parse(&declared.node)
            {
                *lane = declared;
            }
        }
    }
    node.visit_children(|child| find_lane(child, lane));
}

fn walk<'a>(node: Visit<'a, AstNoPayload>, codemap: &CodeMap, idents: &mut Idents) {
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
            if let Expr::Call(function, arguments) = &expression.node
                && let Expr::Identifier(name) = &function.node
            {
                idents.callees.insert(name.node.ident.clone());
                let named: BTreeMap<String, FileSpan> = arguments
                    .args
                    .iter()
                    .filter_map(|argument| match &argument.node {
                        ArgumentP::Named(key, _) => {
                            Some((key.node.clone(), codemap.file_span(argument.span)))
                        }
                        _ => None,
                    })
                    .collect();
                if !named.is_empty() {
                    idents.calls.push(CallArgs {
                        call: codemap.file_span(expression.span),
                        function: name.node.ident.clone(),
                        named,
                    });
                }
            }
        }
    }
    node.visit_children(|child| walk(child, codemap, idents));
}

/// Re-point a whole-call error at the argument it is actually about.
///
/// The evaluator throws with the call site, which is all it knows. This walks the AST's record
/// of where each named argument was written and swaps in the narrower span, so a misspelled
/// kwarg in a multi-line call underlines the kwarg rather than the call.
pub(crate) fn narrow(error: CompileError, idents: &Idents) -> CompileError {
    let CompileError::At { at, inner } = error else {
        return error;
    };
    let (function, argument) = match inner.as_ref() {
        CompileError::UnknownArgument {
            function, argument, ..
        } => (Some(function.as_str()), argument.as_str()),
        // Both come from the `session =` argument of an agent or propose call, and `session`
        // is a kwarg on nothing else, so the call site alone identifies it.
        CompileError::UndeclaredSession { .. } | CompileError::SessionWrongType => {
            (None, "session")
        }
        _ => {
            return CompileError::At { at, inner };
        }
    };
    match idents.argument_span(&at, function, argument) {
        Some(narrower) => CompileError::At {
            at: narrower,
            inner,
        },
        None => CompileError::At { at, inner },
    }
}

impl Idents {
    /// The span of `argument` in the call the evaluator reported. Several calls to one
    /// constructor can carry the same argument name, so the call site picks between them; a
    /// call that cannot be identified falls back to the only unambiguous candidate, and to
    /// nothing when there is more than one.
    fn argument_span(
        &self,
        call: &FileSpan,
        function: Option<&str>,
        argument: &str,
    ) -> Option<FileSpan> {
        let candidates: Vec<&CallArgs> = self
            .calls
            .iter()
            .filter(|c| function.is_none_or(|f| c.function == f))
            .filter(|c| c.named.contains_key(argument))
            .collect();
        let containing = candidates.iter().find(|c| {
            c.call.file.filename() == call.file.filename()
                && c.call.span.begin() <= call.span.begin()
                && c.call.span.end() >= call.span.end()
        });
        match containing.or(candidates.first().filter(|_| candidates.len() == 1)) {
            Some(found) => found.named.get(argument).cloned(),
            None => None,
        }
    }

    fn undefined(&self, name: &str) -> CompileError {
        if self.callees.contains(name) {
            let lane = dsl_functions(self.lane);
            let reachable = lane
                .iter()
                .copied()
                .chain(self.bindings.iter().map(String::as_str));
            return CompileError::UnknownFunction {
                function: name.to_owned(),
                suggestion: diag::suggest(name, reachable).map(str::to_owned),
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
        let idents = scan(&ast, WorkflowType::default());
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
        let idents = scan(&ast, WorkflowType::default());
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
