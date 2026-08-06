//! Deterministic Starlark frontend for [`WorkflowCfg`]. Scope freezes the compiled IR, so runtime
//! never evaluates the source. Only `prompt_file` can read files; process, environment, network,
//! clock, and randomness APIs are unavailable.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use starlark_syntax::codemap::{FileSpan, Span};
use starlark_syntax::lexer::TokenInt;
use starlark_syntax::syntax::ast::{
    Argument, AssignTarget, AstLiteral, AstStmt, BinOp, CallArgsP, Expr, Stmt,
};
use starlark_syntax::syntax::{AstModule, Dialect};

use crate::errors::FileError;
use crate::manifest::{WorkflowCfg, WorkflowError, WorkflowType};
use crate::plan::diag;
use crate::plan::ir::{
    Direction, EngineOp, Isolation, Join, OutputField, Stage, Task, TaskKind, TaskName,
};

type Result<T> = std::result::Result<T, CompileError>;

const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_PROMPT_BYTES: usize = 256 * 1024;
const MAX_TOTAL_PROMPT_BYTES: usize = 1024 * 1024;
const MAX_TASKS: usize = 128;
const MAX_EVAL_STEPS: usize = 10_000;

#[derive(Debug)]
pub struct CompiledWorkflow {
    pub workflow: WorkflowCfg,
    /// Pack-relative prompts embedded in the task IR.
    pub prompt_files: Vec<PathBuf>,
    /// Stable, pretty JSON used by golden tests and review tooling.
    pub canonical_json: String,
}

#[derive(Debug)]
struct CompileContext {
    pack_dir: PathBuf,
    prompt_files: BTreeSet<PathBuf>,
    total_prompt_bytes: usize,
    eval_steps: usize,
    /// Every task a DSL constructor built, keyed by name. A constructed-but-dropped task
    /// silently never runs, so it is a compile error.
    constructed_tasks: BTreeMap<String, FileSpan>,
    /// `session(...)` declarations by name, with the declaring site.
    sessions: BTreeMap<String, (SessionDecl, FileSpan)>,
    /// Declared sessions bound to at least one task.
    bound_sessions: BTreeSet<String>,
    /// Bare-string session refs made before any declaration. A later declaration makes
    /// these errors: a session must be declared before use.
    string_session_refs: BTreeMap<String, FileSpan>,
}

impl CompileContext {
    fn prompt_file(&mut self, raw: &str) -> Result<String> {
        let relative = safe_relative_path(raw)?;
        let root = std::fs::canonicalize(&self.pack_dir)
            .map_err(FileError::at("resolving pack directory", &self.pack_dir))?;
        let mut path = self.pack_dir.clone();
        let mut metadata = None;
        for component in relative.components() {
            let Component::Normal(component) = component else {
                continue;
            };
            path.push(component);
            let current = std::fs::symlink_metadata(&path)
                .map_err(FileError::at("reading prompt metadata", &path))?;
            if current.file_type().is_symlink() {
                return Err(CompileError::PromptSymlink {
                    raw: raw.to_owned(),
                });
            }
            metadata = Some(current);
        }
        if !metadata.is_some_and(|metadata| metadata.is_file()) {
            return Err(CompileError::PromptNotRegularFile {
                raw: raw.to_owned(),
            });
        }
        let canonical =
            std::fs::canonicalize(&path).map_err(FileError::at("resolving prompt file", &path))?;
        if !canonical.starts_with(&root) {
            return Err(CompileError::PromptEscapesPack {
                raw: raw.to_owned(),
            });
        }
        let bytes =
            std::fs::read(&canonical).map_err(FileError::at("reading prompt file", &canonical))?;
        if bytes.len() > MAX_PROMPT_BYTES {
            return Err(CompileError::PromptTooLarge {
                raw: raw.to_owned(),
                bytes: bytes.len(),
            });
        }
        self.total_prompt_bytes = self.total_prompt_bytes.saturating_add(bytes.len());
        if self.total_prompt_bytes > MAX_TOTAL_PROMPT_BYTES {
            return Err(CompileError::PromptBudgetSpent);
        }
        self.prompt_files.insert(relative);
        Ok(String::from_utf8(bytes)?)
    }

    fn step(&mut self) -> Result<()> {
        self.eval_steps += 1;
        if self.eval_steps > MAX_EVAL_STEPS {
            return Err(CompileError::EvalStepsSpent);
        }
        Ok(())
    }
}

fn safe_relative_path(raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    if raw.trim().is_empty() || path.is_absolute() {
        return Err(CompileError::PromptPathEmpty);
    }
    if path.components().any(|part| {
        matches!(
            part,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(CompileError::PromptPathTraversal);
    }
    Ok(path.to_path_buf())
}

#[derive(Clone, Debug)]
enum Value {
    None,
    Bool(bool),
    Int(i32),
    Float(f64),
    String(String),
    List(Vec<Value>),
    Task(Task),
    Session(SessionDecl),
    Workflow(WorkflowCfg),
}

/// A `session(...)` declaration: a durable conversation name plus optional agent
/// defaults that materialize onto the agent tasks bound to it.
#[derive(Clone, Debug)]
struct SessionDecl {
    name: String,
    harness: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

impl SessionDecl {
    fn has_defaults(&self) -> bool {
        self.harness.is_some() || self.model.is_some() || self.effort.is_some()
    }
}

/// Everything the Starlark frontend can reject. [`CompileError::At`] carries the
/// `file:line:col` prefix, so [`Compiler::locate`] can tell a located error from a bare one
/// instead of sniffing a formatted string.
///
/// Causes are real `source()` links, so the message a user reads comes from
/// [`crate::errors::report`] (or anyhow's `{:#}`), not from `Display` alone.
#[derive(Debug, thiserror::Error)]
pub enum CompileError {
    /// An error located at its authoring site. The innermost site wins.
    #[error("{at}")]
    At {
        at: FileSpan,
        #[source]
        inner: Box<CompileError>,
    },
    #[error(transparent)]
    File(#[from] FileError),
    #[error("prompt files must be UTF-8")]
    PromptNotUtf8(#[from] std::string::FromUtf8Error),
    #[error("parsing workflow Starlark: {0}")]
    Parse(String),
    #[error(transparent)]
    Workflow(#[from] WorkflowError),
    #[error("serializing the compiled workflow")]
    Json(#[from] serde_json::Error),

    #[error("prompt_file({raw:?}) may not traverse symlinks")]
    PromptSymlink { raw: String },
    #[error("prompt_file({raw:?}) must name a regular, non-symlink file")]
    PromptNotRegularFile { raw: String },
    #[error("prompt_file({raw:?}) escapes the pack directory")]
    PromptEscapesPack { raw: String },
    #[error("prompt_file({raw:?}) is {bytes} bytes; maximum is {MAX_PROMPT_BYTES}")]
    PromptTooLarge { raw: String, bytes: usize },
    #[error("workflow embeds more than {MAX_TOTAL_PROMPT_BYTES} bytes of prompt files")]
    PromptBudgetSpent,
    #[error("prompt_file path must be a non-empty pack-relative path")]
    PromptPathEmpty,
    #[error("prompt_file path may not contain `..` or escape the pack")]
    PromptPathTraversal,

    #[error("workflow source is {bytes} bytes; maximum is {MAX_SOURCE_BYTES}")]
    SourceTooLarge { bytes: usize },
    #[error("workflow evaluation exceeds {MAX_EVAL_STEPS} expression steps")]
    EvalStepsSpent,
    #[error("{function} expands to {count} tasks; maximum is {MAX_TASKS}")]
    TooManyTasks { function: String, count: usize },

    #[error("workflow assignments must target a single variable name")]
    NonIdentifierAssignment,
    #[error("workflow Starlark may not use load(); use pack-local prompt_file() for prompts")]
    LoadUnsupported,
    #[error(
        "workflow Starlark is declarative: use assignments, lists, list concatenation, and DSL calls"
    )]
    NonDeclarativeStatement,
    #[error("unknown workflow variable {name:?}{}", diag::hint(.suggestion.as_deref()))]
    UnknownVariable {
        name: String,
        suggestion: Option<String>,
    },
    #[error("workflow integers must fit in 32 bits")]
    IntegerTooWide,
    #[error("workflow `+` is supported only for task or dependency lists")]
    UnsupportedAddition,
    #[error("workflow calls must name a DSL constructor directly")]
    IndirectCall,
    #[error(
        "unsupported workflow expression; use strings, integers, booleans, lists, list concatenation, and DSL calls"
    )]
    UnsupportedExpression,

    #[error("unknown workflow DSL function {function:?}{}", diag::hint(.suggestion.as_deref()))]
    UnknownFunction {
        function: String,
        suggestion: Option<String>,
    },
    #[error("{function}() takes exactly one positional argument")]
    NotOnePositional { function: String },
    #[error("{function}() received the wrong value type")]
    WrongPositionalType { function: String },
    #[error("{function}() task constructor arguments must be named")]
    PositionalArgument { function: String },
    #[error("{function}() repeats argument {argument:?}")]
    RepeatedArgument { function: String, argument: String },
    #[error("{function}() has unknown argument {argument:?}{}", diag::hint(.suggestion.as_deref()))]
    UnknownArgument {
        function: String,
        argument: String,
        suggestion: Option<String>,
    },

    #[error("workflow type must be `autoresearch` or `custom`, got {got:?}")]
    UnknownWorkflowType { got: String },
    #[error("workflow tasks must be a list of task constructor values")]
    TasksNotList,
    #[error("deps() entries must be task constructor values")]
    DepsEntryNotTask,
    #[error("{function} entries must be task constructor values")]
    TaskListEntryNotTask { function: String },

    #[error("session name {name:?} must be 1-64 ASCII letters, digits, `.`, `_`, or `-`")]
    InvalidSessionName { name: String },
    #[error("session {name:?} is already declared at {first}")]
    DuplicateSession { name: String, first: FileSpan },
    #[error(
        "task {task:?} sets {knob} {mine:?} but session {session:?} declares {theirs:?}; a session \
         is one conversation under one config"
    )]
    SessionConfigConflict {
        task: String,
        knob: &'static str,
        mine: String,
        session: String,
        theirs: String,
    },
    #[error(
        "session {name:?} declares agent defaults, but propose() takes its agent config from the \
         manifest's [agent]"
    )]
    ProposeSessionDefaults { name: String },
    #[error("session {name:?} is not declared{}", diag::hint(.suggestion.as_deref()))]
    UndeclaredSession {
        name: String,
        suggestion: Option<String>,
    },
    #[error("argument \"session\" must be a session() value or a string")]
    SessionWrongType,

    #[error("top_k k must be >= 1")]
    TopKZero,
    #[error("top_k direction must be `lower` or `higher`, got {got:?}")]
    UnknownTopKDirection { got: String },
    #[error("top_k requires a non-empty depends_on")]
    TopKWithoutDependencies,
    #[error("stage must be `iteration` or `epilogue`, got {got:?}")]
    UnknownStage { got: String },
    #[error("join must be `all` or `passed`, got {got:?}")]
    UnknownJoin { got: String },
    #[error("emits entries must be strings")]
    EmitsEntryNotString,
    #[error("argument \"emits\" must be a list of field-name strings")]
    EmitsNotList,

    #[error("missing required argument {argument:?}")]
    MissingArgument { argument: String },
    /// One arm for every scalar-kwarg type check; `expected` completes the sentence.
    #[error("argument {argument:?} must be {expected}")]
    WrongArgumentType {
        argument: String,
        expected: &'static str,
    },
    #[error("argument {argument:?} must be `lower`, `higher`, or None, got {got:?}")]
    UnknownDirection { argument: String, got: String },
    #[error("{argument} must be a list of tasks or task-name strings")]
    TaskNamesNotList { argument: String },
    #[error("{argument} entries must be tasks or task-name strings")]
    TaskNameEntryWrongType { argument: String },

    #[error("workflow source must end with workflow(...) or default_autoresearch([...])")]
    NoWorkflowResult,
    #[error(
        "task(s) constructed but not included in the workflow: {}; add them to \
         workflow(tasks = ...) or delete them",
        .sites.join(", ")
    )]
    DroppedTasks { sites: Vec<String> },
    #[error(
        "session string reference(s) {} appear before any session() declaration; declare sessions \
         before binding them",
        .sites.join(", ")
    )]
    LateSessionDeclaration { sites: Vec<String> },
    #[error(
        "session(s) declared but never bound to a task: {}; bind with session = <declaration> or \
         delete them",
        .sites.join(", ")
    )]
    UnboundSessions { sites: Vec<String> },
}

