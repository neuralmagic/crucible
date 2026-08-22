//! Deterministic Starlark frontend for [`WorkflowCfg`]. Scope freezes the compiled IR, so runtime
//! never evaluates the source. Only `prompt_file` and `load` can read files, and both are confined
//! to the pack directory; process, environment, network, clock, and randomness APIs are
//! unavailable.

mod globals;
mod idents;
mod loader;
mod values;

use std::cell::{RefCell, RefMut};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use starlark::environment::{Globals, GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::values::ProvidesStaticType;
use starlark_syntax::codemap::{CodeMap, FileSpan};
use starlark_syntax::syntax::{AstModule, Dialect};

use crate::errors::FileError;
use crate::manifest::{WorkflowCfg, WorkflowError, WorkflowType};
use crate::plan::diag;
use crate::plan::ir::{
    Direction, EngineOp, Isolation, Join, OutputField, OutputRef, Stage, Task, TaskKind, TaskName,
};
use crate::plan::starlark::values::WorkflowValue;

type Result<T> = std::result::Result<T, CompileError>;

const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_PROMPT_BYTES: usize = 256 * 1024;
const MAX_TOTAL_PROMPT_BYTES: usize = 1024 * 1024;
const MAX_TASKS: usize = 128;
/// Tasks a source may build before `workflow(...)` picks the ones that ship. Loops and
/// comprehensions can construct far more than they include; these live on the Rust heap, which
/// [`MAX_EVAL_HEAP_BYTES`] does not bound.
const MAX_CONSTRUCTED_TASKS: usize = MAX_TASKS * 8;
/// A tick is one function call or one loop backedge. A hand-written workflow spends tens.
const MAX_EVAL_TICKS: u64 = 100_000;
const MAX_EVAL_HEAP_BYTES: usize = 64 * 1024 * 1024;
const MAX_CALLSTACK: usize = 64;
const MAX_LOAD_MODULES: usize = 32;
const MAX_TOTAL_SOURCE_BYTES: usize = 4 * MAX_SOURCE_BYTES;

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
    /// The first [`CompileError`] a constructor raised, parked while the marker error unwinds
    /// the evaluator.
    thrown: Option<CompileError>,
}

/// What a pack-relative path is being resolved for. Only the `FileError` context strings differ.
#[derive(Clone, Copy)]
enum PathKind {
    Prompt,
    Module,
}

impl PathKind {
    fn metadata(self) -> &'static str {
        match self {
            PathKind::Prompt => "reading prompt metadata",
            PathKind::Module => "reading module metadata",
        }
    }

    /// The constructor the author wrote, quoted back in the diagnostic.
    fn call(self) -> &'static str {
        match self {
            PathKind::Prompt => "prompt_file",
            PathKind::Module => "load",
        }
    }

    fn resolving(self) -> &'static str {
        match self {
            PathKind::Prompt => "resolving prompt file",
            PathKind::Module => "resolving module file",
        }
    }
}

/// Why a path under the pack was refused, before [`PathRejection::prompt`] or
/// [`PathRejection::module`] phrases it for the caller that asked.
enum PathRejection {
    Empty,
    Traversal,
    Symlink,
    HardLink { links: u64 },
    NotRegularFile,
    EscapesPack,
    File(FileError),
}

impl std::fmt::Display for PathRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathRejection::Empty => f.write_str("path must be a non-empty pack-relative path"),
            PathRejection::Traversal => f.write_str("may not contain `..` or escape the pack"),
            PathRejection::Symlink => f.write_str("may not traverse symlinks"),
            PathRejection::HardLink { links } => write!(
                f,
                "has {links} names; a pack's files have one. A second name is how a file outside \
                 the pack is read from inside it, and resolving the path cannot see it"
            ),
            PathRejection::NotRegularFile => f.write_str("must name a regular, non-symlink file"),
            PathRejection::EscapesPack => f.write_str("escapes the pack directory"),
            PathRejection::File(error) => write!(f, "{error}"),
        }
    }
}

impl PathRejection {
    /// Phrase the refusal for whichever constructor asked.
    ///
    /// `prompt_file` and `load` used to carry parallel sets of error variants differing only in
    /// that word, which meant every new refusal had to be added twice, and the hard-link one
    /// nearly was not. The reason is the enum's own Display; the caller only supplies its name.
    fn at(self, kind: PathKind, raw: &str) -> CompileError {
        match self {
            PathRejection::File(error) => CompileError::File(error),
            why => CompileError::PathRejected {
                call: kind.call(),
                raw: raw.to_owned(),
                why: why.to_string(),
            },
        }
    }
}

