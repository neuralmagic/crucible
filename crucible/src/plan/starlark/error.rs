//! What the workflow compiler refuses, with the source span it refused at.

use crate::errors::FileError;
use crate::plan::diag;
use crate::plan::ir::MAX_FANOUT_CEILING;
use crate::plan::workflow::WorkflowError;
use starlark_syntax::codemap::FileSpan;

pub(crate) const MAX_SOURCE_BYTES: usize = 256 * 1024;
pub(crate) const MAX_PROMPT_BYTES: usize = 256 * 1024;
pub(crate) const MAX_TOTAL_PROMPT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_TASKS: usize = 128;
/// Tasks a source may build before `workflow(...)` picks the ones that ship. Loops and
/// comprehensions can construct far more than they include; these live on the Rust heap, which
/// [`MAX_EVAL_HEAP_BYTES`] does not bound.
pub(crate) const MAX_CONSTRUCTED_TASKS: usize = MAX_TASKS * 8;
/// A tick is one function call or one loop backedge. A hand-written workflow spends tens.
pub(crate) const MAX_EVAL_TICKS: u64 = 100_000;
pub(crate) const MAX_EVAL_HEAP_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_CALLSTACK: usize = 64;
pub(crate) const MAX_LOAD_MODULES: usize = 32;
pub(crate) const MAX_TOTAL_SOURCE_BYTES: usize = 4 * MAX_SOURCE_BYTES;
/// `def`, `if`, `for`, and comprehensions at any level; `load()` resolved by
/// [`loader`] against the pack. Re-export is off, so a symbol a library loads does not
/// leak through it, and types stay disabled.
/// How deep a source may nest before it is refused, counted before parsing.
///
/// `AstModule::parse` recurses on the shape of an expression and has no bound of its own, so a
/// few hundred levels overflow the native stack and abort the process. That happens before any
/// evaluation budget exists, and an abort bypasses the channel that turns a bad pack into a
/// recoverable round. Measured, a debug build dies between 200 and 400 levels on every nesting
/// shape the grammar has; this leaves better than a factor of two.
pub(crate) const MAX_NESTING_DEPTH: usize = 128;

/// Everything the Starlark frontend can reject. [`CompileError::At`] carries the
/// `file:line:col` prefix, so a located error is told from a bare one instead of sniffing a
/// formatted string.
///
/// Causes are real `source()` links, so the message a user reads comes from
/// [`crate::errors::report`] (or anyhow's `{:#}`), not from `Display` alone.
/// Where a compile error was found, in lines and columns rather than in prose. Both ends are
/// 1-based, matching what an editor shows and what the rendered message prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SourceSpan {
    pub begin_line: u32,
    pub begin_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

/// A file and the span within it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SourceAnchor {
    pub file: String,
    pub span: SourceSpan,
}

