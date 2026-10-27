//! Bindings and calls of a parsed source, with byte spans.

use std::ops::Range;

use crucible::plan::starlark::idents::declared_lane;
use crucible::plan::workflow::WorkflowType;
use starlark_syntax::codemap::Span;
use starlark_syntax::lexer::TokenInt;
use starlark_syntax::syntax::AstModule;
use starlark_syntax::syntax::ast::{
    ArgumentP, AssignTarget, AstAssignTarget, AstExpr, AstLiteral, AstStmt, ClauseP, Expr,
    ForClauseP, ParameterP, Stmt,
};

/// Constructors that build a graph task, and so take `name` and may `emits`.
pub const TASK_CONSTRUCTORS: &[&str] = &[
    "agent", "apply", "command", "decide", "evaluate", "grade", "measure", "propose", "report",
    "route", "skill", "top_k",
];

#[derive(Debug, Default)]
pub struct Index {
    /// The declared lane, when the source calls `workflow()` or `default_autoresearch()`.
    pub lane: Option<WorkflowType>,
    pub bindings: Vec<Binding>,
    pub calls: Vec<Call>,
    pub loads: Vec<Load>,
    pub params: Vec<ParamDecl>,
    /// Every `param(...)` call: the literal name, or `None` when the name is computed.
    pub param_reads: Vec<(Option<String>, Range<usize>)>,
    pub identifiers: Vec<(String, Range<usize>)>,
}

#[derive(Debug, Clone)]
pub struct Binding {
    pub name: String,
    pub span: Range<usize>,
    pub kind: BindingKind,
    /// The `def` or comprehension the name is local to; `None` at module level.
    pub scope: Option<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingKind {
    /// `name = value`, with the value's index into [`Index::calls`] when it is a bare call.
    Assign {
        call: Option<usize>,
    },
    Def,
    Load {
        module: String,
        original: String,
    },
    Loop,
    Parameter,
}

#[derive(Debug, Clone)]
pub struct Call {
    pub callee: String,
    pub callee_span: Range<usize>,
    pub span: Range<usize>,
    pub args: Vec<Arg>,
}

#[derive(Debug, Clone)]
pub struct Arg {
    pub name: Option<(String, Range<usize>)>,
    pub value: Value,
}

#[derive(Debug, Clone)]
pub struct Value {
    pub span: Range<usize>,
    pub kind: ValueKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ValueKind {
    Str(String),
    Int(i64),
    Bool(bool),
    NoneLit,
    Ident(String),
    List(Vec<Value>),
    Dict(Vec<(Value, Value)>),
    Call(usize),
    Other,
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
    }
}

#[derive(Debug, Clone)]
pub struct Load {
    pub module: String,
    pub module_span: Range<usize>,
}

#[derive(Debug, Clone)]
pub struct ParamDecl {
    pub name: String,
    /// The key's string literal, quotes included.
    pub span: Range<usize>,
}

pub fn bytes(span: Span) -> Range<usize> {
    span.begin().get() as usize..span.end().get() as usize
}

impl Call {
    pub fn kwarg(&self, name: &str) -> Option<&Value> {
        self.args
            .iter()
            .find(|arg| arg.name.as_ref().is_some_and(|(n, _)| n == name))
            .map(|arg| &arg.value)
    }

    pub fn positional(&self, index: usize) -> Option<&Value> {
        self.args
            .iter()
            .filter(|arg| arg.name.is_none())
            .nth(index)
            .map(|arg| &arg.value)
    }

    /// The literal `name = "..."` a task or session constructor was given.
    pub fn declared_name(&self) -> Option<&str> {
        match &self.kwarg("name")?.kind {
            ValueKind::Str(name) => Some(name),
            _ => None,
        }
    }
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match &self.kind {
            ValueKind::Str(s) => Some(s),
            _ => None,
        }
    }
}

impl Index {
    pub fn build(module: &AstModule) -> Index {
        let mut index = Index::default();
        let mut walker = Walker {
            index: &mut index,
            scope: None,
        };
        walker.stmt(module.statement());
        index.params = params_block(module.statement());
        if index
            .calls
            .iter()
            .any(|call| call.callee == "workflow" || call.callee == "default_autoresearch")
        {
            index.lane = Some(declared_lane(module));
        }
        index
    }

    pub fn call(&self, index: usize) -> Option<&Call> {
        self.calls.get(index)
    }

