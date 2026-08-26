//! What a playbook takes from whoever launches it.
//!
//! A `params` block is read out of the AST, never evaluated. That is the whole point of
//! requiring it to be a literal first statement: an operator, a launch form, or an orchestrator
//! validating an ask all need to know what a pack accepts without running a line of it.
//!
//! Values are bound during compilation, so the compiled graph carries no unresolved reference
//! and the frozen artifact is exactly what will run.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use starlark_syntax::syntax::AstModule;
use starlark_syntax::syntax::ast::{AstLiteral, Expr, Stmt};

use crate::plan::diag;
use crate::plan::starlark::CompileError;

type Result<T> = std::result::Result<T, CompileError>;

/// The types a parameter may take. Deliberately small: every one has an unambiguous spelling on
/// a command line and in JSON, which is what lets one declaration serve both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    String,
    Int,
    Number,
    Bool,
    /// Spelled as the declaration spells it, so the stored form and [`ParamType::parse`] agree.
    #[serde(rename = "list<string>")]
    StringList,
}

impl ParamType {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "string" => Some(ParamType::String),
            "int" => Some(ParamType::Int),
            "number" => Some(ParamType::Number),
            "bool" => Some(ParamType::Bool),
            "list<string>" => Some(ParamType::StringList),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ParamType::String => "string",
            ParamType::Int => "int",
            ParamType::Number => "number",
            ParamType::Bool => "bool",
            ParamType::StringList => "list<string>",
        }
    }

    fn numeric(self) -> bool {
        matches!(self, ParamType::Int | ParamType::Number)
    }
}

/// A bound parameter value.
///
/// Stores and reloads as the JSON the value already is (`42`, `true`, `["a"]`), not as a tagged
/// wrapper: see the hand-written codec below.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    String(String),
    Int(i32),
    Number(f64),
    Bool(bool),
    StringList(Vec<String>),
}

impl Serialize for ParamValue {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        self.json().serialize(serializer)
    }
}

/// Decoded by inspecting the value rather than by trying variants in order: `serde(untagged)`
/// cannot read a float back into an `f64` variant, and an integer must stay an integer instead of
/// widening to `42.0`.
impl<'de> Deserialize<'de> for ParamValue {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        use serde::de::Error as _;
        match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::String(s) => Ok(ParamValue::String(s)),
            serde_json::Value::Bool(b) => Ok(ParamValue::Bool(b)),
            serde_json::Value::Number(n) => match n.as_i64() {
                Some(i) => i32::try_from(i)
                    .map(ParamValue::Int)
                    .map_err(|_| D::Error::custom(format!("{i} does not fit in 32 bits"))),
                None => n
                    .as_f64()
                    .map(ParamValue::Number)
                    .ok_or_else(|| D::Error::custom("a parameter number must be finite")),
            },
            serde_json::Value::Array(items) => items
                .into_iter()
                .map(|i| match i {
                    serde_json::Value::String(s) => Ok(s),
                    other => Err(D::Error::custom(format!(
                        "a list parameter holds strings, got {other}"
                    ))),
                })
                .collect::<std::result::Result<Vec<String>, _>>()
                .map(ParamValue::StringList),
            other => Err(D::Error::custom(format!(
                "a parameter value is a string, int, number, bool, or list of strings, got {other}"
            ))),
        }
    }
}

impl ParamValue {
    pub fn json(&self) -> serde_json::Value {
        match self {
            ParamValue::String(s) => serde_json::Value::String(s.clone()),
            ParamValue::Int(n) => serde_json::json!(n),
            ParamValue::Number(n) => serde_json::json!(n),
            ParamValue::Bool(b) => serde_json::Value::Bool(*b),
            ParamValue::StringList(items) => serde_json::json!(items),
        }
    }
}

/// One declared parameter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamSpec {
    pub name: String,
    pub ty: ParamType,
    pub required: bool,
    pub default: Option<ParamValue>,
    pub doc: String,
    pub pattern: Option<String>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub choices: Vec<String>,
}

/// Every field a declaration may carry, for the did-you-mean on a typo.
const FIELDS: &[&str] = &[
    "type", "required", "default", "doc", "pattern", "min", "max", "choices",
];

/// Every parameter a source declares, in source order. Serializes as the bare list, so a launcher
/// can store the declarations it read and bind against them later without the engine in the loop.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Params(Vec<ParamSpec>);

