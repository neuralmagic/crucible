//! The opaque starlark values the DSL constructors hand back. Each wraps owned, immutable
//! compiler data, and nothing here reaches into the compiler except to refuse a call: a workflow
//! can only pass these along to another constructor.
//!
//! A task has exactly one attribute shape, `task.field`, which names one of the fields that task
//! declared it emits and yields another opaque value. It exists because `over = discover.targets`
//! is the clearest way to say what a fan-out runs over, and because naming the field in the
//! source is what lets a typo be a compile error rather than an empty list at run time.

use std::collections::BTreeMap;
use std::fmt::{self, Display};

use allocative::Allocative;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_simple_value;
use starlark::values::{
    Heap, NoSerialize, ProvidesStaticType, StarlarkValue, Value, starlark_value,
};

use crate::plan::diag;
use crucible_contract::decision::{CompiledQuestion, QuestionId};
use crucible_contract::emits::{FieldType, JsonSchema};

use crate::crucible::Direction;
use crate::duration::TaskTimeout;
use crate::knobs::{Harness, ReasoningEffort};
use crate::plan::ir::{
    Decider, Declared, Emits, OutputField, OutputRef, Revise, Task, TaskKind, TaskName,
};
use crate::plan::starlark::error::CompileError;
use crate::plan::workflow::WorkflowCfg;

/// A `session(...)` declaration: a durable conversation name plus optional agent
/// defaults that materialize onto the agent tasks bound to it.
#[derive(Clone, Debug)]
pub(crate) struct SessionDecl {
    pub(crate) name: String,
    pub(crate) harness: Option<Harness>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<ReasoningEffort>,
}

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
        if let TaskKind::Route { questions, .. } = &self.0.task {
            let asked = QuestionId::new(attribute)
                .ok()
                .and_then(|id| questions.get_key_value(&id));
            return Some(match asked {
                Some((question, asked)) if asked.pick_source().is_some() => {
                    heap.alloc(OutputRefValue {
                        reference: OutputRef {
                            task: self.0.name.clone(),
                            field: OutputField(question.to_string()),
                        },
                        declared: Emits::Typed(BTreeMap::from([(
                            OutputField(question.to_string()),
                            FieldType::List,
                        )])),
                    })
                }
                Some((question, asked)) => heap.alloc(AnswerRefValue(AnswerRef {
                    task: self.0.name.clone(),
                    question: question.clone(),
                    asked: asked.clone(),
                })),
                None => heap.alloc(OutputRefValue {
                    reference: OutputRef {
                        task: self.0.name.clone(),
                        field: OutputField(attribute.to_owned()),
                    },
                    declared: Emits::Fields(
                        questions
                            .keys()
                            .map(|id| OutputField(id.to_string()))
                            .collect(),
                    ),
                }),
            });
        }
        Some(heap.alloc(OutputRefValue {
            reference: OutputRef {
                task: self.0.name.clone(),
                field: OutputField(attribute.to_owned()),
            },
            declared: self.0.emits.clone(),
        }))
    }
}

/// `producer.field` once checked against what the producer declares, with the type it declares
/// for the field when it gave one.
#[derive(Clone, Debug)]
pub(crate) struct DeclaredOutput {
    pub(crate) reference: OutputRef,
    pub(crate) ty: Option<FieldType>,
}

/// One task's output field, as `over = producer.field` yields it. `declared` travels with it so
/// the constructor can tell an undeclared field from a declared one without a second lookup.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct OutputRefValue {
    #[allocative(skip)]
    pub(crate) reference: OutputRef,
    #[allocative(skip)]
    pub(crate) declared: Emits,
}

impl OutputRefValue {
    pub(crate) fn resolve(&self) -> Result<DeclaredOutput, CompileError> {
        resolve_output(&self.reference, &self.declared)
    }
}