/// Compiling `workflow.star` into the manifest's generated `[workflow]` block: the compile
/// itself, plus the TOML surgery that installs the result.
#[derive(Debug, thiserror::Error)]
pub enum MaterializeError {
    #[error(transparent)]
    Compile(#[from] CompileError),
    #[error(transparent)]
    File(#[from] FileError),
    #[error("parsing manifest {}", .path.display())]
    ParseManifest {
        path: PathBuf,
        #[source]
        cause: toml_edit::TomlError,
    },
    #[error("serializing the workflow block")]
    Serialize(#[from] toml::ser::Error),
}

/// The callable DSL surface, for unknown-function suggestions.
const DSL_FUNCTIONS: &[&str] = &[
    "agent",
    "apply",
    "command",
    "decide",
    "default_autoresearch",
    "deps",
    "evaluate",
    "grade",
    "measure",
    "prompt_file",
    "propose",
    "session",
    "top_k",
    "workflow",
];

/// Every kwarg a constructor accepts. Unknown-kwarg errors suggest from this table; a
/// test compiles every listed kwarg per constructor so it cannot drift from the arms.
fn known_kwargs(function: &str) -> &'static [&'static str] {
    match function {
        "agent" => &[
            "name",
            "prompt",
            "harness",
            "model",
            "effort",
            "session",
            "emits",
            "depends_on",
            "needs",
            "required",
            "isolated",
            "join",
            "stage",
        ],
        "command" => &[
            "name",
            "run",
            "emits",
            "depends_on",
            "needs",
            "required",
            "isolated",
            "join",
            "stage",
        ],
        "evaluate" => &[
            "name",
            "run",
            "threshold",
            "direction",
            "emits",
            "depends_on",
            "needs",
            "required",
            "isolated",
            "join",
            "stage",
        ],
        "top_k" => &["name", "k", "direction", "depends_on", "required"],
        "propose" => &["name", "session", "depends_on"],
        "apply" | "measure" => &["name", "depends_on"],
        "grade" => &["name", "score", "evidence", "join"],
        "decide" => &["name", "measurement", "depends_on"],
        "session" => &["name", "harness", "model", "effort"],
        "workflow" => &["type", "tasks", "result"],
        _ => &[],
    }
}

struct Compiler<'a> {
    module: &'a AstModule,
    context: CompileContext,
    variables: BTreeMap<String, Value>,
}

impl Compiler<'_> {
    fn err_at(&self, span: Span, error: CompileError) -> CompileError {
        CompileError::At {
            at: self.module.file_span(span),
            inner: Box::new(error),
        }
    }

    /// Attach the call site to an error that does not already carry a location.
    fn locate(&self, span: Span, error: CompileError) -> CompileError {
        match error {
            located @ CompileError::At { .. } => located,
            error => self.err_at(span, error),
        }
    }

    fn statement(&mut self, statement: &AstStmt) -> Result<Option<Value>> {
        self.context.step()?;
        match &statement.node {
            Stmt::Statements(statements) => {
                let mut last = None;
                for statement in statements {
                    last = self.statement(statement)?;
                }
                Ok(last)
            }
            Stmt::Assign(assign) => {
                let AssignTarget::Identifier(name) = &assign.lhs.node else {
                    return Err(self.err_at(assign.lhs.span, CompileError::NonIdentifierAssignment));
                };
                let value = self.expression(&assign.rhs)?;
                self.variables.insert(name.node.ident.clone(), value);
                Ok(None)
            }
            Stmt::Expression(expression) => self.expression(expression).map(Some),
            Stmt::Load(_) => Err(self.err_at(statement.span, CompileError::LoadUnsupported)),
            _ => Err(self.err_at(statement.span, CompileError::NonDeclarativeStatement)),
        }
    }

    fn expression(&mut self, expression: &starlark_syntax::syntax::ast::AstExpr) -> Result<Value> {
        self.context.step()?;
        match &expression.node {
            Expr::Identifier(identifier) => match identifier.node.ident.as_str() {
                "True" => Ok(Value::Bool(true)),
                "False" => Ok(Value::Bool(false)),
                "None" => Ok(Value::None),
                name => match self.variables.get(name) {
                    Some(value) => Ok(value.clone()),
                    None => Err(self.err_at(
                        expression.span,
                        CompileError::UnknownVariable {
                            name: name.to_owned(),
                            suggestion: diag::suggest(
                                name,
                                self.variables.keys().map(String::as_str),
                            )
                            .map(str::to_owned),
                        },
                    )),
                },
            },
            Expr::Literal(AstLiteral::String(value)) => Ok(Value::String(value.node.clone())),
            Expr::Literal(AstLiteral::Int(value)) => match &value.node {
                TokenInt::I32(value) => Ok(Value::Int(*value)),
                TokenInt::BigInt(_) => {
                    Err(self.err_at(expression.span, CompileError::IntegerTooWide))
                }
            },
            Expr::Literal(AstLiteral::Float(value)) => Ok(Value::Float(value.node)),
            Expr::List(items) | Expr::Tuple(items) => items
                .iter()
                .map(|item| self.expression(item))
                .collect::<Result<Vec<_>>>()
                .map(Value::List),
            Expr::Op(left, BinOp::Add, right) => {
                let (Value::List(mut left), Value::List(right)) =
                    (self.expression(left)?, self.expression(right)?)
                else {
                    return Err(self.err_at(expression.span, CompileError::UnsupportedAddition));
                };
                left.extend(right);
                Ok(Value::List(left))
            }
            Expr::Call(function, args) => {
                let Expr::Identifier(identifier) = &function.node else {
                    return Err(self.err_at(function.span, CompileError::IndirectCall));
                };
                self.call(&identifier.node.ident, function.span, args)
            }
            _ => Err(self.err_at(expression.span, CompileError::UnsupportedExpression)),
        }
    }