impl CompileContext {
    /// The pack-relative and canonical forms of `raw`, or why the pack refuses it. Every
    /// component is stat'd with `symlink_metadata`, so a symlinked intermediate directory is
    /// refused as well as a symlinked leaf.
    fn resolve_in_pack(
        &self,
        raw: &str,
        kind: PathKind,
    ) -> std::result::Result<(PathBuf, PathBuf), PathRejection> {
        let relative = safe_relative_path(raw)?;
        let root = std::fs::canonicalize(&self.pack_dir)
            .map_err(FileError::at("resolving pack directory", &self.pack_dir))
            .map_err(PathRejection::File)?;
        let mut path = self.pack_dir.clone();
        let mut metadata = None;
        for component in relative.components() {
            let Component::Normal(component) = component else {
                continue;
            };
            path.push(component);
            let current = std::fs::symlink_metadata(&path)
                .map_err(FileError::at(kind.metadata(), &path))
                .map_err(PathRejection::File)?;
            if current.file_type().is_symlink() {
                return Err(PathRejection::Symlink);
            }
            metadata = Some(current);
        }
        let Some(metadata) = metadata.filter(|metadata| metadata.is_file()) else {
            return Err(PathRejection::NotRegularFile);
        };
        // Canonicalizing resolves symlinks and cannot see a hard link at all: a second name for
        // a file outside the pack sits fully inside it and passes every check above. A pack is a
        // fresh checkout or an unpacked tarball, where no file has a second name, so an extra
        // link is the signature of one having been made.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() > 1 {
                return Err(PathRejection::HardLink {
                    links: metadata.nlink(),
                });
            }
        }
        let canonical = std::fs::canonicalize(&path)
            .map_err(FileError::at(kind.resolving(), &path))
            .map_err(PathRejection::File)?;
        if !canonical.starts_with(&root) {
            return Err(PathRejection::EscapesPack);
        }
        Ok((relative, canonical))
    }

    fn prompt_file(&mut self, raw: &str) -> Result<String> {
        let (relative, canonical) = self
            .resolve_in_pack(raw, PathKind::Prompt)
            .map_err(|rejection| rejection.at(PathKind::Prompt, raw))?;
        // Size comes from the directory entry, before the bytes are read. Reading a file whole
        // and refusing it afterwards is the read the limit exists to prevent.
        let declared = std::fs::metadata(&canonical)
            .map_err(FileError::at("reading prompt file", &canonical))?
            .len();
        if declared > MAX_PROMPT_BYTES as u64 {
            return Err(CompileError::PromptTooLarge {
                raw: raw.to_owned(),
                bytes: declared as usize,
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
}

/// The marker a native constructor returns once its real [`CompileError`] is parked in
/// [`CompileContext::thrown`]; `starlark::Error` cannot carry our type.
#[derive(Debug, thiserror::Error)]
#[error("workflow compile error")]
struct Thrown;

/// A DSL constructor ran without [`CompileState`] on `Evaluator::extra`.
#[derive(Debug, thiserror::Error)]
#[error("workflow compile state is unreachable from the evaluator")]
struct StateMissing;

/// Compile state the native constructors share. `Evaluator::extra` hands out a shared reference,
/// so every mutation goes through the `RefCell`.
#[derive(Debug, ProvidesStaticType)]
pub(crate) struct CompileState {
    /// Stands in when the evaluator cannot name a call site.
    site: FileSpan,
    inner: RefCell<CompileContext>,
}

impl CompileState {
    fn new(pack_dir: &Path, filename: &Path) -> Self {
        let file = CodeMap::new(filename.display().to_string(), String::new());
        let span = file.full_span();
        CompileState {
            site: FileSpan { file, span },
            inner: RefCell::new(CompileContext {
                pack_dir: pack_dir.to_path_buf(),
                prompt_files: BTreeSet::new(),
                total_prompt_bytes: 0,
                constructed_tasks: BTreeMap::new(),
                sessions: BTreeMap::new(),
                bound_sessions: BTreeSet::new(),
                string_session_refs: BTreeMap::new(),
                thrown: None,
            }),
        }
    }

    fn site(&self) -> FileSpan {
        self.site.clone()
    }

    fn context_mut(&self) -> RefMut<'_, CompileContext> {
        self.inner.borrow_mut()
    }

    /// Park `error` and hand the evaluator a marker to unwind with. The first error wins: later
    /// frames cannot overwrite the innermost authoring site.
    fn throw(&self, error: CompileError) -> starlark::Error {
        let mut context = self.inner.borrow_mut();
        if context.thrown.is_none() {
            context.thrown = Some(error);
        }
        starlark::Error::new_native(anyhow::Error::new(Thrown))
    }

    fn take_thrown(&self) -> Option<CompileError> {
        self.inner.borrow_mut().thrown.take()
    }

    fn into_context(self) -> CompileContext {
        self.inner.into_inner()
    }
}

fn safe_relative_path(raw: &str) -> std::result::Result<PathBuf, PathRejection> {
    let path = Path::new(raw);
    if raw.trim().is_empty() || path.is_absolute() {
        return Err(PathRejection::Empty);
    }
    if path.components().any(|part| {
        matches!(
            part,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(PathRejection::Traversal);
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
    /// `producer.field`, already checked against the producer's declared emits.
    Output(OutputRef),
    Session(SessionDecl),
    Workflow(WorkflowCfg),
    /// A starlark value outside the DSL's own space: a dict, a function, a struct. The `take_*`
    /// helpers report it with the same wrong-type sentence a wrong scalar gets.
    Opaque,
}

/// A `session(...)` declaration: a durable conversation name plus optional agent
/// defaults that materialize onto the agent tasks bound to it.
#[derive(Clone, Debug)]
pub(crate) struct SessionDecl {
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
/// `file:line:col` prefix, so a located error is told from a bare one instead of sniffing a
/// formatted string.
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
    #[error("top_k requires a non-empty depends_on")]
    TopKWithoutDependencies,
    #[error("stage must be `iteration` or `epilogue`, got {got:?}")]
    UnknownStage { got: String },
    #[error("join must be `all` or `passed`, got {got:?}")]
    UnknownJoin { got: String },
    #[error(
        "\"over\" must name a declared output field of a task this one depends on, as \
         `over = producer.field`"
    )]
    OverNotOutputField,
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

/// The constructors every lane has.
const COMMON_FUNCTIONS: &[&str] = &[
    "agent",
    "command",
    "evaluate",
    "prompt_file",
    "session",
    "workflow",
];

/// The scored loop's own constructors, absent from a playbook.
const SCORED_FUNCTIONS: &[&str] = &[
    "apply",
    "decide",
    "default_autoresearch",
    "grade",
    "measure",
    "propose",
    "top_k",
];

/// The callable surface of one lane, for unknown-function suggestions. A playbook author is
/// never offered a constructor the lane would then refuse. Built from the two tables above so a
/// constructor cannot be added to one and forgotten in the other.
fn dsl_functions(lane: WorkflowType) -> Vec<&'static str> {
    let mut names = COMMON_FUNCTIONS.to_vec();
    if lane != WorkflowType::Playbook {
        names.extend_from_slice(SCORED_FUNCTIONS);
    }
    names.sort_unstable();
    names
}

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
            "over",
            "max_fanout",
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
            "over",
            "max_fanout",
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
            "over",
            "max_fanout",
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

/// Build the value for a named-argument DSL call. Every kwarg an arm consumes must
/// appear in [`known_kwargs`], which the leftover check reports against.
fn constructor(
    function: &str,
    mut named: BTreeMap<String, Value>,
    state: &CompileState,
    at: &FileSpan,
) -> Result<Value> {
    if function == "workflow" {
        let workflow_type = match take_string_default(&mut named, "type", "autoresearch")?.as_str()
        {
            "autoresearch" => WorkflowType::Autoresearch,
            "custom" => WorkflowType::Custom,
            "playbook" => WorkflowType::Playbook,
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
        no_unknown_kwargs(function, &named)?;
        let workflow = WorkflowCfg {
            workflow_type,
            result,
            tasks,
            file: None,
            resolved_from: None,
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
        no_unknown_kwargs(function, &named)?;
        let mut context = state.context_mut();
        if let Some((_, first)) = context.sessions.get(&decl.name) {
            return Err(CompileError::DuplicateSession {
                name: decl.name.clone(),
                first: first.clone(),
            });
        }
        context
            .sessions
            .insert(decl.name.clone(), (decl.clone(), at.clone()));
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
            let session = take_session(&mut named, state, at)?;
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
                            return Err(CompileError::SessionConfigConflict {
                                task: name.0.clone(),
                                knob,
                                mine: mine.clone(),
                                session: decl.name.clone(),
                                theirs: theirs.clone(),
                            });
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
                over: None,
                max_fanout: None,
            }
        }
        "propose" => {
            let session = take_session(&mut named, state, at)?;
            if let Some(decl) = &session
                && decl.has_defaults()
            {
                // The engine propose turn's agent config is owned by the manifest's
                // [agent]; accepting the defaults here would silently ignore them.
                return Err(CompileError::ProposeSessionDefaults {
                    name: decl.name.clone(),
                });
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
    no_unknown_kwargs(function, &named)?;
    let constructed = {
        let mut context = state.context_mut();
        context
            .constructed_tasks
            .insert(task.name.0.clone(), at.clone());
        context.constructed_tasks.len()
    };
    if constructed > MAX_CONSTRUCTED_TASKS {
        return Err(CompileError::TooManyTasks {
            function: function.to_owned(),
            count: constructed,
        });
    }
    Ok(Value::Task(task))
}

fn no_unknown_kwargs(function: &str, named: &BTreeMap<String, Value>) -> Result<()> {
    let Some(unknown) = named.keys().next() else {
        return Ok(());
    };
    Err(CompileError::UnknownArgument {
        function: function.to_owned(),
        argument: unknown.clone(),
        suggestion: diag::suggest(unknown, known_kwargs(function).iter().copied())
            .map(str::to_owned),
    })
}

/// A `session(...)` value, or a bare string. Strings keep the historical
/// pass-through only while the file declares no sessions; once any `session()`
/// exists every string must name a declared one.
fn take_session(
    named: &mut BTreeMap<String, Value>,
    state: &CompileState,
    at: &FileSpan,
) -> Result<Option<SessionDecl>> {
    match named.remove("session").unwrap_or(Value::None) {
        Value::None => Ok(None),
        Value::Session(decl) => {
            state.context_mut().bound_sessions.insert(decl.name.clone());
            Ok(Some(decl))
        }
        Value::String(name) => {
            let mut context = state.context_mut();
            if let Some((decl, _)) = context.sessions.get(&name) {
                let decl = decl.clone();
                context.bound_sessions.insert(name);
                Ok(Some(decl))
            } else if context.sessions.is_empty() {
                context
                    .string_session_refs
                    .entry(name.clone())
                    .or_insert_with(|| at.clone());
                Ok(Some(SessionDecl {
                    name,
                    harness: None,
                    model: None,
                    effort: None,
                }))
            } else {
                let suggestion = diag::suggest(&name, context.sessions.keys().map(String::as_str))
                    .map(str::to_owned);
                Err(CompileError::UndeclaredSession { name, suggestion })
            }
        }
        _ => Err(CompileError::SessionWrongType),
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
        file: None,
        resolved_from: None,
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
    let task = Task {
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
        over: take_over(named)?,
        max_fanout: take_optional_fanout(named)?,
    };
    check_fanout(&task)?;
    Ok(task)
}

/// A fan-out reads its items from a dependency and states its width before it runs. Both are
/// checked here, where the task is whole, rather than at plan validation, so the diagnostic
/// carries the source location the constructor was written at.
fn check_fanout(task: &Task) -> Result<()> {
    match (&task.over, task.max_fanout) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(CompileError::FanoutWithoutOver {
            task: task.name.0.clone(),
        }),
        (Some(reference), None) => Err(CompileError::OverWithoutFanout {
            task: task.name.0.clone(),
            reference: reference.to_string(),
        }),
        (Some(reference), Some(_)) => {
            if task.depends_on.contains(&reference.task) {
                Ok(())
            } else {
                Err(CompileError::OverNotADependency {
                    task: task.name.0.clone(),
                    reference: reference.to_string(),
                    producer: reference.task.0.clone(),
                })
            }
        }
    }
}

/// The maximum a pack may declare for `max_fanout`. Operator-owned, not author-owned: a bound a
/// pack could raise is not a bound.
pub(crate) const MAX_FANOUT_CEILING: u32 = 256;

fn take_over(named: &mut BTreeMap<String, Value>) -> Result<Option<OutputRef>> {
    match named.remove("over") {
        None | Some(Value::None) => Ok(None),
        Some(Value::Output(reference)) => Ok(Some(reference)),
        Some(_) => Err(CompileError::OverNotOutputField),
    }
}

fn take_optional_fanout(named: &mut BTreeMap<String, Value>) -> Result<Option<u32>> {
    match named.remove("max_fanout") {
        None | Some(Value::None) => Ok(None),
        Some(Value::Int(n)) if n >= 1 && n as u32 <= MAX_FANOUT_CEILING => Ok(Some(n as u32)),
        Some(Value::Int(n)) => Err(CompileError::FanoutOutOfRange { got: n }),
        Some(_) => Err(CompileError::FanoutNotInteger),
    }
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
        over: None,
        max_fanout: None,
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

/// Stack for the compile thread.
///
/// Two recursions are not bounded by anything the compiler can check first. Dropping a value the
/// evaluator built recurses once per level, and a loop can nest a value as deep as it has ticks
/// to spend: `for i in range(n): x = [x]` costs a handful of ticks per level, so
/// [`MAX_EVAL_TICKS`] is what bounds the depth, and the stack has to cover that bound. Measured
/// at roughly 1.3KB per level, 100k levels needs ~130MB. This is virtual, committed only if
/// touched, so an ordinary compile pays nothing for it.
const COMPILE_STACK_BYTES: usize = 256 * 1024 * 1024;

/// Compile a source on a stack large enough for the depths the evaluator's own budgets permit.
///
/// The thread is the answer to recursion the compiler cannot refuse in advance. Where it can
/// refuse in advance it does: [`reject_deep_nesting`] bounds the parser, because a 256KB source
/// can nest further than any stack would cover.
pub fn compile_source(source: &str, filename: &Path, pack_dir: &Path) -> Result<CompiledWorkflow> {
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .stack_size(COMPILE_STACK_BYTES)
            .name("crucible-compile".to_string())
            .spawn_scoped(scope, || compile_source_here(source, filename, pack_dir))
            .map_err(|error| CompileError::Eval(format!("spawning the compile thread: {error}")))?;
        handle.join().map_err(|_| CompileError::EvalPanic {
            detail: "the compile thread died".to_owned(),
        })?
    })
}

fn compile_source_here(source: &str, filename: &Path, pack_dir: &Path) -> Result<CompiledWorkflow> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(CompileError::SourceTooLarge {
            bytes: source.len(),
        });
    }
    reject_deep_nesting(source, filename)?;
    let ast = AstModule::parse(
        &filename.display().to_string(),
        source.to_owned(),
        &dialect(),
    )
    .map_err(|error| CompileError::Parse(error.to_string()))?;
    let lane = idents::declared_lane(&ast);
    let idents = idents::scan(&ast, lane);
    let globals = lane_globals(lane);
    let state = CompileState::new(pack_dir, filename);
    let loader = loader::resolve(&ast, &state, &globals, source.len(), lane)?;
    let workflow = catching_panics(|| {
        Module::with_temp_heap(|module| -> Result<WorkflowCfg> {
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&state);
            eval.set_loader(&loader);
            budgets(&mut eval)?;
            match eval.eval_module(ast, &globals) {
                Ok(value) => match WorkflowValue::from_value(value) {
                    Some(workflow) => Ok(workflow.0.clone()),
                    None => Err(CompileError::NoWorkflowResult),
                },
                Err(error) => Err(state
                    .take_thrown()
                    .map(|thrown| idents::narrow(thrown, &idents))
                    .unwrap_or_else(|| idents::map_error(&error, &idents))),
            }
        })
    })??;
    let context = state.into_context();
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

/// The evaluation ceilings every module runs under. A budget setter only fails when it is set
/// twice, which a fresh [`Evaluator`] cannot be.
fn budgets(eval: &mut Evaluator<'_, '_, '_>) -> Result<()> {
    let budget = |error: anyhow::Error| CompileError::Eval(error.to_string());
    eval.set_max_tick_count(MAX_EVAL_TICKS).map_err(budget)?;
    eval.set_max_heap_size(MAX_EVAL_HEAP_BYTES)
        .map_err(budget)?;
    eval.set_max_callstack_size(MAX_CALLSTACK).map_err(budget)?;
    Ok(())
}

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

/// Refuse a source whose shape could overflow the parser, without parsing it.
///
/// The measure is deliberately an over-approximation of the AST depth the parser will build:
/// bracket nesting at the point of measurement, plus every operator in the statement, since an
/// operator chain nests one level per operator whether or not it carries brackets
/// (`not not not x`, `1 + 1 + 1`, `1 if c else 1 if c else 1` all recurse and none of them
/// bracket). Over-counting only refuses sources no author writes: a real statement is a handful
/// of operators inside two or three brackets.
pub(crate) fn reject_deep_nesting(source: &str, filename: &Path) -> Result<()> {
    let mut depth: usize = 0;
    let mut operators: usize = 0;
    let mut line: usize = 1;
    let mut worst: usize = 0;
    let mut worst_line: usize = 1;

    let bytes: Vec<char> = source.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            '\n' => {
                line += 1;
                // A statement ends at a newline outside brackets and outside a continuation,
                // and its operator count starts again. Nesting across statements is bounded by
                // the dialect's own indentation limit, which already refuses cleanly.
                if depth == 0 && !(i > 0 && bytes[i - 1] == '\\') {
                    operators = 0;
                }
                i += 1;
            }
            '#' => {
                while i < bytes.len() && bytes[i] != '\n' {
                    i += 1;
                }
            }
            '"' | '\'' => {
                let quote = c;
                let triple = bytes.get(i + 1) == Some(&quote) && bytes.get(i + 2) == Some(&quote);
                i += if triple { 3 } else { 1 };
                while let Some(&ch) = bytes.get(i) {
                    if ch == '\\' {
                        if bytes.get(i + 1) == Some(&'\n') {
                            line += 1;
                        }
                        i += 2;
                        continue;
                    }
                    if ch == '\n' {
                        line += 1;
                        // An unterminated single-quoted string is the parser's error to
                        // report, not ours; stop consuming so the line count stays honest.
                        if !triple {
                            break;
                        }
                    }
                    if ch == quote {
                        if !triple {
                            i += 1;
                            break;
                        }
                        if bytes.get(i + 1) == Some(&quote) && bytes.get(i + 2) == Some(&quote) {
                            i += 3;
                            break;
                        }
                    }
                    i += 1;
                }
            }
            '(' | '[' | '{' => {
                depth += 1;
                i += 1;
            }
            ')' | ']' | '}' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            '+' | '-' | '*' | '/' | '%' | '<' | '>' | '=' | '!' | '|' | '&' | '^' | '~' | '.' => {
                // `=` counts only as part of a comparison: a kwarg or an assignment builds no
                // expression node, and counting it would punish a wide constructor call.
                let comparison = c != '=' || bytes.get(i + 1) == Some(&'=');
                let assignment_target =
                    c == '=' && i > 0 && !matches!(bytes[i - 1], '=' | '<' | '>' | '!');
                if comparison || !assignment_target {
                    operators += 1;
                }
                i += 1;
            }
            _ if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_alphanumeric() || bytes[i] == '_') {
                    i += 1;
                }
                let word: String = bytes[start..i].iter().collect();
                if matches!(word.as_str(), "not" | "and" | "or" | "if" | "else" | "in") {
                    operators += 1;
                }
            }
            _ => i += 1,
        }
        let here = depth + operators;
        if here > worst {
            worst = here;
            worst_line = line;
        }
        if here > MAX_NESTING_DEPTH {
            return Err(CompileError::SourceTooDeep {
                depth: here,
                line: worst_line,
                path: filename.display().to_string(),
            });
        }
    }
    Ok(())
}