impl Params {
    /// Read the block without evaluating the source.
    ///
    /// A source with no `params` declares none. A `params` that is not the first statement, or
    /// not a literal, is refused rather than ignored: silently skipping it would let a pack
    /// believe it declared parameters that no launcher can see.
    pub(crate) fn read(module: &AstModule) -> Result<Self> {
        let statement = module.statement();
        let statements: &[_] = match &statement.node {
            Stmt::Statements(statements) => statements,
            _ => std::slice::from_ref(statement),
        };
        let declared = statements.iter().position(|s| {
            matches!(&s.node, Stmt::Assign(assign)
                if matches!(&assign.lhs.node,
                    starlark_syntax::syntax::ast::AssignTarget::Identifier(name)
                        if name.node.ident == "params"))
        });
        let Some(position) = declared else {
            return Ok(Params::default());
        };
        if position != 0 {
            return Err(CompileError::ParamsNotFirst);
        }
        let Stmt::Assign(assign) = &statements[position].node else {
            return Ok(Params::default());
        };
        let Expr::Dict(entries) = &assign.rhs.node else {
            return Err(CompileError::ParamsNotLiteral {
                detail: "params must be a dictionary literal".to_string(),
            });
        };
        let mut specs = Vec::new();
        for (key, value) in entries {
            let name = literal_string(&key.node).ok_or(CompileError::ParamsNotLiteral {
                detail: "every parameter name must be a string literal".to_string(),
            })?;
            specs.push(spec(&name, &value.node)?);
        }
        Ok(Params(specs))
    }

    /// Bind supplied values, filling defaults and refusing what the declaration forbids.
    ///
    /// Everything a launcher supplies arrives as text, because a command line and a JSON form
    /// and an ask all hand over text. Parsing is the declaration's job, which is what keeps
    /// `max_steps = "many"` from reaching the graph.
    pub fn bind(
        &self,
        supplied: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, ParamValue>> {
        if let Some(unknown) = supplied
            .keys()
            .find(|k| !self.0.iter().any(|s| &&s.name == k))
        {
            return Err(CompileError::UnknownParam {
                name: unknown.clone(),
                suggestion: diag::suggest(unknown, self.0.iter().map(|s| s.name.as_str()))
                    .map(str::to_owned),
            });
        }
        let mut bound = BTreeMap::new();
        for spec in &self.0 {
            let value = match supplied.get(&spec.name) {
                Some(raw) => parse(spec, raw)?,
                None => match &spec.default {
                    Some(default) => default.clone(),
                    None => {
                        return Err(CompileError::MissingParam {
                            name: spec.name.clone(),
                            doc: spec.doc.clone(),
                        });
                    }
                },
            };
            check(spec, &value)?;
            bound.insert(spec.name.clone(), value);
        }
        Ok(bound)
    }

    /// The declarations, in the order the source wrote them.
    pub fn specs(&self) -> &[ParamSpec] {
        &self.0
    }

    /// Whether the source declares no parameters at all. A graph that is not a function of its
    /// launch arguments can be frozen; one that is must be compiled per run.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The declaration as a JSON Schema document, so one source serves command-line validation,
    /// ask validation, and a generated launch form.
    pub fn json_schema(&self) -> serde_json::Value {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for spec in &self.0 {
            let mut property = serde_json::Map::new();
            let (ty, items) = match spec.ty {
                ParamType::String => ("string", None),
                ParamType::Int => ("integer", None),
                ParamType::Number => ("number", None),
                ParamType::Bool => ("boolean", None),
                ParamType::StringList => ("array", Some(serde_json::json!({"type": "string"}))),
            };
            property.insert("type".into(), serde_json::json!(ty));
            if let Some(items) = items {
                property.insert("items".into(), items);
            }
            if !spec.doc.is_empty() {
                property.insert("description".into(), serde_json::json!(spec.doc));
            }
            if let Some(default) = &spec.default {
                property.insert("default".into(), default.json());
            }
            if let Some(pattern) = &spec.pattern {
                property.insert("pattern".into(), serde_json::json!(pattern));
            }
            if let Some(min) = spec.min {
                property.insert("minimum".into(), serde_json::json!(min));
            }
            if let Some(max) = spec.max {
                property.insert("maximum".into(), serde_json::json!(max));
            }
            if !spec.choices.is_empty() {
                property.insert("enum".into(), serde_json::json!(spec.choices));
            }
            if spec.required {
                required.push(spec.name.clone());
            }
            properties.insert(spec.name.clone(), serde_json::Value::Object(property));
        }
        serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "properties": properties,
            "required": required,
        })
    }
}

