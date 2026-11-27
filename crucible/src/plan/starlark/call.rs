//! `call(...)`, `budget(...)` and `item()`: a task that dispatches a `[calls]` target as its own
//! run, compiled against the pack's `calls.lock` alone.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display};

use allocative::Allocative;
use starlark::starlark_simple_value;
use starlark::values::{Heap, NoSerialize, ProvidesStaticType, StarlarkValue, starlark_value};

use crate::plan::diag;
use crate::plan::ir::Binding;
use crate::plan::ir::{Emits, OutputField, Task, TaskKind};
use crate::plan::starlark::error::CompileError;
use crate::plan::starlark::params::{ParamType, embedded, embedded_schema};
use crate::plan::starlark::targets;
use crate::plan::starlark::values::{CONVERTED, ExternalText, Segment};
use crate::plan::starlark::{
    CompileState, Result, Value, check_fanout, imply_dependencies, note_explicit_required,
    parse_join, parse_stage, take_bool_default, take_declared_name, take_dependencies, take_fanout,
    take_string_default, take_when, wrong_type,
};
use crucible_contract::calls::{
    CallBudget, CallKind, MAX_CALL_FANOUT, TargetInterface, WATCH_APPEAR_PARAM, WATCH_COMMIT_PARAM,
};
use crucible_contract::emits::{FieldType, JsonSchema};

/// `param(name)` of a json or file parameter. Its value reaches a command under `params`, a
/// call's target as a binding, and an agent only inside the external-input markers, never as
/// text in the source.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct ParamRefValue {
    pub(crate) name: String,
    #[allocative(skip)]
    pub(crate) ty: ParamType,
    pub(crate) shown: String,
}

impl ParamRefValue {
    fn segments(&self) -> Vec<Segment> {
        vec![Segment {
            text: self.shown.clone(),
            external: true,
        }]
    }
}

starlark_simple_value!(ParamRefValue);

impl Display for ParamRefValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{CONVERTED}param({:?}){CONVERTED}", self.name)
    }
}

#[starlark_value(type = "param")]
impl<'v> StarlarkValue<'v> for ParamRefValue {
    fn add(
        &self,
        rhs: starlark::values::Value<'v>,
        heap: Heap<'v>,
    ) -> Option<starlark::Result<starlark::values::Value<'v>>> {
        let after = match rhs.unpack_str() {
            Some(text) => ExternalText::plain(text),
            None => ExternalText::from_value(rhs)?.0.clone(),
        };
        Some(Ok(
            heap.alloc(ExternalText::joined(&self.segments(), &after))
        ))
    }

    fn radd(
        &self,
        lhs: starlark::values::Value<'v>,
        heap: Heap<'v>,
    ) -> Option<starlark::Result<starlark::values::Value<'v>>> {
        let before = match lhs.unpack_str() {
            Some(text) => ExternalText::plain(text),
            None => ExternalText::from_value(lhs)?.0.clone(),
        };
        Some(Ok(
            heap.alloc(ExternalText::joined(&before, &self.segments()))
        ))
    }
}

pub(crate) fn param_text(name: &str, ty: ParamType, value: &serde_json::Value) -> String {
    if ty != ParamType::File {
        return serde_json::to_string_pretty(value).unwrap_or_default();
    }
    let path = value
        .get("path")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let mut text = format!(
        "file parameter {name}, staged read-only at `{}`",
        crate::plan::param_files::input_path(name, std::path::Path::new(path))
    );
    for (key, unit) in [("size", " bytes"), ("media_type", ""), ("digest", "")] {
        match value.get(key) {
            Some(serde_json::Value::Number(n)) => text.push_str(&format!(", {n}{unit}")),
            Some(serde_json::Value::String(s)) => text.push_str(&format!(", {s}{unit}")),
            _ => {}
        }
    }
    text
}

/// `item()`: a mapped call's own item.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct ItemValue;

starlark_simple_value!(ItemValue);

impl Display for ItemValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{CONVERTED}item(){CONVERTED}")
    }
}

#[starlark_value(type = "item")]
impl<'v> StarlarkValue<'v> for ItemValue {}

/// `budget(cost = ..., wall_clock = ...)`.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct BudgetValue(#[allocative(skip)] pub(crate) CallBudget);

starlark_simple_value!(BudgetValue);

impl Display for BudgetValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("budget(...)")
    }
}

#[starlark_value(type = "budget")]
impl<'v> StarlarkValue<'v> for BudgetValue {}

/// A value an `@crucible//calls` constructor or enum built.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct TargetValue(#[allocative(skip)] pub(crate) targets::Built);

starlark_simple_value!(TargetValue);

impl Display for TargetValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match targets::ENUMS.iter().find(|(name, _)| *name == self.0.ctor) {
            Some((name, _)) => write!(f, "{name}.{}", self.0.json.as_str().unwrap_or_default()),
            None => write!(f, "{}(...)", self.0.ctor),
        }
    }
}

#[starlark_value(type = "call_target")]
impl<'v> StarlarkValue<'v> for TargetValue {}

/// An `@crucible//calls` enum, whose members are its attributes.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
pub(crate) struct EnumValue(#[allocative(skip)] pub(crate) &'static str);

starlark_simple_value!(EnumValue);

impl Display for EnumValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

#[starlark_value(type = "enum")]
impl<'v> StarlarkValue<'v> for EnumValue {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<starlark::values::Value<'v>> {
        targets::member(self.0, attribute)
            .ok()
            .map(|built| heap.alloc(TargetValue(built)))
    }

    fn dir_attr(&self) -> Vec<String> {
        targets::ENUMS
            .iter()
            .find(|(name, _)| *name == self.0)
            .map(|(_, members)| members.iter().map(|m| (*m).to_owned()).collect())
            .unwrap_or_default()
    }
}

fn arg(value: Value) -> Result<targets::Arg> {
    let plain = |value| {
        json(value).ok_or_else(|| CompileError::CallTargetNotLiteral {
            detail: "target arguments are built from dictionaries, lists, strings, numbers, \
                     booleans and values from @crucible//calls"
                .to_owned(),
        })
    };
    match value {
        Value::Target(built) => Ok(targets::Arg::Part(built.ctor, built.json)),
        Value::List(items) => items
            .into_iter()
            .map(arg)
            .collect::<Result<Vec<_>>>()
            .map(targets::Arg::List),
        other => plain(other).map(targets::Arg::Json),
    }
}