/// `reference` checked against what its producer declares.
pub(crate) fn resolve_output(
    reference: &OutputRef,
    declared: &Emits,
) -> Result<DeclaredOutput, CompileError> {
    {
        let ty = match declared.field(&reference.field.0) {
            Declared::Untyped => None,
            Declared::Typed(ty) => Some(ty.clone()),
            Declared::Unchecked | Declared::Omitted => {
                let names = declared.names();
                return Err(CompileError::UndeclaredOutputField {
                    task: reference.task.0.clone(),
                    field: reference.field.0.clone(),
                    suggestion: diag::suggest(&reference.field.0, names.iter().map(String::as_str))
                        .map(str::to_owned),
                    declared: names.join(", "),
                });
            }
        };
        Ok(DeclaredOutput {
            reference: reference.clone(),
            ty,
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
impl<'v> StarlarkValue<'v> for OutputRefValue {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        let method = AnswerMethodKind::named(attribute)?;
        Some(heap.alloc(AnswerMethod {
            subject: Subject::Field {
                reference: self.reference.clone(),
                declared: self.declared.clone(),
            },
            method,
        }))
    }
}

/// A `noul(...)`, `choice(...)`, or `score(...)` declaration, consumed by `route(questions = ...)`.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct QuestionValue(#[allocative(skip)] pub(crate) CompiledQuestion);

starlark_simple_value!(QuestionValue);

impl Display for QuestionValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "question({})", self.0.instructions())
    }
}

#[starlark_value(type = "question")]
impl<'v> StarlarkValue<'v> for QuestionValue {}

/// One question of one route, as `when = gate.area` yields it.
#[derive(Clone, Debug)]
pub(crate) struct AnswerRef {
    pub(crate) task: TaskName,
    pub(crate) question: QuestionId,
    pub(crate) asked: CompiledQuestion,
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct AnswerRefValue(#[allocative(skip)] pub(crate) AnswerRef);

starlark_simple_value!(AnswerRefValue);

impl Display for AnswerRefValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.0.task, self.0.question)
    }
}

#[starlark_value(type = "answer")]
impl<'v> StarlarkValue<'v> for AnswerRefValue {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        let method = AnswerMethodKind::named(attribute)?;
        Some(heap.alloc(AnswerMethod {
            subject: Subject::Question(self.0.clone()),
            method,
        }))
    }
}

/// Which answers of a question a `when` runs on.
#[derive(Clone, Debug)]
pub(crate) enum Selection {
    Answers(Vec<String>),
    Otherwise,
    /// `.any_of(...)` was handed something other than strings.
    NotLabels,
}

/// What a `when` branches on: a route's question, or a typed field a task emits.
#[derive(Clone, Debug)]
pub(crate) enum Subject {
    Question(AnswerRef),
    Field {
        reference: OutputRef,
        declared: Emits,
    },
}

impl Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Subject::Question(asked) => write!(f, "{}.{}", asked.task, asked.question),
            Subject::Field { reference, .. } => write!(f, "{reference}"),
        }
    }
}

/// `subject.any_of(...)` or `subject.otherwise()`.
#[derive(Clone, Debug)]
pub(crate) struct WhenSpec {
    pub(crate) subject: Subject,
    pub(crate) selection: Selection,
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct WhenValue(#[allocative(skip)] pub(crate) WhenSpec);

starlark_simple_value!(WhenValue);

impl Display for WhenValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let subject = &self.0.subject;
        match &self.0.selection {
            Selection::Answers(labels) => write!(f, "{subject}.any_of({})", labels.join(", ")),
            Selection::Otherwise => write!(f, "{subject}.otherwise()"),
            Selection::NotLabels => write!(f, "{subject}.any_of(...)"),
        }
    }
}

#[starlark_value(type = "when")]
impl<'v> StarlarkValue<'v> for WhenValue {}

#[derive(Clone, Copy, Debug)]
enum AnswerMethodKind {
    AnyOf,
    Otherwise,
}

