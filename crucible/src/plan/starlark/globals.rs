//! The DSL global surface handed to the evaluator.
//!
//! Every constructor takes `*args, **kwargs` and marshals into the shared argument tables, so the
//! argument diagnostics stay the compiler's own rather than starlark's parameter-signature ones.

use std::collections::BTreeMap;

use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::float::StarlarkFloat;
use starlark::values::list::{AllocList, ListRef};
use starlark::values::tuple::{TupleRef, UnpackTuple};
use starlark::values::{Heap, Value, ValueLike};
use starlark_syntax::codemap::FileSpan;

use crate::manifest::{WorkflowCfg, WorkflowType};
use crate::plan::starlark as dsl;
use crate::plan::starlark::values::{OutputRefValue, SessionValue, TaskValue, WorkflowValue};

/// Constructors that historically took one positional argument. Everything else is named-only.
const POSITIONAL: &[&str] = &["prompt_file", "workflow", "default_autoresearch"];

/// The constructors every lane has.
#[starlark_module]
pub(crate) fn common(builder: &mut GlobalsBuilder) {
    fn agent<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        dispatch("agent", args, kwargs, eval)
    }

    fn command<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        dispatch("command", args, kwargs, eval)
    }

    fn evaluate<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        dispatch("evaluate", args, kwargs, eval)
    }

    fn prompt_file<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        dispatch("prompt_file", args, kwargs, eval)
    }

    fn session<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        dispatch("session", args, kwargs, eval)
    }

    fn workflow<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        dispatch("workflow", args, kwargs, eval)
    }
}

/// The scored loop's own constructors. A playbook never sees these, so a playbook author
/// cannot name one and cannot be offered one by a did-you-mean.
#[starlark_module]
pub(crate) fn scored(builder: &mut GlobalsBuilder) {
    fn apply<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        dispatch("apply", args, kwargs, eval)
    }

    fn decide<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        dispatch("decide", args, kwargs, eval)
    }

    fn default_autoresearch<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        dispatch("default_autoresearch", args, kwargs, eval)
    }

    fn grade<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        dispatch("grade", args, kwargs, eval)
    }

    fn measure<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        dispatch("measure", args, kwargs, eval)
    }

    fn propose<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        dispatch("propose", args, kwargs, eval)
    }

    fn top_k<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        dispatch("top_k", args, kwargs, eval)
    }
}

fn dispatch<'v>(
    function: &'static str,
    args: UnpackTuple<Value<'v>>,
    kwargs: SmallMap<String, Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let Some(state) = eval
        .extra
        .and_then(|extra| extra.downcast_ref::<dsl::CompileState>())
    else {
        return Err(starlark::Error::new_native(anyhow::Error::new(
            dsl::StateMissing,
        )));
    };
    let at = eval
        .call_stack_top_location()
        .unwrap_or_else(|| state.site());
    match call(function, args, kwargs, state, &at) {
        Ok(value) => Ok(alloc(eval.heap(), value)),
        Err(error) => Err(state.throw(error)),
    }
}

fn call<'v>(
    function: &'static str,
    args: UnpackTuple<Value<'v>>,
    kwargs: SmallMap<String, Value<'v>>,
    state: &dsl::CompileState,
    at: &FileSpan,
) -> dsl::Result<dsl::Value> {
    if POSITIONAL.contains(&function) && kwargs.is_empty() {
        let one_positional = || {
            located(
                at,
                dsl::CompileError::NotOnePositional {
                    function: function.to_owned(),
                },
            )
        };
        let [argument] = args.items.as_slice() else {
            return Err(one_positional());
        };
        let argument = convert(*argument).map_err(|error| located(at, error))?;
        return match (function, argument) {
            ("prompt_file", dsl::Value::String(path)) => state
                .context_mut()
                .prompt_file(&path)
                .map(dsl::Value::String),
            ("workflow", dsl::Value::List(tasks)) => {
                let tasks = dsl::task_list("workflow", tasks)?;
                let workflow = WorkflowCfg {
                    workflow_type: WorkflowType::Autoresearch,
                    result: None,
                    tasks,
                    file: None,
                    resolved_from: None,
                };
                workflow.validate()?;
                Ok(dsl::Value::Workflow(workflow))
            }
            ("default_autoresearch", dsl::Value::List(tasks)) => {
                dsl::default_autoresearch(dsl::task_list("default_autoresearch", tasks)?)
                    .map(dsl::Value::Workflow)
            }
            _ => Err(located(
                at,
                dsl::CompileError::WrongPositionalType {
                    function: function.to_owned(),
                },
            )),
        };
    }
    if !args.items.is_empty() {
        return Err(located(
            at,
            dsl::CompileError::PositionalArgument {
                function: function.to_owned(),
            },
        ));
    }
    let named = kwargs
        .into_iter()
        .map(|(name, value)| Ok((name, convert(value)?)))
        .collect::<dsl::Result<BTreeMap<String, dsl::Value>>>()
        .map_err(|error| located(at, error))?;
    dsl::constructor(function, named, state, at).map_err(|error| located(at, error))
}