fn json(value: Value) -> Option<serde_json::Value> {
    Some(match value {
        Value::None => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(b),
        Value::Int(n) => serde_json::Value::from(n),
        Value::Float(f) => serde_json::Value::Number(serde_json::Number::from_f64(f)?),
        Value::String(s) => serde_json::Value::String(s),
        Value::List(items) => {
            serde_json::Value::Array(items.into_iter().map(json).collect::<Option<_>>()?)
        }
        Value::Map(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(k, v)| json(v).map(|v| (k, v)))
                .collect::<Option<_>>()?,
        ),
        Value::Ordered(pairs) => serde_json::Value::Object(
            pairs
                .into_iter()
                .map(|(k, v)| json(v).map(|v| (k, v)))
                .collect::<Option<_>>()?,
        ),
        _ => return None,
    })
}

pub(crate) fn construct(ctor: &'static str, named: BTreeMap<String, Value>) -> Result<Value> {
    let named = named
        .into_iter()
        .map(|(name, value)| arg(value).map(|value| (name, value)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    targets::build(ctor, named).map(Value::Target)
}

pub(crate) fn budget(named: &mut BTreeMap<String, Value>) -> Result<Value> {
    let invalid = |detail: &str| CompileError::BudgetInvalid {
        detail: detail.to_owned(),
    };
    let cost_usd = match named.remove("cost") {
        None | Some(Value::None) => None,
        Some(Value::Int(n)) if n > 0 => Some(f64::from(n)),
        Some(Value::Float(n)) if n.is_finite() && n > 0.0 => Some(n),
        Some(_) => return Err(invalid("cost must be a positive number of dollars")),
    };
    let wall_clock_secs = match named.remove("wall_clock") {
        None | Some(Value::None) => None,
        Some(Value::String(raw)) => match crate::duration::parse_duration(&raw) {
            Some(d) if !d.is_zero() => Some(d.as_secs().max(1)),
            _ => return Err(invalid("wall_clock must be a duration such as \"30m\"")),
        },
        Some(_) => return Err(invalid("wall_clock must be a duration such as \"30m\"")),
    };
    if cost_usd.is_none() && wall_clock_secs.is_none() {
        return Err(invalid("set cost, wall_clock, or both"));
    }
    Ok(Value::Budget(CallBudget {
        cost_usd,
        wall_clock_secs,
    }))
}

/// One target parameter as the lock's interface declares it.
struct TargetParam {
    ty: ParamType,
    required: bool,
    schema: Option<serde_json::Value>,
}

fn target_params(interface: &TargetInterface) -> BTreeMap<String, TargetParam> {
    let required: BTreeSet<&str> = interface
        .params
        .get("required")
        .and_then(serde_json::Value::as_array)
        .map(|names| names.iter().filter_map(serde_json::Value::as_str).collect())
        .unwrap_or_default();
    interface
        .params
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .map(|properties| {
            properties
                .iter()
                .map(|(name, property)| {
                    (
                        name.clone(),
                        TargetParam {
                            ty: ParamType::from_schema(property),
                            required: required.contains(name.as_str()),
                            schema: embedded_schema(property).cloned(),
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn result_emits(interface: &TargetInterface) -> Emits {
    let typed: Option<BTreeMap<OutputField, FieldType>> = interface
        .result
        .iter()
        .map(|wire| Some((OutputField(wire.field.clone()), wire.ty.clone()?)))
        .collect();
    match typed {
        Some(fields) if !fields.is_empty() => Emits::Typed(fields),
        _ => Emits::Fields(
            interface
                .result
                .iter()
                .map(|wire| OutputField(wire.field.clone()))
                .collect(),
        ),
    }
}

/// Whether a call task starts its target's run or follows the runs a commit started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Door {
    Call,
    Watch,
}

pub(crate) fn watch_task(
    named: &mut BTreeMap<String, Value>,
    state: &CompileState,
) -> Result<Task> {
    for taken in ["params", "budget", "over", "max_fanout"] {
        if named.contains_key(taken) {
            return Err(CompileError::BudgetInvalid {
                detail: format!("watch() takes no {taken}"),
            });
        }
    }
    let commit = named
        .remove("commit")
        .ok_or_else(|| CompileError::MissingArgument {
            argument: "commit".to_owned(),
        })?;
    let timeout = named
        .remove("timeout")
        .ok_or_else(|| CompileError::MissingArgument {
            argument: "timeout".to_owned(),
        })?;
    let mut bound = BTreeMap::from([(WATCH_COMMIT_PARAM.to_owned(), commit)]);
    if let Some(appear) = named.remove("appear").filter(|v| !matches!(v, Value::None)) {
        bound.insert(WATCH_APPEAR_PARAM.to_owned(), appear);
    }
    let mut limits = BTreeMap::from([("wall_clock".to_owned(), timeout)]);
    let budget = budget(&mut limits).map_err(|_| CompileError::BudgetInvalid {
        detail: "watch timeout must be a duration such as \"30m\"".to_owned(),
    })?;
    named.insert("params".to_owned(), Value::Map(bound));
    named.insert("budget".to_owned(), budget);
    task(named, state, Door::Watch)
}

pub(crate) fn call_task(named: &mut BTreeMap<String, Value>, state: &CompileState) -> Result<Task> {
    task(named, state, Door::Call)
}

fn task(named: &mut BTreeMap<String, Value>, state: &CompileState, door: Door) -> Result<Task> {
    let name = take_declared_name(named, "name")?;
    let built = match named.remove("target") {
        Some(Value::Target(built)) => built,
        None => {
            return Err(CompileError::MissingArgument {
                argument: "target".to_owned(),
            });
        }
        Some(_) => {
            return Err(wrong_type(
                "target",
                "a target from @crucible//calls, such as gitlab_pipeline(...)",
            ));
        }
    };
    let shown = built.ctor.to_owned();
    let refused = |detail: String| CompileError::CallTarget {
        task: name.0.clone(),
        target: shown.clone(),
        detail,
    };
    let target = built.target().unwrap_or_else(|| {
        Err(format!(
            "{} builds part of a target, not a target",
            built.ctor
        ))
    });
    let target = target.map_err(&refused)?;
    let alias = targets::alias_of(&state.context_mut().declared_calls, &name.0, &target)
        .ok_or_else(|| {
            refused(
                "this target is not declared in workflow.star; write it in the call there or \
                 bind it to a top-level name"
                    .to_owned(),
            )
        })?;
    let locked = state.context_mut().locked_call(&alias).map_err(refused)?;
    match (door, locked.kind == CallKind::Tekton) {
        (Door::Call, true) => {
            return Err(refused(
                "a tekton target follows the runs a commit started; use watch()".to_owned(),
            ));
        }
        (Door::Watch, false) => {
            return Err(refused(format!(
                "a {} target starts its own run; use call()",
                locked.kind
            )));
        }
        _ => {}
    }
    let interface = &locked.interface;
    let declared = target_params(interface);
    if matches!(
        locked.kind,
        CallKind::GithubWorkflow | CallKind::GitlabPipeline
    ) && let Some((param, _)) = declared.iter().find(|(_, p)| p.ty == ParamType::File)
    {
        return Err(refused(format!(
            "the lock declares {param:?} a file parameter, which a {} input cannot take",
            locked.kind.presentation().name
        )));
    }
    let budget = match named.remove("budget") {
        None | Some(Value::None) => None,
        Some(Value::Budget(budget)) => Some(budget),
        Some(_) => return Err(wrong_type("budget", "budget(cost = ..., wall_clock = ...)")),
    };
    let bound = match named.remove("params") {
        None | Some(Value::None) => BTreeMap::new(),
        Some(Value::Map(bound)) => bound,
        Some(_) => return Err(wrong_type("params", "a dict from parameter name to value")),
    };
    let when = take_when(named, state, &name)?;
    let (mut depends_on, optional) = take_dependencies(named, state, &name)?;
    let (over, max_fanout) = take_fanout(named)?;
    if let Some(width) = max_fanout.filter(|width| *width > MAX_CALL_FANOUT) {
        return Err(CompileError::CallFanout {
            task: name.0.clone(),
            got: width,
            max: MAX_CALL_FANOUT,
        });
    }
    note_explicit_required(named, state, &name);
    let required = take_bool_default(named, "required", true)?;
    let join = parse_join(&take_string_default(named, "join", "all")?)?;
    let stage = parse_stage(&take_string_default(named, "stage", "iteration")?)?;

    let mut bindings = BTreeMap::new();
    let mut file_params = BTreeSet::new();
    for (param, value) in bound {
        let Some(wanted) = declared.get(&param) else {
            return Err(CompileError::CallBinding {
                task: name.0.clone(),
                param: param.clone(),
                detail: format!(
                    "target {alias} declares no such parameter{}",
                    diag::suggest(&param, declared.keys().map(String::as_str))
                        .map(|s| format!("; did you mean {s:?}?"))
                        .unwrap_or_default()
                ),
            });
        };
        let binding = bind(wanted, value, over.is_some(), locked.kind).map_err(|detail| {
            CompileError::CallBinding {
                task: name.0.clone(),
                param: param.clone(),
                detail,
            }
        })?;
        if wanted.ty == ParamType::File {
            file_params.insert(param.clone());
        }
        if let Some(producer) = binding.producer()
            && !depends_on.contains(producer)
        {
            depends_on.push(producer.clone());
        }
        bindings.insert(param, binding);
    }
    if let Some((param, _)) = declared
        .iter()
        .find(|(param, p)| p.required && !bindings.contains_key(*param))
    {
        return Err(CompileError::CallBinding {
            task: name.0.clone(),
            param: param.clone(),
            detail: format!("target {alias} requires it and params does not bind it"),
        });
    }
    let mut task = Task {
        task: TaskKind::Call {
            alias,
            target: locked.kind,
            bindings,
            budget,
            interface_digest: locked.interface_digest.clone(),
            file_params,
        },
        depends_on,
        optional,
        session: None,
        needs: "any".to_owned(),
        required,
        isolation: None,
        join,
        stage,
        emits: result_emits(interface),
        emits_files: interface.result_files.clone(),
        over,
        max_fanout,
        keyed: Vec::new(),
        when,
        revise: None,
        timeout: None,
        history: None,
        repair: 0,
        name,
    };
    imply_dependencies(&mut task);
    check_fanout(&task)?;
    Ok(task)
}

fn bind(
    wanted: &TargetParam,
    value: Value,
    mapped: bool,
    kind: CallKind,
) -> std::result::Result<Binding, String> {
    let expected = wanted.ty.as_str();
    match value {
        Value::Output(output) => {
            let Some(ty) = output.ty else {
                return Err(format!(
                    "{} declares no type for the field; type it in emits",
                    output.reference
                ));
            };
            field_admits(wanted, &ty, kind)?;
            Ok(Binding::Field {
                producer: output.reference.task,
                field: output.reference.field,
            })
        }
        Value::CallFile(file) => {
            if wanted.ty != ParamType::File {
                return Err(format!(
                    "a file is bound to a {expected} parameter; only a file parameter takes one"
                ));
            }
            if !file.declared {
                return Err(format!(
                    "{} does not declare {:?} in emits_files",
                    file.producer, file.path
                ));
            }
            Ok(Binding::File {
                producer: file.producer,
                path: file.path,
            })
        }
        Value::ParamRef { name, ty, .. } if ty == wanted.ty => Ok(Binding::Param { name }),
        Value::ParamRef {
            name,
            ty: ParamType::Json,
            ..
        } if kind == CallKind::GithubWorkflow && wanted.ty == ParamType::String => {
            Ok(Binding::Param { name })
        }
        Value::ParamRef { name, ty, .. } => Err(format!(
            "param({name:?}) is a {} parameter, not a {expected} one",
            ty.as_str()
        )),
        Value::Item if !mapped => Err("item() binds only in a call mapped with over".to_owned()),
        Value::Item if wanted.ty == ParamType::String => Ok(Binding::Item { field: None }),
        Value::Item => Err(format!(
            "item() is a string and the parameter is a {expected}"
        )),
        literal => {
            if wanted.ty == ParamType::File {
                return Err("a file parameter takes producer.files[path] or param(...)".into());
            }
            let value = literal_json(&literal)
                .ok_or_else(|| "the value is not a literal a call can send".to_owned())?;
            literal_admits(wanted, &value)?;
            Ok(Binding::Literal { value })
        }
    }
}

fn field_admits(
    wanted: &TargetParam,
    ty: &FieldType,
    kind: CallKind,
) -> std::result::Result<(), String> {
    let fits = match (wanted.ty, ty) {
        (ParamType::String, FieldType::Object | FieldType::List | FieldType::Schema(_))
            if kind == CallKind::GithubWorkflow =>
        {
            true
        }
        (ParamType::Json, FieldType::Schema(schema)) => {
            return match &wanted.schema {
                Some(param) if *param != embedded(schema) => Err(
                    "the field's schema differs from the parameter's; they must be equal".into(),
                ),
                _ => Ok(()),
            };
        }
        (_, FieldType::Schema(_)) => false,
        (ParamType::Json, FieldType::Object | FieldType::List) => wanted.schema.is_none(),
        (ParamType::String, FieldType::String | FieldType::Link | FieldType::OneOf(_)) => true,
        (ParamType::Int, FieldType::Integer) => true,
        (ParamType::Number, FieldType::Integer | FieldType::Number) => true,
        (ParamType::Bool, FieldType::Boolean) => true,
        (ParamType::StringList, FieldType::Links | FieldType::List) => true,
        _ => false,
    };
    if fits {
        return Ok(());
    }
    Err(format!(
        "a {} field cannot feed a {} parameter",
        field_type_name(ty),
        wanted.ty.as_str()
    ))
}

fn field_type_name(ty: &FieldType) -> &'static str {
    match ty {
        FieldType::String => "string",
        FieldType::Integer => "integer",
        FieldType::Number => "number",
        FieldType::Boolean => "boolean",
        FieldType::List => "list",
        FieldType::Object => "object",
        FieldType::Link => "link",
        FieldType::Links => "links",
        FieldType::OneOf(_) => "labelled string",
        FieldType::Schema(_) => "schema-typed",
    }
}

fn literal_admits(
    wanted: &TargetParam,
    value: &serde_json::Value,
) -> std::result::Result<(), String> {
    use serde_json::Value as Json;
    let fits = match (wanted.ty, value) {
        (ParamType::String, Json::String(_)) | (ParamType::Bool, Json::Bool(_)) => true,
        (ParamType::Int, Json::Number(n)) => n.is_i64(),
        (ParamType::Number, Json::Number(_)) => true,
        (ParamType::StringList, Json::Array(items)) => items.iter().all(Json::is_string),
        (ParamType::Json, Json::Object(_) | Json::Array(_)) => {
            if let Some(schema) = &wanted.schema
                && let Ok(schema) = JsonSchema::new(schema.clone())
                && let Some(why) = schema.refusal(value)
            {
                return Err(format!(
                    "the literal does not match the parameter's schema: {why}"
                ));
            }
            true
        }
        _ => false,
    };
    if fits {
        Ok(())
    } else {
        Err(format!("{value} is not a {}", wanted.ty.as_str()))
    }
}

fn literal_json(value: &Value) -> Option<serde_json::Value> {
    Some(match value {
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(n) => serde_json::json!(n),
        Value::Float(n) => serde_json::json!(n),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::External(segments) => serde_json::Value::String(
            segments
                .iter()
                .map(|segment| segment.text.as_str())
                .collect(),
        ),
        Value::List(items) => {
            serde_json::Value::Array(items.iter().map(literal_json).collect::<Option<_>>()?)
        }
        Value::Map(entries) => serde_json::Value::Object(
            entries
                .iter()
                .map(|(k, v)| Some((k.clone(), literal_json(v)?)))
                .collect::<Option<_>>()?,
        ),
        _ => return None,
    })
}

pub(crate) fn refuse_param_refs(named: &BTreeMap<String, Value>, prompts: &[&str]) -> Result<()> {
    fn find(value: &Value) -> Option<&str> {
        match value {
            Value::ParamRef { name, .. } => Some(name),
            Value::List(items) => items.iter().find_map(find),
            Value::Map(entries) => entries.values().find_map(find),
            Value::Ordered(entries) => entries.iter().find_map(|(_, v)| find(v)),
            _ => None,
        }
    }
    match named
        .iter()
        .filter(|(argument, _)| !prompts.contains(&argument.as_str()))
        .find_map(|(_, value)| find(value))
    {
        Some(name) => Err(CompileError::ParamNotAValue {
            name: name.to_owned(),
        }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use crate::plan::ir::Binding;
    use crate::plan::ir::{Emits, OutputField, TaskKind, TaskName};
    use crate::plan::starlark::params::PARAM_TYPE_KEY as TYPE_KEY;
    use crate::plan::starlark::{compile_source, compile_source_with};
    use crucible_contract::calls::{
        CallAlias, CallsLock, GitRefName, GithubRepo, LOCK_FILE, LOCK_VERSION, LockedCall, Pin,
        ProgressGraph, TargetInterface, WorkflowPath,
    };
    use crucible_contract::decision::Label;
    use crucible_contract::emits::{DeclaredFile, EmitWire, FieldType};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::path::Path;

    fn interface() -> TargetInterface {
        TargetInterface {
            params: json!({
                "type": "object",
                "properties": {
                    "repo": {"type": "string"},
                    "count": {"type": "integer"},
                    "matrix": {"type": ["object", "array"], TYPE_KEY: "json"},
                    "notes": {"type": "object", TYPE_KEY: "file", "x-max-bytes": 1024},
                    "dry_run": {"type": "boolean"},
                },
                "required": ["repo"],
            }),
            result: vec![
                EmitWire {
                    field: "verdict".into(),
                    ty: Some(FieldType::OneOf(vec![
                        Label::new("ok".to_owned()).unwrap(),
                        Label::new("bad".to_owned()).unwrap(),
                    ])),
                },
                EmitWire {
                    field: "score".into(),
                    ty: Some(FieldType::Number),
                },
            ],
            result_files: vec![DeclaredFile::from("report.md")],
            graph: ProgressGraph::default(),
        }
    }

    fn github_interface(file: bool) -> TargetInterface {
        let mut properties = json!({"version": {"type": "string"}});
        if file {
            properties["blob"] = json!({"type": "object", TYPE_KEY: "file"});
        }
        TargetInterface {
            params: json!({"type": "object", "properties": properties, "required": []}),
            result: vec![EmitWire {
                field: "version".into(),
                ty: Some(FieldType::String),
            }],
            result_files: vec![],
            graph: ProgressGraph::default(),
        }
    }

    fn pack(github_file: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let lock = CallsLock {
            version: LOCK_VERSION,
            calls: BTreeMap::from([
                (
                    CallAlias::parse("triage").unwrap(),
                    LockedCall::new(
                        Pin::Revision {
                            playbook: "triage".into(),
                            digest: "sha256:aa".into(),
                        },
                        interface(),
                    )
                    .unwrap(),
                ),
                (
                    CallAlias::parse("release").unwrap(),
                    LockedCall::new(
                        Pin::GithubRef {
                            repo: GithubRepo::parse("acme/app").unwrap(),
                            workflow: WorkflowPath::parse(".github/workflows/release.yml").unwrap(),
                            git_ref: GitRefName::parse("main").unwrap(),
                            commit: "abc".into(),
                        },
                        github_interface(github_file),
                    )
                    .unwrap(),
                ),
            ]),
        };
        std::fs::write(dir.path().join(LOCK_FILE), lock.render().unwrap()).unwrap();
        dir
    }

    const CALLS: &str = r#"load("@crucible//calls", "artifact_outputs", "github_workflow", "playbook")

triage = playbook("triage")
release = github_workflow(
    repo = "acme/app",
    workflow = ".github/workflows/release.yml",
    ref = "main",
    outputs = artifact_outputs(artifact = "result"),
)
"#;

    const PLAN: &str = r#"
plan = command(
    name = "plan",
    run = "true",
    emits = {"repo": "string", "n": "integer", "items": "list", "conf": "object", "ok": "boolean"},
    emits_files = ["notes.md"],
)
"#;

    fn compile(pack: &Path, body: &str) -> Result<crate::plan::workflow::WorkflowCfg, String> {
        let source = format!("{CALLS}{PLAN}\n{body}");
        compile_source(&source, Path::new("workflow.star"), pack)
            .map(|c| c.workflow)
            .map_err(|e| e.message())
    }

    fn the_call(workflow: &crate::plan::workflow::WorkflowCfg) -> &crate::plan::ir::Task {
        workflow
            .tasks
            .iter()
            .find(|t| matches!(t.task, TaskKind::Call { .. }))
            .unwrap()
    }

    #[test]
    fn a_call_compiles_its_bindings_and_takes_the_target_result_as_its_output() {
        let pack = pack(false);
        let workflow = compile(
            pack.path(),
            r#"
t = call(
    name = "triage",
    target = triage,
    params = {
        "repo": plan.repo,
        "count": plan.n,
        "matrix": {"os": ["linux"]},
        "notes": plan.files["notes.md"],
        "dry_run": True,
    },
    budget = budget(cost = 2, wall_clock = "30m"),
)
after = command(name = "after", run = "true", depends_on = [t], when = t.verdict.any_of("ok"))
workflow(type = "playbook", tasks = [plan, t, after])
"#,
        )
        .unwrap();
        let call = the_call(&workflow);
        let TaskKind::Call {
            alias,
            bindings,
            budget,
            interface_digest,
            file_params,
            ..
        } = &call.task
        else {
            panic!()
        };
        assert_eq!(alias.as_str(), "triage");
        assert_eq!(interface_digest, &interface().digest().unwrap());
        assert_eq!(
            bindings["repo"],
            Binding::Field {
                producer: TaskName("plan".into()),
                field: OutputField("repo".into())
            }
        );
        assert_eq!(
            bindings["notes"],
            Binding::File {
                producer: TaskName("plan".into()),
                path: "notes.md".into()
            }
        );
        assert_eq!(
            bindings["matrix"],
            Binding::Literal {
                value: json!({"os": ["linux"]})
            }
        );
        assert_eq!(file_params.iter().collect::<Vec<_>>(), ["notes"]);
        assert_eq!(budget.and_then(|b| b.wall_clock_secs), Some(1800));
        assert_eq!(budget.and_then(|b| b.cost_usd), Some(2.0));
        assert_eq!(call.depends_on, [TaskName("plan".into())]);
        assert!(matches!(&call.emits, Emits::Typed(fields) if fields.len() == 2));
        assert_eq!(call.emits_files, [DeclaredFile::from("report.md")]);
    }

    #[test]
    fn a_call_refuses_what_the_lock_does_not_admit() {
        let pack = pack(false);
        let refused = |params: &str| {
            compile(
                pack.path(),
                &format!(
                    "t = call(name = \"t\", target = triage, params = {params})\n\
                     workflow(type = \"playbook\", tasks = [plan, t])"
                ),
            )
            .unwrap_err()
        };
        for (params, says) in [
            ("{}", "requires it"),
            (r#"{"repo": "a", "rpo": "b"}"#, "did you mean \"repo\""),
            (r#"{"repo": plan.n}"#, "integer field cannot feed a string"),
            (r#"{"repo": 3}"#, "3 is not a string"),
            (r#"{"repo": "a", "count": plan.repo}"#, "string field"),
            (r#"{"repo": "a", "notes": plan.repo}"#, "string field"),
            (
                r#"{"repo": plan.files["notes.md"]}"#,
                "only a file parameter",
            ),
            (
                r#"{"repo": "a", "notes": plan.files["x.md"]}"#,
                "does not declare",
            ),
            (r#"{"repo": "a", "notes": "x.md"}"#, "file parameter takes"),
            (r#"{"repo": "a", "matrix": plan.repo}"#, "string field"),
            (r#"{"repo": item()}"#, "mapped with over"),
        ] {
            let error = refused(params);
            assert!(error.contains(says), "{params}: {error}");
        }
        let named = compile(
            pack.path(),
            "t = call(name = \"t\", target = \"triage\", params = {})\n\
             workflow(type = \"playbook\", tasks = [plan, t])",
        )
        .unwrap_err();
        assert!(named.contains("@crucible//calls"), "{named}");
        let unpinned = compile(
            pack.path(),
            "t = call(name = \"t\", target = playbook(\"other\"), params = {})\n\
             workflow(type = \"playbook\", tasks = [plan, t])",
        )
        .unwrap_err();
        assert!(unpinned.contains("calls.lock"), "{unpinned}");
        assert!(unpinned.contains("t"), "{unpinned}");
    }

    #[test]
    fn a_call_without_a_lock_is_refused() {
        let pack = tempfile::tempdir().unwrap();
        let error = compile(
            pack.path(),
            "t = call(name = \"t\", target = triage, params = {})\n\
             workflow(type = \"playbook\", tasks = [plan, t])",
        )
        .unwrap_err();
        assert!(error.contains("calls.lock"), "{error}");
    }

    #[test]
    fn a_github_target_never_takes_a_file() {
        let error = compile(
            pack(true).path(),
            "t = call(name = \"t\", target = release, params = {})\n\
             workflow(type = \"playbook\", tasks = [plan, t])",
        )
        .unwrap_err();
        assert!(error.contains("GitHub Actions input"), "{error}");
        let ok = compile(
            pack(false).path(),
            "t = call(name = \"t\", target = release, params = {\"version\": plan.repo})\n\
             workflow(type = \"playbook\", tasks = [plan, t])",
        )
        .unwrap();
        assert_eq!(
            the_call(&ok).task.label(),
            "call",
            "a GitHub target compiles like any other"
        );
    }

    #[test]
    fn a_mapped_call_binds_its_item_and_is_bounded() {
        let pack = pack(false);
        let workflow = compile(
            pack.path(),
            r#"
t = call(name = "t", target = triage, params = {"repo": item()}, over = each(plan.items, max = 8))
workflow(type = "playbook", tasks = [plan, t])
"#,
        )
        .unwrap();
        let call = the_call(&workflow);
        assert_eq!(call.max_fanout, Some(8));
        let TaskKind::Call { bindings, .. } = &call.task else {
            panic!()
        };
        assert_eq!(bindings["repo"], Binding::Item { field: None });
        let wide = compile(
            pack.path(),
            r#"
t = call(name = "t", target = triage, params = {"repo": item()}, over = each(plan.items, max = 65))
workflow(type = "playbook", tasks = [plan, t])
"#,
        )
        .unwrap_err();
        assert!(wide.contains("64"), "{wide}");
    }

    #[test]
    fn json_and_file_params_bind_only_by_param_inside_a_call() {
        let pack = pack(false);
        let source = &format!(
            "{}{CALLS}{}",
            r#"
params = {
    "matrix": {"type": "json", "required": True},
    "notes": {"type": "file", "required": True, "max_bytes": 1024},
}
"#,
            r#"plan = command(name = "plan", run = "true", emits = {"repo": "string"})
t = call(
    name = "t",
    target = triage,
    params = {"repo": plan.repo, "matrix": param("matrix"), "notes": param("notes")},
)
workflow(type = "playbook", tasks = [plan, t])
"#
        );
        let notes = pack.path().join("notes.md");
        std::fs::write(&notes, "hello").unwrap();
        let supplied = BTreeMap::from([
            ("matrix".to_owned(), r#"{"os": ["linux"]}"#.to_owned()),
            ("notes".to_owned(), notes.display().to_string()),
        ]);
        let compiled =
            compile_source_with(source, Path::new("workflow.star"), pack.path(), &supplied)
                .unwrap();
        let TaskKind::Call {
            bindings,
            file_params,
            ..
        } = &the_call(&compiled.workflow).task
        else {
            panic!()
        };
        assert_eq!(
            bindings["matrix"],
            Binding::Param {
                name: "matrix".into()
            }
        );
        assert!(file_params.contains("notes"));
        assert_eq!(
            compiled.workflow.params["notes"].json()["digest"],
            json!(crucible_contract::artifact::content_digest(b"hello"))
        );

        let swapped = source.replace(
            r#""matrix": param("matrix"), "notes": param("notes")"#,
            r#""matrix": param("notes"), "notes": param("matrix")"#,
        );
        let error =
            compile_source_with(&swapped, Path::new("workflow.star"), pack.path(), &supplied)
                .unwrap_err()
                .message();
        assert!(error.contains("not a json one"), "{error}");

        let outside = source.replace(
            "plan = command(name = \"plan\", run = \"true\",",
            "plan = command(name = \"plan\", run = param(\"matrix\"),",
        );
        let error =
            compile_source_with(&outside, Path::new("workflow.star"), pack.path(), &supplied)
                .unwrap_err()
                .message();
        assert!(error.contains("json or file parameter"), "{error}");
    }

    #[test]
    fn a_call_binds_another_calls_result_field_and_file() {
        let pack = pack(false);
        let chained = |second: &str| {
            compile(
                pack.path(),
                &format!(
                    "first = call(name = \"first\", target = triage, params = {{\"repo\": plan.repo}})\n\
                     second = call(name = \"second\", target = triage, params = {second})\n\
                     workflow(type = \"playbook\", tasks = [plan, first, second])"
                ),
            )
        };
        let workflow =
            chained(r#"{"repo": first.verdict, "notes": first.files["report.md"]}"#).unwrap();
        let second = workflow
            .tasks
            .iter()
            .find(|t| t.name.0 == "second")
            .unwrap();
        let TaskKind::Call {
            bindings,
            file_params,
            ..
        } = &second.task
        else {
            panic!()
        };
        assert_eq!(
            bindings["repo"],
            Binding::Field {
                producer: TaskName("first".into()),
                field: OutputField("verdict".into()),
            }
        );
        assert_eq!(
            bindings["notes"],
            Binding::File {
                producer: TaskName("first".into()),
                path: "report.md".into(),
            }
        );
        assert!(file_params.contains("notes"));
        assert_eq!(second.depends_on, [TaskName("first".into())]);

        for (params, says) in [
            (
                r#"{"repo": first.verdict, "notes": first.files["notes.md"]}"#,
                "does not declare",
            ),
            (
                r#"{"repo": first.verdict, "count": first.score}"#,
                "number field cannot feed",
            ),
            (
                r#"{"repo": first.files["report.md"]}"#,
                "only a file parameter",
            ),
        ] {
            let error = chained(params).unwrap_err();
            assert!(error.contains(says), "{params}: {error}");
        }
    }

    #[test]
    fn an_agent_reads_json_and_file_params_only_inside_the_external_markers() {
        use crate::plan::starlark::{EXTERNAL_CLOSE, EXTERNAL_OPEN};
        let pack = pack(false);
        let notes = pack.path().join("notes.md");
        std::fs::write(&notes, "hello").unwrap();
        let supplied = BTreeMap::from([
            ("matrix".to_owned(), r#"{"os": ["linux"]}"#.to_owned()),
            ("notes".to_owned(), notes.display().to_string()),
        ]);
        let compiled = |body: &str| {
            let source = format!(
                "params = {{\n    \"matrix\": {{\"type\": \"json\", \"required\": True}},\n    \
                 \"notes\": {{\"type\": \"file\", \"required\": True, \"max_bytes\": 1024}},\n}}\n\
                 {body}"
            );
            compile_source_with(&source, Path::new("workflow.star"), pack.path(), &supplied)
                .map(|c| c.workflow)
                .map_err(|e| e.message())
        };
        let workflow = compiled(
            "a = agent(name = \"a\", prompt = \"Plan for \" + param(\"matrix\") + \" using \" + param(\"notes\"))\n\
             b = agent(name = \"b\", prompt = param(\"matrix\"))\n\
             workflow(type = \"playbook\", tasks = [a, b])",
        )
        .unwrap();
        let prompt = |name: &str| {
            let task = workflow.tasks.iter().find(|t| t.name.0 == name).unwrap();
            let TaskKind::Agent { prompt, .. } = &task.task else {
                panic!()
            };
            prompt.clone()
        };
        let json = serde_json::to_string_pretty(&json!({"os": ["linux"]})).unwrap();
        let file = format!(
            "file parameter notes, staged read-only at `inputs/params/notes/notes.md`, 5 bytes, {}",
            crucible_contract::artifact::content_digest(b"hello")
        );
        assert_eq!(
            prompt("a"),
            format!(
                "Plan for {EXTERNAL_OPEN}{json}{EXTERNAL_CLOSE} using {EXTERNAL_OPEN}{file}{EXTERNAL_CLOSE}"
            )
        );
        assert_eq!(
            prompt("b"),
            format!("{EXTERNAL_OPEN}{json}{EXTERNAL_CLOSE}")
        );

        for (body, says) in [
            (
                "a = agent(name = \"a\", prompt = \"Plan for %s\" % param(\"matrix\"))",
                "",
            ),
            (
                "a = agent(name = \"a\", prompt = \"Plan for \" + str(param(\"matrix\")))",
                "",
            ),
            (
                "a = command(name = \"a\", run = \"cat \" + param(\"notes\"))",
                "prompt",
            ),
            (
                "a = agent(name = \"a\", prompt = \"go\", emits = {\"x\": param(\"matrix\")})",
                "json or file parameter",
            ),
        ] {
            let error = compiled(&format!(
                "{body}\nworkflow(type = \"playbook\", tasks = [a])"
            ))
            .unwrap_err();
            assert!(error.contains(says), "{body}: {error}");
            assert!(!error.contains("linux"), "{body} shows the value: {error}");
        }
    }

    #[test]
    fn every_compile_path_checks_the_lock_first() {
        let source = format!(
            "params = {{\"x\": {{\"type\": \"no such type\"}}}}\n{CALLS}{PLAN}\n\
             workflow(type = \"playbook\", tasks = [plan])\n"
        );
        let unlocked = tempfile::tempdir().unwrap();
        let stale = pack(false);
        let mut lock =
            CallsLock::parse(&std::fs::read_to_string(stale.path().join(LOCK_FILE)).unwrap())
                .unwrap();
        lock.calls.remove(&CallAlias::parse("release").unwrap());
        std::fs::write(stale.path().join(LOCK_FILE), lock.render().unwrap()).unwrap();
        for (dir, says) in [
            (unlocked.path(), "cannot be read"),
            (stale.path(), "release"),
        ] {
            let file = dir.join("workflow.star");
            std::fs::write(&file, &source).unwrap();
            let compiled = [
                compile_source_with(&source, &file, dir, &BTreeMap::new()),
                crate::plan::starlark::compile_launch_file(&file, dir, &BTreeMap::new()),
                crate::plan::starlark::compile_file_with(&file, dir, &BTreeMap::new()),
            ];
            for result in compiled {
                let error = result.unwrap_err();
                assert!(
                    matches!(
                        error,
                        crate::plan::starlark::error::CompileError::CallsUnpinned { .. }
                    ),
                    "{}",
                    error.message()
                );
                assert!(error.message().contains(says), "{}", error.message());
            }
        }
    }

    #[test]
    fn budget_needs_a_ceiling() {
        let pack = pack(false);
        for (budget, says) in [
            ("budget()", "set cost"),
            ("budget(cost = -1)", "positive"),
            ("budget(wall_clock = \"soon\")", "duration"),
        ] {
            let error = compile(
                pack.path(),
                &format!(
                    "t = call(name = \"t\", target = triage, params = {{\"repo\": \"a\"}}, \
                     budget = {budget})\nworkflow(type = \"playbook\", tasks = [plan, t])"
                ),
            )
            .unwrap_err();
            assert!(error.contains(says), "{budget}: {error}");
        }
    }

    #[test]
    fn autoresearch_and_revise_loops_refuse_a_call() {
        let pack = pack(false);
        let revised = compile(
            pack.path(),
            r#"
t = call(name = "t", target = triage, params = {"repo": "a"})
review = command(name = "review", run = "true", depends_on = [t], revise = send_back([t], rounds = 2))
workflow(type = "playbook", tasks = [plan, t, review])
"#,
        )
        .unwrap_err();
        assert!(revised.contains("revise"), "{revised}");
        let scored = compile(
            pack.path(),
            r#"
t = call(name = "t", target = triage, params = {"repo": "a"})
workflow(type = "autoresearch", tasks = [plan, t])
"#,
        )
        .unwrap_err();
        assert!(scored.contains("call"), "{scored}");
    }

    fn watch_pack() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let watched = TargetInterface {
            params: json!({
                "type": "object",
                "properties": {
                    "commit": {"type": "string"},
                    "appear": {"type": "string", "default": "10m"},
                },
                "required": ["commit"],
            }),
            result: vec![
                EmitWire {
                    field: "runs".into(),
                    ty: Some(FieldType::List),
                },
                EmitWire {
                    field: "server".into(),
                    ty: Some(FieldType::Object),
                },
            ],
            result_files: vec![],
            graph: ProgressGraph::default(),
        };
        let lock = CallsLock {
            version: LOCK_VERSION,
            calls: BTreeMap::from([
                (
                    CallAlias::parse("konflux").unwrap(),
                    LockedCall::new(Pin::TektonWatch, watched).unwrap(),
                ),
                (
                    CallAlias::parse("release").unwrap(),
                    LockedCall::new(
                        Pin::GithubRef {
                            repo: GithubRepo::parse("acme/app").unwrap(),
                            workflow: WorkflowPath::parse(".github/workflows/release.yml").unwrap(),
                            git_ref: GitRefName::parse("main").unwrap(),
                            commit: "abc".into(),
                        },
                        github_interface(false),
                    )
                    .unwrap(),
                ),
            ]),
        };
        std::fs::write(dir.path().join(LOCK_FILE), lock.render().unwrap()).unwrap();
        dir
    }

    const WATCH_CALLS: &str = r#"load("@crucible//calls", "artifact_outputs", "github_workflow", "pipeline_run", "tekton")

konflux = tekton(
    cluster = "konflux",
    namespace = "tenant",
    repository = "acme-app",
    expected = [pipeline_run("server", results = ["IMAGE_DIGEST"])],
)
release = github_workflow(
    repo = "acme/app",
    workflow = ".github/workflows/release.yml",
    ref = "main",
    outputs = artifact_outputs(artifact = "result"),
)
push = command(name = "push", run = "true", emits = {"commit": "string"})
"#;

    fn compile_watch(body: &str) -> Result<crate::plan::workflow::WorkflowCfg, String> {
        let pack = watch_pack();
        let source = format!("{WATCH_CALLS}\n{body}");
        compile_source(&source, Path::new("workflow.star"), pack.path())
            .map(|c| c.workflow)
            .map_err(|e| e.message())
    }

    fn gitlab_pack(file_param: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let mut properties = json!({
            "target": {"type": "string", "enum": ["staging", "production"]},
            "tags": {"type": "array", "items": {"type": "string"}},
        });
        if file_param {
            properties["bundle"] = json!({TYPE_KEY: "file"});
        }
        let interface = TargetInterface {
            params: json!({
                "type": "object",
                "properties": properties,
                "required": ["target"],
            }),
            result: vec![EmitWire {
                field: "version".into(),
                ty: Some(FieldType::String),
            }],
            result_files: vec![],
            graph: ProgressGraph::default(),
        };
        let lock = CallsLock {
            version: LOCK_VERSION,
            calls: BTreeMap::from([(
                CallAlias::parse("deploy").unwrap(),
                LockedCall::new(
                    Pin::GitlabRef {
                        host: "gitlab.example.com".into(),
                        project: "group/app".into(),
                        git_ref: GitRefName::parse("main").unwrap(),
                        commit: "abc".into(),
                    },
                    interface,
                )
                .unwrap(),
            )]),
        };
        std::fs::write(dir.path().join(LOCK_FILE), lock.render().unwrap()).unwrap();
        dir
    }

    const GITLAB_CALLS: &str = r#"load("@crucible//calls", "gitlab_pipeline", "job_outputs")

deploy = gitlab_pipeline(
    host = "gitlab.example.com",
    project = "group/app",
    ref = "main",
    outputs = job_outputs(job = "report", fields = {"version": "string"}),
)
"#;

    fn compile_gitlab(
        file_param: bool,
        body: &str,
    ) -> Result<crate::plan::workflow::WorkflowCfg, String> {
        let pack = gitlab_pack(file_param);
        let source = format!("{GITLAB_CALLS}\n{body}");
        compile_source(&source, Path::new("workflow.star"), pack.path())
            .map(|c| c.workflow)
            .map_err(|e| e.message())
    }

    #[test]
    fn a_gitlab_call_binds_typed_inputs_and_returns_its_fields() {
        let workflow = compile_gitlab(
            false,
            r#"
d = call(name = "ship", target = deploy, params = {"target": "staging", "tags": ["a", "b"]})
workflow(type = "playbook", tasks = [d], result = d)
"#,
        )
        .expect("compiles");
        let task = the_call(&workflow);
        let TaskKind::Call { bindings, .. } = &task.task else {
            panic!("a call");
        };
        assert_eq!(
            bindings.keys().map(String::as_str).collect::<Vec<_>>(),
            ["tags", "target"]
        );
        assert!(
            matches!(&task.emits, Emits::Typed(fields) if fields.contains_key(&OutputField("version".into())))
        );
    }

    #[test]
    fn a_gitlab_call_refuses_a_file_parameter() {
        let refused = compile_gitlab(
            true,
            r#"
d = call(name = "ship", target = deploy, params = {"target": "staging"})
workflow(type = "playbook", tasks = [d], result = d)
"#,
        )
        .unwrap_err();
        assert!(
            refused.contains("which a GitLab CI input cannot take"),
            "{refused}"
        );
    }

    #[test]
    fn a_watch_binds_its_commit_and_is_bounded_by_its_timeout() {
        let workflow = compile_watch(
            r#"
w = watch(name = "builds", target = konflux, commit = push.commit, timeout = "2h", appear = "5m")
workflow(type = "playbook", tasks = [push, w], result = w)
"#,
        )
        .expect("compiles");
        let task = the_call(&workflow);
        let TaskKind::Call {
            bindings, budget, ..
        } = &task.task
        else {
            panic!("a watch is a call");
        };
        assert_eq!(
            bindings.keys().map(String::as_str).collect::<Vec<_>>(),
            ["appear", "commit"]
        );
        assert_eq!(budget.and_then(|b| b.wall_clock_secs), Some(7200));
        assert!(task.depends_on.iter().any(|d| d.0 == "push"));
        assert!(
            matches!(&task.emits, Emits::Typed(fields) if fields.contains_key(&OutputField("server".into())))
        );
    }

    #[test]
    fn a_watch_and_a_call_each_refuse_the_others_target() {
        let call = compile_watch(
            r#"
c = call(name = "builds", target = konflux, params = {"commit": push.commit})
workflow(type = "playbook", tasks = [push, c], result = c)
"#,
        )
        .unwrap_err();
        assert!(call.contains("use watch()"), "{call}");
        let watch = compile_watch(
            r#"
w = watch(name = "rel", target = release, commit = push.commit, timeout = "1h")
workflow(type = "playbook", tasks = [push, w], result = w)
"#,
        )
        .unwrap_err();
        assert!(watch.contains("use call()"), "{watch}");
        let untimed = compile_watch(
            r#"
w = watch(name = "builds", target = konflux, commit = push.commit)
workflow(type = "playbook", tasks = [push, w], result = w)
"#,
        )
        .unwrap_err();
        assert!(untimed.contains("timeout"), "{untimed}");
    }
}
