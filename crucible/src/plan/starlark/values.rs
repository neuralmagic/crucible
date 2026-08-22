//! The opaque starlark values the DSL constructors hand back. Each wraps owned, immutable
//! compiler data: there are no methods, no attributes, and no mutation, so a workflow can only
//! pass them along to another constructor.

use std::fmt::{self, Display};

use allocative::Allocative;
use starlark::starlark_simple_value;
use starlark::values::{NoSerialize, ProvidesStaticType, StarlarkValue, starlark_value};

use crate::manifest::WorkflowCfg;
use crate::plan::ir::Task;
use crate::plan::starlark::SessionDecl;

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct TaskValue(#[allocative(skip)] pub(crate) Task);

starlark_simple_value!(TaskValue);

impl Display for TaskValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "task({})", self.0.name)
    }
}

#[starlark_value(type = "task")]
impl<'v> StarlarkValue<'v> for TaskValue {}

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
