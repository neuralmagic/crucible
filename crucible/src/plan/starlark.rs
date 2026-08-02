//! Deterministic Starlark frontend for [`WorkflowCfg`]. Scope freezes the compiled IR, so runtime
//! never evaluates the source. Only `prompt_file` can read files; process, environment, network,
//! clock, and randomness APIs are unavailable.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use starlark_syntax::lexer::TokenInt;
use starlark_syntax::syntax::ast::{
    Argument, AssignTarget, AstLiteral, AstStmt, BinOp, CallArgsP, Expr, Stmt,
};
use starlark_syntax::syntax::{AstModule, Dialect};

use crate::manifest::{WorkflowCfg, WorkflowType};
use crate::plan::ir::{Direction, EngineOp, Isolation, Join, Task, TaskKind, TaskName};

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
}

impl CompileContext {
    fn prompt_file(&mut self, raw: &str) -> Result<String> {
        let relative = safe_relative_path(raw)?;
        let root = std::fs::canonicalize(&self.pack_dir)
            .with_context(|| format!("resolving pack directory {}", self.pack_dir.display()))?;
        let mut path = self.pack_dir.clone();
        let mut metadata = None;
        for component in relative.components() {
            let Component::Normal(component) = component else {
                continue;
            };
            path.push(component);
            let current = std::fs::symlink_metadata(&path)
                .with_context(|| format!("reading prompt metadata {}", path.display()))?;
            if current.file_type().is_symlink() {
                bail!("prompt_file({raw:?}) may not traverse symlinks");
            }
            metadata = Some(current);
        }
        if !metadata.is_some_and(|metadata| metadata.is_file()) {
            bail!("prompt_file({raw:?}) must name a regular, non-symlink file");
        }
        let canonical = std::fs::canonicalize(&path)
            .with_context(|| format!("resolving prompt file {}", path.display()))?;
        if !canonical.starts_with(&root) {
            bail!("prompt_file({raw:?}) escapes the pack directory");
        }
        let bytes = std::fs::read(&canonical)
            .with_context(|| format!("reading prompt file {}", canonical.display()))?;
        if bytes.len() > MAX_PROMPT_BYTES {
            bail!(
                "prompt_file({raw:?}) is {} bytes; maximum is {MAX_PROMPT_BYTES}",
                bytes.len()
            );
        }
        self.total_prompt_bytes = self.total_prompt_bytes.saturating_add(bytes.len());
        if self.total_prompt_bytes > MAX_TOTAL_PROMPT_BYTES {
            bail!("workflow embeds more than {MAX_TOTAL_PROMPT_BYTES} bytes of prompt files");
        }
        self.prompt_files.insert(relative);
        String::from_utf8(bytes).context("prompt files must be UTF-8")
    }

    fn step(&mut self) -> Result<()> {
        self.eval_steps += 1;
        if self.eval_steps > MAX_EVAL_STEPS {
            bail!("workflow evaluation exceeds {MAX_EVAL_STEPS} expression steps");
        }
        Ok(())
    }
}