    /// The binding `name` resolves to at `offset`, innermost scope first.
    pub fn resolve(&self, name: &str, offset: usize) -> Option<&Binding> {
        self.bindings
            .iter()
            .filter(|binding| binding.name == name)
            .filter(|binding| {
                binding
                    .scope
                    .as_ref()
                    .is_none_or(|scope| scope.contains(&offset))
            })
            .min_by_key(|binding| binding.scope.as_ref().map_or(usize::MAX, |s| s.len()))
    }

    /// The call a module-level name was assigned from.
    pub fn assigned_call(&self, name: &str) -> Option<&Call> {
        self.bindings
            .iter()
            .rev()
            .filter(|binding| binding.name == name && binding.scope.is_none())
            .find_map(|binding| match binding.kind {
                BindingKind::Assign { call: Some(call) } => self.calls.get(call),
                _ => None,
            })
    }

    /// The constructor call whose literal `name` is `task`.
    pub fn task_named(&self, task: &str) -> Option<&Call> {
        self.calls.iter().find(|call| {
            TASK_CONSTRUCTORS.contains(&call.callee.as_str()) && call.declared_name() == Some(task)
        })
    }
}

struct Walker<'a> {
    index: &'a mut Index,
    scope: Option<Range<usize>>,
}

impl Walker<'_> {
    fn bind(&mut self, name: &str, span: Span, kind: BindingKind) {
        self.index.bindings.push(Binding {
            name: name.to_owned(),
            span: bytes(span),
            kind,
            scope: self.scope.clone(),
        });
    }

    fn target(&mut self, target: &AstAssignTarget, kind: &BindingKind) {
        match &target.node {
            AssignTarget::Identifier(ident) => {
                self.bind(&ident.node.ident, ident.span, kind.clone())
            }
            AssignTarget::Tuple(items) => {
                for item in items {
                    self.target(item, kind);
                }
            }
            AssignTarget::Index(pair) => {
                self.expr(&pair.0);
                self.expr(&pair.1);
            }
            AssignTarget::Dot(object, _) => {
                self.expr(object);
            }
        }
    }

    fn stmt(&mut self, statement: &AstStmt) {
        match &statement.node {
            Stmt::Statements(statements) => statements.iter().for_each(|s| self.stmt(s)),
            Stmt::Expression(expression) => {
                self.expr(expression);
            }
            Stmt::Return(expression) => {
                if let Some(expression) = expression {
                    self.expr(expression);
                }
            }
            Stmt::Assign(assign) => {
                let value = self.expr(&assign.rhs);
                let call = match value.kind {
                    ValueKind::Call(call) => Some(call),
                    _ => None,
                };
                self.target(&assign.lhs, &BindingKind::Assign { call });
            }
            Stmt::AssignModify(target, _, value) => {
                self.expr(value);
                if let AssignTarget::Identifier(ident) = &target.node {
                    self.index
                        .identifiers
                        .push((ident.node.ident.clone(), bytes(ident.span)));
                }
            }
            Stmt::If(condition, body) => {
                self.expr(condition);
                self.stmt(body);
            }
            Stmt::IfElse(condition, branches) => {
                self.expr(condition);
                self.stmt(&branches.0);
                self.stmt(&branches.1);
            }
            Stmt::For(for_) => {
                self.expr(&for_.over);
                self.target(&for_.var, &BindingKind::Loop);
                self.stmt(&for_.body);
            }
            Stmt::Def(def) => {
                self.bind(&def.name.node.ident, def.name.span, BindingKind::Def);
                for parameter in &def.params {
                    if let ParameterP::Normal(_, _, Some(default)) = &parameter.node {
                        self.expr(default);
                    }
                }
                let outer = self.scope.replace(bytes(statement.span));
                for parameter in &def.params {
                    if let Some(ident) = parameter.node.ident() {
                        self.bind(&ident.node.ident, ident.span, BindingKind::Parameter);
                    }
                }
                self.stmt(&def.body);
                self.scope = outer;
            }
            Stmt::Load(load) => {
                self.index.loads.push(Load {
                    module: load.module.node.clone(),
                    module_span: bytes(load.module.span),
                });
                for argument in &load.args {
                    self.bind(
                        &argument.local.node.ident,
                        argument.local.span,
                        BindingKind::Load {
                            module: load.module.node.clone(),
                            original: argument.their.node.clone(),
                        },
                    );
                }
            }
            Stmt::Break | Stmt::Continue | Stmt::Pass => {}
        }
    }

    fn comprehension(
        &mut self,
        span: Span,
        first: &ForClauseP<starlark_syntax::syntax::ast::AstNoPayload>,
        rest: &[ClauseP<starlark_syntax::syntax::ast::AstNoPayload>],
        body: impl FnOnce(&mut Self),
    ) {
        self.expr(&first.over);
        let outer = self.scope.replace(bytes(span));
        self.target(&first.var, &BindingKind::Loop);
        for clause in rest {
            match clause {
                ClauseP::For(clause) => {
                    self.expr(&clause.over);
                    self.target(&clause.var, &BindingKind::Loop);
                }
                ClauseP::If(condition) => {
                    self.expr(condition);
                }
            }
        }
        body(self);
        self.scope = outer;
    }

    fn expr(&mut self, expression: &AstExpr) -> Value {
        let span = bytes(expression.span);
        let kind = match &expression.node {
            Expr::Literal(AstLiteral::String(s)) => ValueKind::Str(s.node.clone()),
            Expr::Literal(AstLiteral::Int(n)) => match &n.node {
                TokenInt::I32(n) => ValueKind::Int(i64::from(*n)),
                TokenInt::BigInt(_) => ValueKind::Other,
            },
            Expr::Literal(_) => ValueKind::Other,
            Expr::Identifier(ident) => {
                self.index
                    .identifiers
                    .push((ident.node.ident.clone(), span.clone()));
                match ident.node.ident.as_str() {
                    "True" => ValueKind::Bool(true),
                    "False" => ValueKind::Bool(false),
                    "None" => ValueKind::NoneLit,
                    name => ValueKind::Ident(name.to_owned()),
                }
            }
            Expr::List(items) => {
                ValueKind::List(items.iter().map(|item| self.expr(item)).collect())
            }
            Expr::Tuple(items) => {
                items.iter().for_each(|item| {
                    self.expr(item);
                });
                ValueKind::Other
            }
            Expr::Dict(entries) => ValueKind::Dict(
                entries
                    .iter()
                    .map(|(key, value)| (self.expr(key), self.expr(value)))
                    .collect(),
            ),
            Expr::Call(function, arguments) => {
                let callee = match &function.node {
                    Expr::Identifier(ident) => {
                        self.index
                            .identifiers
                            .push((ident.node.ident.clone(), bytes(function.span)));
                        Some((ident.node.ident.clone(), bytes(function.span)))
                    }
                    _ => {
                        self.expr(function);
                        None
                    }
                };
                let args: Vec<Arg> = arguments
                    .args
                    .iter()
                    .map(|argument| match &argument.node {
                        ArgumentP::Named(name, value) => Arg {
                            name: Some((name.node.clone(), bytes(name.span))),
                            value: self.expr(value),
                        },
                        other => Arg {
                            name: None,
                            value: self.expr(other.expr()),
                        },
                    })
                    .collect();
                match callee {
                    Some((callee, callee_span)) => {
                        if callee == "param" {
                            let name = match args.as_slice() {
                                [Arg { name: None, value }] => value.as_str().map(str::to_owned),
                                _ => None,
                            };
                            self.index.param_reads.push((name, span.clone()));
                        }
                        self.index.calls.push(Call {
                            callee,
                            callee_span,
                            span: span.clone(),
                            args,
                        });
                        ValueKind::Call(self.index.calls.len() - 1)
                    }
                    None => ValueKind::Other,
                }
            }
            Expr::Dot(object, _) => {
                self.expr(object);
                ValueKind::Other
            }
            Expr::Index(pair) => {
                self.expr(&pair.0);
                self.expr(&pair.1);
                ValueKind::Other
            }
            Expr::Index2(triple) => {
                self.expr(&triple.0);
                self.expr(&triple.1);
                self.expr(&triple.2);
                ValueKind::Other
            }
            Expr::Slice(object, a, b, c) => {
                self.expr(object);
                for part in [a, b, c].into_iter().flatten() {
                    self.expr(part);
                }
                ValueKind::Other
            }
            Expr::Lambda(lambda) => {
                let outer = self.scope.replace(span.clone());
                for parameter in &lambda.params {
                    if let Some(ident) = parameter.node.ident() {
                        self.bind(&ident.node.ident, ident.span, BindingKind::Parameter);
                    }
                }
                self.expr(&lambda.body);
                self.scope = outer;
                ValueKind::Other
            }
            Expr::Not(inner) | Expr::Minus(inner) | Expr::Plus(inner) | Expr::BitNot(inner) => {
                self.expr(inner);
                ValueKind::Other
            }
            Expr::Op(left, _, right) => {
                self.expr(left);
                self.expr(right);
                ValueKind::Other
            }
            Expr::If(triple) => {
                self.expr(&triple.0);
                self.expr(&triple.1);
                self.expr(&triple.2);
                ValueKind::Other
            }
            Expr::ListComprehension(body, first, rest) => {
                self.comprehension(expression.span, first, rest, |walker| {
                    walker.expr(body);
                });
                ValueKind::Other
            }
            Expr::DictComprehension(pair, first, rest) => {
                self.comprehension(expression.span, first, rest, |walker| {
                    walker.expr(&pair.0);
                    walker.expr(&pair.1);
                });
                ValueKind::Other
            }
            Expr::FString(fstring) => {
                for part in &fstring.expressions {
                    self.expr(part);
                }
                ValueKind::Other
            }
        };
        Value { span, kind }
    }
}