/// Run the evaluator with a panic caught and turned into a compile error.
///
/// The evaluator is a boundary around untrusted input, so a panic behind it has to arrive as a
/// diagnostic rather than as a dead process: `scope` turns a compile error into a recoverable
/// round and has nothing to do with a corpse. One panic is reachable from four tokens of
/// starlark (`"abcd" * 2000000000` asserts on a length that does not fit a `u32`, before it
/// allocates anything), and the existence of one says nothing useful about the number of others.
///
/// A caught panic abandons the whole compile, so no partially-mutated state is read afterwards
/// and the unwind-safety assertion holds. A stack overflow is not caught here because it does
/// not unwind, which is why depth is bounded before parsing instead.
fn catching_panics<T>(evaluate: impl FnOnce() -> T) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(evaluate)).map_err(|payload| {
        let detail = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a panic with no message".to_owned());
        CompileError::EvalPanic { detail }
    })
}

/// The globals one lane sees. The scored constructors are not merely refused for a playbook,
/// they are absent, so `measure` in a playbook is an unknown name rather than a name that
/// compiles and fails validation later.
fn lane_globals(lane: WorkflowType) -> Globals {
    let builder = GlobalsBuilder::standard().with(globals::common);
    match lane {
        WorkflowType::Playbook => builder.build(),
        _ => builder.with(globals::scored).build(),
    }
}