fn safe_relative_path(raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    if raw.trim().is_empty() || path.is_absolute() {
        bail!("prompt_file path must be a non-empty pack-relative path");
    }
    if path.components().any(|part| {
        matches!(
            part,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        bail!("prompt_file path may not contain `..` or escape the pack");
    }
    Ok(path.to_path_buf())
}

#[derive(Clone, Debug)]
enum Value {
    None,
    Bool(bool),
    Int(i32),
    String(String),
    List(Vec<Value>),
    Task(Task),
    Workflow(WorkflowCfg),
}

struct Compiler {
    context: CompileContext,
    variables: BTreeMap<String, Value>,
}

impl Compiler {
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
                    bail!("workflow assignments must target a single variable name");
                };
                let value = self.expression(&assign.rhs)?;
                self.variables.insert(name.node.ident.clone(), value);
                Ok(None)
            }
            Stmt::Expression(expression) => self.expression(expression).map(Some),
            Stmt::Load(_) => bail!(
                "workflow Starlark may not use load(); use pack-local prompt_file() for prompts"
            ),
            _ => bail!(
                "workflow Starlark is declarative: use assignments, lists, list concatenation, and DSL calls"
            ),
        }
    }

    fn expression(&mut self, expression: &starlark_syntax::syntax::ast::AstExpr) -> Result<Value> {
        self.context.step()?;
        match &expression.node {
            Expr::Identifier(identifier) => match identifier.node.ident.as_str() {
                "True" => Ok(Value::Bool(true)),
                "False" => Ok(Value::Bool(false)),
                "None" => Ok(Value::None),
                name => self
                    .variables
                    .get(name)
                    .cloned()
                    .with_context(|| format!("unknown workflow variable {name:?}")),
            },
            Expr::Literal(AstLiteral::String(value)) => Ok(Value::String(value.node.clone())),
            Expr::Literal(AstLiteral::Int(value)) => match &value.node {
                TokenInt::I32(value) => Ok(Value::Int(*value)),
                TokenInt::BigInt(_) => bail!("workflow integers must fit in 32 bits"),
            },
            Expr::List(items) | Expr::Tuple(items) => items
                .iter()
                .map(|item| self.expression(item))
                .collect::<Result<Vec<_>>>()
                .map(Value::List),
            Expr::Op(left, BinOp::Add, right) => {
                let (Value::List(mut left), Value::List(right)) =
                    (self.expression(left)?, self.expression(right)?)
                else {
                    bail!("workflow `+` is supported only for task or dependency lists");
                };
                left.extend(right);
                Ok(Value::List(left))
            }
            Expr::Call(function, args) => {
                let Expr::Identifier(function) = &function.node else {
                    bail!("workflow calls must name a DSL constructor directly");
                };
                self.call(&function.node.ident, args)
            }
            _ => bail!(
                "unsupported workflow expression; use strings, integers, booleans, lists, list concatenation, and DSL calls"
            ),
        }
    }

    fn call(
        &mut self,
        function: &str,
        args: &CallArgsP<starlark_syntax::syntax::ast::AstNoPayload>,
    ) -> Result<Value> {
        if matches!(
            function,
            "prompt_file" | "deps" | "workflow" | "default_autoresearch"
        ) && args
            .args
            .iter()
            .all(|argument| matches!(argument.node, Argument::Positional(_)))
        {
            let [argument] = args.args.as_slice() else {
                bail!("{function}() takes exactly one positional argument");
            };
            let Argument::Positional(argument) = &argument.node else {
                bail!("{function}() takes exactly one positional argument");
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
                        _ => bail!("deps() entries must be task constructor values"),
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
                _ => bail!("{function}() received the wrong value type"),
            };
        }

        let mut named = BTreeMap::new();
        for argument in &args.args {
            let Argument::Named(name, value) = &argument.node else {
                bail!("{function}() task constructor arguments must be named");
            };
            let value = self.expression(value)?;
            if named.insert(name.node.clone(), value).is_some() {
                bail!("{function}() repeats argument {:?}", name.node);
            }
        }
        if function == "workflow" {
            let workflow_type = match take_string_default(&mut named, "type", "autoresearch")?
                .as_str()
            {
                "autoresearch" => WorkflowType::Autoresearch,
                "custom" => WorkflowType::Custom,
                other => bail!("workflow type must be `autoresearch` or `custom`, got {other:?}"),
            };
            let tasks = match take_value(&mut named, "tasks")? {
                Value::List(tasks) => task_list("workflow", tasks)?,
                _ => bail!("workflow tasks must be a list of task constructor values"),
            };
            let result = take_optional_task_name(&mut named, "result")?;
            if !named.is_empty() {
                bail!(
                    "workflow() has unknown argument(s): {}",
                    named.keys().cloned().collect::<Vec<_>>().join(", ")
                );
            }
            let workflow = WorkflowCfg {
                workflow_type,
                result,
                tasks,
            };
            workflow.validate()?;
            return Ok(Value::Workflow(workflow));
        }

        let task = match function {
            "agent" => Task {
                name: TaskName(take_string(&mut named, "name")?),
                task: TaskKind::Agent {
                    prompt: take_string(&mut named, "prompt")?,
                    harness: take_optional_string(&mut named, "harness")?,
                    model: take_optional_string(&mut named, "model")?,
                    effort: take_optional_string(&mut named, "effort")?,
                },
                depends_on: take_task_names(&mut named)?,
                needs: take_string_default(&mut named, "needs", "any")?,
                required: take_bool_default(&mut named, "required", true)?,
                isolation: isolation(take_bool_default(&mut named, "isolated", false)?),
                join: parse_join(&take_string_default(&mut named, "join", "all")?)?,
            },
            "command" => Task {
                name: TaskName(take_string(&mut named, "name")?),
                task: TaskKind::Command {
                    command: take_string(&mut named, "run")?,
                },
                depends_on: take_task_names(&mut named)?,
                needs: take_string_default(&mut named, "needs", "any")?,
                required: take_bool_default(&mut named, "required", true)?,
                isolation: isolation(take_bool_default(&mut named, "isolated", false)?),
                join: parse_join(&take_string_default(&mut named, "join", "all")?)?,
            },
            "top_k" => {
                let k = take_int(&mut named, "k")?;
                if k <= 0 {
                    bail!("top_k k must be >= 1");
                }
                let direction = match take_string(&mut named, "direction")?.as_str() {
                    "lower" => Direction::Lower,
                    "higher" => Direction::Higher,
                    other => bail!("top_k direction must be `lower` or `higher`, got {other:?}"),
                };
                if !named.contains_key("depends_on") {
                    bail!("missing required argument \"depends_on\"");
                }
                Task {
                    name: TaskName(take_string(&mut named, "name")?),
                    task: TaskKind::TopK {
                        k: k as u32,
                        direction,
                    },
                    depends_on: take_task_names(&mut named)?,
                    needs: "any".to_owned(),
                    required: take_bool_default(&mut named, "required", true)?,
                    isolation: None,
                    join: Join::Passed,
                }
            }
            "propose" => engine_task(&mut named, EngineOp::Propose, None)?,
            "apply" => engine_task(&mut named, EngineOp::Apply, None)?,
            "measure" => engine_task(&mut named, EngineOp::Measure, None)?,
            "decide" => {
                let source = take_task_name(&mut named, "measurement")?;
                if !named.contains_key("depends_on") {
                    named.insert(
                        "depends_on".to_owned(),
                        Value::List(vec![Value::String(source.0.clone())]),
                    );
                }
                engine_task(&mut named, EngineOp::Decide, Some(source))?
            }
            _ => bail!("unknown workflow DSL function {function:?}"),
        };
        if !named.is_empty() {
            bail!(
                "{function}() has unknown argument(s): {}",
                named.keys().cloned().collect::<Vec<_>>().join(", ")
            );
        }
        Ok(Value::Task(task))
    }
}