    fn call(
        &mut self,
        function: &str,
        function_span: Span,
        args: &CallArgsP<starlark_syntax::syntax::ast::AstNoPayload>,
    ) -> Result<Value> {
        if !DSL_FUNCTIONS.contains(&function) {
            return Err(self.err_at(
                function_span,
                CompileError::UnknownFunction {
                    function: function.to_owned(),
                    suggestion: diag::suggest(function, DSL_FUNCTIONS.iter().copied())
                        .map(str::to_owned),
                },
            ));
        }
        if matches!(
            function,
            "prompt_file" | "deps" | "workflow" | "default_autoresearch"
        ) && args
            .args
            .iter()
            .all(|argument| matches!(argument.node, Argument::Positional(_)))
        {
            let one_positional = || CompileError::NotOnePositional {
                function: function.to_owned(),
            };
            let [argument] = args.args.as_slice() else {
                return Err(self.err_at(function_span, one_positional()));
            };
            let Argument::Positional(argument) = &argument.node else {
                return Err(self.err_at(function_span, one_positional()));
            };
            let value = self.expression(argument)?;
            return match (function, value) {
                ("prompt_file", Value::String(path)) => {
                    self.context.prompt_file(&path).map(Value::String)
                }
                ("deps", Value::List(tasks)) => tasks
                    .into_iter()
                    .map(|task| match task {
                        Value::Task(task) => Ok(Value::String(task.name.0)),
                        _ => Err(CompileError::DepsEntryNotTask),
                    })
                    .collect::<Result<Vec<_>>>()
                    .map(Value::List),
                ("workflow", Value::List(tasks)) => {
                    let tasks = task_list("workflow", tasks)?;
                    let workflow = WorkflowCfg {
                        workflow_type: WorkflowType::Autoresearch,
                        result: None,
                        tasks,
                    };
                    workflow.validate()?;
                    Ok(Value::Workflow(workflow))
                }
                ("default_autoresearch", Value::List(tasks)) => {
                    default_autoresearch(task_list("default_autoresearch", tasks)?)
                        .map(Value::Workflow)
                }
                _ => Err(self.err_at(
                    function_span,
                    CompileError::WrongPositionalType {
                        function: function.to_owned(),
                    },
                )),
            };
        }

        let mut named = BTreeMap::new();
        let mut arg_spans: BTreeMap<String, Span> = BTreeMap::new();
        for argument in &args.args {
            let Argument::Named(name, value) = &argument.node else {
                return Err(self.err_at(
                    argument.span,
                    CompileError::PositionalArgument {
                        function: function.to_owned(),
                    },
                ));
            };
            let value = self.expression(value)?;
            if named.insert(name.node.clone(), value).is_some() {
                return Err(self.err_at(
                    argument.span,
                    CompileError::RepeatedArgument {
                        function: function.to_owned(),
                        argument: name.node.clone(),
                    },
                ));
            }
            arg_spans.insert(name.node.clone(), argument.span);
        }
        self.constructor(function, function_span, named, arg_spans)
            .map_err(|error| self.locate(function_span, error))
    }

    /// Build the value for a named-argument DSL call. Every kwarg an arm consumes must
    /// appear in [`known_kwargs`], which the leftover check reports against.
    fn constructor(
        &mut self,
        function: &str,
        span: Span,
        mut named: BTreeMap<String, Value>,
        arg_spans: BTreeMap<String, Span>,
    ) -> Result<Value> {
        if function == "workflow" {
            let workflow_type =
                match take_string_default(&mut named, "type", "autoresearch")?.as_str() {
                    "autoresearch" => WorkflowType::Autoresearch,
                    "custom" => WorkflowType::Custom,
                    other => {
                        return Err(CompileError::UnknownWorkflowType {
                            got: other.to_owned(),
                        });
                    }
                };
            let tasks = match take_value(&mut named, "tasks")? {
                Value::List(tasks) => task_list("workflow", tasks)?,
                _ => return Err(CompileError::TasksNotList),
            };
            let result = take_optional_task_name(&mut named, "result")?;
            self.no_unknown_kwargs(function, span, &named, &arg_spans)?;
            let workflow = WorkflowCfg {
                workflow_type,
                result,
                tasks,
            };
            workflow.validate()?;
            return Ok(Value::Workflow(workflow));
        }
        if function == "session" {
            let name = take_string(&mut named, "name")?;
            if !is_valid_session_name(&name) {
                return Err(CompileError::InvalidSessionName { name });
            }
            let decl = SessionDecl {
                harness: take_optional_string(&mut named, "harness")?,
                model: take_optional_string(&mut named, "model")?,
                effort: take_optional_string(&mut named, "effort")?,
                name,
            };
            self.no_unknown_kwargs(function, span, &named, &arg_spans)?;
            if let Some((_, first)) = self.context.sessions.get(&decl.name) {
                return Err(self.err_at(
                    span,
                    CompileError::DuplicateSession {
                        name: decl.name.clone(),
                        first: first.clone(),
                    },
                ));
            }
            self.context.sessions.insert(
                decl.name.clone(),
                (decl.clone(), self.module.file_span(span)),
            );
            return Ok(Value::Session(decl));
        }
        if matches!(function, "prompt_file" | "deps" | "default_autoresearch") {
            return Err(CompileError::NotOnePositional {
                function: function.to_owned(),
            });
        }

        let task = match function {
            "agent" => {
                let name = TaskName(take_string(&mut named, "name")?);
                let prompt = take_string(&mut named, "prompt")?;
                let mut harness = take_optional_string(&mut named, "harness")?;
                let mut model = take_optional_string(&mut named, "model")?;
                let mut effort = take_optional_string(&mut named, "effort")?;
                let session = self.take_session(&mut named, &arg_spans, span)?;
                if let Some(decl) = &session {
                    // A session is one serial conversation under one agent config, so
                    // declared defaults fill unset knobs and conflicts are errors.
                    for (knob, own, default) in [
                        ("harness", &mut harness, &decl.harness),
                        ("model", &mut model, &decl.model),
                        ("effort", &mut effort, &decl.effort),
                    ] {
                        let Some(theirs) = default else { continue };
                        match own {
                            None => *own = Some(theirs.clone()),
                            Some(mine) if mine.as_str() != theirs.as_str() => {
                                return Err(self.err_at(
                                    span,
                                    CompileError::SessionConfigConflict {
                                        task: name.0.clone(),
                                        knob,
                                        mine: mine.clone(),
                                        session: decl.name.clone(),
                                        theirs: theirs.clone(),
                                    },
                                ));
                            }
                            Some(_) => {}
                        }
                    }
                }
                let kind = TaskKind::Agent {
                    prompt,
                    harness,
                    model,
                    effort,
                };
                dsl_task(&mut named, name, kind, session.map(|decl| decl.name))?
            }
            "command" => {
                let name = TaskName(take_string(&mut named, "name")?);
                let kind = TaskKind::Command {
                    command: take_string(&mut named, "run")?,
                };
                dsl_task(&mut named, name, kind, None)?
            }
            "evaluate" => {
                let name = TaskName(take_string(&mut named, "name")?);
                let kind = TaskKind::Evaluate {
                    command: take_string(&mut named, "run")?,
                    threshold: take_optional_number(&mut named, "threshold")?,
                    direction: take_optional_direction(&mut named, "direction")?,
                };
                dsl_task(&mut named, name, kind, None)?
            }
            "top_k" => {
                let k = take_int(&mut named, "k")?;
                if k <= 0 {
                    return Err(CompileError::TopKZero);
                }
                let direction = match take_string(&mut named, "direction")?.as_str() {
                    "lower" => Direction::Lower,
                    "higher" => Direction::Higher,
                    other => {
                        return Err(CompileError::UnknownTopKDirection {
                            got: other.to_owned(),
                        });
                    }
                };
                let depends_on = take_task_names(&mut named)?;
                if depends_on.is_empty() {
                    return Err(CompileError::TopKWithoutDependencies);
                }
                Task {
                    name: TaskName(take_string(&mut named, "name")?),
                    task: TaskKind::TopK {
                        k: k as u32,
                        direction,
                    },
                    depends_on,
                    session: None,
                    needs: "any".to_owned(),
                    required: take_bool_default(&mut named, "required", true)?,
                    isolation: None,
                    join: Join::Passed,
                    stage: Stage::Iteration,
                    emits: Vec::new(),
                }
            }
            "propose" => {
                let session = self.take_session(&mut named, &arg_spans, span)?;
                if let Some(decl) = &session
                    && decl.has_defaults()
                {
                    // The engine propose turn's agent config is owned by the manifest's
                    // [agent]; accepting the defaults here would silently ignore them.
                    return Err(self.err_at(
                        arg_spans.get("session").copied().unwrap_or(span),
                        CompileError::ProposeSessionDefaults {
                            name: decl.name.clone(),
                        },
                    ));
                }
                let mut task = engine_task(&mut named, EngineOp::Propose, None)?;
                task.session = session.map(|decl| decl.name);
                task
            }
            "apply" => engine_task(&mut named, EngineOp::Apply, None)?,
            "measure" => engine_task(&mut named, EngineOp::Measure, None)?,
            "grade" => {
                let source = take_task_name(&mut named, "score")?;
                // Optional secondary axis: the named task's score breaks primary-score ties.
                let tiebreak = take_optional_task_name(&mut named, "tiebreak")?;
                let mut evidence = take_named_task_names(&mut named, "evidence")?;
                if !evidence.contains(&source) {
                    evidence.push(source.clone());
                }
                if let Some(t) = &tiebreak
                    && !evidence.contains(t)
                {
                    evidence.push(t.clone());
                }
                let mut task = engine(
                    &take_string(&mut named, "name")?,
                    EngineOp::Grade,
                    Some(source),
                    evidence,
                );
                if let TaskKind::Engine { tiebreak: slot, .. } = &mut task.task {
                    *slot = tiebreak;
                }
                task.join = parse_join(&take_string_default(&mut named, "join", "passed")?)?;
                task
            }
            "decide" => {
                let source = take_task_name(&mut named, "measurement")?;
                let depends_on = match named.remove("depends_on") {
                    None => vec![source.clone()],
                    Some(value) => task_names("depends_on", value)?,
                };
                engine(
                    &take_string(&mut named, "name")?,
                    EngineOp::Decide,
                    Some(source),
                    depends_on,
                )
            }
            _ => {
                return Err(CompileError::UnknownFunction {
                    function: function.to_owned(),
                    suggestion: None,
                });
            }
        };
        self.no_unknown_kwargs(function, span, &named, &arg_spans)?;
        self.context
            .constructed_tasks
            .insert(task.name.0.clone(), self.module.file_span(span));
        Ok(Value::Task(task))
    }