/// The keys of a leading `params = {...}` literal, with their spans.
fn params_block(statement: &AstStmt) -> Vec<ParamDecl> {
    let first = match &statement.node {
        Stmt::Statements(statements) => statements.first(),
        _ => Some(statement),
    };
    let Some(Stmt::Assign(assign)) = first.map(|s| &s.node) else {
        return Vec::new();
    };
    match (&assign.lhs.node, &assign.rhs.node) {
        (AssignTarget::Identifier(ident), Expr::Dict(entries)) if ident.node.ident == "params" => {
            entries
                .iter()
                .filter_map(|(key, _)| match &key.node {
                    Expr::Literal(AstLiteral::String(name)) => Some(ParamDecl {
                        name: name.node.clone(),
                        span: bytes(key.span),
                    }),
                    _ => None,
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use crucible::plan::starlark::dialect;
    use starlark_syntax::syntax::AstModule;

    use crate::index::*;

    fn index(source: &str) -> Index {
        let module =
            AstModule::parse("workflow.star", source.to_owned(), &dialect()).expect("parses");
        Index::build(&module)
    }

    #[test]
    fn module_bindings_record_the_call_they_were_assigned() {
        let source = "load(\"lib.star\", \"helper\", other = \"thing\")\n\
                      review = agent(name = \"review\", prompt = \"p\", emits = [\"verdict\"])\n\
                      def make(n):\n    x = n\n    return x\n\
                      workflow(type = \"playbook\", tasks = [review])\n";
        let index = index(source);
        assert_eq!(index.lane, Some(WorkflowType::Playbook));
        let review = index.assigned_call("review").expect("call");
        assert_eq!(review.callee, "agent");
        assert_eq!(review.declared_name(), Some("review"));
        let emits = review.kwarg("emits").expect("emits");
        assert!(matches!(&emits.kind, ValueKind::List(items) if items.len() == 1));
        let other = index.resolve("other", 0).expect("load alias");
        assert_eq!(
            other.kind,
            BindingKind::Load {
                module: "lib.star".into(),
                original: "thing".into()
            }
        );
        assert_eq!(&source[other.span.clone()], "other");
        assert_eq!(
            index.loads.first().map(|l| l.module.as_str()),
            Some("lib.star")
        );
    }

    #[test]
    fn local_scopes_shadow_module_names_only_inside_them() {
        let source = "x = 1\ndef f(x):\n    return x\ny = x\n";
        let index = index(source);
        let inside = source.find("return x").expect("return") + 7;
        let outside = source.rfind("x\n").expect("tail");
        assert_eq!(
            index.resolve("x", inside).map(|b| b.kind.clone()),
            Some(BindingKind::Parameter)
        );
        assert!(matches!(
            index.resolve("x", outside).map(|b| &b.kind),
            Some(BindingKind::Assign { .. })
        ));
        assert_eq!(index.lane, None, "a module with no workflow() is a library");
    }

    #[test]
    fn params_and_their_reads_are_indexed() {
        let source = "params = {\"repo\": {\"type\": \"string\", \"required\": True}, \"depth\": {\"type\": \"int\", \"default\": 1}}\n\
                      n = \"depth\"\na = param(\"repo\")\nb = param(n)\n";
        let index = index(source);
        let names: Vec<_> = index.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["repo", "depth"]);
        let span = index.params.first().expect("repo").span.clone();
        assert_eq!(&source[span], "\"repo\"");
        let reads: Vec<_> = index
            .param_reads
            .iter()
            .map(|(name, _)| name.as_deref())
            .collect();
        assert_eq!(reads, [Some("repo"), None]);
    }

    #[test]
    fn comprehension_variables_stay_in_the_comprehension() {
        let source = "xs = [agent(name = n, prompt = \"p\") for n in [\"a\", \"b\"]]\nn2 = 1\n";
        let index = index(source);
        let n = index.bindings.iter().find(|b| b.name == "n").expect("n");
        assert!(n.scope.is_some());
        assert!(index.resolve("n", source.len() - 1).is_none());
        assert_eq!(
            index.calls.iter().filter(|c| c.callee == "agent").count(),
            1
        );
    }
}