fn task_list(function: &str, tasks: Vec<Value>) -> Result<Vec<Task>> {
    if tasks.len() > MAX_TASKS {
        bail!(
            "{function} expands to {} tasks; maximum is {MAX_TASKS}",
            tasks.len()
        );
    }
    tasks
        .into_iter()
        .map(|task| match task {
            Value::Task(task) => Ok(task),
            _ => bail!("{function} entries must be task constructor values"),
        })
        .collect()
}

fn default_autoresearch(mut extras: Vec<Task>) -> Result<WorkflowCfg> {
    let propose = engine("propose", EngineOp::Propose, None, Vec::new());
    for task in &mut extras {
        if task.depends_on.is_empty() {
            task.depends_on.push(propose.name.clone());
        }
    }
    let mut tasks = vec![propose];
    tasks.extend(extras);
    let sinks: Vec<TaskName> = tasks
        .iter()
        .filter(|task| {
            !tasks.iter().any(|other| {
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
        bail!(
            "default_autoresearch expands to {} tasks; maximum is {MAX_TASKS}",
            tasks.len()
        );
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

fn engine(name: &str, op: EngineOp, source: Option<TaskName>, depends_on: Vec<TaskName>) -> Task {
    Task {
        name: TaskName(name.to_owned()),
        task: TaskKind::Engine { op, source },
        depends_on,
        needs: "any".to_owned(),
        required: true,
        isolation: None,
        join: Join::All,
    }
}

fn take_value(named: &mut BTreeMap<String, Value>, name: &str) -> Result<Value> {
    named
        .remove(name)
        .with_context(|| format!("missing required argument {name:?}"))
}

fn take_string(named: &mut BTreeMap<String, Value>, name: &str) -> Result<String> {
    match take_value(named, name)? {
        Value::String(value) => Ok(value),
        _ => bail!("argument {name:?} must be a string"),
    }
}

fn take_optional_string(named: &mut BTreeMap<String, Value>, name: &str) -> Result<Option<String>> {
    match named.remove(name).unwrap_or(Value::None) {
        Value::None => Ok(None),
        Value::String(value) => Ok(Some(value)),
        _ => bail!("argument {name:?} must be a string or None"),
    }
}

fn take_task_name(named: &mut BTreeMap<String, Value>, name: &str) -> Result<TaskName> {
    match take_value(named, name)? {
        Value::String(value) => Ok(TaskName(value)),
        Value::Task(task) => Ok(task.name),
        _ => bail!("argument {name:?} must be a task or task-name string"),
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
        _ => bail!("argument {name:?} must be a task, task-name string, or None"),
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
        Some(_) => bail!("argument {name:?} must be a string"),
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
        Some(_) => bail!("argument {name:?} must be True or False"),
    }
}

fn take_int(named: &mut BTreeMap<String, Value>, name: &str) -> Result<i32> {
    match take_value(named, name)? {
        Value::Int(value) => Ok(value),
        _ => bail!("argument {name:?} must be an integer"),
    }
}

fn take_task_names(named: &mut BTreeMap<String, Value>) -> Result<Vec<TaskName>> {
    match named.remove("depends_on") {
        None => Ok(Vec::new()),
        Some(Value::List(names)) => names
            .into_iter()
            .map(|name| match name {
                Value::String(name) => Ok(TaskName(name)),
                Value::Task(task) => Ok(task.name),
                _ => bail!("depends_on entries must be tasks or task-name strings"),
            })
            .collect(),
        Some(_) => bail!("depends_on must be a list of tasks or task-name strings"),
    }
}

fn isolation(isolated: bool) -> Option<Isolation> {
    isolated.then_some(Isolation::Worktree)
}

fn parse_join(join: &str) -> Result<Join> {
    match join {
        "all" => Ok(Join::All),
        "passed" => Ok(Join::Passed),
        other => bail!("join must be `all` or `passed`, got {other:?}"),
    }
}

pub fn compile_file(path: &Path, pack_dir: &Path) -> Result<CompiledWorkflow> {
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("reading workflow source {}", path.display()))?;
    compile_source(&source, path, pack_dir)
}

/// Compile `workflow.star` into the manifest's generated `[workflow]` block.
pub fn materialize_manifest(source_path: &Path, manifest_path: &Path) -> Result<CompiledWorkflow> {
    use toml_edit::DocumentMut;

    #[derive(serde::Serialize)]
    struct ManifestWorkflow<'a> {
        workflow: &'a WorkflowCfg,
    }

    let pack_dir = manifest_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let compiled = compile_file(source_path, pack_dir)?;
    let manifest = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("reading manifest {}", manifest_path.display()))?;
    let mut document: DocumentMut = manifest
        .parse()
        .with_context(|| format!("parsing manifest {}", manifest_path.display()))?;
    let workflow_toml = toml::to_string(&ManifestWorkflow {
        workflow: &compiled.workflow,
    })?;
    workflow_toml
        .parse::<DocumentMut>()
        .context("rendering compiled workflow as TOML")?;
    document.remove("workflow");
    let mut materialized = document.to_string();
    if !materialized.ends_with('\n') {
        materialized.push('\n');
    }
    materialized.push_str(
        "\n# Generated from workflow.star. Edit the Starlark source; scope recompiles it.\n",
    );
    materialized.push_str(&workflow_toml);
    write_atomically(manifest_path, &materialized)
        .with_context(|| format!("writing manifest {}", manifest_path.display()))?;
    Ok(compiled)
}

/// Replace a manifest atomically.
fn write_atomically(path: &Path, body: &str) -> Result<()> {
    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating temp file beside {}", path.display()))?;
    // Preserve the manifest's mode across the temporary-file rename.
    if let Ok(existing) = std::fs::metadata(path) {
        file.as_file()
            .set_permissions(existing.permissions())
            .with_context(|| format!("preserving permissions on {}", path.display()))?;
    }
    std::io::Write::write_all(file.as_file_mut(), body.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(path)
        .map_err(|e| anyhow::anyhow!("installing {}: {e}", path.display()))?;
    Ok(())
}

/// Compile a conventional sibling `workflow.star` when one is present.
pub fn materialize_sibling_manifest(manifest_path: &Path) -> Result<Option<CompiledWorkflow>> {
    let pack_dir = manifest_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let source_path = pack_dir.join("workflow.star");
    source_path
        .exists()
        .then(|| materialize_manifest(&source_path, manifest_path))
        .transpose()
}

pub fn compile_source(source: &str, filename: &Path, pack_dir: &Path) -> Result<CompiledWorkflow> {
    if source.len() > MAX_SOURCE_BYTES {
        bail!(
            "workflow source is {} bytes; maximum is {MAX_SOURCE_BYTES}",
            source.len()
        );
    }
    let ast = AstModule::parse(
        &filename.display().to_string(),
        source.to_owned(),
        &Dialect::Standard,
    )
    .map_err(|error| anyhow::anyhow!("parsing workflow Starlark: {error}"))?;
    let mut compiler = Compiler {
        context: CompileContext {
            pack_dir: pack_dir.to_path_buf(),
            prompt_files: BTreeSet::new(),
            total_prompt_bytes: 0,
            eval_steps: 0,
        },
        variables: BTreeMap::new(),
    };
    let Some(Value::Workflow(workflow)) = compiler.statement(ast.statement())? else {
        bail!("workflow source must end with workflow(...) or default_autoresearch([...])");
    };
    workflow.validate()?;
    let canonical_json = serde_json::to_string_pretty(&workflow)? + "\n";
    Ok(CompiledWorkflow {
        workflow,
        prompt_files: compiler.context.prompt_files.into_iter().collect(),
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
candidate = propose(name = "invent")
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
    fn prompt_file_refuses_escape_symlink_and_oversize_content() {
        let pack = temp_pack("paths");
        let source = |path: &str| {
            format!("workflow([agent(name = \"r\", prompt = prompt_file({path:?}))])\n")
        };
        assert!(
            compile_source(&source("../secret.md"), &pack.join("workflow.star"), &pack)
                .unwrap_err()
                .to_string()
                .contains("may not contain")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            std::fs::write(pack.join("real.md"), "secret").unwrap();
            symlink(pack.join("real.md"), pack.join("prompts/link.md")).unwrap();
            assert!(
                compile_source(
                    &source("prompts/link.md"),
                    &pack.join("workflow.star"),
                    &pack
                )
                .unwrap_err()
                .to_string()
                .contains("symlink")
            );
        }
        std::fs::write(
            pack.join("prompts/huge.md"),
            vec![b'x'; MAX_PROMPT_BYTES + 1],
        )
        .unwrap();
        assert!(
            compile_source(
                &source("prompts/huge.md"),
                &pack.join("workflow.star"),
                &pack
            )
            .unwrap_err()
            .to_string()
            .contains("maximum")
        );
        let _ = std::fs::remove_dir_all(&pack);
    }

    #[test]
    fn rejects_loads_and_runaway_evaluation() {
        let pack = temp_pack("bounds");
        let load = "load(\"x.star\", \"x\")\nworkflow([])\n";
        assert!(
            compile_source(load, &pack.join("workflow.star"), &pack)
                .unwrap_err()
                .to_string()
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

    /// Temp file plus rename, and the mode survives it.
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
        // No temp file left beside the manifest.
        let strays: Vec<_> = std::fs::read_dir(&pack)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .filter(|name| name.to_string_lossy().starts_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "left temp files: {strays:?}");
        let _ = std::fs::remove_dir_all(&pack);
    }
}