fn spec(name: &str, expression: &Expr) -> Result<ParamSpec> {
    let Expr::Dict(fields) = expression else {
        return Err(CompileError::ParamsNotLiteral {
            detail: format!("parameter {name:?} must be a dictionary literal"),
        });
    };
    let mut declared: BTreeMap<String, &Expr> = BTreeMap::new();
    for (key, value) in fields {
        let field = literal_string(&key.node).ok_or(CompileError::ParamsNotLiteral {
            detail: format!("every field of parameter {name:?} must be named by a string"),
        })?;
        if !FIELDS.contains(&field.as_str()) {
            return Err(CompileError::UnknownParamField {
                param: name.to_owned(),
                field: field.clone(),
                suggestion: diag::suggest(&field, FIELDS.iter().copied()).map(str::to_owned),
            });
        }
        declared.insert(field, &value.node);
    }

    let ty_name =
        field(&declared, name, "type", "a string literal", literal_string)?.ok_or_else(|| {
            CompileError::ParamNeedsType {
                param: name.to_owned(),
            }
        })?;
    let ty = ParamType::parse(&ty_name).ok_or_else(|| CompileError::UnknownParamType {
        param: name.to_owned(),
        got: ty_name.clone(),
        suggestion: diag::suggest(
            &ty_name,
            ["string", "int", "number", "bool", "list<string>"],
        )
        .map(str::to_owned),
    })?;

    let required =
        field(&declared, name, "required", "True or False", literal_bool)?.unwrap_or(false);
    let default = declared
        .get("default")
        .map(|e| literal_value(ty, e, name))
        .transpose()?;
    if required && default.is_some() {
        return Err(CompileError::ParamRequiredWithDefault {
            param: name.to_owned(),
        });
    }
    if !required && default.is_none() {
        return Err(CompileError::ParamNeedsDefault {
            param: name.to_owned(),
        });
    }

    let pattern = field(
        &declared,
        name,
        "pattern",
        "a string literal",
        literal_string,
    )?;
    if pattern.is_some() && ty != ParamType::String {
        return Err(CompileError::ParamConstraintMismatch {
            param: name.to_owned(),
            constraint: "pattern",
            ty: ty.as_str(),
        });
    }
    if let Some(pattern) = &pattern
        && let Err(error) = regex_lite::Regex::new(pattern)
    {
        return Err(CompileError::ParamBadPattern {
            param: name.to_owned(),
            detail: error.to_string(),
        });
    }
    let min = field(&declared, name, "min", "a number literal", literal_number)?;
    let max = field(&declared, name, "max", "a number literal", literal_number)?;
    if (min.is_some() || max.is_some()) && !ty.numeric() {
        return Err(CompileError::ParamConstraintMismatch {
            param: name.to_owned(),
            constraint: "min/max",
            ty: ty.as_str(),
        });
    }
    let choices = declared
        .get("choices")
        .map(|e| literal_strings(e, name))
        .transpose()?
        .unwrap_or_default();
    if !choices.is_empty() && ty != ParamType::String {
        return Err(CompileError::ParamConstraintMismatch {
            param: name.to_owned(),
            constraint: "choices",
            ty: ty.as_str(),
        });
    }

    Ok(ParamSpec {
        name: name.to_owned(),
        ty,
        required,
        default,
        doc: field(&declared, name, "doc", "a string literal", literal_string)?.unwrap_or_default(),
        pattern,
        min,
        max,
        choices,
    })
}

/// Read one declared field. A field that is absent and a field whose value is not the literal
/// shape the declaration needs are different facts, so a wrong shape is refused rather than
/// falling back to the absent case's default.
fn field<T>(
    declared: &BTreeMap<String, &Expr>,
    param: &str,
    name: &'static str,
    expected: &'static str,
    read: impl Fn(&Expr) -> Option<T>,
) -> Result<Option<T>> {
    match declared.get(name) {
        None => Ok(None),
        Some(expression) => match read(expression) {
            Some(value) => Ok(Some(value)),
            None => Err(CompileError::ParamFieldWrongShape {
                param: param.to_owned(),
                field: name,
                expected,
            }),
        },
    }
}

fn literal_string(expression: &Expr) -> Option<String> {
    match expression {
        Expr::Literal(AstLiteral::String(s)) => Some(s.node.clone()),
        _ => None,
    }
}

fn literal_bool(expression: &Expr) -> Option<bool> {
    match expression {
        Expr::Identifier(name) if name.node.ident == "True" => Some(true),
        Expr::Identifier(name) if name.node.ident == "False" => Some(false),
        _ => None,
    }
}

fn literal_number(expression: &Expr) -> Option<f64> {
    match expression {
        Expr::Minus(inner) => literal_number(&inner.node).map(|n| -n),
        Expr::Literal(AstLiteral::Int(n)) => match &n.node {
            starlark_syntax::lexer::TokenInt::I32(v) => Some(f64::from(*v)),
            starlark_syntax::lexer::TokenInt::BigInt(_) => None,
        },
        Expr::Literal(AstLiteral::Float(f)) => Some(f.node),
        _ => None,
    }
}

