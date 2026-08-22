//! The opaque starlark values the DSL constructors hand back. Each wraps owned, immutable
//! compiler data, and nothing here mutates or reaches into the compiler: a workflow can only
//! pass these along to another constructor.
//!
//! A task has exactly one attribute shape, `task.field`, which names one of the fields that task
//! declared it emits and yields another opaque value. It exists because `over = discover.targets`
//! is the clearest way to say what a fan-out runs over, and because naming the field in the
//! source is what lets a typo be a compile error rather than an empty list at run time.

use std::fmt::{self, Display};

use allocative::Allocative;
use starlark::starlark_simple_value;
use starlark::values::{
    Heap, NoSerialize, ProvidesStaticType, StarlarkValue, Value, starlark_value,
};

use crate::manifest::WorkflowCfg;
use crate::plan::diag;
use crate::plan::ir::{OutputField, OutputRef, Task};
use crate::plan::starlark::{CompileError, SessionDecl};

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct TaskValue(#[allocative(skip)] pub(crate) Task);

starlark_simple_value!(TaskValue);

impl Display for TaskValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "task({})", self.0.name)
    }
}

#[starlark_value(type = "task")]
impl<'v> StarlarkValue<'v> for TaskValue {
    /// Any attribute yields a reference; whether the field was declared is decided where the
    /// reference is used. starlark's own missing-attribute error would say nothing about
    /// `emits`, and the constructor that consumes the reference is where a useful message can
    /// name the task, the field, and what the task does declare.
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        Some(heap.alloc(OutputRefValue {
            reference: OutputRef {
                task: self.0.name.clone(),
                field: OutputField(attribute.to_owned()),
            },
            declared: self.0.emits.iter().map(|field| field.0.clone()).collect(),
        }))
    }
}

/// One task's output field, as `over = producer.field` yields it. `declared` travels with it so
/// the constructor can tell an undeclared field from a declared one without a second lookup.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct OutputRefValue {
    #[allocative(skip)]
    pub(crate) reference: OutputRef,
    pub(crate) declared: Vec<String>,
}

impl OutputRefValue {
    pub(crate) fn resolve(&self) -> Result<OutputRef, CompileError> {
        if self.declared.contains(&self.reference.field.0) {
            return Ok(self.reference.clone());
        }
        Err(CompileError::UndeclaredOutputField {
            task: self.reference.task.0.clone(),
            field: self.reference.field.0.clone(),
            suggestion: diag::suggest(
                &self.reference.field.0,
                self.declared.iter().map(String::as_str),
            )
            .map(str::to_owned),
            declared: self.declared.join(", "),
        })
    }
}

starlark_simple_value!(OutputRefValue);

impl Display for OutputRefValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reference)
    }
}

#[starlark_value(type = "output")]
impl<'v> StarlarkValue<'v> for OutputRefValue {}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct SessionValue(#[allocative(skip)] pub(crate) SessionDecl);

starlark_simple_value!(SessionValue);

impl Display for SessionValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "session({})", self.0.name)
    }
}

#[starlark_value(type = "session")]
impl<'v> StarlarkValue<'v> for SessionValue {}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct WorkflowValue(#[allocative(skip)] pub(crate) WorkflowCfg);

starlark_simple_value!(WorkflowValue);

impl Display for WorkflowValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "workflow({} tasks)", self.0.tasks.len())
    }
}

#[starlark_value(type = "workflow")]
impl<'v> StarlarkValue<'v> for WorkflowValue {}