fn dialect() -> Dialect {
    Dialect {
        enable_top_level_stmt: true,
        enable_load_reexport: false,
        ..Dialect::Standard
    }
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
        depends_on = reviews,
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
    fn an_advisory_sink_cannot_gate_the_synthesized_apply() {
        let pack = temp_pack("advisory-sink");
        let rejected = r#"
review = command(name = "review", run = "./review.sh", required = False)
default_autoresearch([review])
"#;
        let error = crate::errors::report(
            &compile_source(rejected, &pack.join("rejected.star"), &pack).unwrap_err(),
        );
        assert!(error.contains("\"apply\""), "{error}");
        assert!(error.contains("\"review\""), "{error}");

        let gated = r#"
review = command(name = "review", run = "./review.sh", required = False)
gate = command(name = "gate", run = "./gate.sh", depends_on = [review], join = "passed")
default_autoresearch([review, gate])
"#;
        let compiled = compile_source(gated, &pack.join("gated.star"), &pack).unwrap();
        let apply = compiled
            .workflow
            .tasks
            .iter()
            .find(|task| task.name.0 == "apply")
            .unwrap();
        assert_eq!(apply.depends_on, ["gate".into()]);
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
    fn rejects_runaway_evaluation() {
        let pack = temp_pack("bounds");
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

    /// The playbook pack carries its graph twice: `workflow.star` for the lane it is waiting
    /// on, `plan.toml` for the runner that executes it today. Drift between them would make
    /// the pack a reference to nothing.
    #[test]
    fn playbook_example_matches_its_golden_and_runnable_plan() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let pack = root.join("examples/playbook");
        let compiled = compile_file(&pack.join("workflow.star"), &pack).unwrap();
        assert_eq!(
            compiled.canonical_json,
            std::fs::read_to_string(pack.join("expected-workflow.json")).unwrap()
        );
        let plan = crate::plan::ir::Plan::from_toml_str(
            &std::fs::read_to_string(pack.join("plan.toml")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&compiled.workflow.tasks).unwrap(),
            serde_json::to_value(&plan.tasks).unwrap()
        );
        plan.validate().unwrap();
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

    /// A misspelled kwarg must underline the kwarg, not the call it sits in.
    ///
    /// Every other span assertion in this file uses a single-line source, where an
    /// argument span and a whole-call span are indistinguishable, which is how this
    /// regressed unnoticed once already. The source here is deliberately multi-line and
    /// the assertion is on the column as well as the line.
    #[test]
    fn an_unknown_kwarg_points_at_the_argument_not_the_whole_call() {
        let pack = temp_pack("arg-span");
        let source = "\ndraft = agent(\n    name = \"draft\",\n    prompt = \"write it\",\n    promt = \"typo\",\n)\nworkflow(type = \"playbook\", tasks = [draft])\n";
        let error = crate::errors::report(
            &compile_source(source, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        // `promt` is on line 5, indented four spaces.
        assert!(
            error.contains("workflow.star:5:5"),
            "expected the argument's own span, got: {error}"
        );
        assert!(
            !error.contains("workflow.star:2:"),
            "the whole call was underlined instead of the argument: {error}"
        );
        assert!(error.contains("prompt"), "no suggestion: {error}");

        // Two calls to one constructor with the same bad kwarg: the call site has to pick.
        let twice = "\na = agent(\n    name = \"a\",\n    prompt = \"p\",\n)\nb = agent(\n    name = \"b\",\n    prompt = \"p\",\n    promt = \"typo\",\n)\nworkflow(type = \"playbook\", tasks = [a, b])\n";
        let error = crate::errors::report(
            &compile_source(twice, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        assert!(
            error.contains("workflow.star:9:5"),
            "the second call's argument, not the first call's: {error}"
        );
        let _ = std::fs::remove_dir_all(&pack);
    }

    /// A session error is about the `session =` argument, so it underlines that argument.
    #[test]
    fn an_undeclared_session_points_at_the_session_argument() {
        let pack = temp_pack("session-span");
        let source = "\nscribe = session(name = \"scribe\")\n\ndraft = agent(\n    name = \"draft\",\n    prompt = \"write it\",\n    session = \"scrib\",\n)\nworkflow(type = \"playbook\", tasks = [draft])\n";
        let error = crate::errors::report(
            &compile_source(source, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        assert!(
            error.contains("workflow.star:7:5"),
            "expected the session argument's own span, got: {error}"
        );
        assert!(
            !error.contains("workflow.star:4:"),
            "the whole call was underlined instead of the argument: {error}"
        );
        assert!(error.contains("scribe"), "no suggestion: {error}");
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
                "s = session(name = \"sess\")\nu = command(name = \"u\", run = \"true\", emits = [\"items\"])\na = agent(name = \"a\", prompt = \"p\", harness = \"claude\", model = \"m\", effort = \"high\", session = s, emits = [\"score\"], depends_on = [u], needs = \"any\", required = True, isolated = False, join = \"all\", stage = \"iteration\", over = u.items, max_fanout = 4{extra})\nworkflow(type = \"custom\", tasks = [u, a], result = a)\n",
            ),
            (
                "command",
                "u = command(name = \"u\", run = \"true\", emits = [\"items\"])\nc = command(name = \"c\", run = \"true\", emits = [\"score\"], depends_on = [u], needs = \"any\", required = True, isolated = False, join = \"all\", stage = \"iteration\", over = u.items, max_fanout = 4{extra})\nworkflow(type = \"custom\", tasks = [u, c], result = c)\n",
            ),
            (
                "evaluate",
                "u = command(name = \"u\", run = \"true\", emits = [\"items\"])\ne = evaluate(name = \"e\", run = \"true\", threshold = 1, direction = \"higher\", emits = [\"score\"], depends_on = [u], needs = \"any\", required = True, isolated = False, join = \"all\", stage = \"iteration\", over = u.items, max_fanout = 4{extra})\nworkflow(type = \"custom\", tasks = [u, e], result = e)\n",
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

    fn task_names(compiled: &CompiledWorkflow) -> Vec<&str> {
        compiled
            .workflow
            .tasks
            .iter()
            .map(|task| task.name.0.as_str())
            .collect()
    }

    #[test]
    fn a_def_macro_expands_into_the_workflow_task_list() {
        let pack = temp_pack("def-macro");
        let source = r#"
def critic(topic):
    review = agent(
        name = "review-" + topic,
        prompt = "Review the " + topic + ".",
        isolated = True,
        required = False,
    )
    gate = command(
        name = "gate-" + topic,
        run = "./gate.sh " + topic,
        depends_on = [review],
        join = "passed",
    )
    return [review, gate]

tasks = critic("correctness") + critic("copy")
workflow(tasks)
"#;
        let compiled = compile_source(source, &pack.join("workflow.star"), &pack).unwrap();
        assert_eq!(
            task_names(&compiled),
            [
                "review-correctness",
                "gate-correctness",
                "review-copy",
                "gate-copy"
            ]
        );
        assert_eq!(
            compiled.workflow.tasks[1].depends_on,
            ["review-correctness".into()]
        );
        let _ = std::fs::remove_dir_all(&pack);
    }

    /// The same fan-out three ways: a `for` loop, a comprehension, and the hand-written list all
    /// compile to identical IR, so the widened grammar is sugar and nothing more.
    #[test]
    fn a_for_loop_and_a_comprehension_build_the_same_parallel_branches() {
        let pack = temp_pack("fanout");
        let treatments = ["surreal", "minimal", "documentary"];
        let looped = r#"
branches = []
for treatment in ["surreal", "minimal", "documentary"]:
    branches.append(agent(
        name = "draft-" + treatment,
        prompt = "Draft in the " + treatment + " register.",
        isolated = True,
    ))
curate = command(
    name = "curate",
    run = "./curate.sh",
    depends_on = branches,
    join = "passed",
)
workflow(branches + [curate])
"#;
        let comprehension = r#"
branches = [
    agent(
        name = "draft-" + treatment,
        prompt = "Draft in the " + treatment + " register.",
        isolated = True,
    )
    for treatment in ["surreal", "minimal", "documentary"]
]
curate = command(
    name = "curate",
    run = "./curate.sh",
    depends_on = branches,
    join = "passed",
)
workflow(branches + [curate])
"#;
        let literal = r#"
branches = [
    agent(name = "draft-surreal", prompt = "Draft in the surreal register.", isolated = True),
    agent(name = "draft-minimal", prompt = "Draft in the minimal register.", isolated = True),
    agent(
        name = "draft-documentary",
        prompt = "Draft in the documentary register.",
        isolated = True,
    ),
]
curate = command(
    name = "curate",
    run = "./curate.sh",
    depends_on = branches,
    join = "passed",
)
workflow(branches + [curate])
"#;
        let compile = |source: &str| {
            compile_source(source, &pack.join("workflow.star"), &pack)
                .unwrap_or_else(|error| panic!("{}", crate::errors::report(&error)))
        };
        let looped = compile(looped);
        let comprehension = compile(comprehension);
        let literal = compile(literal);
        assert_eq!(
            task_names(&looped),
            [
                "draft-surreal",
                "draft-minimal",
                "draft-documentary",
                "curate"
            ]
        );
        assert_eq!(looped.canonical_json, comprehension.canonical_json);
        assert_eq!(looped.canonical_json, literal.canonical_json);
        let curate = looped.workflow.tasks.last().unwrap();
        let expected: Vec<TaskName> = treatments
            .iter()
            .map(|treatment| TaskName(format!("draft-{treatment}")))
            .collect();
        assert_eq!(curate.depends_on, expected);
        let _ = std::fs::remove_dir_all(&pack);
    }

    /// The lane owns the namespace. A playbook cannot name a scored constructor, and the
    /// did-you-mean never offers one, so the author is not sent toward a name the lane refuses.
    /// Every source here aborted the engine process before it was bounded, and each is a
    /// different way in: parse-time nesting through five grammar shapes, a value nested by a
    /// loop rather than written, and a load graph that walked past its own budget.
    ///
    /// They run in-process on purpose. An abort takes the test runner with it, so a regression
    /// here is a failed assertion rather than a suite that stops reporting.
    #[test]
    fn a_hostile_pack_is_refused_rather_than_aborting_the_process() {
        let pack = temp_pack("hostile");
        let deep = |open: &str, close: &str, n: usize| {
            format!("x = {}{}\nworkflow([])\n", open.repeat(n), close.repeat(n))
        };
        let cases: Vec<(&str, String)> = vec![
            ("list nesting", deep("[", "]", 3000)),
            ("paren nesting", deep("(", ")", 3000)),
            ("dict nesting", deep("{'k': ", "}", 3000)),
            (
                "call nesting",
                format!(
                    "x = {}\"\"{}\nworkflow([])\n",
                    "len(".repeat(3000),
                    ")".repeat(3000)
                ),
            ),
            (
                "unary chain",
                format!("x = {}True\nworkflow([])\n", "not ".repeat(3000)),
            ),
            (
                "binary chain",
                format!(
                    "x = {}\nworkflow([])\n",
                    std::iter::repeat_n("1", 3000).collect::<Vec<_>>().join("+")
                ),
            ),
            (
                "ternary chain",
                format!("x = {}1\nworkflow([])\n", "1 if True else ".repeat(3000)),
            ),
            (
                // Four shallow lines that build a value no literal is allowed to express. The
                // parse guard cannot see this one: the source is trivial.
                "value nested by a loop",
                "x = [1]\nfor i in range(3000):\n    x = [x]\ncommand(name = \"a\", run = \"true\", emits = x)\nworkflow([])\n"
                    .to_string(),
            ),
            (
                "nesting inside a constructor argument",
                format!(
                    "command(name = \"a\", run = \"true\", emits = {}{})\nworkflow([])\n",
                    "[".repeat(3000),
                    "]".repeat(3000)
                ),
            ),
        ];
        for (what, source) in cases {
            let error = compile_source(&source, &pack.join("workflow.star"), &pack)
                .err()
                .unwrap_or_else(|| panic!("{what}: compiled instead of being refused"));
            let report = crate::errors::report(&error);
            assert!(
                report.contains("levels deep") && report.contains("maximum is"),
                "{what}: refused for the wrong reason: {report}"
            );
            assert!(
                report.contains("128"),
                "{what}: the bound is not named: {report}"
            );
        }

        // A chain of loads walks past the module budget one level at a time; the budget has to
        // be spent on the way down, not counted on the way back up.
        std::fs::create_dir_all(pack.join("chain")).unwrap();
        for i in 0..200 {
            let body = if i == 199 {
                "v = 1\n".to_string()
            } else {
                format!("load(\"chain/m{}.star\", \"v\")\n", i + 1)
            };
            std::fs::write(pack.join(format!("chain/m{i}.star")), body).unwrap();
        }
        let chained = "load(\"chain/m0.star\", \"v\")\nworkflow([])\n";
        let error = crate::errors::report(
            &compile_source(chained, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        assert!(
            error.contains("loads more than"),
            "a load chain outran its budget: {error}"
        );

        let _ = std::fs::remove_dir_all(&pack);
    }

    /// A panic behind the evaluator has to arrive as a diagnostic. One is reachable from four
    /// tokens of starlark (`"abcd" * 2000000000` asserts on a length that will not fit a
    /// `u32`), but that source spends a minute inside starlark before it gets there, so the
    /// mechanism is pinned directly and the source is left to the corpus that runs out of band.
    #[test]
    fn a_panic_behind_the_evaluator_becomes_a_compile_error() {
        let caught = catching_panics(|| panic!("len overflow"));
        let report = crate::errors::report(&caught.expect_err("the panic escaped"));
        assert!(report.contains("evaluator panicked"), "{report}");
        assert!(report.contains("len overflow"), "{report}");

        let fine = catching_panics(|| 7).expect("a value must pass straight through");
        assert_eq!(fine, 7);
    }

    /// A hard link is a second name for one file. Canonicalizing a path resolves symlinks and
    /// cannot see one, so a link made inside the pack to a file outside it passes every path
    /// check and reads the target anyway.
    #[cfg(unix)]
    #[test]
    fn a_second_name_for_an_outside_file_is_refused() {
        let pack = temp_pack("hardlink");
        let outside = pack
            .parent()
            .unwrap()
            .join(format!("crucible-outside-{}.md", std::process::id()));
        std::fs::write(&outside, "a secret\n").unwrap();
        std::fs::hard_link(&outside, pack.join("prompts/linked.md")).unwrap();
        std::fs::hard_link(&outside, pack.join("linked.star")).unwrap();

        let prompt =
            "workflow([agent(name = \"r\", prompt = prompt_file(\"prompts/linked.md\"))])\n";
        let error = crate::errors::report(
            &compile_source(prompt, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        assert!(error.contains("a pack's files have one"), "{error}");

        let loaded = "load(\"linked.star\", \"x\")\nworkflow([])\n";
        let error = crate::errors::report(
            &compile_source(loaded, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        assert!(error.contains("a pack's files have one"), "{error}");

        // An ordinary file in the same pack is unaffected.
        std::fs::write(pack.join("prompts/plain.md"), "fine\n").unwrap();
        let plain = "workflow([agent(name = \"r\", prompt = prompt_file(\"prompts/plain.md\"))])\n";
        compile_source(plain, &pack.join("workflow.star"), &pack)
            .unwrap_or_else(|error| panic!("{}", crate::errors::report(&error)));

        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&pack);
    }

    /// One node in the graph however many items arrive: the fan-out's width is decided at run
    /// time, its shape is not. `producer.field` is checked against what the producer declares,
    /// so a typo is a compile error rather than an empty list on the day it runs.
    #[test]
    fn a_mapped_task_names_a_declared_field_of_a_dependency() {
        let pack = temp_pack("over");
        let good = r#"
discover = command(name = "discover", run = "./find.sh", emits = ["targets"])
audit = agent(
    name = "audit",
    prompt = "audit it",
    depends_on = [discover],
    over = discover.targets,
    max_fanout = 16,
    isolated = True,
)
workflow(type = "playbook", tasks = [discover, audit])
"#;
        let compiled = compile_source(good, &pack.join("workflow.star"), &pack)
            .unwrap_or_else(|error| panic!("{}", crate::errors::report(&error)));
        let audit = compiled
            .workflow
            .tasks
            .iter()
            .find(|t| t.name.0 == "audit")
            .expect("audit");
        let reference = audit.over.as_ref().expect("over");
        assert_eq!(reference.task.0, "discover");
        assert_eq!(reference.field.0, "targets");
        assert_eq!(audit.max_fanout, Some(16));
        // The graph has two nodes, not seventeen.
        assert_eq!(compiled.workflow.tasks.len(), 2);

        let refusals = [
            ("over = discover.tagrets", "declares no output field"),
            (
                "over = discover.targets,\n    max_fanout = 0",
                "outside 1..=",
            ),
            (
                "over = discover.targets,\n    max_fanout = 9999",
                "outside 1..=",
            ),
            (
                "over = \"discover.targets\",\n    max_fanout = 4",
                "must name a declared output field",
            ),
        ];
        for (clause, expected) in refusals {
            let source = good.replace(
                "over = discover.targets,\n    max_fanout = 16,",
                &format!("{clause},"),
            );
            let error = crate::errors::report(
                &compile_source(&source, &pack.join("workflow.star"), &pack)
                    .err()
                    .unwrap_or_else(|| panic!("{clause}: compiled")),
            );
            assert!(error.contains(expected), "{clause}: {error}");
        }
        let _ = std::fs::remove_dir_all(&pack);
    }

    /// A fan-out reads its items from a dependency, and says how wide it may get before it runs.
    #[test]
    fn a_fanout_must_depend_on_its_producer_and_declare_its_width() {
        let pack = temp_pack("over-coherence");
        let cases = [
            (
                "depends_on = [],\n    over = discover.targets,\n    max_fanout = 4",
                "does not depend on",
            ),
            (
                "depends_on = [discover],\n    over = discover.targets",
                "without max_fanout",
            ),
            (
                "depends_on = [discover],\n    max_fanout = 4",
                "without \"over\"",
            ),
        ];
        for (clause, expected) in cases {
            let source = format!(
                "discover = command(name = \"discover\", run = \"./f.sh\", emits = [\"targets\"])\naudit = agent(\n    name = \"audit\",\n    prompt = \"p\",\n    {clause},\n)\nworkflow(type = \"playbook\", tasks = [discover, audit])\n"
            );
            let error = crate::errors::report(
                &compile_source(&source, &pack.join("workflow.star"), &pack)
                    .err()
                    .unwrap_or_else(|| panic!("{clause}: compiled")),
            );
            assert!(error.contains(expected), "{clause}: {error}");
        }
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn a_playbook_cannot_see_the_scored_constructors() {
        let pack = temp_pack("lane-scope");
        for scored in SCORED_FUNCTIONS {
            let source = format!(
                "t = command(name = \"t\", run = \"true\")\nx = {scored}(name = \"s\")\nworkflow(type = \"playbook\", tasks = [t])\n"
            );
            let error = crate::errors::report(
                &compile_source(&source, &pack.join("workflow.star"), &pack).unwrap_err(),
            );
            assert!(
                error.contains("unknown workflow DSL function") && error.contains(*scored),
                "{scored}: {error}"
            );
        }

        // The same source compiles once the lane is one that has the constructor.
        let scored = "p = propose(name = \"p\")\na = apply(name = \"a\", depends_on = [p])\nm = measure(name = \"m\", depends_on = [a])\nd = decide(name = \"d\", measurement = m)\nworkflow(type = \"autoresearch\", tasks = [p, a, m, d], result = d)\n";
        compile_source(scored, &pack.join("workflow.star"), &pack)
            .unwrap_or_else(|error| panic!("{}", crate::errors::report(&error)));

        // A near-miss in a playbook is corrected toward a playbook name, never a scored one.
        let typo = "t = commnd(name = \"t\", run = \"true\")\nworkflow(type = \"playbook\", tasks = [t])\n";
        let error = crate::errors::report(
            &compile_source(typo, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        assert!(error.contains("command"), "{error}");

        // And a typo'd call to the source's own helper is corrected toward that helper.
        let helper = "def auditor(topic):\n    return command(name = topic, run = \"true\")\nt = auditr(\"a\")\nworkflow(type = \"playbook\", tasks = [t])\n";
        let error = crate::errors::report(
            &compile_source(helper, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        assert!(error.contains("auditor"), "{error}");
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn a_top_level_if_selects_between_task_lists() {
        let pack = temp_pack("top-level-if");
        let source = r#"
thorough = True
if thorough:
    panel = [
        agent(name = "review-correctness", prompt = "Correctness.", isolated = True),
        agent(name = "review-copy", prompt = "Copy.", isolated = True),
    ]
else:
    panel = [agent(name = "review-correctness", prompt = "Correctness.", isolated = True)]
workflow(panel)
"#;
        let compiled = compile_source(source, &pack.join("workflow.star"), &pack).unwrap();
        assert_eq!(task_names(&compiled), ["review-correctness", "review-copy"]);
        let _ = std::fs::remove_dir_all(&pack);
    }

    /// A branch that is never taken must not leave a constructed-but-dropped task behind: only the
    /// executed arm runs its constructors.
    #[test]
    fn an_untaken_branch_constructs_nothing() {
        let pack = temp_pack("untaken");
        let source = r#"
if False:
    extra = command(name = "never", run = "false")
panel = [agent(name = "review", prompt = "Review.", isolated = True)]
workflow(panel)
"#;
        let compiled = compile_source(source, &pack.join("workflow.star"), &pack).unwrap();
        assert_eq!(task_names(&compiled), ["review"]);
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn a_loop_may_not_construct_unbounded_tasks() {
        let pack = temp_pack("task-cap");
        let source = r#"
for i in range(2000):
    command(name = "t" + str(i), run = "true")
workflow([])
"#;
        let error = crate::errors::report(
            &compile_source(source, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        assert!(error.contains("maximum is 128"), "{error}");
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn load_pulls_a_helper_from_a_sibling_file_in_the_pack() {
        let pack = temp_pack("load-sibling");
        std::fs::create_dir_all(pack.join("lib")).unwrap();
        std::fs::write(
            pack.join("lib/gate.star"),
            "def gate(name, sources):\n    return command(\n        name = name,\n        run = \"./gate.sh\",\n        depends_on = sources,\n        join = \"passed\",\n    )\n",
        )
        .unwrap();
        std::fs::write(
            pack.join("panel.star"),
            "load(\"lib/gate.star\", \"gate\")\n\ndef panel(topics):\n    return [\n        agent(name = \"review-\" + topic, prompt = \"Review \" + topic, isolated = True)\n        for topic in topics\n    ]\n",
        )
        .unwrap();
        let source = r#"
load("panel.star", "panel")
load("lib/gate.star", "gate")

reviews = panel(["correctness", "copy"])
workflow(reviews + [gate("gate", reviews)])
"#;
        let compiled = compile_source(source, &pack.join("workflow.star"), &pack).unwrap();
        assert_eq!(
            task_names(&compiled),
            ["review-correctness", "review-copy", "gate"]
        );
        let gate = compiled.workflow.tasks.last().unwrap();
        assert_eq!(gate.join, Join::Passed);
        assert_eq!(
            gate.depends_on,
            ["review-correctness".into(), "review-copy".into()]
        );

        // `enable_load_reexport` is off: `panel.star` loads `gate`, but loading `panel.star`
        // does not hand `gate` through it.
        let leaked = "load(\"panel.star\", \"gate\")\nworkflow([])\n";
        assert!(compile_source(leaked, &pack.join("workflow.star"), &pack).is_err());
        let _ = std::fs::remove_dir_all(&pack);
    }

    /// A library's own `prompt_file` resolves against the pack root, and its tasks land in the
    /// same constructed-task ledger the root is checked against.
    #[test]
    fn a_loaded_module_shares_the_pack_and_the_dropped_task_check() {
        let pack = temp_pack("load-shared");
        std::fs::write(pack.join("prompts/review.md"), "Review it.\n").unwrap();
        std::fs::write(
            pack.join("lib.star"),
            "reviewer = agent(\n    name = \"review\",\n    prompt = prompt_file(\"prompts/review.md\"),\n    isolated = True,\n)\n",
        )
        .unwrap();
        let source = "load(\"lib.star\", \"reviewer\")\nworkflow([reviewer])\n";
        let compiled = compile_source(source, &pack.join("workflow.star"), &pack).unwrap();
        assert_eq!(compiled.prompt_files, [PathBuf::from("prompts/review.md")]);
        let TaskKind::Agent { prompt, .. } = &compiled.workflow.tasks[0].task else {
            panic!("expected agent task")
        };
        assert_eq!(prompt, "Review it.\n");

        let dropped = "load(\"lib.star\", \"reviewer\")\nworkflow([])\n";
        let error = crate::errors::report(
            &compile_source(dropped, &pack.join("workflow.star"), &pack).unwrap_err(),
        );
        assert!(error.contains("constructed but not included"), "{error}");
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[cfg(unix)]
    #[test]
    fn load_refuses_absolute_parent_symlink_and_cyclic_paths() {
        let pack = temp_pack("load-refusals");
        let outside = pack
            .parent()
            .unwrap()
            .join("crucible-starlark-outside.star");
        std::fs::write(&outside, "smuggled = 1\n").unwrap();
        std::os::unix::fs::symlink(&outside, pack.join("link.star")).unwrap();
        std::fs::write(
            pack.join("cycle-a.star"),
            "load(\"cycle-b.star\", \"b\")\na = b\n",
        )
        .unwrap();
        std::fs::write(
            pack.join("cycle-b.star"),
            "load(\"cycle-a.star\", \"a\")\nb = a\n",
        )
        .unwrap();
        let cases = [
            (
                "load(\"/etc/passwd\", \"x\")\nworkflow([])\n",
                "must be a non-empty pack-relative path",
                "workflow.star:1:",
            ),
            (
                "load(\"../crucible-starlark-outside.star\", \"smuggled\")\nworkflow([])\n",
                "may not contain `..` or escape the pack",
                "workflow.star:1:",
            ),
            (
                "load(\"link.star\", \"smuggled\")\nworkflow([])\n",
                "may not traverse symlinks",
                "workflow.star:1:",
            ),
            (
                "load(\"prompts\", \"x\")\nworkflow([])\n",
                "must name a regular, non-symlink file",
                "workflow.star:1:",
            ),
            (
                "load(\"cycle-a.star\", \"a\")\nworkflow([])\n",
                "load(\"cycle-a.star\") forms a cycle",
                "cycle-b.star:1:",
            ),
        ];
        for (source, expected, at) in cases {
            let error = crate::errors::report(
                &compile_source(source, &pack.join("workflow.star"), &pack).unwrap_err(),
            );
            assert!(error.contains(expected), "{source}: {error}");
            assert!(error.contains(at), "{source}: {error}");
        }
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&pack);
    }

    /// The widened grammar buys expressiveness, not reach: the global surface is the starlark
    /// standard plus the DSL, and nothing in it touches a process, a socket, a clock, or a PRNG.
    #[test]
    fn no_process_network_clock_or_randomness_is_reachable() {
        let globals = lane_globals(WorkflowType::Autoresearch);
        let names: BTreeSet<String> = globals
            .names()
            .map(|name| name.as_str().to_owned())
            .collect();
        for function in dsl_functions(WorkflowType::Autoresearch) {
            assert!(names.contains(function), "{function} is not callable");
        }
        for forbidden in [
            "open",
            "read",
            "write",
            "print",
            "input",
            "time",
            "now",
            "clock",
            "random",
            "exec",
            "eval",
            "compile",
            "import",
            "os",
            "sys",
            "subprocess",
            "getenv",
            "environ",
            "host",
            "http",
            "socket",
            "struct",
            "json",
        ] {
            assert!(!names.contains(forbidden), "{forbidden} is reachable");
        }

        let pack = temp_pack("sandbox");
        for probe in [
            "open(\"/etc/passwd\")",
            "time()",
            "random()",
            "exec(\"/bin/sh\")",
            "getenv(\"HOME\")",
            "print(\"leak\")",
        ] {
            let source = format!("x = {probe}\nworkflow([])\n");
            let error = crate::errors::report(
                &compile_source(&source, &pack.join("workflow.star"), &pack).unwrap_err(),
            );
            assert!(error.contains("unknown workflow"), "{probe}: {error}");
        }
        // Loaded modules run under the same globals, so a library cannot smuggle reach in.
        std::fs::write(pack.join("lib.star"), "x = open(\"/etc/passwd\")\n").unwrap();
        let error = crate::errors::report(
            &compile_source(
                "load(\"lib.star\", \"x\")\nworkflow([])\n",
                &pack.join("workflow.star"),
                &pack,
            )
            .unwrap_err(),
        );
        assert!(error.contains("unknown workflow"), "{error}");
        let _ = std::fs::remove_dir_all(&pack);
    }

    /// Two compiles of the same pack in one process must agree byte for byte.
    #[test]
    fn compiling_the_adversarial_example_twice_is_byte_identical() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let pack = root.join("examples/adversarial-review");
        let first = compile_file(&pack.join("workflow.star"), &pack).unwrap();
        let second = compile_file(&pack.join("workflow.star"), &pack).unwrap();
        assert_eq!(first.canonical_json, second.canonical_json);
        assert_eq!(first.prompt_files, second.prompt_files);
    }
}