impl AnswerMethodKind {
    fn named(attribute: &str) -> Option<Self> {
        match attribute {
            "any_of" => Some(AnswerMethodKind::AnyOf),
            "otherwise" => Some(AnswerMethodKind::Otherwise),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            AnswerMethodKind::AnyOf => "any_of",
            AnswerMethodKind::Otherwise => "otherwise",
        }
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct AnswerMethod {
    #[allocative(skip)]
    subject: Subject,
    #[allocative(skip)]
    method: AnswerMethodKind,
}

starlark_simple_value!(AnswerMethod);

impl Display for AnswerMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.subject, self.method.name())
    }
}

#[starlark_value(type = "answer_method")]
impl<'v> StarlarkValue<'v> for AnswerMethod {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let heap = eval.heap();
        args.no_named_args()?;
        let selection = match self.method {
            AnswerMethodKind::Otherwise => {
                args.no_positional_args(heap)?;
                Selection::Otherwise
            }
            AnswerMethodKind::AnyOf => args
                .positions(heap)?
                .map(|label| label.unpack_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .map_or(Selection::NotLabels, Selection::Answers),
        };
        Ok(heap.alloc(WhenValue(WhenSpec {
            subject: self.subject.clone(),
            selection,
        })))
    }
}

/// `each(producer.field, max = N)`.
#[derive(Clone, Debug)]
pub(crate) struct Each {
    pub(crate) output: DeclaredOutput,
    pub(crate) max: u32,
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct EachValue(#[allocative(skip)] pub(crate) Each);

starlark_simple_value!(EachValue);

impl Display for EachValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "each({}, max = {})", self.0.output.reference, self.0.max)
    }
}

#[starlark_value(type = "each")]
impl<'v> StarlarkValue<'v> for EachValue {}

/// `send_back(tasks, rounds = N)`.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct SendBackValue(#[allocative(skip)] pub(crate) Revise);

starlark_simple_value!(SendBackValue);

impl Display for SendBackValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "send_back(rounds = {})", self.0.max_rounds)
    }
}

#[starlark_value(type = "send_back")]
impl<'v> StarlarkValue<'v> for SendBackValue {}

/// `at_least(n)` or `at_most(n)`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bound {
    pub(crate) threshold: f64,
    pub(crate) direction: Direction,
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct BoundValue(#[allocative(skip)] pub(crate) Bound);

starlark_simple_value!(BoundValue);

impl Display for BoundValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let function = match self.0.direction {
            Direction::Higher => "at_least",
            Direction::Lower => "at_most",
        };
        write!(f, "{function}({})", self.0.threshold)
    }
}

#[starlark_value(type = "bound")]
impl<'v> StarlarkValue<'v> for BoundValue {}

/// `model(...)`, `human(...)`, or `output(task)`, with the timeout a human decider carries.
#[derive(Clone, Debug)]
pub(crate) struct DeciderSpec {
    pub(crate) decider: Decider,
    pub(crate) timeout: Option<TaskTimeout>,
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct DeciderValue(#[allocative(skip)] pub(crate) DeciderSpec);

starlark_simple_value!(DeciderValue);

impl Display for DeciderValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0.decider {
            Decider::Model { .. } => "model(...)",
            Decider::Human { .. } => "human(...)",
            Decider::Output { .. } => "output(...)",
        })
    }
}

#[starlark_value(type = "decider")]
impl<'v> StarlarkValue<'v> for DeciderValue {}

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

/// The character that stands in for external text turned into a plain string.
///
/// `str()`, `repr()`, `%` and `.format()` all reach [`Display`], and starlark gives none of them
/// a way to fail. So the conversion yields this placeholder instead of the value, and a
/// constructor handed a string carrying it refuses the string. A noncharacter survives case
/// changes and is never in text a pack author wrote.
pub(crate) const CONVERTED: char = '\u{FDD0}';

starlark_simple_value!(ExternalText);

impl Display for ExternalText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{CONVERTED}external text{CONVERTED}")
    }
}

#[starlark_value(type = "external")]
impl<'v> StarlarkValue<'v> for ExternalText {
    /// Every attribute is a method that refuses when called. A string method on external text
    /// would hand back a plain string and quietly lose the fact that it came from outside, and
    /// starlark's own missing-attribute error would not say what to write instead.
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        Some(heap.alloc(ExternalMethod(attribute.to_owned())))
    }

    /// `external + "text"`.
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