    fn no_unknown_kwargs(
        &self,
        function: &str,
        span: Span,
        named: &BTreeMap<String, Value>,
        arg_spans: &BTreeMap<String, Span>,
    ) -> Result<()> {
        let Some(unknown) = named.keys().next() else {
            return Ok(());
        };
        let at = arg_spans.get(unknown).copied().unwrap_or(span);
        Err(self.err_at(
            at,
            CompileError::UnknownArgument {
                function: function.to_owned(),
                argument: unknown.clone(),
                suggestion: diag::suggest(unknown, known_kwargs(function).iter().copied())
                    .map(str::to_owned),
            },
        ))
    }

    /// A `session(...)` value, or a bare string. Strings keep the historical
    /// pass-through only while the file declares no sessions; once any `session()`
    /// exists every string must name a declared one.
    fn take_session(
        &mut self,
        named: &mut BTreeMap<String, Value>,
        arg_spans: &BTreeMap<String, Span>,
        call_span: Span,
    ) -> Result<Option<SessionDecl>> {
        let span = arg_spans.get("session").copied().unwrap_or(call_span);
        match named.remove("session").unwrap_or(Value::None) {
            Value::None => Ok(None),
            Value::Session(decl) => {
                self.context.bound_sessions.insert(decl.name.clone());
                Ok(Some(decl))
            }
            Value::String(name) => {
                if let Some((decl, _)) = self.context.sessions.get(&name) {
                    let decl = decl.clone();
                    self.context.bound_sessions.insert(name);
                    Ok(Some(decl))
                } else if self.context.sessions.is_empty() {
                    let site = self.module.file_span(span);
                    self.context
                        .string_session_refs
                        .entry(name.clone())
                        .or_insert(site);
                    Ok(Some(SessionDecl {
                        name,
                        harness: None,
                        model: None,
                        effort: None,
                    }))
                } else {
                    Err(self.err_at(
                        span,
                        CompileError::UndeclaredSession {
                            suggestion: diag::suggest(
                                &name,
                                self.context.sessions.keys().map(String::as_str),
                            )
                            .map(str::to_owned),
                            name,
                        },
                    ))
                }
            }
            _ => Err(self.err_at(span, CompileError::SessionWrongType)),
        }
    }
}

fn task_list(function: &str, tasks: Vec<Value>) -> Result<Vec<Task>> {
    if tasks.len() > MAX_TASKS {
        return Err(CompileError::TooManyTasks {
            function: function.to_owned(),
            count: tasks.len(),
        });
    }
    tasks
        .into_iter()
        .map(|task| match task {
            Value::Task(task) => Ok(task),
            _ => Err(CompileError::TaskListEntryNotTask {
                function: function.to_owned(),
            }),
        })
        .collect()
}

fn default_autoresearch(mut extras: Vec<Task>) -> Result<WorkflowCfg> {
    let propose = engine("propose", EngineOp::Propose, None, Vec::new());
    // Epilogue extras never splice into the iteration chain: no implicit propose
    // dependency, and they cannot be the sinks apply waits on.
    for task in &mut extras {
        if task.stage == Stage::Iteration && task.depends_on.is_empty() {
            task.depends_on.push(propose.name.clone());
        }
    }
    let mut tasks = vec![propose];
    tasks.extend(extras);
    let sinks: Vec<TaskName> = tasks
        .iter()
        .filter(|task| {
            task.stage == Stage::Iteration
                && !tasks.iter().any(|other| {
                    other
                        .depends_on
                        .iter()
                        .any(|dependency| dependency == &task.name)
                })
        })
        .map(|task| task.name.clone())
        .collect();
    tasks.push(engine("apply", EngineOp::Apply, None, sinks));
    tasks.push(engine(
        "measure",
        EngineOp::Measure,
        None,
        vec!["apply".into()],
    ));
    tasks.push(engine(
        "decide",
        EngineOp::Decide,
        Some("measure".into()),
        vec!["measure".into()],
    ));
    if tasks.len() > MAX_TASKS {
        return Err(CompileError::TooManyTasks {
            function: "default_autoresearch".to_owned(),
            count: tasks.len(),
        });
    }
    let workflow = WorkflowCfg {
        workflow_type: WorkflowType::Autoresearch,
        result: Some("decide".into()),
        tasks,
    };
    workflow.validate()?;
    Ok(workflow)
}

fn engine_task(
    named: &mut BTreeMap<String, Value>,
    op: EngineOp,
    source: Option<TaskName>,
) -> Result<Task> {
    Ok(engine(
        &take_string(named, "name")?,
        op,
        source,
        take_task_names(named)?,
    ))
}

fn dsl_task(
    named: &mut BTreeMap<String, Value>,
    name: TaskName,
    kind: TaskKind,
    session: Option<String>,
) -> Result<Task> {
    Ok(Task {
        name,
        task: kind,
        depends_on: take_task_names(named)?,
        session,
        needs: take_string_default(named, "needs", "any")?,
        required: take_bool_default(named, "required", true)?,
        isolation: isolation(take_bool_default(named, "isolated", false)?),
        join: parse_join(&take_string_default(named, "join", "all")?)?,
        stage: parse_stage(&take_string_default(named, "stage", "iteration")?)?,
        emits: take_output_fields(named)?,
    })
}

fn parse_stage(value: &str) -> Result<Stage> {
    match value {
        "iteration" => Ok(Stage::Iteration),
        "epilogue" => Ok(Stage::Epilogue),
        other => Err(CompileError::UnknownStage {
            got: other.to_owned(),
        }),
    }
}

fn engine(name: &str, op: EngineOp, source: Option<TaskName>, depends_on: Vec<TaskName>) -> Task {
    Task {
        name: TaskName(name.to_owned()),
        task: TaskKind::Engine {
            op,
            source,
            tiebreak: None,
        },
        depends_on,
        session: None,
        needs: "any".to_owned(),
        required: true,
        isolation: None,
        join: Join::All,
        stage: Stage::Iteration,
        emits: Vec::new(),
    }
}