fn literal_strings(expression: &Expr, param: &str) -> Result<Vec<String>> {
    let Expr::List(items) = expression else {
        return Err(CompileError::ParamsNotLiteral {
            detail: format!("parameter {param:?} choices must be a list of string literals"),
        });
    };
    items
        .iter()
        .map(|item| {
            literal_string(&item.node).ok_or(CompileError::ParamsNotLiteral {
                detail: format!("parameter {param:?} choices must be string literals"),
            })
        })
        .collect()
}

fn literal_value(ty: ParamType, expression: &Expr, param: &str) -> Result<ParamValue> {
    let wrong = || CompileError::ParamDefaultWrongType {
        param: param.to_owned(),
        ty: ty.as_str(),
    };
    match ty {
        ParamType::String => literal_string(expression)
            .map(ParamValue::String)
            .ok_or_else(wrong),
        ParamType::Bool => literal_bool(expression)
            .map(ParamValue::Bool)
            .ok_or_else(wrong),
        ParamType::Int => literal_number(expression)
            .filter(|n| n.fract() == 0.0 && *n >= f64::from(i32::MIN) && *n <= f64::from(i32::MAX))
            .map(|n| ParamValue::Int(n as i32))
            .ok_or_else(wrong),
        ParamType::Number => literal_number(expression)
            .map(ParamValue::Number)
            .ok_or_else(wrong),
        ParamType::StringList => literal_strings(expression, param).map(ParamValue::StringList),
    }
}

/// Turn what a launcher typed into the declared type.
fn parse(spec: &ParamSpec, raw: &str) -> Result<ParamValue> {
    let wrong = |expected: &str| CompileError::ParamValueWrongType {
        param: spec.name.clone(),
        got: raw.to_owned(),
        expected: expected.to_owned(),
    };
    match spec.ty {
        ParamType::String => Ok(ParamValue::String(raw.to_owned())),
        ParamType::Bool => match raw {
            "true" | "True" => Ok(ParamValue::Bool(true)),
            "false" | "False" => Ok(ParamValue::Bool(false)),
            _ => Err(wrong("true or false")),
        },
        ParamType::Int => raw
            .parse::<i32>()
            .map(ParamValue::Int)
            .map_err(|_| wrong("a whole number")),
        ParamType::Number => raw
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .map(ParamValue::Number)
            .ok_or_else(|| wrong("a number")),
        // A JSON array where the items might contain commas, a comma-separated list where they
        // do not. Both spellings are unambiguous on sight, which a bare split is not.
        ParamType::StringList => {
            if raw.trim_start().starts_with('[') {
                let items: Vec<String> = crucible_contract::json::from_str(raw)
                    .map_err(|_| wrong("a JSON array of strings"))?;
                Ok(ParamValue::StringList(items))
            } else {
                Ok(ParamValue::StringList(
                    raw.split(',').map(|s| s.trim().to_owned()).collect(),
                ))
            }
        }
    }
}

/// Enforce the declared constraint on a bound value, whether it was supplied or defaulted.
fn check(spec: &ParamSpec, value: &ParamValue) -> Result<()> {
    let out_of_range =
        |n: f64| spec.min.is_some_and(|min| n < min) || spec.max.is_some_and(|max| n > max);
    match value {
        ParamValue::String(s) => {
            if let Some(pattern) = &spec.pattern
                && !regex_lite::Regex::new(pattern).is_ok_and(|regex| regex.is_match(s))
            {
                return Err(CompileError::ParamConstraintFailed {
                    param: spec.name.clone(),
                    got: s.clone(),
                    detail: format!("must match {pattern}"),
                });
            }
            if !spec.choices.is_empty() && !spec.choices.contains(s) {
                return Err(CompileError::ParamConstraintFailed {
                    param: spec.name.clone(),
                    got: s.clone(),
                    detail: format!("must be one of {}", spec.choices.join(", ")),
                });
            }
        }
        ParamValue::Int(n) if out_of_range(f64::from(*n)) => {
            return Err(CompileError::ParamConstraintFailed {
                param: spec.name.clone(),
                got: n.to_string(),
                detail: range(spec),
            });
        }
        ParamValue::Number(n) if out_of_range(*n) => {
            return Err(CompileError::ParamConstraintFailed {
                param: spec.name.clone(),
                got: n.to_string(),
                detail: range(spec),
            });
        }
        _ => {}
    }
    Ok(())
}

fn range(spec: &ParamSpec) -> String {
    match (spec.min, spec.max) {
        (Some(min), Some(max)) => format!("must be between {min} and {max}"),
        (Some(min), None) => format!("must be at least {min}"),
        (None, Some(max)) => format!("must be at most {max}"),
        (None, None) => "must be in range".to_string(),
    }
}