/// `external.method`, which exists only to refuse being called.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct ExternalMethod(String);

starlark_simple_value!(ExternalMethod);

impl Display for ExternalMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "external.{}", self.0)
    }
}

#[starlark_value(type = "external_method")]
impl<'v> StarlarkValue<'v> for ExternalMethod {
    fn invoke(
        &self,
        _me: Value<'v>,
        _args: &Arguments<'v, '_>,
        _eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Err(starlark::Error::new_native(anyhow::Error::new(
            CompileError::ExternalMethod {
                method: self.0.clone(),
            },
        )))
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

/// `schema_file(path)`: a JSON Schema read from the pack, and the path it was read from.
#[derive(Clone, Debug)]
pub(crate) struct SchemaFile {
    pub(crate) path: String,
    pub(crate) schema: JsonSchema,
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct SchemaValue(#[allocative(skip)] pub(crate) SchemaFile);

starlark_simple_value!(SchemaValue);

impl Display for SchemaValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "schema_file({:?})", self.0.path)
    }
}

#[starlark_value(type = "schema")]
impl<'v> StarlarkValue<'v> for SchemaValue {}

/// `optional(task)`: an entry of `depends_on` whose settling not taken satisfies an `all` join.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct OptionalValue(#[allocative(skip)] pub(crate) TaskName);

starlark_simple_value!(OptionalValue);

impl Display for OptionalValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "optional({})", self.0)
    }
}

#[starlark_value(type = "optional")]
impl<'v> StarlarkValue<'v> for OptionalValue {}

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

/// `others()`: an entry of `depends_on` that stands for every other main-graph task.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct OthersValue;

starlark_simple_value!(OthersValue);

impl Display for OthersValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("others()")
    }
}

#[starlark_value(type = "others")]
impl<'v> StarlarkValue<'v> for OthersValue {}

/// `task_defaults(...)`: what a task takes for an argument it does not set.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TaskDefaults {
    pub(crate) required: Option<bool>,
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct TaskDefaultsValue(#[allocative(skip)] pub(crate) TaskDefaults);

starlark_simple_value!(TaskDefaultsValue);

impl Display for TaskDefaultsValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("task_defaults(...)")
    }
}

#[starlark_value(type = "task_defaults")]
impl<'v> StarlarkValue<'v> for TaskDefaultsValue {}

/// What a constructor hands back once its error is recorded, so evaluation goes on and later
/// errors are found too. Anything read off it, called on it, or added to it is the same value.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct PoisonValue;

starlark_simple_value!(PoisonValue);

impl Display for PoisonValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<error>")
    }
}

#[starlark_value(type = "error")]
impl<'v> StarlarkValue<'v> for PoisonValue {
    fn get_attr(&self, _attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        Some(heap.alloc(PoisonValue))
    }

    fn invoke(
        &self,
        _me: Value<'v>,
        _args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(PoisonValue))
    }

    fn add(&self, _rhs: Value<'v>, heap: Heap<'v>) -> Option<starlark::Result<Value<'v>>> {
        Some(Ok(heap.alloc(PoisonValue)))
    }

    fn radd(&self, _lhs: Value<'v>, heap: Heap<'v>) -> Option<starlark::Result<Value<'v>>> {
        Some(Ok(heap.alloc(PoisonValue)))
    }
}

/// A task whose constructor failed, kept by name so the tasks that depend on it are still
/// checked. Its fields are [`PoisonValue`].
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct BrokenTask(#[allocative(skip)] pub(crate) TaskName);

starlark_simple_value!(BrokenTask);

impl Display for BrokenTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "task({})", self.0)
    }
}

#[starlark_value(type = "task")]
impl<'v> StarlarkValue<'v> for BrokenTask {
    fn get_attr(&self, _attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        Some(heap.alloc(PoisonValue))
    }
}