fn take_output_fields(named: &mut BTreeMap<String, Value>) -> Result<Vec<OutputField>> {
    match named.remove("emits").unwrap_or(Value::None) {
        Value::None => Ok(Vec::new()),
        Value::List(fields) => fields
            .into_iter()
            .map(|field| match field {
                Value::String(field) => Ok(OutputField(field)),
                _ => Err(CompileError::EmitsEntryNotString),
            })
            .collect(),
        _ => Err(CompileError::EmitsNotList),
    }
}

/// Mirrors the session charset rule enforced by plan validation, so the error lands at
/// the declaration instead of after the graph is assembled.
fn is_valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The type an argument had to be, for [`CompileError::WrongArgumentType`].
fn wrong_type(name: &str, expected: &'static str) -> CompileError {
    CompileError::WrongArgumentType {
        argument: name.to_owned(),
        expected,
    }
}

fn take_value(named: &mut BTreeMap<String, Value>, name: &str) -> Result<Value> {
    named
        .remove(name)
        .ok_or_else(|| CompileError::MissingArgument {
            argument: name.to_owned(),
        })
}

fn take_string(named: &mut BTreeMap<String, Value>, name: &str) -> Result<String> {
    match take_value(named, name)? {
        Value::String(value) => Ok(value),
        _ => Err(wrong_type(name, "a string")),
    }
}

fn take_optional_string(named: &mut BTreeMap<String, Value>, name: &str) -> Result<Option<String>> {
    match named.remove(name).unwrap_or(Value::None) {
        Value::None => Ok(None),
        Value::String(value) => Ok(Some(value)),
        _ => Err(wrong_type(name, "a string or None")),
    }
}

fn take_task_name(named: &mut BTreeMap<String, Value>, name: &str) -> Result<TaskName> {
    match take_value(named, name)? {
        Value::String(value) => Ok(TaskName(value)),
        Value::Task(task) => Ok(task.name),
        _ => Err(wrong_type(name, "a task or task-name string")),
    }
}

fn take_optional_task_name(
    named: &mut BTreeMap<String, Value>,
    name: &str,
) -> Result<Option<TaskName>> {
    match named.remove(name).unwrap_or(Value::None) {
        Value::None => Ok(None),
        Value::String(value) => Ok(Some(TaskName(value))),
        Value::Task(task) => Ok(Some(task.name)),
        _ => Err(wrong_type(name, "a task, task-name string, or None")),
    }
}

fn take_string_default(
    named: &mut BTreeMap<String, Value>,
    name: &str,
    default: &str,
) -> Result<String> {
    match named.remove(name) {
        None => Ok(default.to_owned()),
        Some(Value::String(value)) => Ok(value),
        Some(_) => Err(wrong_type(name, "a string")),
    }
}

fn take_bool_default(
    named: &mut BTreeMap<String, Value>,
    name: &str,
    default: bool,
) -> Result<bool> {
    match named.remove(name) {
        None => Ok(default),
        Some(Value::Bool(value)) => Ok(value),
        Some(_) => Err(wrong_type(name, "True or False")),
    }
}

fn take_int(named: &mut BTreeMap<String, Value>, name: &str) -> Result<i32> {
    match take_value(named, name)? {
        Value::Int(value) => Ok(value),
        _ => Err(wrong_type(name, "an integer")),
    }
}

fn take_optional_number(named: &mut BTreeMap<String, Value>, name: &str) -> Result<Option<f64>> {
    match named.remove(name).unwrap_or(Value::None) {
        Value::None => Ok(None),
        Value::Int(value) => Ok(Some(value as f64)),
        Value::Float(value) => Ok(Some(value)),
        _ => Err(wrong_type(name, "a number or None")),
    }
}

fn take_optional_direction(
    named: &mut BTreeMap<String, Value>,
    name: &str,
) -> Result<Option<Direction>> {
    match named.remove(name).unwrap_or(Value::None) {
        Value::None => Ok(None),
        Value::String(value) if value == "lower" => Ok(Some(Direction::Lower)),
        Value::String(value) if value == "higher" => Ok(Some(Direction::Higher)),
        Value::String(value) => Err(CompileError::UnknownDirection {
            argument: name.to_owned(),
            got: value,
        }),
        _ => Err(wrong_type(name, "a string or None")),
    }
}

fn take_task_names(named: &mut BTreeMap<String, Value>) -> Result<Vec<TaskName>> {
    take_named_task_names_optional(named, "depends_on")
}

fn take_named_task_names(named: &mut BTreeMap<String, Value>, name: &str) -> Result<Vec<TaskName>> {
    if !named.contains_key(name) {
        return Err(CompileError::MissingArgument {
            argument: name.to_owned(),
        });
    }
    take_named_task_names_optional(named, name)
}

fn take_named_task_names_optional(
    named: &mut BTreeMap<String, Value>,
    argument: &str,
) -> Result<Vec<TaskName>> {
    named
        .remove(argument)
        .map_or(Ok(Vec::new()), |value| task_names(argument, value))
}

fn task_names(argument: &str, value: Value) -> Result<Vec<TaskName>> {
    let Value::List(names) = value else {
        return Err(CompileError::TaskNamesNotList {
            argument: argument.to_owned(),
        });
    };
    names
        .into_iter()
        .map(|name| match name {
            Value::String(name) => Ok(TaskName(name)),
            Value::Task(task) => Ok(task.name),
            _ => Err(CompileError::TaskNameEntryWrongType {
                argument: argument.to_owned(),
            }),
        })
        .collect()
}

fn isolation(isolated: bool) -> Option<Isolation> {
    isolated.then_some(Isolation::Worktree)
}

fn parse_join(join: &str) -> Result<Join> {
    match join {
        "all" => Ok(Join::All),
        "passed" => Ok(Join::Passed),
        other => Err(CompileError::UnknownJoin {
            got: other.to_owned(),
        }),
    }
}

pub fn compile_file(path: &Path, pack_dir: &Path) -> Result<CompiledWorkflow> {
    let source =
        std::fs::read_to_string(path).map_err(FileError::at("reading workflow source", path))?;
    compile_source(&source, path, pack_dir)
}

/// Compile `workflow.star` into the manifest's generated `[workflow]` block.
pub fn materialize_manifest(
    source_path: &Path,
    manifest_path: &Path,
) -> std::result::Result<CompiledWorkflow, MaterializeError> {
    use toml_edit::DocumentMut;

    #[derive(serde::Serialize)]
    struct ManifestWorkflow<'a> {
        workflow: &'a WorkflowCfg,
    }

    let compiled = compile_file(source_path, parent_or_cwd(manifest_path))?;
    let manifest = std::fs::read_to_string(manifest_path)
        .map_err(FileError::at("reading manifest", manifest_path))?;
    let mut document: DocumentMut =
        manifest
            .parse()
            .map_err(|cause| MaterializeError::ParseManifest {
                path: manifest_path.to_path_buf(),
                cause,
            })?;
    let workflow_toml = toml::to_string(&ManifestWorkflow {
        workflow: &compiled.workflow,
    })?;
    document.remove("workflow");
    let mut materialized = document.to_string();
    if !materialized.ends_with('\n') {
        materialized.push('\n');
    }
    materialized.push_str(
        "\n# Generated from workflow.star. Edit the Starlark source; scope recompiles it.\n",
    );
    materialized.push_str(&workflow_toml);
    write_atomically(manifest_path, &materialized)?;
    Ok(compiled)
}

fn write_atomically(path: &Path, body: &str) -> std::result::Result<(), FileError> {
    let mut file = tempfile::NamedTempFile::new_in(parent_or_cwd(path))
        .map_err(FileError::at("creating temp file beside", path))?;
    // Preserve the manifest's mode across the temporary-file rename.
    if let Ok(existing) = std::fs::metadata(path) {
        file.as_file()
            .set_permissions(existing.permissions())
            .map_err(FileError::at("preserving permissions on", path))?;
    }
    let write = FileError::at("writing", path);
    std::io::Write::write_all(file.as_file_mut(), body.as_bytes())
        .and_then(|()| file.as_file().sync_all())
        .map_err(write)?;
    file.persist(path)
        .map_err(|error| FileError::at("installing", path)(error.error))?;
    Ok(())
}

