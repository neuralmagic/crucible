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
use crate::plan::ir::{ApprovalSourceSpec, OutputField, OutputRef, Task};
use crate::plan::starlark::{CompileError, SessionDecl};
use crucible_contract::JiraUntil;

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

/// Text that carries where each of its spans came from.
///
/// A value a launcher supplied did not originate inside the pack, so a prompt containing it has
/// to say which span is which: an agent reading `Read the paper at <url>` cannot otherwise tell
/// the pack's instruction from whatever the URL's author put there. Concatenation keeps the
/// spans apart, which is what lets the rendered prompt mark exactly the outside text and nothing
/// else.
///
/// A defaulted value is not external. It was written in the pack by the same author as the
/// prompt around it.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct ExternalText(pub(crate) Vec<Segment>);

/// One run of text, and whether it came from outside the pack.
#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub(crate) struct Segment {
    pub(crate) text: String,
    pub(crate) external: bool,
}

impl ExternalText {
    pub(crate) fn external(text: impl Into<String>) -> Self {
        ExternalText(vec![Segment {
            text: text.into(),
            external: true,
        }])
    }

    fn joined(before: &[Segment], after: &[Segment]) -> Self {
        let mut segments: Vec<Segment> = Vec::with_capacity(before.len() + after.len());
        for segment in before.iter().chain(after) {
            match segments.last_mut() {
                Some(last) if last.external == segment.external => {
                    last.text.push_str(&segment.text)
                }
                _ => segments.push(segment.clone()),
            }
        }
        ExternalText(segments)
    }

    fn plain(text: &str) -> Vec<Segment> {
        vec![Segment {
            text: text.to_owned(),
            external: false,
        }]
    }
}

starlark_simple_value!(ExternalText);

impl Display for ExternalText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for segment in &self.0 {
            f.write_str(&segment.text)?;
        }
        Ok(())
    }
}

#[starlark_value(type = "external")]
impl<'v> StarlarkValue<'v> for ExternalText {
    /// `external + "text"`. Nothing else is offered: a string method on external text would
    /// hand back a plain string and quietly lose the fact that it came from outside.
    fn add(&self, rhs: Value<'v>, heap: Heap<'v>) -> Option<starlark::Result<Value<'v>>> {
        let after = match rhs.unpack_str() {
            Some(text) => ExternalText::plain(text),
            None => ExternalText::from_value(rhs)?.0.clone(),
        };
        Some(Ok(heap.alloc(ExternalText::joined(&self.0, &after))))
    }

    /// `"text" + external`.
    fn radd(&self, lhs: Value<'v>, heap: Heap<'v>) -> Option<starlark::Result<Value<'v>>> {
        let before = ExternalText::plain(lhs.unpack_str()?);
        Some(Ok(heap.alloc(ExternalText::joined(&before, &self.0))))
    }
}

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

/// Where an `approve` task's resolution comes from: `github_pr(...)` or `jira(...)`.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct GateSourceValue(#[allocative(skip)] pub(crate) ApprovalSourceSpec);

starlark_simple_value!(GateSourceValue);

impl Display for GateSourceValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            ApprovalSourceSpec::Native => write!(f, "native"),
            ApprovalSourceSpec::GithubPr { .. } => write!(f, "github_pr(...)"),
            ApprovalSourceSpec::Jira { .. } => write!(f, "jira(...)"),
        }
    }
}

#[starlark_value(type = "approval_source")]
impl<'v> StarlarkValue<'v> for GateSourceValue {}

/// What a Jira issue must reach: `status("...")` or `label("...")`.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct UntilValue(#[allocative(skip)] pub(crate) JiraUntil);

starlark_simple_value!(UntilValue);

impl Display for UntilValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            JiraUntil::Status(s) => write!(f, "status({s})"),
            JiraUntil::Label(l) => write!(f, "label({l})"),
        }
    }
}

#[starlark_value(type = "until")]
impl<'v> StarlarkValue<'v> for UntilValue {}