impl From<&FileSpan> for SourceAnchor {
    fn from(at: &FileSpan) -> Self {
        let resolved = at.resolve();
        SourceAnchor {
            file: resolved.file,
            span: SourceSpan {
                begin_line: resolved.span.begin.line as u32 + 1,
                begin_column: resolved.span.begin.column as u32 + 1,
                end_line: resolved.span.end.line as u32 + 1,
                end_column: resolved.span.end.column as u32 + 1,
            },
        }
    }
}

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
    #[error("parsing workflow Starlark: {message}")]
    Parse {
        message: String,
        /// Where the parser gave up, when it said. Kept as data so a marker can be placed without
        /// re-parsing the rendered message.
        at: Option<SourceAnchor>,
    },
    #[error(transparent)]
    Workflow(#[from] WorkflowError),
    #[error("serializing the compiled workflow")]
    Json(#[from] serde_json::Error),

    #[error("prompt_file({raw:?}) is {bytes} bytes; maximum is {MAX_PROMPT_BYTES}")]
    PromptTooLarge { raw: String, bytes: usize },
    #[error("workflow embeds more than {MAX_TOTAL_PROMPT_BYTES} bytes of prompt files")]
    PromptBudgetSpent,

    #[error(
        "an argument nests {depth} levels deep; maximum is {MAX_NESTING_DEPTH}. A loop can build \
         a value deeper than a source may be written."
    )]
    ValueTooDeep { depth: usize },
    #[error(
        "{path}:{line}: nests {depth} levels deep; maximum is {MAX_NESTING_DEPTH}. A source this \
         nested overflows the parser before any budget applies."
    )]
    SourceTooDeep {
        depth: usize,
        line: usize,
        path: String,
    },
    #[error("workflow source is {bytes} bytes; maximum is {MAX_SOURCE_BYTES}")]
    SourceTooLarge { bytes: usize },
    #[error(
        "the workflow evaluator panicked: {detail}. This is a defect in the evaluator, not in \
         the workflow, but the workflow is what reached it."
    )]
    EvalPanic { detail: String },
    #[error(
        "task {task:?} declares no output field {field:?}{}{}",
        suggestion.as_ref().map(|s| format!("; did you mean {s:?}?")).unwrap_or_default(),
        if declared.is_empty() {
            " (it declares no emits at all)".to_string()
        } else {
            format!(" (it emits: {declared})")
        }
    )]
    UndeclaredOutputField {
        task: String,
        field: String,
        suggestion: Option<String>,
        declared: String,
    },
    #[error("{call}({raw:?}) {why}")]
    PathRejected {
        call: &'static str,
        raw: String,
        why: String,
    },
    #[error("params must be the source's first statement")]
    ParamsNotFirst,
    #[error("params must be readable without evaluating the source: {detail}")]
    ParamsNotLiteral { detail: String },
    #[error("parameter {param:?} declares no type")]
    ParamNeedsType { param: String },
    #[error("parameter {param:?} field {field} must be {expected}")]
    ParamFieldWrongShape {
        param: String,
        field: &'static str,
        expected: &'static str,
    },
    #[error(
        "parameter {param:?} has unknown type {got:?}{}",
        suggestion.as_ref().map(|s| format!("; did you mean {s:?}?")).unwrap_or_default()
    )]
    UnknownParamType {
        param: String,
        got: String,
        suggestion: Option<String>,
    },
    #[error(
        "parameter {param:?} has unknown field {field:?}{}",
        suggestion.as_ref().map(|s| format!("; did you mean {s:?}?")).unwrap_or_default()
    )]
    UnknownParamField {
        param: String,
        field: String,
        suggestion: Option<String>,
    },
    #[error("parameter {param:?} is required and also has a default; it is one or the other")]
    ParamRequiredWithDefault { param: String },
    #[error(
        "parameter {param:?} is neither required nor defaulted, so a run that omits it has no \
         value to use"
    )]
    ParamNeedsDefault { param: String },
    #[error("parameter {param:?} default is not a {ty}")]
    ParamDefaultWrongType { param: String, ty: &'static str },
    #[error("parameter {param:?} declares {constraint} but is a {ty}")]
    ParamConstraintMismatch {
        param: String,
        constraint: &'static str,
        ty: &'static str,
    },
    #[error("parameter {param:?} pattern does not compile: {detail}")]
    ParamBadPattern { param: String, detail: String },
    #[error(
        "no value for required parameter {name:?}{}",
        if doc.is_empty() { String::new() } else { format!(" ({doc})") }
    )]
    MissingParam { name: String, doc: String },
    #[error(
        "no parameter {name:?} is declared{}",
        suggestion.as_ref().map(|s| format!("; did you mean {s:?}?")).unwrap_or_default()
    )]
    UnknownParam {
        name: String,
        suggestion: Option<String>,
    },
    #[error("parameter {param:?} value {got:?} is not {expected}")]
    ParamValueWrongType {
        param: String,
        got: String,
        expected: String,
    },
    #[error("parameter {param:?} value {got:?} {detail}")]
    ParamConstraintFailed {
        param: String,
        got: String,
        detail: String,
    },
    #[error("param({name:?}) names no declared parameter")]
    ParamNotDeclared { name: String },
    #[error("workflow evaluation failed: {0}")]
    Eval(String),
    #[error("workflow evaluation called fail(): {0}")]
    Failed(String),
    #[error("{function} expands to {count} tasks; maximum is {MAX_TASKS}")]
    TooManyTasks { function: String, count: usize },

    #[error("workflow loads more than {MAX_LOAD_MODULES} modules")]
    LoadBudgetSpent,
    #[error("load({raw:?}) forms a cycle")]
    LoadCycle { raw: String },
    #[error("workflow and its loaded modules exceed {MAX_TOTAL_SOURCE_BYTES} bytes of source")]
    LoadSourceBudgetSpent,
    #[error("load({raw:?}) resolved to no module")]
    LoadUnresolved { raw: String },
    #[error("unknown workflow variable {name:?}{}", diag::hint(.suggestion.as_deref()))]
    UnknownVariable {
        name: String,
        suggestion: Option<String>,
    },
    #[error("workflow integers must fit in 32 bits")]
    IntegerTooWide,

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
    #[error("report destination kind must be `slack`, got {got:?}")]
    UnknownReportDestination { got: String },
    #[error("report destination must be an object such as {{\"kind\": \"slack\"}}")]
    ReportDestinationNotObject,
    #[error("slack report destination has unknown parameter {parameter:?}")]
    UnknownSlackDestinationParameter { parameter: String },
    #[error("top_k requires a non-empty depends_on")]
    TopKWithoutDependencies,
    #[error("stage must be `iteration` or `epilogue`, got {got:?}")]
    UnknownStage { got: String },
    #[error("join must be `all`, `passed`, or `settled`, got {got:?}")]
    UnknownJoin { got: String },
    #[error(
        "grade() folds the evidence that passed, so it does not accept join = \"settled\"; use \
         join = \"passed\", or read the failed evidence from a command or evaluate task that \
         joins settled"
    )]
    SettledJoinOnGrade,
    #[error(
        "join = \"settled\" names the dependencies it reports on, so it needs at least one; \
         give {task:?} a depends_on, or drop the join"
    )]
    SettledJoinWithoutDependencies { task: String },
    #[error(
        "task {task:?} depends on {dependency:?}, which is a key the engine writes into a task's \
         inputs itself and would overwrite the dependency's entry with; rename the dependency"
    )]
    ReservedDependencyName { task: String, dependency: String },
    #[error(
        "\"over\" must name a declared output field of a task this one depends on, as \
         `over = producer.field`"
    )]
    OverNotOutputField,
    #[error(
        "argument {argument:?} carries a value supplied from outside the pack. A prompt marks \
         such a span so an agent can tell it from an instruction; nothing else can, so pass it \
         to the task as a file or an environment variable instead of building it into {argument:?}."
    )]
    ExternalOutsidePrompt { argument: String },
    #[error("task {task:?} argument \"args\" must be a dictionary")]
    SkillArgsNotDict { task: String },
    #[error("task {task:?} argument {key:?} is not something a prompt can render")]
    SkillArgNotRenderable { task: String, key: String },
    #[error("a dictionary key must be a string")]
    DictKeyNotString,
    #[error("\"emits_files\" must be a list of workspace-relative path strings")]
    EmitsFilesNotList,
    #[error(
        "emits_files entry {path:?} is not workspace-relative; a declared output cannot be an \
         absolute path or reach outside the workspace with `..`"
    )]
    EmitsFileNotRelative { path: String },
    #[error("\"max_fanout\" must be an integer")]
    FanoutNotInteger,
    #[error("max_fanout = {got} is outside 1..={MAX_FANOUT_CEILING}")]
    FanoutOutOfRange { got: i32 },
    #[error(
        "task {task:?} maps over {reference} but does not depend on {producer:?}; a fan-out \
         reads its items from a dependency's output"
    )]
    OverNotADependency {
        task: String,
        reference: String,
        producer: String,
    },
    #[error(
        "task {task:?} maps over {reference} without max_fanout; a fan-out states how wide it \
         may get before it runs, not after"
    )]
    OverWithoutFanout { task: String, reference: String },
    #[error(
        "task {task:?} maps over a list and resumes session {session:?}. Instances of a \
         shared-workspace node run one at a time, and resuming one named session from each of \
         them interleaves a single transcript."
    )]
    OverWithSession { task: String, session: String },
    #[error(
        "task name {task:?} contains `[` or `]`. Those are reserved for a mapped node's \
         instances, which are named `node[item]`."
    )]
    BracketInTaskName { task: String },
    #[error("task {task:?} declares max_fanout without \"over\"; there is nothing to bound")]
    FanoutWithoutOver { task: String },
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