/// A bare filename has an empty parent; tempfiles and prompt resolution need a real directory.
pub(crate) fn parent_or_cwd(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// Compile a conventional sibling `workflow.star` when one is present.
pub fn materialize_sibling_manifest(
    manifest_path: &Path,
) -> std::result::Result<Option<CompiledWorkflow>, MaterializeError> {
    let source_path = parent_or_cwd(manifest_path).join("workflow.star");
    source_path
        .exists()
        .then(|| materialize_manifest(&source_path, manifest_path))
        .transpose()
}

pub fn compile_source(source: &str, filename: &Path, pack_dir: &Path) -> Result<CompiledWorkflow> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(CompileError::SourceTooLarge {
            bytes: source.len(),
        });
    }
    let ast = AstModule::parse(
        &filename.display().to_string(),
        source.to_owned(),
        &Dialect::Standard,
    )
    .map_err(|error| CompileError::Parse(error.to_string()))?;
    let mut compiler = Compiler {
        module: &ast,
        context: CompileContext {
            pack_dir: pack_dir.to_path_buf(),
            prompt_files: BTreeSet::new(),
            total_prompt_bytes: 0,
            eval_steps: 0,
            constructed_tasks: BTreeMap::new(),
            sessions: BTreeMap::new(),
            bound_sessions: BTreeSet::new(),
            string_session_refs: BTreeMap::new(),
        },
        variables: BTreeMap::new(),
    };
    let Some(Value::Workflow(workflow)) = compiler.statement(ast.statement())? else {
        return Err(CompileError::NoWorkflowResult);
    };
    let context = compiler.context;
    let included: BTreeSet<&str> = workflow.tasks.iter().map(|t| t.name.0.as_str()).collect();
    let dropped: Vec<String> = context
        .constructed_tasks
        .iter()
        .filter(|(name, _)| !included.contains(name.as_str()))
        .map(|(name, site)| format!("{name:?} ({site})"))
        .collect();
    if !dropped.is_empty() {
        return Err(CompileError::DroppedTasks { sites: dropped });
    }
    if !context.sessions.is_empty() && !context.string_session_refs.is_empty() {
        let refs: Vec<String> = context
            .string_session_refs
            .iter()
            .map(|(name, site)| format!("{name:?} ({site})"))
            .collect();
        return Err(CompileError::LateSessionDeclaration { sites: refs });
    }
    let unbound: Vec<String> = context
        .sessions
        .iter()
        .filter(|(name, _)| !context.bound_sessions.contains(*name))
        .map(|(name, (_, site))| format!("{name:?} ({site})"))
        .collect();
    if !unbound.is_empty() {
        return Err(CompileError::UnboundSessions { sites: unbound });
    }
    let canonical_json = serde_json::to_string_pretty(&workflow)? + "\n";
    Ok(CompiledWorkflow {
        workflow,
        prompt_files: context.prompt_files.into_iter().collect(),
        canonical_json,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_pack(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("crucible-starlark-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("prompts")).unwrap();
        dir
    }

    #[test]
    fn compiles_a_panel_and_embeds_pack_relative_prompts() {
        let pack = temp_pack("panel");
        std::fs::write(pack.join("prompts/correctness.md"), "Check correctness.\n").unwrap();
        let source = r#"
reviews = [
    agent(
        name = "review-correctness",
        prompt = prompt_file("prompts/correctness.md"),
        model = "claude-opus-4-6",
        effort = "high",
        isolated = True,
    ),
    agent(
        name = "review-copy",
        prompt = "Review prose.",
        model = "claude-sonnet-5",
        required = False,
        isolated = True,
    ),
]
workflow(reviews + [
    command(
        name = "gate",
        run = "./join_gate.sh",
        depends_on = deps(reviews),
        join = "passed",
    ),
])
"#;
        let compiled = compile_source(source, &pack.join("workflow.star"), &pack).unwrap();
        assert_eq!(compiled.workflow.tasks.len(), 3);
        assert_eq!(
            compiled.prompt_files,
            [PathBuf::from("prompts/correctness.md")]
        );
        let TaskKind::Agent { prompt, .. } = &compiled.workflow.tasks[0].task else {
            panic!("expected agent task")
        };
        assert_eq!(prompt, "Check correctness.\n");
        assert!(compiled.canonical_json.contains("review-copy"));

        std::fs::write(
            pack.join("crucible.toml"),
            "# preserved\n[repo]\npath = \".\"\n\n[[workflow.task]]\nname = \"stale\"\nkind = \"command\"\ncommand = \"false\"\n",
        )
        .unwrap();
        std::fs::write(pack.join("workflow.star"), source).unwrap();
        materialize_manifest(&pack.join("workflow.star"), &pack.join("crucible.toml")).unwrap();
        let manifest = std::fs::read_to_string(pack.join("crucible.toml")).unwrap();
        assert!(manifest.contains("# preserved"));
        assert!(manifest.contains("Generated from workflow.star"));
        assert!(manifest.contains("[[workflow.task]]"));
        assert!(!manifest.contains("name = \"stale\""));
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn compiles_explicit_autoresearch_custom_and_default_workflows() {
        let pack = temp_pack("types");
        let explicit = r#"
candidate = propose(name = "invent", session = "solver")
review = command(name = "review", run = "./review.sh", depends_on = [candidate])
live = apply(name = "deploy", depends_on = [review])
score = measure(name = "benchmark", depends_on = [live])
choice = decide(name = "choose", measurement = score)
workflow(
    type = "autoresearch",
    tasks = [candidate, review, live, score, choice],
    result = choice,
)
"#;
        let compiled = compile_source(explicit, &pack.join("explicit.star"), &pack).unwrap();
        assert_eq!(compiled.workflow.workflow_type, WorkflowType::Autoresearch);
        assert_eq!(compiled.workflow.result, Some("choose".into()));
        assert_eq!(compiled.workflow.tasks[0].name.0, "invent");
        assert_eq!(
            compiled.workflow.tasks[0].session.as_deref(),
            Some("solver")
        );

        let custom = r#"
publish = command(name = "publish", run = "./publish.sh")
workflow(type = "custom", tasks = [publish], result = publish)
"#;
        let compiled = compile_source(custom, &pack.join("custom.star"), &pack).unwrap();
        assert_eq!(compiled.workflow.workflow_type, WorkflowType::Custom);
        assert_eq!(compiled.workflow.tasks.len(), 1);

        let default = r#"
review = command(name = "review", run = "./review.sh")
default_autoresearch([review])
"#;
        let compiled = compile_source(default, &pack.join("default.star"), &pack).unwrap();
        let names: Vec<&str> = compiled
            .workflow
            .tasks
            .iter()
            .map(|task| task.name.0.as_str())
            .collect();
        assert_eq!(names, ["propose", "review", "apply", "measure", "decide"]);
        assert_eq!(compiled.workflow.result, Some("decide".into()));
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn stage_epilogue_authors_a_run_scoped_task() {
        let pack = temp_pack("epilogue");
        let source = r#"
review = command(name = "review", run = "./review.sh")
racecheck = command(
    name = "racecheck",
    run = "./racecheck.sh",
    stage = "epilogue",
    required = False,
)
default_autoresearch([review, racecheck])
"#;
        let compiled = compile_source(source, &pack.join("epilogue.star"), &pack).unwrap();
        let racecheck = compiled
            .workflow
            .tasks
            .iter()
            .find(|task| task.name.0 == "racecheck")
            .unwrap();
        assert_eq!(racecheck.stage, Stage::Epilogue);
        assert!(
            racecheck.depends_on.is_empty(),
            "epilogue tasks get no implicit propose dependency"
        );
        let apply = compiled
            .workflow
            .tasks
            .iter()
            .find(|task| task.name.0 == "apply")
            .unwrap();
        assert_eq!(
            apply.depends_on,
            ["review".into()],
            "apply waits on the iteration sink only"
        );

        let bad = r#"
racecheck = command(name = "racecheck", run = "./racecheck.sh", stage = "finale")
default_autoresearch([racecheck])
"#;
        let error =
            crate::errors::report(&compile_source(bad, &pack.join("bad.star"), &pack).unwrap_err());
        assert!(error.contains("`iteration` or `epilogue`"), "{error}");
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn compiles_parallel_measurement_rungs_and_grade() {
        let pack = temp_pack("measurement");
        let source = r#"
candidate = propose(name = "invent")
live = apply(name = "deploy", depends_on = [candidate])
correctness = evaluate(
    name = "correctness",
    run = "./correctness.sh",
    depends_on = [live],
    threshold = 1,
    direction = "higher",
    isolated = True,
)
latency = evaluate(
    name = "latency",
    run = "./latency.sh",
    depends_on = [correctness],
    threshold = 12.5,
    direction = "lower",
    isolated = True,
)
racecheck = evaluate(
    name = "racecheck",
    run = "./racecheck.sh",
    depends_on = [correctness],
    required = False,
    isolated = True,
)
measurement = grade(
    name = "final-grade",
    evidence = [correctness, latency, racecheck],
    score = latency,
    tiebreak = racecheck,
)
choice = decide(name = "choose", measurement = measurement)
workflow(
    type = "autoresearch",
    tasks = [candidate, live, correctness, latency, racecheck, measurement, choice],
    result = choice,
)
"#;
        let compiled = compile_source(source, &pack.join("workflow.star"), &pack).unwrap();
        assert_eq!(
            compiled.workflow.tasks[3].depends_on,
            vec!["correctness".into()]
        );
        assert_eq!(
            compiled.workflow.tasks[4].depends_on,
            vec!["correctness".into()]
        );
        let grade = &compiled.workflow.tasks[5];
        assert_eq!(grade.join, Join::Passed);
        assert_eq!(grade.depends_on.len(), 3);
        assert!(matches!(
            grade.task,
            TaskKind::Engine {
                op: EngineOp::Grade,
                source: Some(ref source),
                tiebreak: Some(ref tiebreak),
            } if source == &TaskName("latency".to_string())
                && tiebreak == &TaskName("racecheck".to_string())
        ));
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn grade_join_is_authorable_with_passed_as_the_default() {
        let pack = temp_pack("grade-join");
        let source = r#"
score = evaluate(name = "score", run = "./score.sh")
strict = grade(name = "strict", evidence = [score], score = score, join = "all")
lossy = grade(name = "lossy", evidence = [score], score = score)
workflow(type = "custom", tasks = [score, strict, lossy], result = strict)
"#;
        let compiled = compile_source(source, &pack.join("workflow.star"), &pack).unwrap();
        assert_eq!(compiled.workflow.tasks[1].join, Join::All);
        assert_eq!(compiled.workflow.tasks[2].join, Join::Passed);
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn prompt_file_refuses_escape_symlink_and_oversize_content() {
        let pack = temp_pack("paths");
        let source = |path: &str| {
            format!("workflow([agent(name = \"r\", prompt = prompt_file({path:?}))])\n")
        };
        assert!(
            crate::errors::report(
                &compile_source(&source("../secret.md"), &pack.join("workflow.star"), &pack)
                    .unwrap_err()
            )
            .contains("may not contain")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            std::fs::write(pack.join("real.md"), "secret").unwrap();
            symlink(pack.join("real.md"), pack.join("prompts/link.md")).unwrap();
            assert!(
                crate::errors::report(
                    &compile_source(
                        &source("prompts/link.md"),
                        &pack.join("workflow.star"),
                        &pack
                    )
                    .unwrap_err()
                )
                .contains("symlink")
            );
        }
        std::fs::write(
            pack.join("prompts/huge.md"),
            vec![b'x'; MAX_PROMPT_BYTES + 1],
        )
        .unwrap();
        assert!(
            crate::errors::report(
                &compile_source(
                    &source("prompts/huge.md"),
                    &pack.join("workflow.star"),
                    &pack
                )
                .unwrap_err()
            )
            .contains("maximum")
        );
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn rejects_loads_and_runaway_evaluation() {
        let pack = temp_pack("bounds");
        let load = "load(\"x.star\", \"x\")\nworkflow([])\n";
        assert!(
            crate::errors::report(
                &compile_source(load, &pack.join("workflow.star"), &pack).unwrap_err()
            )
            .contains("may not use load")
        );
        let runaway = "xs = []\nfor i in range(1000000):\n    xs.append(i)\nworkflow([])\n";
        assert!(compile_source(runaway, &pack.join("workflow.star"), &pack).is_err());
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn adversarial_example_matches_its_golden_and_materialized_manifest() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let pack = root.join("examples/adversarial-review");
        let compiled = compile_file(&pack.join("workflow.star"), &pack).unwrap();
        assert_eq!(
            compiled.canonical_json,
            std::fs::read_to_string(pack.join("expected-workflow.json")).unwrap()
        );
        let manifest = crate::manifest::Manifest::load(&pack.join("crucible.toml")).unwrap();
        assert_eq!(
            serde_json::to_value(&compiled.workflow).unwrap(),
            serde_json::to_value(&manifest.workflow).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn materializing_is_atomic_and_keeps_the_manifest_mode() {
        use std::os::unix::fs::PermissionsExt;

        let pack = temp_pack("atomic");
        std::fs::write(pack.join("prompts/review.md"), "Review it.\n").unwrap();
        std::fs::write(
            pack.join("workflow.star"),
            "workflow([agent(name = \"review\", prompt = prompt_file(\"prompts/review.md\"))])\n",
        )
        .unwrap();
        let manifest_path = pack.join("crucible.toml");
        std::fs::write(&manifest_path, "# preserved\n[repo]\npath = \".\"\n").unwrap();
        std::fs::set_permissions(&manifest_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        materialize_manifest(&pack.join("workflow.star"), &manifest_path).unwrap();

        let mode = std::fs::metadata(&manifest_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o644, "materializing must not narrow the mode");
        let body = std::fs::read_to_string(&manifest_path).unwrap();
        assert!(body.contains("# preserved"), "{body}");
        assert!(body.contains("[[workflow.task]]"), "{body}");
        let strays: Vec<_> = std::fs::read_dir(&pack)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .filter(|name| name.to_string_lossy().starts_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "left temp files: {strays:?}");
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn unknown_kwarg_function_and_variable_errors_carry_location_and_suggestion() {
        let pack = temp_pack("diagnostics");
        let kwarg = "a = agent(name = \"a\", prompt = \"p\", depend_on = [])\nworkflow([a])\n";
        let err = crate::errors::report(
            &compile_source(kwarg, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        assert!(err.contains("workflow.star:1:"), "{err}");
        assert!(
            err.contains("agent() has unknown argument \"depend_on\""),
            "{err}"
        );
        assert!(err.contains("did you mean \"depends_on\"?"), "{err}");

        let function = "a = agnt(name = \"a\", prompt = \"p\")\nworkflow([a])\n";
        let err = crate::errors::report(
            &compile_source(function, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        assert!(err.contains("workflow.star:1:"), "{err}");
        assert!(
            err.contains("unknown workflow DSL function \"agnt\""),
            "{err}"
        );
        assert!(err.contains("did you mean \"agent\"?"), "{err}");

        let variable = "candidate = command(name = \"c\", run = \"true\")\nworkflow([candidat])\n";
        let err = crate::errors::report(
            &compile_source(variable, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        assert!(err.contains("workflow.star:2:"), "{err}");
        assert!(
            err.contains("unknown workflow variable \"candidat\""),
            "{err}"
        );
        assert!(err.contains("did you mean \"candidate\"?"), "{err}");
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn constructed_but_dropped_tasks_are_a_compile_error_naming_the_site() {
        let pack = temp_pack("dropped");
        let source = r#"
keep = command(name = "keep", run = "true")
racecheck = evaluate(name = "racecheck", run = "true")
workflow(type = "custom", tasks = [keep], result = keep)
"#;
        let err = crate::errors::report(
            &compile_source(source, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        assert!(err.contains("not included in the workflow"), "{err}");
        assert!(err.contains("\"racecheck\""), "{err}");
        assert!(err.contains("workflow.star:3:"), "{err}");
        assert!(!err.contains("\"keep\""), "{err}");
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn session_defaults_materialize_and_conflicts_are_rejected() {
        let pack = temp_pack("session-defaults");
        let source = r#"
solver = session(name = "solver", model = "claude-opus-4-6", effort = "high")
a = agent(name = "a", prompt = "p", session = solver)
b = agent(name = "b", prompt = "p", model = "claude-opus-4-6", session = "solver", depends_on = [a])
workflow(type = "custom", tasks = [a, b], result = b)
"#;
        let compiled = compile_source(source, &pack.join("workflow.star"), &pack).unwrap();
        for task in &compiled.workflow.tasks {
            assert_eq!(task.session.as_deref(), Some("solver"));
            let TaskKind::Agent { model, effort, .. } = &task.task else {
                panic!("expected agent task");
            };
            assert_eq!(model.as_deref(), Some("claude-opus-4-6"), "{}", task.name);
            assert_eq!(effort.as_deref(), Some("high"), "{}", task.name);
        }

        let conflict = r#"
solver = session(name = "solver", model = "claude-opus-4-6")
a = agent(name = "a", prompt = "p", model = "claude-sonnet-5", session = solver)
workflow(type = "custom", tasks = [a], result = a)
"#;
        let err = crate::errors::report(
            &compile_source(conflict, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        assert!(err.contains("one conversation under one config"), "{err}");
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn duplicate_session_declarations_error_at_the_second_site() {
        let pack = temp_pack("session-dup");
        let source =
            "a = session(name = \"solver\")\nb = session(name = \"solver\")\nworkflow([])\n";
        let err = crate::errors::report(
            &compile_source(source, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        assert!(err.contains("already declared"), "{err}");
        assert!(err.contains("workflow.star:2:"), "{err}");
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn string_session_refs_are_checked_once_any_session_is_declared() {
        let pack = temp_pack("session-strings");
        let typo = r#"
solver = session(name = "solver")
a = agent(name = "a", prompt = "p", session = solver)
b = agent(name = "b", prompt = "p", session = "sovler", depends_on = [a])
workflow(type = "custom", tasks = [a, b], result = b)
"#;
        let err = crate::errors::report(
            &compile_source(typo, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        assert!(err.contains("session \"sovler\" is not declared"), "{err}");
        assert!(err.contains("did you mean \"solver\"?"), "{err}");

        // Without any declaration, bare strings keep the historical pass-through.
        let bare = r#"
a = agent(name = "a", prompt = "p", session = "solver")
b = agent(name = "b", prompt = "p", session = "sovler", depends_on = [a])
workflow(type = "custom", tasks = [a, b], result = b)
"#;
        let compiled = compile_source(bare, &pack.join("workflow.star"), &pack).unwrap();
        assert_eq!(
            compiled.workflow.tasks[1].session.as_deref(),
            Some("sovler")
        );

        let late = r#"
a = agent(name = "a", prompt = "p", session = "solver")
solver = session(name = "solver")
b = agent(name = "b", prompt = "p", session = solver, depends_on = [a])
workflow(type = "custom", tasks = [a, b], result = b)
"#;
        let err = crate::errors::report(
            &compile_source(late, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        assert!(err.contains("before any session() declaration"), "{err}");
        assert!(err.contains("\"solver\""), "{err}");
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn unbound_sessions_and_propose_session_defaults_are_rejected() {
        let pack = temp_pack("session-rules");
        let unbound = r#"
solver = session(name = "solver")
a = agent(name = "a", prompt = "p")
workflow(type = "custom", tasks = [a], result = a)
"#;
        let err = crate::errors::report(
            &compile_source(unbound, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        assert!(err.contains("never bound to a task"), "{err}");
        assert!(err.contains("\"solver\""), "{err}");

        let propose_defaults = r#"
solver = session(name = "solver", model = "claude-opus-4-6")
c = propose(name = "c", session = solver)
workflow(type = "custom", tasks = [c], result = c)
"#;
        let err = crate::errors::report(
            &compile_source(propose_defaults, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        assert!(err.contains("manifest's [agent]"), "{err}");

        // A default-free session binds to propose exactly as a string does.
        let plain = r#"
solver = session(name = "solver")
c = propose(name = "c", session = solver)
workflow(type = "custom", tasks = [c], result = c)
"#;
        let compiled = compile_source(plain, &pack.join("workflow.star"), &pack).unwrap();
        assert_eq!(
            compiled.workflow.tasks[0].session.as_deref(),
            Some("solver")
        );

        let bad_name = "s = session(name = \"has space\")\nworkflow([])\n";
        let err = crate::errors::report(
            &compile_source(bad_name, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        assert!(err.contains("1-64 ASCII"), "{err}");
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn emits_parses_on_dsl_tasks_and_is_unknown_on_engine_constructors() {
        let pack = temp_pack("emits");
        let source = r#"
e = evaluate(name = "e", run = "true", emits = ["score", "pass"])
workflow(type = "custom", tasks = [e], result = e)
"#;
        let compiled = compile_source(source, &pack.join("workflow.star"), &pack).unwrap();
        assert_eq!(
            compiled.workflow.tasks[0].emits,
            vec![
                crate::plan::ir::OutputField("score".into()),
                crate::plan::ir::OutputField("pass".into()),
            ]
        );

        for source in [
            "c = propose(name = \"c\", emits = [\"score\"])\nworkflow([c])\n",
            "m = command(name = \"m\", run = \"true\")\np = top_k(name = \"p\", k = 1, direction = \"lower\", depends_on = [m], emits = [\"kept\"])\nworkflow([m, p])\n",
        ] {
            let err = compile_source(source, &pack.join("workflow.star"), &pack).unwrap_err();
            let err = crate::errors::report(&err);
            assert!(err.contains("unknown argument \"emits\""), "{err}");
        }
        let _ = std::fs::remove_dir_all(&pack);
    }

    /// The drift guard for [`known_kwargs`]: per constructor, a call carrying every
    /// listed kwarg compiles (so the arm consumes each one) and an unlisted kwarg
    /// errors (so the arm consumes nothing off-table).
    #[test]
    fn every_declared_kwarg_compiles_and_an_unlisted_one_errors() {
        let pack = temp_pack("kwarg-slices");
        let cases: &[(&str, &str)] = &[
            (
                "agent",
                "s = session(name = \"sess\")\na = agent(name = \"a\", prompt = \"p\", harness = \"claude\", model = \"m\", effort = \"high\", session = s, emits = [\"score\"], depends_on = [], needs = \"any\", required = True, isolated = False, join = \"all\", stage = \"iteration\"{extra})\nworkflow(type = \"custom\", tasks = [a], result = a)\n",
            ),
            (
                "command",
                "c = command(name = \"c\", run = \"true\", emits = [\"score\"], depends_on = [], needs = \"any\", required = True, isolated = False, join = \"all\", stage = \"iteration\"{extra})\nworkflow(type = \"custom\", tasks = [c], result = c)\n",
            ),
            (
                "evaluate",
                "e = evaluate(name = \"e\", run = \"true\", threshold = 1, direction = \"higher\", emits = [\"score\"], depends_on = [], needs = \"any\", required = True, isolated = False, join = \"all\", stage = \"iteration\"{extra})\nworkflow(type = \"custom\", tasks = [e], result = e)\n",
            ),
            (
                "top_k",
                "m = command(name = \"m\", run = \"true\")\np = top_k(name = \"p\", k = 1, direction = \"lower\", depends_on = [m], required = True{extra})\nworkflow(type = \"custom\", tasks = [m, p], result = p)\n",
            ),
            (
                "propose",
                "c = propose(name = \"c\", session = \"solver\", depends_on = []{extra})\nworkflow(type = \"custom\", tasks = [c], result = c)\n",
            ),
            (
                "apply",
                "a = apply(name = \"a\", depends_on = []{extra})\nworkflow(type = \"custom\", tasks = [a], result = a)\n",
            ),
            (
                "measure",
                "m = measure(name = \"m\", depends_on = []{extra})\nworkflow(type = \"custom\", tasks = [m], result = m)\n",
            ),
            (
                "grade",
                "s = evaluate(name = \"s\", run = \"true\")\ng = grade(name = \"g\", score = s, evidence = [s], join = \"passed\"{extra})\nworkflow(type = \"custom\", tasks = [s, g], result = g)\n",
            ),
            (
                "decide",
                "s = evaluate(name = \"s\", run = \"true\")\ng = grade(name = \"g\", score = s, evidence = [s])\nd = decide(name = \"d\", measurement = g, depends_on = [g]{extra})\nworkflow(type = \"custom\", tasks = [s, g, d], result = d)\n",
            ),
            (
                "session",
                "s = session(name = \"s\", harness = \"claude\", model = \"m\", effort = \"high\"{extra})\na = agent(name = \"a\", prompt = \"p\", session = s)\nworkflow(type = \"custom\", tasks = [a], result = a)\n",
            ),
            (
                "workflow",
                "c = command(name = \"c\", run = \"true\")\nworkflow(type = \"custom\", tasks = [c], result = c{extra})\n",
            ),
        ];
        for (function, template) in cases {
            for kwarg in known_kwargs(function) {
                assert!(
                    template.contains(&format!("{kwarg} = ")),
                    "{function} template does not exercise kwarg {kwarg:?}"
                );
            }
            let ok = template.replace("{extra}", "");
            compile_source(&ok, &pack.join("workflow.star"), &pack)
                .unwrap_or_else(|e| panic!("{function} full-kwarg call must compile: {e}"));
            let bad = template.replace("{extra}", ", bogus_zzz = \"x\"");
            let err = compile_source(&bad, &pack.join("workflow.star"), &pack).unwrap_err();
            let err = crate::errors::report(&err);
            assert!(
                err.contains("unknown argument \"bogus_zzz\""),
                "{function}: {err}"
            );
            assert!(err.contains(&format!("{function}()")), "{function}: {err}");
        }
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn counter_smoke_workflow_matches_its_materialized_manifest() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let pack = root.join("examples/counter");
        let compiled = compile_file(&pack.join("workflow.star"), &pack).unwrap();
        let manifest = crate::manifest::Manifest::load(&pack.join("crucible.toml")).unwrap();
        assert_eq!(
            serde_json::to_value(&compiled.workflow).unwrap(),
            serde_json::to_value(&manifest.workflow).unwrap()
        );
        assert_eq!(
            compiled.workflow.tasks[0].session.as_deref(),
            Some("solver")
        );
    }
}