/// Attach the call site to an error that does not already carry a location.
fn located(at: &FileSpan, error: dsl::CompileError) -> dsl::CompileError {
    match error {
        located @ dsl::CompileError::At { .. } => located,
        error => dsl::CompileError::At {
            at: at.clone(),
            inner: Box::new(error),
        },
    }
}

/// A starlark argument as the constructor tables see it. Anything outside the DSL's own value
/// space becomes [`dsl::Value::Opaque`], which every `take_*` helper reports as a wrong type.
fn convert(value: Value<'_>) -> dsl::Result<dsl::Value> {
    convert_at(value, 0)
}

/// Marshal one argument, refusing a value nested deeper than a source is allowed to nest.
///
/// A loop can build a value far deeper than any literal: `for i in range(3000): x = [x]` is four
/// shallow lines. Both this and [`alloc`] recurse once per level, so an unbounded value
/// overflows the native stack and aborts the process while every declared budget is still
/// satisfied. A value deeper than [`dsl::MAX_NESTING_DEPTH`] has no legitimate origin, since a
/// source may not nest that far either.
fn convert_at(value: Value<'_>, depth: usize) -> dsl::Result<dsl::Value> {
    if depth > dsl::MAX_NESTING_DEPTH {
        return Err(dsl::CompileError::ValueTooDeep { depth });
    }
    if value.is_none() {
        return Ok(dsl::Value::None);
    }
    if let Some(boolean) = value.unpack_bool() {
        return Ok(dsl::Value::Bool(boolean));
    }
    if let Some(integer) = value.unpack_i32() {
        return Ok(dsl::Value::Int(integer));
    }
    if value.get_type() == "int" {
        return Err(dsl::CompileError::IntegerTooWide);
    }
    if let Some(float) = value.downcast_ref::<StarlarkFloat>() {
        return Ok(dsl::Value::Float(float.0));
    }
    if let Some(text) = value.unpack_str() {
        return Ok(dsl::Value::String(text.to_owned()));
    }
    if let Some(list) = ListRef::from_value(value) {
        return list
            .iter()
            .map(|item| convert_at(item, depth + 1))
            .collect::<dsl::Result<Vec<_>>>()
            .map(dsl::Value::List);
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        return tuple
            .iter()
            .map(|item| convert_at(item, depth + 1))
            .collect::<dsl::Result<Vec<_>>>()
            .map(dsl::Value::List);
    }
    if let Some(task) = TaskValue::from_value(value) {
        return Ok(dsl::Value::Task(task.0.clone()));
    }
    if let Some(output) = OutputRefValue::from_value(value) {
        return output.resolve().map(dsl::Value::Output);
    }
    if let Some(session) = SessionValue::from_value(value) {
        return Ok(dsl::Value::Session(session.0.clone()));
    }
    if let Some(workflow) = WorkflowValue::from_value(value) {
        return Ok(dsl::Value::Workflow(workflow.0.clone()));
    }
    Ok(dsl::Value::Opaque)
}

fn alloc<'v>(heap: Heap<'v>, value: dsl::Value) -> Value<'v> {
    alloc_at(heap, value, 0)
}

/// The mirror of [`convert_at`]. A value that reached the tables is already bounded, so a level
/// past the bound means the tables themselves built one and the list is truncated rather than
/// the process aborted.
fn alloc_at<'v>(heap: Heap<'v>, value: dsl::Value, depth: usize) -> Value<'v> {
    if depth > dsl::MAX_NESTING_DEPTH {
        return Value::new_none();
    }
    match value {
        dsl::Value::None | dsl::Value::Opaque => Value::new_none(),
        dsl::Value::Bool(boolean) => Value::new_bool(boolean),
        dsl::Value::Int(integer) => heap.alloc(integer),
        dsl::Value::Float(float) => heap.alloc(float),
        dsl::Value::String(text) => heap.alloc(text),
        dsl::Value::List(items) => {
            let items: Vec<Value<'v>> = items
                .into_iter()
                .map(|item| alloc_at(heap, item, depth + 1))
                .collect();
            heap.alloc(AllocList(items))
        }
        dsl::Value::Task(task) => heap.alloc(TaskValue(task)),
        dsl::Value::Output(reference) => heap.alloc(OutputRefValue {
            declared: vec![reference.field.0.clone()],
            reference,
        }),
        dsl::Value::Session(session) => heap.alloc(SessionValue(session)),
        dsl::Value::Workflow(workflow) => heap.alloc(WorkflowValue(workflow)),
    }
}
